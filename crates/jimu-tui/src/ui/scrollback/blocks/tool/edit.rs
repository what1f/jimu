//! # Progressive highlight
//!
//! First paint uses per-hunk syntect (fast).
//! When the post-edit file is available and under size/line caps, a background worker upgrades to full-file-scoped styles.
//! Mid-file multi-line scopes (e.g. a closing `"""`) then paint correctly.
//!
//! # Caps ([`EDIT_HL_MAX_BYTES`] / [`EDIT_HL_MAX_LINES`])
//!
//! Full-file HL costs up to **O(file lines)** in syntect, not O(hunk lines).
//! The walk stops at the last hunk line, but a hunk near EOF pays for the whole file.
//! Caps keep background work bounded so a multi-megabyte monorepo dump never freezes the worker or balloons the style map:
//!
//! | Gate | Default | On exceed |
//! |------|---------|-----------|
//! | File bytes | 2 MiB | stay [`EditHighlightPhase::HunkOnly`] |
//! | Line count | 50_000 | stay hunk-only |
//!
//! Cost magnitudes: see `benches/edit_highlight`.
//! Hunk-only first paint is cheap; full-file runs once per upgrade; the naïve prefix-per-hunk approach is not shipped.

use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use similar::ChangeTag;
#[cfg(feature = "syntax")]
use syntect::easy::HighlightLines;
#[cfg(feature = "syntax")]
use syntect::highlighting::Style as SyntectStyle;

use super::TOOL_HEADER_RANGE;
use crate::engine::diff::{DiffHunk, diff_hunks_to_patch};
#[cfg(feature = "syntax")]
use crate::engine::render::syntax::{Syntect, get_syntect};
use crate::engine::theme::{Theme, ThemeKind};
use crate::ui::scrollback::appearance::AppearanceConfig;
use crate::ui::scrollback::block::BlockContent;
use crate::ui::scrollback::types::{
    AccentStyle, BlockBackground, BlockContext, BlockLine, BlockOutput, DisplayMode,
    RenderedBlockOutput, Selectable, SelectionBoundaries, SelectionBoundary,
    SelectionBoundaryEntry,
};

/// Skip full-file HL when the post-edit file exceeds this size (2 MiB).
/// Full-file syntect on multi-MB sources is poor background work vs staying hunk-only; see `benches/edit_highlight`.
pub const EDIT_HL_MAX_BYTES: u64 = 2 * 1024 * 1024;
/// Skip full-file HL when the post-edit file has more lines than this (50k).
/// Larger files stay hunk-only so the worker never does an unbounded walk over a dump.
pub const EDIT_HL_MAX_LINES: usize = 50_000;

/// Content spans for one source line: FG styles only (height-neutral).
pub type EditLineStyles = Vec<(Style, String)>;

/// Progressive syntax-highlight state for an edit block.
/// `HunkOnly` / `Pending` use per-hunk syntect; `FileScoped` maps full-file FG styles onto Equal/Insert hunk text (Deletes keep per-hunk syntect).
/// Clone via [`Arc`].
#[derive(Debug, Clone, Default)]
pub enum EditHighlightPhase {
    #[default]
    HunkOnly,
    /// Full-file job in flight; paint is still hunk-only.
    Pending { job_id: u64 },
    /// Precomputed full-file styles keyed by 1-based new-file line.
    FileScoped {
        by_new_line: Arc<HashMap<usize, EditLineStyles>>,
        /// Theme the styles were baked under; paint falls back to hunk-only on mismatch so a runtime theme flip never mixes palettes.
        theme: ThemeKind,
    },
}

/// Configuration for diff rendering (tweakable for dev testing).
#[derive(Debug, Clone)]
pub struct DiffRenderConfig {
    /// If true, add 2-char indent before line numbers.
    pub indent: bool,
    /// If true, background extends from line start (including gutter).
    /// If false (default), background only covers the content area.
    pub gutter_bg: bool,
    /// If true, skip indent columns in background (keep them clean).
    /// If false, include indent in background.
    pub indent_bg: bool,
    /// Separator string between hunks.
    /// Options: "───" (line), "…" (ellipsis), "⋯" (midline ellipsis), "" (none).
    pub hunk_separator: String,
    /// Show two line-number columns (old and new) like GitHub's unified diff.
    /// When false (default), show a single column with the new-file line number.
    pub dual_line_numbers: bool,
}

impl Default for DiffRenderConfig {
    fn default() -> Self {
        Self {
            indent: true,
            gutter_bg: false,
            indent_bg: false,
            hunk_separator: "…".to_string(),
            dual_line_numbers: false,
        }
    }
}

/// Layout constants.
const INDENT: &str = "  ";
const GUTTER_GAP: &str = " ";
const CONTENT_GAP: &str = "  ";

/// A rendered diff line with optional background color.
pub struct DiffLineOutput {
    pub line: Line<'static>,
    pub background: Option<Color>,
    /// Column where background starts (for partial background).
    pub content_start_col: u16,
    /// Number of gutter spans at the start of this line (for selection exclusion).
    pub gutter_span_count: usize,
    /// Raw content text for this line (excluding gutter).
    pub content_text: String,
    /// Joiner for soft-wrap reconstruction (None = hard break, Some = continuation).
    pub joiner: Option<String>,
    /// True for hunk separator lines (not selectable, break range continuity).
    pub is_separator: bool,
}

/// Render a diff hunk to lines with syntax highlighting.
///
/// Returns lines with optional background colors for insert/delete.
pub fn render_diff_hunk_highlighted(
    hunk: &DiffHunk,
    path: &Path,
    theme: &Theme,
    width: u16,
    config: &DiffRenderConfig,
) -> Vec<DiffLineOutput> {
    render_diff_hunks_core(std::slice::from_ref(hunk), path, None, theme, width, config)
}

/// Render multiple hunks with separators, returning lines with backgrounds.
pub fn render_diff_hunks_highlighted(
    hunks: &[DiffHunk],
    path: &Path,
    theme: &Theme,
    width: u16,
    config: &DiffRenderConfig,
) -> Vec<DiffLineOutput> {
    render_diff_hunks_core(hunks, path, None, theme, width, config)
}

