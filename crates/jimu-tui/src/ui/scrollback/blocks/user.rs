use std::ops::Range;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::engine::render::wrapping::{RtOptions, word_wrap_line_with_joiners};
use crate::ui::scrollback::block::BlockContent;
use crate::ui::scrollback::types::{
    AccentStyle, BlockBackground, BlockContext, BlockLine, BlockOutput, DisplayMode, Selectable,
};

const USER_PROMPT_BODY_RANGE: u16 = 0;
/// Max visible lines when a user prompt is collapsed.
const COLLAPSED_MAX_LINES: usize = 3;
use crate::engine::theme::Theme;
use crate::ui::scrollback::appearance::AppearanceConfig;

/// Drop invalid token ranges (replayed session metadata is untrusted): out of bounds, not on char boundaries, or empty.
/// Survivors are sorted; overlaps with an earlier kept range are dropped so span slicing never goes backwards.
fn sanitize_token_ranges(text: &str, mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.retain(|r| {
        r.start < r.end
            && r.end <= text.len()
            && text.is_char_boundary(r.start)
            && text.is_char_boundary(r.end)
    });
    ranges.sort_by_key(|r| (r.start, r.end));
    let mut out: Vec<Range<usize>> = Vec::new();
    for r in ranges {
        if out.last().is_some_and(|prev| r.start < prev.end) {
            continue;
        }
        out.push(r);
    }
    out
}

/// Split one logical line into token/body spans by intersecting the line's byte span in the block text with the (sanitized) token ranges.
fn token_styled_line(
    line_text: &str,
    line_start: usize,
    ranges: &[Range<usize>],
    token_style: Style,
    body_style: Style,
) -> Line<'static> {
    let line_end = line_start + line_text.len();
    let mut spans = Vec::new();
    let mut pos = line_start;
    for r in ranges {
        let start = r.start.clamp(line_start, line_end);
        let end = r.end.clamp(line_start, line_end);
        if start >= end {
            continue;
        }
        if start > pos {
            let Some(body) = pos.checked_sub(line_start).and_then(|a| {
                start
                    .checked_sub(line_start)
                    .and_then(|b| line_text.get(a..b))
            }) else {
                continue;
            };
            spans.push(Span::styled(body.to_string(), body_style));
        }
        let Some(token) = start.checked_sub(line_start).and_then(|a| {
            end.checked_sub(line_start)
                .and_then(|b| line_text.get(a..b))
        }) else {
            continue;
        };
        spans.push(Span::styled(token.to_string(), token_style));
        pos = end;
    }
    if pos < line_end
        && let Some(body) = pos.checked_sub(line_start).and_then(|a| line_text.get(a..))
    {
        spans.push(Span::styled(body.to_string(), body_style));
    }
    Line::from(spans)
}

/// Find byte ranges of `[Image #N]` placeholders inside `line_text`.
fn find_image_chip_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let bytes = text.as_bytes();
    let needle = b"[Image #";
    let mut pos = 0;
    while pos + needle.len() < bytes.len() {
        let Some(rel) = text[pos..].find("[Image #") else {
            break;
        };
        let start = pos + rel;
        let after = start + needle.len();
        let mut idx = after;
        while idx < bytes.len() && bytes[idx].is_ascii_digit() {
            idx += 1;
        }
        if idx > after && idx < bytes.len() && bytes[idx] == b']' {
            ranges.push(start..idx + 1);
            pos = idx + 1;
        } else {
            pos = after;
        }
    }
    ranges
}