/// The single hunk walker behind both public fronts. Every line renders its per-hunk syntect spans, keeping the
/// highlighter state exactly as in the hunk-only phase.
fn render_diff_hunks_core(
    hunks: &[DiffHunk],
    path: &Path,
    by_new_line: Option<&HashMap<usize, EditLineStyles>>,
    theme: &Theme,
    width: u16,
    config: &DiffRenderConfig,
) -> Vec<DiffLineOutput> {
    #[cfg(feature = "syntax")]
    let syntect = get_syntect();
    #[cfg(not(feature = "syntax"))]
    let _ = path;
    let mut lines = Vec::new();

    for (i, hunk) in hunks.iter().enumerate() {
        if i > 0 && !lines.is_empty() && !config.hunk_separator.is_empty() {
            // Add separator between hunks (no background)
            let indent = if config.indent { INDENT } else { "" };
            let sep_text = match i
                .checked_sub(1)
                .and_then(|j| hunks.get(j))
                .and_then(|prev| hunk_gap_lines(prev, hunk))
            {
                Some(1) => format!("{} 1 unchanged line", config.hunk_separator),
                Some(n) => format!("{} {n} unchanged lines", config.hunk_separator),
                None => config.hunk_separator.clone(),
            };
            let sep_line = Line::from(vec![
                Span::raw(indent),
                Span::styled(sep_text, theme.muted()),
            ]);
            lines.push(DiffLineOutput {
                line: sep_line,
                background: None,
                content_start_col: 0,
                gutter_span_count: 0,
                content_text: String::new(),
                joiner: None,
                is_separator: true,
            });
        }
        if hunk.is_empty() {
            continue;
        }
        let layout = gutter_layout(hunk, config);
        let indent_width = if config.indent { INDENT.len() } else { 0 };
        let content_width = (width as usize).saturating_sub(layout.total);
        // A diff interleaves two file versions; give each side its own highlighter so a multi-line construct can't leak across sides
        // Equal lines render on the new side and advance both
        #[cfg(feature = "syntax")]
        let mut old_highlighter = syntect.highlight_lines_by_file_path(path);
        #[cfg(feature = "syntax")]
        let mut new_highlighter = syntect.highlight_lines_by_file_path(path);
        for line in hunk {
            let trimmed = line.text.trim_end_matches(['\r', '\n']);
            let text = crate::ui::scrollback::appearance::expand_tabs(trimmed);
            // Cold spans render unconditionally so Delete lines and any map miss (text drift) paint exactly like the hunk-only phase
            #[cfg(feature = "syntax")]
            let mut content_spans = match line.tag {
                ChangeTag::Delete => {
                    render_content_spans(&text, line.tag, theme, &mut old_highlighter, syntect)
                }
                ChangeTag::Insert => {
                    render_content_spans(&text, line.tag, theme, &mut new_highlighter, syntect)
                }
                ChangeTag::Equal => {
                    let spans =
                        render_content_spans(&text, line.tag, theme, &mut new_highlighter, syntect);
                    advance_highlighter(&mut old_highlighter, &text, syntect);
                    spans
                }
            };
            #[cfg(not(feature = "syntax"))]
            let mut content_spans = vec![painted(&text, plain_diff_style(line.tag, theme))];
            if let Some(map) = by_new_line
                && let Some(spans) = map_spans_for_line(line, &text, map, theme)
            {
                content_spans = spans;
            }
            lines.extend(assemble_diff_line_outputs(
                line,
                content_spans,
                &layout,
                indent_width,
                content_width,
                theme,
                config,
            ));
        }
    }

    lines
}

/// Unchanged new-file lines hidden between two hunks, when computable. Each call's hunks are numbered against its
/// own file snapshot, so a count would be wrong there.
fn hunk_gap_lines(prev: &DiffHunk, next: &DiffHunk) -> Option<usize> {
    let prev_last = prev.iter().rev().find(|l| l.tag != ChangeTag::Delete)?.ln;
    let next_first = next.iter().find(|l| l.tag != ChangeTag::Delete)?.ln;
    next_first
        .checked_sub(prev_last)
        .and_then(|d| d.checked_sub(1))
        .filter(|n| *n > 0)
}

/// Tab-expanded text for Equal/Insert hunk lines, keyed by 1-based ln.
#[cfg(feature = "syntax")]
fn hunk_new_line_texts(hunks: &[DiffHunk]) -> HashMap<usize, String> {
    let mut out = HashMap::new();
    for hunk in hunks {
        for line in hunk {
            if matches!(line.tag, ChangeTag::Equal | ChangeTag::Insert) && line.ln > 0 {
                let trimmed = line.text.trim_end_matches(['\r', '\n']);
                out.insert(
                    line.ln,
                    crate::ui::scrollback::appearance::expand_tabs(trimmed).into_owned(),
                );
            }
        }
    }
    out
}

/// Whether `file_text` is within the full-file HL caps (bytes + lines).
pub fn file_text_within_hl_caps(file_text: &str) -> bool {
    if file_text.len() as u64 > EDIT_HL_MAX_BYTES {
        return false;
    }
    let lines = if file_text.is_empty() {
        0
    } else {
        file_text.bytes().filter(|&b| b == b'\n').count() + usize::from(!file_text.ends_with('\n'))
    };
    lines <= EDIT_HL_MAX_LINES
}

/// Full-file HL once; keep new-side lines referenced by `hunks`. Only Equal/Insert lines present in `hunks` are
/// retained. Returns `None` if any needed disk line differs from hunk text (so upgrade never rewrites displayed
/// content).
#[cfg(feature = "syntax")]
pub fn compute_file_scoped_styles(
    path: &Path,
    file_text: &str,
    hunks: &[DiffHunk],
) -> Option<HashMap<usize, EditLineStyles>> {
    let expected = hunk_new_line_texts(hunks);
    if expected.is_empty() {
        return Some(HashMap::new());
    }

    let syntect = get_syntect();
    let mut highlighter = syntect.highlight_lines_by_file_path(path)?;

    // Lines past the last needed one cannot affect earlier syntect state.
    let max_needed = expected.keys().copied().max().unwrap_or(0);
    let mut out = HashMap::with_capacity(expected.len());
    for (idx, line) in file_text.lines().enumerate() {
        let ln = idx + 1;
        if ln > max_needed {
            break;
        }
        let expanded = crate::ui::scrollback::appearance::expand_tabs(line);
        let owned = format!("{expanded}\n");
        let ranges = highlighter
            .highlight_line(&owned, &syntect.syntax_set)
            .unwrap_or_default();
        let Some(expected_text) = expected.get(&ln) else {
            continue;
        };
        if expanded.as_ref() != expected_text.as_str() {
            return None;
        }
        let mut spans = Vec::new();
        for (style, segment) in ranges {
            let mut text = segment.to_owned();
            while text.ends_with('\n') || text.ends_with('\r') {
                text.pop();
            }
            if text.is_empty() {
                continue;
            }
            spans.push((syntect_to_ratatui_fg(style), text));
        }
        if spans.is_empty() {
            spans.push((Style::default(), String::new()));
        }
        out.insert(ln, spans);
    }
    if out.len() != expected.len() {
        return None;
    }
    Some(out)
}

#[cfg(not(feature = "syntax"))]
pub fn compute_file_scoped_styles(
    _path: &Path,
    _file_text: &str,
    _hunks: &[DiffHunk],
) -> Option<HashMap<usize, EditLineStyles>> {
    None
}

/// Render hunks with precomputed full-file styles (FileScoped). The map only overrides Equal/Insert foregrounds.
pub fn render_diff_hunks_with_styles(
    hunks: &[DiffHunk],
    path: &Path,
    by_new_line: &HashMap<usize, EditLineStyles>,
    theme: &Theme,
    width: u16,
    config: &DiffRenderConfig,
) -> Vec<DiffLineOutput> {
    render_diff_hunks_core(hunks, path, Some(by_new_line), theme, width, config)
}

/// Full-file map spans for one line, when they may override the cold spans. Equal always; Insert only on banded
/// themes (bandless paints changed lines with a solid line FG). Delete never: it keeps the per-hunk syntect paint
/// the user already saw.
fn map_spans_for_line(
    line: &crate::engine::diff::DiffLine,
    expanded: &str,
    by_new_line: &HashMap<usize, EditLineStyles>,
    theme: &Theme,
) -> Option<Vec<Span<'static>>> {
    let overridable = match line.tag {
        ChangeTag::Equal => true,
        ChangeTag::Insert => !theme.diff_uses_line_fg(),
        ChangeTag::Delete => false,
    };
    if !overridable {
        return None;
    }
    let styles = by_new_line.get(&line.ln)?;
    let joined: String = styles.iter().map(|(_, t)| t.as_str()).collect();
    if joined != expanded {
        return None;
    }
    let spans: Vec<Span<'static>> = styles
        .iter()
        .filter(|(_, t)| !t.is_empty())
        .map(|(st, t)| Span::styled(t.clone(), *st))
        .collect();
    if spans.is_empty() {
        return None;
    }
    Some(spans)
}

/// Assemble gutter + content spans into one or more wrapped [`DiffLineOutput`]s.
fn assemble_diff_line_outputs(
    line: &crate::engine::diff::DiffLine,
    content_spans: Vec<Span<'static>>,
    layout: &GutterLayout,
    indent_width: usize,
    content_width: usize,
    theme: &Theme,
    config: &DiffRenderConfig,
) -> Vec<DiffLineOutput> {
    let bg = match line.tag {
        ChangeTag::Equal => None,
        ChangeTag::Delete => Some(theme.diff_delete_bg),
        ChangeTag::Insert => Some(theme.diff_insert_bg),
    };

    let content_char_count: usize = content_spans
        .iter()
        .map(|s| s.content.chars().count())
        .sum();
    let raw_content: String = content_spans.iter().map(|s| s.content.as_ref()).collect();

    if content_width == 0 || content_char_count <= content_width {
        let mut spans = Vec::new();
        render_gutter(&mut spans, line, layout, theme, config);
        let gutter_count = spans.len();
        spans.extend(content_spans);
        let bg_start = compute_bg_start(config, layout.total, indent_width);
        return vec![DiffLineOutput {
            line: Line::from(spans),
            background: bg,
            content_start_col: bg_start,
            gutter_span_count: gutter_count,
            content_text: raw_content,
            joiner: None,
            is_separator: false,
        }];
    }

    // Wrap: project the precomputed styles onto the wrap segments; fall back to a solid FG per segment when the projection cannot be aligned
    let mut outputs = Vec::new();
    let gutter_padding = " ".repeat(layout.total);
    let wrapped_lines = wrap_text(&raw_content, content_width);
    let bg_start = compute_bg_start(config, layout.total, indent_width);
    let styled_rows = match project_styles_onto_wrap_segments(&content_spans, &wrapped_lines) {
        Some(rows) if rows.len() == wrapped_lines.len() => rows,
        _ => {
            let style = match line.tag {
                ChangeTag::Equal => Style::default().fg(theme.diff_equal_fg),
                ChangeTag::Delete | ChangeTag::Insert => {
                    if theme.diff_uses_line_fg() {
                        let fg = if line.tag == ChangeTag::Delete {
                            theme.diff_delete_fg
                        } else {
                            theme.diff_insert_fg
                        };
                        Style::default().fg(fg)
                    } else {
                        Style::default().fg(theme.text_primary)
                    }
                }
            };
            wrapped_lines
                .iter()
                .map(|wrapped| vec![Span::styled(wrapped.clone(), style)])
                .collect()
        }
    };
    for (i, (wrapped, row)) in wrapped_lines.iter().zip(styled_rows).enumerate() {
        let mut spans = Vec::new();
        let gutter_count = if i == 0 {
            render_gutter(&mut spans, line, layout, theme, config);
            spans.len()
        } else {
            spans.push(Span::raw(gutter_padding.clone()));
            1
        };
        spans.extend(row);
        outputs.push(DiffLineOutput {
            line: Line::from(spans),
            background: bg,
            content_start_col: bg_start,
            gutter_span_count: gutter_count,
            content_text: wrapped.clone(),
            joiner: if i == 0 { None } else { Some(String::new()) },
            is_separator: false,
        });
    }
    outputs
}

/// Compute where background starts based on config.
fn compute_bg_start(config: &DiffRenderConfig, gutter_width: usize, indent_width: usize) -> u16 {
    if config.gutter_bg {
        // Gutter bg is on: check if we should skip indent
        // indent_bg = true means skip indent (keep it clean)
        // indent_bg = false means include indent in background
        if config.indent && config.indent_bg {
            indent_width as u16
        } else {
            0
        }
    } else {
        // Gutter bg is off: color from content only
        gutter_width as u16
    }
}

fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 || text.is_empty() {
        return vec![text.to_string()];
    }

    let mut lines = Vec::new();
    let mut current_line = String::new();
    let mut current_width = 0;

    for word in text.split_inclusive(|c: char| c.is_whitespace()) {
        let word_width = word.chars().count();

        if current_width + word_width > max_width && !current_line.is_empty() {
            lines.push(current_line);
            current_line = String::new();
            current_width = 0;
        }

        current_line.push_str(word);
        current_width += word_width;
    }

    if !current_line.is_empty() {
        lines.push(current_line);
    }

    if lines.is_empty() {
        lines.push(String::new());
    }

    lines
}

/// Walks the source spans with a monotonic cursor, splitting only at existing span edges or wrap edges. Each whole
/// [`Style`] is copied onto owned substrings, so the concat of every returned row's text equals its wrap segment.
fn project_styles_onto_wrap_segments(
    content_spans: &[Span<'static>],
    wrapped_segments: &[String],
) -> Option<Vec<Vec<Span<'static>>>> {
    let spans_text: String = content_spans.iter().map(|s| s.content.as_ref()).collect();
    if spans_text != wrapped_segments.concat() {
        return None;
    }

    let mut rows = Vec::with_capacity(wrapped_segments.len());
    let mut span_idx = 0;
    let mut span_byte = 0;
    for segment in wrapped_segments {
        let mut row = Vec::new();
        let mut remaining = segment.len();
        while remaining > 0 {
            let span = content_spans.get(span_idx)?;
            let available = span.content.len().checked_sub(span_byte)?;
            if available == 0 {
                span_idx += 1;
                span_byte = 0;
                continue;
            }
            let take = available.min(remaining);
            // `get` fails closed if either edge is not a char boundary.
            let piece = span.content.get(span_byte..span_byte + take)?;
            row.push(Span::styled(piece.to_owned(), span.style));
            span_byte += take;
            remaining -= take;
        }
        rows.push(row);
    }
    Some(rows)
}

/// Gutter layout computed from a hunk and config.
struct GutterLayout {
    /// Width of old-file column (0 in single mode).
    width_old: usize,
    /// Width of new-file column.
    width_new: usize,
    /// Total gutter width including indent and gaps.
    total: usize,
    /// Whether dual mode is active.
    dual: bool,
}

/// Calculate gutter layout for a hunk.
fn gutter_layout(hunk: &DiffHunk, config: &DiffRenderConfig) -> GutterLayout {
    let mut max_old = 1usize;
    let mut max_new = 1usize;

    for line in hunk {
        max_old = max_old.max(line.lo.max(1));
        max_new = max_new.max(line.ln.max(1));
    }

    let indent_width = if config.indent { INDENT.len() } else { 0 };

    if config.dual_line_numbers {
        let width_old = max_old.ilog10() as usize + 1;
        let width_new = max_new.ilog10() as usize + 1;
        let total = indent_width + width_old + GUTTER_GAP.len() + width_new + CONTENT_GAP.len();
        GutterLayout {
            width_old,
            width_new,
            total,
            dual: true,
        }
    } else {
        let max_num = max_old.max(max_new);
        let width_new = max_num.ilog10() as usize + 1;
        let total = indent_width + width_new + CONTENT_GAP.len();
        GutterLayout {
            width_old: 0,
            width_new,
            total,
            dual: false,
        }
    }
}

/// Render the line number gutter.
fn render_gutter(
    spans: &mut Vec<Span<'static>>,
    line: &crate::engine::diff::DiffLine,
    layout: &GutterLayout,
    theme: &Theme,
    config: &DiffRenderConfig,
) {
    let gutter_style = Style::default().fg(theme.diff_gutter_fg);

    // Indent (configurable)
    if config.indent {
        spans.push(Span::raw(INDENT));
    }

    if layout.dual {
        // Dual mode: two columns (old and new) like GitHub unified diff
        let w_old = layout.width_old;
        let w_new = layout.width_new;
        match line.tag {
            ChangeTag::Equal => {
                spans.push(Span::styled(format!("{:>w_old$}", line.lo), gutter_style));
                spans.push(Span::raw(GUTTER_GAP));
                spans.push(Span::styled(format!("{:>w_new$}", line.ln), gutter_style));
            }
            ChangeTag::Delete => {
                spans.push(Span::styled(
                    format!("{:>w_old$}", line.lo),
                    Style::default().fg(theme.diff_delete_fg),
                ));
                spans.push(Span::raw(GUTTER_GAP));
                spans.push(Span::styled(" ".repeat(w_new), gutter_style));
            }
            ChangeTag::Insert => {
                spans.push(Span::styled(" ".repeat(w_old), gutter_style));
                spans.push(Span::raw(GUTTER_GAP));
                spans.push(Span::styled(
                    format!("{:>w_new$}", line.ln),
                    Style::default().fg(theme.diff_insert_fg),
                ));
            }
        }
    } else {
        // Single mode: one column with the relevant line number
        let w = layout.width_new;
        match line.tag {
            ChangeTag::Equal => {
                spans.push(Span::styled(format!("{:>w$}", line.ln), gutter_style));
            }
            ChangeTag::Delete => {
                spans.push(Span::styled(
                    format!("{:>w$}", line.lo),
                    Style::default().fg(theme.diff_delete_fg),
                ));
            }
            ChangeTag::Insert => {
                spans.push(Span::styled(
                    format!("{:>w$}", line.ln),
                    Style::default().fg(theme.diff_insert_fg),
                ));
            }
        }
    }

    // Gap between gutter and content
    spans.push(Span::raw(CONTENT_GAP));
}

/// Span with `style`; empty text paints a single space so the row keeps a visible background band and stays selectable.
fn painted(text: &str, style: Style) -> Span<'static> {
    let text = if text.is_empty() { " " } else { text };
    Span::styled(text.to_string(), style)
}

#[cfg(feature = "syntax")]
fn advance_highlighter(
    highlighter: &mut Option<HighlightLines<'_>>,
    content: &str,
    syntect: &Syntect,
) {
    if let Some(hl) = highlighter.as_mut() {
        let _ = hl.highlight_line(&format!("{content}\n"), &syntect.syntax_set);
    }
}