/// Apply `[Image #N]` chip styling on top of an already-styled `Line`.
fn style_image_chips_in_line(line: Line<'static>, chip_style: Style) -> Line<'static> {
    let mut out_spans: Vec<Span<'static>> = Vec::with_capacity(line.spans.len());
    for span in line.spans {
        let ranges = find_image_chip_ranges(&span.content);
        if ranges.is_empty() {
            out_spans.push(span);
            continue;
        }
        let base_style = span.style;
        let text = span.content.as_ref();
        let mut pos = 0;
        for range in ranges {
            if range.start > pos {
                out_spans.push(Span::styled(text[pos..range.start].to_string(), base_style));
            }
            out_spans.push(Span::styled(
                text[range.start..range.end].to_string(),
                chip_style,
            ));
            pos = range.end;
        }
        if pos < text.len() {
            out_spans.push(Span::styled(text[pos..].to_string(), base_style));
        }
    }
    Line::from(out_spans)
}

#[derive(Debug, Clone)]
pub struct UserPromptBlock {
    pub text: String,
    /// Whether this was a bash command (! prefix).
    pub is_bash: bool,
    /// Whether this prompt was injected by the scheduler (cron/loop).
    pub is_cron: bool,
    /// Mid-turn interjection.
    /// Renders identically to a typed prompt but is excluded from shell prompt-index bookkeeping.
    /// The shell numbers only turn-starting prompts, so counting interjections would skew the positional prompt-to-entry mapping rewind uses.
    pub is_interjection: bool,
    pub prompt_index: Option<usize>,
    /// Sanitized byte ranges into `text` rendered in the skill accent color (recognized `/command` tokens).
    /// Empty means plain prompt styling.
    /// This is the sole skill signal; a leading skill invocation is `[0..token_end]`.
    pub skill_token_ranges: Vec<Range<usize>>,
}

impl UserPromptBlock {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_bash: false,
            is_cron: false,
            is_interjection: false,
            prompt_index: None,
            skill_token_ranges: Vec::new(),
        }
    }

    pub fn copy_text(&self) -> String {
        self.text.clone()
    }

    /// Number of `[Image #N]` attachment chips present in this prompt's display text.
    pub fn image_chip_count(&self) -> usize {
        find_image_chip_ranges(&self.text).len()
    }

    pub fn bash(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_bash: true,
            is_cron: false,
            is_interjection: false,
            prompt_index: None,
            skill_token_ranges: Vec::new(),
        }
    }

    /// The leading token on the first line (up to whitespace, or the whole line) gets the skill accent.
    pub fn skill(text: impl Into<String>) -> Self {
        let text = text.into();
        let first_line = text.lines().next().unwrap_or("");
        let token_end = first_line
            .find(char::is_whitespace)
            .unwrap_or(first_line.len());
        #[allow(clippy::single_range_in_vec_init)] // field is multi-range capable
        let skill_token_ranges = if token_end > 0 {
            vec![0..token_end]
        } else {
            Vec::new()
        };
        Self {
            text,
            is_bash: false,
            is_cron: false,
            is_interjection: false,
            prompt_index: None,
            skill_token_ranges,
        }
    }

    /// Create a plain user prompt with recognized mid-text slash tokens styled in the skill accent.
    /// Ranges are sanitized here: replayed session metadata is untrusted, so invalid ranges are dropped rather than panicking.
    pub fn with_skill_tokens(text: impl Into<String>, ranges: Vec<Range<usize>>) -> Self {
        let text = text.into();
        let skill_token_ranges = sanitize_token_ranges(&text, ranges);
        Self {
            text,
            is_bash: false,
            is_cron: false,
            is_interjection: false,
            prompt_index: None,
            skill_token_ranges,
        }
    }

    pub fn cron(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_bash: false,
            is_cron: true,
            is_interjection: false,
            prompt_index: None,
            skill_token_ranges: Vec::new(),
        }
    }

    pub fn interjection(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_bash: false,
            is_cron: false,
            is_interjection: true,
            prompt_index: None,
            skill_token_ranges: Vec::new(),
        }
    }

    /// The elevated band behind user-prompt rows in the full TUI.
    /// Minimal / terminal-native keeps bold only — no fill.
    ///
    /// RGB themes: `bg_light` (matches the fullscreen prompt band).
    ///
    /// Takes `theme` so unit tests can call it without toggling `terminal_native_lock`, which races `Theme::current()`
    /// tests that do not hold the theme test mutex.
    fn prompt_band_color_for(
        theme: &Theme,
        _is_selected: bool,
        terminal_native: bool,
    ) -> Option<ratatui::style::Color> {
        use ratatui::style::Color;
        if terminal_native {
            None
        } else {
            match theme.bg_light {
                Color::Reset => None,
                c => Some(c),
            }
        }
    }

    /// Prefix/body/skill styles. Bold only when `terminal_native` (minimal mode). Minimal keeps a Cyan pointer when
    /// `accent_user` is Reset (its prompt rows have no band, so the pointer is the turn cue).
    fn prompt_styles(theme: &Theme, terminal_native: bool) -> (Style, Style, Style) {
        let prefix_color = match theme.accent_user {
            ratatui::style::Color::Reset if terminal_native => ratatui::style::Color::Cyan,
            c => c,
        };
        let mut prefix_style = theme.fg(prefix_color);
        let mut text_style = theme.fg(theme.text_primary);
        let mut skill_style = theme.fg(theme.accent_skill);
        if terminal_native {
            prefix_style = prefix_style.add_modifier(Modifier::BOLD);
            text_style = text_style.add_modifier(Modifier::BOLD);
            skill_style = skill_style.add_modifier(Modifier::BOLD);
        }
        (prefix_style, text_style, skill_style)
    }

    /// Wrap and style the prompt text, returning visual lines.
    /// When `max_lines` is set and content exceeds it, the last line is truncated with a " …" ellipsis.
    fn wrap_prompt_lines(
        &self,
        width: u16,
        max_lines: Option<usize>,
        show_prefix: bool,
        is_selected: bool,
    ) -> Vec<BlockLine> {
        let theme = Theme::current();
        // Minimal mode engages this lock; read it here instead of app state.
        let terminal_native = crate::engine::theme::cache::terminal_native_locked();
        // The terminal theme (fullscreen) renders prompts bandless: bold
        // primary text instead of a bright-black band, which can sit too
        // close to the default fg on some profiles. Minimal keeps its band.
        let attribute_emphasis =
            !terminal_native && crate::engine::theme::cache::terminal_native_active();
        let (mut prefix_style, mut text_style, mut skill_style) =
            Self::prompt_styles(&theme, terminal_native);
        if attribute_emphasis {
            prefix_style = prefix_style.add_modifier(Modifier::BOLD);
            text_style = text_style.add_modifier(Modifier::BOLD);
            skill_style = skill_style.add_modifier(Modifier::BOLD);
        }
        let band = Self::prompt_band_color_for(&theme, is_selected, terminal_native);
        // Semantic line bg (not a "panel") so it survives minimal's flat_background.
        // Bandless prompts (terminal theme) carry no extra selected cue: the
        // rewind picker and the dimmed tail already mark the target.
        let with_band = |line: BlockLine| -> BlockLine {
            match band {
                Some(c) => line.with_background(c),
                None => line,
            }
        };

        let prefix = if !show_prefix {
            ""
        } else if self.is_bash {
            "$ "
        } else if self.is_cron {
            "\u{21BB}  "
        } else {
            crate::engine::render::glyphs::prompt_arrow()
        };
        let prefix_width = prefix.width();
        let has_visible_prefix = prefix_width > 0;
        let ellipsis = " \u{2026}";
        let ellipsis_width = 2;

        let base_content_width = (width as usize).saturating_sub(prefix_width).max(1);

        let mut all_lines: Vec<BlockLine> = Vec::new();
        let logical_lines: Vec<&str> = self.text.lines().collect();
        let total_logical = logical_lines.len();

        for (logical_idx, line_text) in logical_lines.iter().enumerate() {
            if line_text.is_empty() {
                // Empty line: just show prefix/indent
                let indent = " ".repeat(prefix_width);
                let line = if logical_idx == 0 {
                    Line::from(vec![Span::styled(prefix.to_string(), prefix_style)])
                } else {
                    Line::from(vec![Span::styled(indent, prefix_style)])
                };
                let block_line = with_band(BlockLine {
                    selectable: if has_visible_prefix {
                        Selectable::Spans(1..1) // empty content range past prefix
                    } else {
                        Selectable::All
                    },
                    selection_range: Some(USER_PROMPT_BODY_RANGE),
                    content: line,
                    ..Default::default()
                });
                all_lines.push(block_line);

                // Check if we hit max_lines
                if let Some(max) = max_lines
                    && all_lines.len() >= max
                {
                    let has_more = logical_idx + 1 < total_logical;
                    if has_more {
                        // Add ellipsis to last line
                        if let Some(last) = all_lines.last_mut() {
                            last.content
                                .spans
                                .push(Span::styled(ellipsis.to_string(), text_style));
                        }
                    }
                    return all_lines;
                }
                continue;
            }

            // Wrap this line's content.
            let content_line = if self.skill_token_ranges.is_empty() {
                Line::from(Span::styled(line_text.to_string(), text_style))
            } else {
                // `lines()` strips terminators, so recover this line's byte offset from the subslice pointer into `self.text`
                debug_assert!(
                    self.text
                        .as_bytes()
                        .as_ptr_range()
                        .contains(&line_text.as_ptr()),
                    "line_text must be a subslice of self.text"
                );
                let line_start = line_text.as_ptr() as usize - self.text.as_ptr() as usize;
                token_styled_line(
                    line_text,
                    line_start,
                    &self.skill_token_ranges,
                    skill_style,
                    text_style,
                )
            };
            let chip_style = if terminal_native || attribute_emphasis {
                Style::default()
                    .fg(theme.accent_user)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.paste_fg).bg(theme.paste_bg)
            };
            let content_line = style_image_chips_in_line(content_line, chip_style);
            let (wrapped, wrap_joiners) =
                word_wrap_line_with_joiners(&content_line, RtOptions::new(base_content_width));
            let wrapped_count = wrapped.len();

            for (wrap_idx, (wrapped_line, wrap_joiner)) in
                wrapped.into_iter().zip(wrap_joiners).enumerate()
            {
                let is_first_line = logical_idx == 0 && wrap_idx == 0;
                let indent: String = " ".repeat(prefix_width);
                let line_prefix = if is_first_line { prefix } else { &indent };

                let will_be_last = max_lines.is_some_and(|max| all_lines.len() + 1 == max);

                // Check if there's more content after this line
                let has_more_wrapped = wrap_idx + 1 < wrapped_count;
                let has_more_logical = logical_idx + 1 < total_logical;
                let has_more = has_more_wrapped || has_more_logical;

                // First line of block or new logical line: hard break (None).
                // Continuation within same logical line: use wrap joiner.
                let joiner = if is_first_line || wrap_idx == 0 {
                    None
                } else {
                    wrap_joiner
                };

                if will_be_last && has_more {
                    // Re-wrap the current line's content with reduced width to make room for the ellipsis
                    // Re-wrapping the styled line (not flattened text) keeps token spans teal here
                    let reduced_width = base_content_width.saturating_sub(ellipsis_width);
                    let (re_wrapped_lines, _) =
                        word_wrap_line_with_joiners(&wrapped_line, RtOptions::new(reduced_width));

                    let final_content = re_wrapped_lines
                        .into_iter()
                        .next()
                        .unwrap_or_else(Line::default);

                    // Build final line with prefix, content, and ellipsis
                    let mut spans = vec![Span::styled(line_prefix.to_string(), prefix_style)];
                    spans.extend(final_content.spans.into_iter().map(|s| Span {
                        content: s.content.to_string().into(),
                        style: s.style,
                    }));
                    spans.push(Span::styled(ellipsis.to_string(), text_style));

                    let content_start = if has_visible_prefix { 1 } else { 0 };
                    let content_end = spans.len() - 1; // exclude ellipsis
                    let block_line = with_band(BlockLine {
                        content: Line::from(spans),
                        selectable: if content_start < content_end {
                            Selectable::Spans(content_start..content_end)
                        } else {
                            Selectable::Spans(content_start..content_start)
                        },
                        selection_range: Some(USER_PROMPT_BODY_RANGE),
                        joiner,
                        ..Default::default()
                    });
                    all_lines.push(block_line);
                    return all_lines;
                }

                // Normal line (not truncated)
                let mut spans = vec![Span::styled(line_prefix.to_string(), prefix_style)];
                spans.extend(wrapped_line.spans.into_iter().map(|s| Span {
                    content: s.content.to_string().into(),
                    style: s.style,
                }));
                let content_end = spans.len();
                let block_line = with_band(BlockLine {
                    content: Line::from(spans),
                    selectable: if has_visible_prefix {
                        Selectable::Spans(1..content_end)
                    } else {
                        Selectable::All
                    },
                    selection_range: Some(USER_PROMPT_BODY_RANGE),
                    joiner,
                    ..Default::default()
                });
                all_lines.push(block_line);

                // Stop at max_lines (the case where no more content follows)
                if let Some(max) = max_lines
                    && all_lines.len() >= max
                {
                    return all_lines;
                }
            }
        }

        // Handle empty text
        if all_lines.is_empty() {
            all_lines.push(with_band(BlockLine {
                content: Line::from(Span::styled(prefix.trim().to_string(), prefix_style)),
                selectable: Selectable::All,
                selection_range: Some(USER_PROMPT_BODY_RANGE),
                ..Default::default()
            }));
        }

        all_lines
    }
}