/// Render content spans with syntax highlighting.
#[cfg(feature = "syntax")]
fn render_content_spans(
    content: &str,
    tag: ChangeTag,
    theme: &Theme,
    highlighter: &mut Option<HighlightLines<'_>>,
    syntect: &Syntect,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();

    if tag != ChangeTag::Equal && theme.diff_uses_line_fg() {
        let fg = match tag {
            ChangeTag::Delete => theme.diff_delete_fg,
            _ => theme.diff_insert_fg,
        };
        spans.push(painted(content, Style::default().fg(fg)));
        return spans;
    }

    // Try syntax highlighting
    if let Some(hl) = highlighter.as_mut()
        && let Ok(ranges) = hl.highlight_line(&format!("{content}\n"), &syntect.syntax_set)
    {
        let mut wrote = false;
        for (style, segment) in ranges {
            let mut text = segment.to_owned();
            while text.ends_with('\n') || text.ends_with('\r') {
                text.pop();
            }
            if text.is_empty() {
                continue;
            }
            let fg_style = syntect_to_ratatui_fg(style);
            spans.push(Span::styled(text, fg_style));
            wrote = true;
        }
        if wrote {
            return spans;
        }
    }

    // Fallback: plain text
    spans.push(painted(content, plain_diff_style(tag, theme)));
    spans
}

fn plain_diff_style(tag: ChangeTag, theme: &Theme) -> Style {
    if tag != ChangeTag::Equal && theme.diff_uses_line_fg() {
        let fg = match tag {
            ChangeTag::Delete => theme.diff_delete_fg,
            _ => theme.diff_insert_fg,
        };
        return Style::default().fg(fg);
    }

    match tag {
        ChangeTag::Equal => Style::default().fg(theme.diff_equal_fg),
        ChangeTag::Delete | ChangeTag::Insert => Style::default().fg(theme.text_primary),
    }
}

/// Convert syntect style to ratatui style (FG only, no BG).
#[cfg(feature = "syntax")]
fn syntect_to_ratatui_fg(style: SyntectStyle) -> Style {
    crate::engine::render::syntax::syntect_to_ratatui_fg(style)
}

#[derive(Debug, Clone)]
pub struct EditToolCallBlock {
    /// File path being edited.
    pub path: String,
    pub hunks: Vec<DiffHunk>,
    /// Number of edits (for multi-edit display).
    pub edit_count: usize,
    /// Error message if the tool call failed (None = success).
    pub error: Option<String>,
    /// When the tool started running.
    pub started_at: Option<std::time::Instant>,
    /// Elapsed time in ms after completion.
    pub elapsed_ms: Option<i64>,
    /// Header prefix (e.g. "Edit " or "Creating ").
    pub prefix: &'static str,
    pub display_name: Option<String>,
    /// One-liner summary can't be trusted: the call touched multiple files, or the path fell back to the tool title.
    /// (apply_patch emits one Diff per file; only the first becomes hunks.)
    /// Suppresses the diffstat suffix; `ScrollbackState`'s materialize policy keeps such blocks expanded.
    pub summary_untrusted: bool,
    /// Cached `(insertions, deletions)` count, computed eagerly from hunks.
    change_counts: (usize, usize),
    /// Hunk-only first paint; may upgrade to FileScoped via the edit-HL worker.
    pub highlight: EditHighlightPhase,
    /// Whether this ordinary edit targets a memory v2 scope.
    pub is_memory_activity: bool,
}

fn workflow_script_name(path: &str) -> Option<String> {
    let p = Path::new(path);
    if p.extension().is_none_or(|e| e != "rhai") {
        return None;
    }
    if !p
        .ancestors()
        .skip(1)
        .any(|a| a.file_name().is_some_and(|n| n == "workflows"))
    {
        return None;
    }
    Some(p.file_stem()?.to_string_lossy().into_owned())
}

impl EditToolCallBlock {
    /// Pre-completed blocks have no meaningful local timing, so `started_at` is `None`.
    /// Timing is only set for blocks that enter a running UI state (via `set_last_running(true)` in `ScrollbackState`).
    pub fn new(path: impl Into<String>, hunks: Vec<DiffHunk>) -> Self {
        let path = path.into();
        let edit_count = hunks.len().max(1);
        let change_counts = Self::compute_changes(&hunks);
        let display_name = workflow_script_name(&path);
        Self {
            path,
            hunks,
            edit_count,
            error: None,
            started_at: None,
            elapsed_ms: None,
            prefix: if display_name.is_some() {
                "Editing workflow "
            } else {
                "Edit "
            },
            display_name,
            summary_untrusted: false,
            change_counts,
            highlight: EditHighlightPhase::HunkOnly,
            is_memory_activity: false,
        }
    }

    pub fn with_memory_activity(mut self) -> Self {
        self.is_memory_activity = true;
        self
    }

    pub fn with_prefix(mut self, prefix: &'static str) -> Self {
        self.prefix = if self.display_name.is_some() && prefix == "Creating " {
            "Creating workflow "
        } else {
            prefix
        };
        self
    }

    /// Mark the one-liner summary as covering only part of the call.
    pub fn with_untrusted_summary(mut self) -> Self {
        self.summary_untrusted = true;
        self
    }

    /// Set error (marks as failed).
    pub fn with_error(mut self, error: impl Into<String>) -> Self {
        self.error = Some(error.into());
        self
    }

    /// Check if successful (no error).
    pub fn is_success(&self) -> bool {
        self.error.is_none()
    }

    /// Set error (mutable); computes elapsed time if not already set.
    pub fn set_error(&mut self, error: Option<String>) {
        if self.elapsed_ms.is_none()
            && let Some(start) = self.started_at
        {
            self.elapsed_ms = Some(start.elapsed().as_millis() as i64);
        }
        self.error = error;
    }

    /// Finalize elapsed time from `started_at`.
    /// Idempotent: no-op if `started_at` is `None` (pre-completed block) or if `elapsed_ms` is already set (already finalized).
    pub fn finish(&mut self) {
        if self.elapsed_ms.is_some() {
            return;
        }
        if let Some(start) = self.started_at {
            self.elapsed_ms = Some(start.elapsed().as_millis() as i64);
        }
    }

    pub fn elapsed_ms(&self) -> Option<i64> {
        match self.elapsed_ms {
            Some(ms) => Some(ms),
            None => self
                .started_at
                .map(|start| start.elapsed().as_millis() as i64),
        }
    }

    pub fn with_edit_count(mut self, count: usize) -> Self {
        self.edit_count = count;
        self
    }

    /// Get copyable text as a unified diff patch, suitable for `git apply` or clipboard sharing.
    pub fn copy_text(&self) -> String {
        diff_hunks_to_patch(&self.path, &self.hunks)
    }

    pub fn set_hunks(&mut self, hunks: Vec<DiffHunk>) {
        self.edit_count = hunks.len().max(1);
        self.change_counts = Self::compute_changes(&hunks);
        self.hunks = hunks;
    }

    /// Count insertions and deletions across all hunks.
    fn compute_changes(hunks: &[DiffHunk]) -> (usize, usize) {
        let mut insertions = 0;
        let mut deletions = 0;
        for hunk in hunks {
            for line in hunk {
                match line.tag {
                    ChangeTag::Insert => insertions += 1,
                    ChangeTag::Delete => deletions += 1,
                    ChangeTag::Equal => {}
                }
            }
        }
        (insertions, deletions)
    }

    /// Get pre-computed insertion/deletion counts.
    fn count_changes(&self) -> (usize, usize) {
        self.change_counts
    }

    /// Header line: path painted for `surface` (Collapsed / Expanded / Fullscreen).
    #[allow(clippy::too_many_arguments)]
    fn header_line(
        &self,
        theme: &Theme,
        muted: bool,
        show_summary: bool,
        dim_details: bool,
        surface: crate::engine::render::tool_paths::ToolPathSurface,
        cwd: Option<&Path>,
        width: Option<usize>,
    ) -> Line<'static> {
        let text_style = if muted {
            theme.muted()
        } else {
            theme.primary()
        };
        let bold_style = text_style.add_modifier(Modifier::BOLD);
        let path_style = if muted {
            theme.muted()
        } else {
            Style::default().fg(theme.path)
        };
        let detail_style = if dim_details {
            theme.dim()
        } else {
            theme.muted()
        };

        let prefix = self.prefix;

        // Build the suffix spans first so we can reserve space for them. The suffix (diffstat / "(N edits)") renders only
        // on the collapsed one-liner. Untrusted summaries (multi-file, title-fallback path) never show counts that would
        // only describe the first diff.
        let collapsed = matches!(
            surface,
            crate::engine::render::tool_paths::ToolPathSurface::Collapsed
        );
        let suffix_spans: Vec<Span<'static>> =
            if collapsed && show_summary && !self.hunks.is_empty() && !self.summary_untrusted {
                let (ins, del) = self.count_changes();
                if ins > 0 || del > 0 {
                    vec![
                        Span::styled(
                            format!(" +{ins}"),
                            Style::default().fg(theme.diff_insert_fg),
                        ),
                        Span::styled("/", detail_style),
                        Span::styled(format!("-{del}"), Style::default().fg(theme.diff_delete_fg)),
                    ]
                } else {
                    Vec::new()
                }
            } else if collapsed && self.edit_count > 1 {
                vec![Span::styled(
                    format!(" ({} edits)", self.edit_count),
                    detail_style,
                )]
            } else {
                Vec::new()
            };
        let suffix_width: usize = suffix_spans
            .iter()
            .map(|span| unicode_width::UnicodeWidthStr::width(span.content.as_ref()))
            .sum();

        let path = match &self.display_name {
            Some(name) => name.clone(),
            None => crate::engine::render::tool_paths::path_for_tool_surface(
                &self.path,
                surface,
                cwd,
                width,
                prefix.len() + suffix_width,
            ),
        };

        let mut spans = vec![
            Span::styled(prefix, bold_style),
            Span::styled(path, path_style),
        ];
        spans.extend(suffix_spans);

        Line::from(spans)
    }

    fn path_link_target(
        &self,
        cwd: Option<&Path>,
    ) -> Option<crate::engine::render::osc8::LinkTarget> {
        crate::engine::render::osc8::tool_path_file_target(&self.path, cwd)
    }

    /// Render this block's hunks for its current highlight phase.
    /// This is the single dispatch point shared by scrollback `output()` and the fullscreen block viewer.
    /// FileScoped styles baked under a different theme than the live one are skipped (hunk-only paint) so a theme flip never mixes palettes.
    pub fn render_diff_lines(
        &self,
        theme: &Theme,
        width: u16,
        config: &DiffRenderConfig,
    ) -> Vec<DiffLineOutput> {
        let path = Path::new(&self.path);
        match &self.highlight {
            EditHighlightPhase::FileScoped {
                by_new_line,
                theme: baked,
            } if *baked == crate::engine::theme::cache::current_kind() => {
                render_diff_hunks_with_styles(
                    &self.hunks,
                    path,
                    by_new_line.as_ref(),
                    theme,
                    width,
                    config,
                )
            }
            _ => render_diff_hunks_highlighted(&self.hunks, path, theme, width, config),
        }
    }
}

struct WrappedEditHeaderLine {
    content: Line<'static>,
    joiner: Option<String>,
    path_spans: Option<Range<usize>>,
    selection_text: Option<String>,
    selection_boundary: Option<EditSelectionBoundary>,
}

#[derive(Default)]
struct EditSelectionBoundary {
    prefix: String,
    suffix: String,
}

fn split_joiner_by_path(
    joiner: &str,
    source_start: usize,
    path_range: &Range<usize>,
) -> (String, String) {
    let source_end = source_start.saturating_add(joiner.len());
    let owned_start = source_start.max(path_range.start);
    let owned_end = source_end.min(path_range.end);
    if owned_start >= owned_end {
        return (joiner.to_owned(), String::new());
    }

    let local_start = owned_start - source_start;
    let local_end = owned_end - source_start;
    let mut remainder = String::with_capacity(joiner.len() - (local_end - local_start));
    let Some(before) = joiner.get(..local_start) else {
        return (joiner.to_owned(), String::new());
    };
    let Some(owned) = joiner.get(local_start..local_end) else {
        return (joiner.to_owned(), String::new());
    };
    let Some(after) = joiner.get(local_end..) else {
        return (joiner.to_owned(), String::new());
    };
    remainder.push_str(before);
    remainder.push_str(after);
    (remainder, owned.to_owned())
}