impl BlockContent for UserPromptBlock {
    fn output(&self, ctx: &BlockContext) -> BlockOutput {
        let max_lines = match ctx.mode {
            DisplayMode::Expanded => None,
            DisplayMode::Collapsed | DisplayMode::Truncated => Some(COLLAPSED_MAX_LINES),
        };

        let prompt_cfg = &ctx.appearance.scrollback.blocks.prompt;
        let compact = ctx.appearance.prompt.compact;
        let lines = self.wrap_prompt_lines(
            ctx.width,
            max_lines,
            prompt_cfg.show_prefix && !compact,
            ctx.is_selected,
        );

        BlockOutput { lines }
    }

    fn accent(&self, _ctx: &BlockContext) -> Option<AccentStyle> {
        None
    }

    fn accent_background(&self, _ctx: &BlockContext) -> bool {
        true // fill accent column with block bg so it matches content
    }

    fn background(&self, ctx: &BlockContext) -> BlockBackground {
        ctx.appearance.scrollback.blocks.prompt.bg
    }

    fn has_vpad_for(&self, appearance: &AppearanceConfig) -> bool {
        appearance.scrollback.blocks.prompt.vpad && !appearance.prompt.compact
    }

    fn has_raw_mode(&self) -> bool {
        false
    }

    fn is_foldable(&self) -> bool {
        // Estimate visual line count to catch long single-line prompts that wrap past the limit. Uses a conservative
        // content width (terminal width minus prefix/padding). at wider terminals we may slightly over-report foldability,
        // which is harmless.
        const MIN_CONTENT_WIDTH: usize = 60;
        let mut visual_lines = 0usize;
        for line in self.text.lines() {
            let w = line.width();
            visual_lines += if w == 0 {
                1
            } else {
                w.div_ceil(MIN_CONTENT_WIDTH)
            };
            if visual_lines > COLLAPSED_MAX_LINES {
                return true;
            }
        }
        false
    }

    fn default_display_mode(&self) -> DisplayMode {
        if self.is_foldable() {
            DisplayMode::Collapsed
        } else {
            DisplayMode::Expanded
        }
    }

    fn next_fold_mode(&self, current: DisplayMode, _is_running: bool) -> DisplayMode {
        match current {
            DisplayMode::Collapsed | DisplayMode::Truncated => DisplayMode::Expanded,
            DisplayMode::Expanded => DisplayMode::Collapsed,
        }
    }
}