fn wrap_edit_header(
    header: Line<'static>,
    width: usize,
    extra_indent: usize,
) -> Vec<WrappedEditHeaderLine> {
    let prefix_width = header
        .spans
        .first()
        .map(|span| unicode_width::UnicodeWidthStr::width(span.content.as_ref()))
        .unwrap_or(0);
    let total_indent = extra_indent + prefix_width;
    let wrap_width = width.saturating_sub(total_indent);
    let prefix_bytes = header.spans.first().map_or(0, |span| span.content.len());
    let path_bytes = header.spans.get(1).map_or(0, |span| span.content.len());
    let path_text = header
        .spans
        .get(1)
        .map_or_else(String::new, |span| span.content.to_string());
    let (wrapped, joiners, path_range, prefix_span) = if prefix_width > 0
        && let Some(prefix_span) = header.spans.first().cloned()
    {
        let rest: Vec<_> = header.spans.iter().skip(1).cloned().collect();
        let content_line = Line::from(rest);
        let (wrapped, joiners) = crate::engine::render::wrapping::word_wrap_lines_with_joiners(
            std::iter::once(content_line),
            wrap_width,
        );
        (wrapped, joiners, 0..path_bytes, Some(prefix_span))
    } else {
        let (wrapped, joiners) = crate::engine::render::wrapping::word_wrap_lines_with_joiners(
            std::iter::once(header),
            width,
        );
        (
            wrapped,
            joiners,
            prefix_bytes..prefix_bytes.saturating_add(path_bytes),
            None,
        )
    };

    let indent = " ".repeat(total_indent);
    let mut source_offset = 0usize;
    let mut rows: Vec<WrappedEditHeaderLine> = Vec::with_capacity(wrapped.len());
    let mut last_path_row: Option<usize> = None;
    for (row, (mut content, joiner)) in wrapped.into_iter().zip(joiners).enumerate() {
        let (original_joiner, non_path_joiner, path_joiner) = match joiner {
            Some(joiner) => {
                let (remainder, owned) = split_joiner_by_path(&joiner, source_offset, &path_range);
                source_offset = source_offset.saturating_add(joiner.len());
                (Some(joiner), Some(remainder), owned)
            }
            None => (None, None, String::new()),
        };
        let mut span_offset = source_offset;
        let mut path_span_start = None;
        let mut path_span_end = 0usize;
        for (span_idx, span) in content.spans.iter().enumerate() {
            let span_end = span_offset.saturating_add(span.content.len());
            if span_offset < path_range.end && path_range.start < span_end {
                path_span_start.get_or_insert(span_idx);
                path_span_end = span_idx + 1;
            }
            span_offset = span_end;
        }
        source_offset = span_offset;

        let selection_text = path_span_start.map(|start| {
            let mut selected = String::new();
            for span in content
                .spans
                .get(start..path_span_end)
                .into_iter()
                .flatten()
            {
                selected.push_str(span.content.as_ref());
            }
            selected
        });
        if selection_text.is_none()
            && !path_joiner.is_empty()
            && let Some(previous) = last_path_row.and_then(|index| rows.get_mut(index))
        {
            previous
                .selection_boundary
                .get_or_insert_with(EditSelectionBoundary::default)
                .suffix
                .push_str(&path_joiner);
        }

        let decoration_spans = if let Some(prefix) = &prefix_span {
            let decoration = if row == 0 {
                prefix.clone()
            } else {
                Span::raw(indent.clone())
            };
            content.spans.insert(0, decoration);
            1
        } else {
            0
        };
        let path_spans =
            path_span_start.map(|start| start + decoration_spans..path_span_end + decoration_spans);
        let has_path = path_spans.is_some();
        let first_path_row = has_path && last_path_row.is_none();
        let selection_boundary = first_path_row
            .then_some(path_joiner)
            .filter(|prefix| !prefix.is_empty())
            .map(|prefix| EditSelectionBoundary {
                prefix,
                suffix: String::new(),
            });
        let output_joiner = if has_path && !first_path_row {
            original_joiner
        } else {
            non_path_joiner
        };
        let output_index = rows.len();
        rows.push(WrappedEditHeaderLine {
            content,
            joiner: output_joiner,
            path_spans,
            selection_text,
            selection_boundary,
        });
        if has_path {
            last_path_row = Some(output_index);
        }
    }
    if source_offset < path_range.end
        && let Some(previous) = last_path_row.and_then(|index| rows.get_mut(index))
    {
        let suffix_start = source_offset.max(path_range.start) - path_range.start;
        if let Some(suffix) = path_text.get(suffix_start..) {
            previous
                .selection_boundary
                .get_or_insert_with(EditSelectionBoundary::default)
                .suffix
                .push_str(suffix);
        }
    }
    rows
}

impl EditToolCallBlock {
    pub(crate) fn rendered_output(&self, ctx: &BlockContext) -> RenderedBlockOutput {
        let theme = Theme::current();
        let edit_cfg = &ctx.appearance.scrollback.blocks.edit;
        let tool_cfg = &ctx.appearance.scrollback.blocks.tool;
        let muted_collapsed = ctx.mute_when_collapsed(tool_cfg.muted_collapsed);
        let dim_details = tool_cfg.dim_details;
        // Explicit pager.toml value wins; unset follows the shell-owned flag.
        let show_summary = edit_cfg.effective_line_summary(ctx.appearance.collapsed_edit_blocks);

        let cwd = ctx.cwd.as_deref();
        let link_target = self.path_link_target(cwd);

        match ctx.mode {
            DisplayMode::Collapsed => {
                // Collapsed: just header line. Path (span 1) is selectable.
                let line = self.header_line(
                    &theme,
                    muted_collapsed,
                    show_summary,
                    dim_details,
                    crate::engine::render::tool_paths::ToolPathSurface::Collapsed,
                    cwd,
                    Some(ctx.content_width()),
                );
                // Spans: ["Edit ", path, optional suffix spans...]
                // Only the path span (index 1) is selectable.
                let path_end = if line.spans.len() > 2 {
                    2
                } else {
                    line.spans.len()
                };
                RenderedBlockOutput::from(BlockOutput {
                    lines: vec![BlockLine {
                        selectable: Selectable::Spans(1..path_end),
                        selection_range: Some(TOOL_HEADER_RANGE),
                        // Copy the painted path span (basename when collapsed).
                        content: line,
                        link_target,
                        ..Default::default()
                    }],
                })
            }
            DisplayMode::Truncated | DisplayMode::Expanded => {
                // Convert appearance config to render config
                let config = DiffRenderConfig {
                    indent: edit_cfg.indent,
                    gutter_bg: edit_cfg.gutter_bg,
                    indent_bg: edit_cfg.indent_bg,
                    hunk_separator: edit_cfg.hunk_separator.clone(),
                    dual_line_numbers: edit_cfg.dual_line_numbers,
                };
                // Header with word-wrap and hanging indent
                let header = self.header_line(
                    &theme,
                    false,
                    show_summary,
                    dim_details,
                    crate::engine::render::tool_paths::ToolPathSurface::Expanded,
                    cwd,
                    None,
                );
                let bullet_indent = ctx
                    .appearance
                    .scrollback
                    .blocks
                    .tool
                    .bullet
                    .char()
                    .map(|c| unicode_width::UnicodeWidthStr::width(c) + 1)
                    .unwrap_or(0);
                let wrapped_header = wrap_edit_header(header, ctx.width as usize, bullet_indent);
                let mut lines: Vec<BlockLine> = Vec::with_capacity(wrapped_header.len());
                let mut boundary_entries = Vec::new();
                for (line_index, line) in wrapped_header.into_iter().enumerate() {
                    let has_path = line.path_spans.is_some();
                    if let Some(boundary) = line.selection_boundary {
                        boundary_entries.push(SelectionBoundaryEntry {
                            line_index,
                            boundary: Arc::new(SelectionBoundary::new(
                                boundary.prefix,
                                boundary.suffix,
                            )),
                        });
                    }
                    lines.push(BlockLine {
                        selectable: line.path_spans.map_or(Selectable::None, Selectable::Spans),
                        selection_range: Some(TOOL_HEADER_RANGE),
                        selection_text: line.selection_text,
                        joiner: line.joiner,
                        content: line.content,
                        link_target: if has_path { link_target.clone() } else { None },
                        ..Default::default()
                    });
                }

                // Error message (non-selectable decoration)
                if let Some(err) = &self.error {
                    lines.push(BlockLine::separator(Line::from("")));
                    for line in err.lines() {
                        lines.push(BlockLine::separator(Line::from(Span::styled(
                            line.to_string(),
                            theme.muted(),
                        ))));
                    }
                }

                // Diff content with syntax highlighting and full-width backgrounds
                if !self.hunks.is_empty() {
                    // Empty line after header (non-selectable)
                    lines.push(BlockLine::separator(Line::from("")));

                    let diff_lines = self.render_diff_lines(&theme, ctx.width, &config);
                    let mut diff_range_id: u16 = 1;
                    for output in diff_lines {
                        if output.is_separator {
                            lines.push(BlockLine::separator(output.line));
                            diff_range_id += 1;
                            continue;
                        }
                        let total = output.line.spans.len();
                        let gc = output.gutter_span_count;
                        let mut block_line = BlockLine {
                            selectable: if gc < total {
                                Selectable::Spans(gc..total)
                            } else {
                                Selectable::All
                            },
                            selection_range: Some(diff_range_id),
                            selection_text: Some(output.content_text),
                            joiner: output.joiner,
                            content: output.line,
                            ..Default::default()
                        };
                        if let Some(bg_color) = output.background {
                            block_line =
                                block_line.with_background_from(bg_color, output.content_start_col);
                        }
                        lines.push(block_line);
                    }
                }

                // Apply max_lines if set
                if let Some(max) = ctx.max_lines {
                    lines.truncate(max as usize);
                }

                boundary_entries.retain(|entry| entry.line_index < lines.len());
                RenderedBlockOutput {
                    output: BlockOutput { lines },
                    boundaries: SelectionBoundaries::from_entries(boundary_entries),
                }
            }
        }
    }
}

impl BlockContent for EditToolCallBlock {
    fn output(&self, ctx: &BlockContext) -> BlockOutput {
        self.rendered_output(ctx).output
    }

    fn accent(&self, ctx: &BlockContext) -> Option<AccentStyle> {
        // Edit blocks: use config accent color if set, otherwise no accent.
        // Errors show a red bullet instead of a red accent
        ctx.appearance
            .scrollback
            .blocks
            .edit
            .accent
            .map(AccentStyle::static_color)
    }

    fn bullet(&self, _ctx: &BlockContext) -> Option<AccentStyle> {
        // Failed edit: red bullet. Successful: default (gray/primary).
        if self.error.is_some() {
            let theme = Theme::current();
            Some(AccentStyle::static_color(theme.accent_error))
        } else {
            None
        }
    }

    fn accent_background(&self, ctx: &BlockContext) -> bool {
        ctx.appearance.scrollback.blocks.edit.accent_bg
    }

    fn has_vpad_for(&self, appearance: &AppearanceConfig) -> bool {
        appearance.scrollback.blocks.edit.vpad
    }

    fn background(&self, ctx: &BlockContext) -> BlockBackground {
        ctx.appearance.scrollback.blocks.edit.bg
    }

    fn has_raw_mode(&self) -> bool {
        false
    }

    fn is_foldable(&self) -> bool {
        // Foldable if there are hunks to show, or an error message to reveal.
        !self.hunks.is_empty() || self.error.is_some()
    }

    fn default_display_mode(&self) -> DisplayMode {
        // Context-free. Expanded default and the untrusted-summary escape are applied in ScrollbackState push / replace_tool_block.
        DisplayMode::Collapsed
    }

    fn finished_display_mode(&self) -> Option<DisplayMode> {
        if self.error.is_some() {
            Some(DisplayMode::Collapsed)
        } else {
            None // keep current mode (the config-aware default, or a user fold)
        }
    }

    fn next_fold_mode(&self, current: DisplayMode, _is_running: bool) -> DisplayMode {
        match current {
            DisplayMode::Collapsed => DisplayMode::Expanded,
            _ => DisplayMode::Collapsed,
        }
    }

    fn preamble(&self, ctx: &BlockContext) -> Option<Text<'static>> {
        let theme = Theme::current();
        let dim_details = ctx.appearance.scrollback.blocks.tool.dim_details;
        // Same effective toggle as `rendered_output` (moot here: the suffix is collapsed-only and this is the Fullscreen surface)
        let show_summary = ctx
            .appearance
            .scrollback
            .blocks
            .edit
            .effective_line_summary(ctx.appearance.collapsed_edit_blocks);
        Some(Text::from(self.header_line(
            &theme,
            false,
            show_summary,
            dim_details,
            crate::engine::render::tool_paths::ToolPathSurface::Fullscreen,
            ctx.cwd.as_deref(),
            None,
        )))
    }
}
