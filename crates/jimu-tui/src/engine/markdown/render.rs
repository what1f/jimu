//! Markdown renderer: transforms parsed markdown buffers into styled output.
//!
//! After parsing with `MarkdownParser`, use `ParsedMarkdown` to render to either ratatui Lines or ANSI strings.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fmt::Write as FmtWrite;
use std::ops::Range;

use super::syntax::SyntectStyle;
use anstyle::{Effects, Reset, Style};
use ratatui::text::{Line, Span};

use crate::engine::markdown::buffers::{
    MarkdownBuffers, RenderEvent, RenderEventKind, TableCopyMeta, unicode_display_width,
};
use crate::engine::markdown::checkpoint::Checkpoint;
use crate::engine::markdown::colors::adapt_style;
use crate::engine::markdown::hyperlinks::{
    ChunkLinkRange, chunk_link_offsets, emit_segment_hyperlinks,
};
use crate::engine::markdown::output::{HyperlinkTarget, MarkdownRenderOutput};
use crate::engine::markdown::parse::ParsedMarkdown;
use crate::engine::markdown::source_map::SourceMap;
use crate::engine::markdown::style::{all_hidden, merge_styles};

/// Trait for converting anstyle to ratatui style.
trait StyleInto<T> {
    fn style_into(self) -> T;
}

impl StyleInto<ratatui::style::Style> for Style {
    fn style_into(self) -> ratatui::style::Style {
        use ratatui::style::{Modifier, Style as RStyle};

        let mut style = RStyle::default();

        if let Some(fg) = self.get_fg_color() {
            style = style.fg(anstyle_to_ratatui_color(fg));
        }
        if let Some(bg) = self.get_bg_color() {
            style = style.bg(anstyle_to_ratatui_color(bg));
        }

        let effects = self.get_effects();
        let mut modifiers = Modifier::empty();
        if effects.contains(Effects::BOLD) {
            modifiers |= Modifier::BOLD;
        }
        if effects.contains(Effects::DIMMED) {
            modifiers |= Modifier::DIM;
        }
        if effects.contains(Effects::ITALIC) {
            modifiers |= Modifier::ITALIC;
        }
        if effects.contains(Effects::UNDERLINE) {
            modifiers |= Modifier::UNDERLINED;
        }
        if effects.contains(Effects::STRIKETHROUGH) {
            modifiers |= Modifier::CROSSED_OUT;
        }
        if effects.contains(Effects::HIDDEN) {
            modifiers |= Modifier::HIDDEN;
        }

        style.add_modifier(modifiers)
    }
}

fn anstyle_to_ratatui_color(color: anstyle::Color) -> ratatui::style::Color {
    use ratatui::style::Color;
    match color {
        anstyle::Color::Ansi(ansi) => match ansi {
            anstyle::AnsiColor::Black => Color::Black,
            anstyle::AnsiColor::Red => Color::Red,
            anstyle::AnsiColor::Green => Color::Green,
            anstyle::AnsiColor::Yellow => Color::Yellow,
            anstyle::AnsiColor::Blue => Color::Blue,
            anstyle::AnsiColor::Magenta => Color::Magenta,
            anstyle::AnsiColor::Cyan => Color::Cyan,
            anstyle::AnsiColor::White => Color::Gray,
            anstyle::AnsiColor::BrightBlack => Color::DarkGray,
            anstyle::AnsiColor::BrightRed => Color::LightRed,
            anstyle::AnsiColor::BrightGreen => Color::LightGreen,
            anstyle::AnsiColor::BrightYellow => Color::LightYellow,
            anstyle::AnsiColor::BrightBlue => Color::LightBlue,
            anstyle::AnsiColor::BrightMagenta => Color::LightMagenta,
            anstyle::AnsiColor::BrightCyan => Color::LightCyan,
            anstyle::AnsiColor::BrightWhite => Color::White,
        },
        anstyle::Color::Ansi256(idx) => Color::Indexed(idx.index()),
        anstyle::Color::Rgb(rgb) => Color::Rgb(rgb.0, rgb.1, rgb.2),
    }
}

/// Render raw highlighted spans to an ANSI string.
fn render_replace_ansi(highlighted: &[Vec<(SyntectStyle, String)>]) -> String {
    let mut out = String::new();
    for line_spans in highlighted {
        for (style, text) in line_spans {
            if text.is_empty() {
                continue;
            }
            let full_style = super::syntax::to_anstyle(*style);
            let fg_only = full_style.bg_color(None);
            let adapted = adapt_style(fg_only);
            if adapted != Style::new() {
                write!(out, "{adapted}{text}\x1b[0m").ok();
            } else {
                out.push_str(text);
            }
        }
    }
    out
}

/// Stylize trait for ANSI rendering.
trait Stylize {
    fn astyle(&self, style: Style) -> StyledStr<'_>;
}

impl Stylize for str {
    fn astyle(&self, style: Style) -> StyledStr<'_> {
        StyledStr { text: self, style }
    }
}

struct StyledStr<'a> {
    text: &'a str,
    style: Style,
}

impl<'a> std::fmt::Display for StyledStr<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.style.is_plain() {
            write!(f, "{}", self.text)
        } else {
            write!(f, "{}{}\x1b[0m", self.style, self.text)
        }
    }
}

impl<'a, 'b> ParsedMarkdown<'a, 'b> {
    fn apply_transforms<'t>(&self, text: &'t str, start: usize, pretty: bool) -> Cow<'t, str> {
        if self.buffers.transforms.is_empty() {
            return Cow::Borrowed(text);
        }
        // Raw mode applies only `force` transforms (e.g. soft-break collapse).
        if !pretty && !self.buffers.transforms.iter().any(|t| t.force) {
            return Cow::Borrowed(text);
        }

        let end = start + text.len();
        let mut result = String::new();
        let mut pos = start;
        let mut applied = false;

        for transform in &self.buffers.transforms {
            if transform.range.end <= start || transform.range.start >= end {
                continue;
            }
            if !pretty && !transform.force {
                continue;
            }
            applied = true;
            // Clamp transform range to our text range
            let t_start = transform.range.start.max(start);
            let t_end = transform.range.end.min(end);

            // Copy text before transform
            if t_start > pos
                && let Some(before) = text.get((pos - start)..(t_start - start))
            {
                result.push_str(before);
            }

            // Apply transform
            result.push_str(&transform.to);

            pos = t_end;
        }

        if !applied {
            Cow::Borrowed(text)
        } else {
            // Copy remaining text
            if pos < end
                && let Some(rest) = text.get(pos - start..)
            {
                result.push_str(rest);
            }
            Cow::Owned(result)
        }
    }

    /// Build sorted render events into the provided Vec.
    fn build_render_events_into(&self, events: &mut Vec<RenderEvent>) {
        events.clear();
        let capacity = self.buffers.highlights.len() * 2
            + self.buffers.replaces.len() * 2
            + self.buffers.table_replaces.len() * 2
            + self.buffers.mermaid_replaces.len() * 2;
        events.reserve(capacity);

        for (i, hl) in self.buffers.highlights.iter().enumerate() {
            events.push(RenderEvent {
                pos: hl.range.start,
                kind: RenderEventKind::Highlight,
                index: i,
                is_end: false,
            });
            events.push(RenderEvent {
                pos: hl.range.end,
                kind: RenderEventKind::Highlight,
                index: i,
                is_end: true,
            });
        }
        for (i, r) in self.buffers.replaces.iter().enumerate() {
            events.push(RenderEvent {
                pos: r.range.start,
                kind: RenderEventKind::Replace,
                index: i,
                is_end: false,
            });
            events.push(RenderEvent {
                pos: r.range.end,
                kind: RenderEventKind::Replace,
                index: i,
                is_end: true,
            });
        }
        for (i, t) in self.buffers.table_replaces.iter().enumerate() {
            events.push(RenderEvent {
                pos: t.range.start,
                kind: RenderEventKind::Table,
                index: i,
                is_end: false,
            });
            events.push(RenderEvent {
                pos: t.range.end,
                kind: RenderEventKind::Table,
                index: i,
                is_end: true,
            });
        }
        for (i, m) in self.buffers.mermaid_replaces.iter().enumerate() {
            events.push(RenderEvent {
                pos: m.range.start,
                kind: RenderEventKind::Mermaid,
                index: i,
                is_end: false,
            });
            events.push(RenderEvent {
                pos: m.range.end,
                kind: RenderEventKind::Mermaid,
                index: i,
                is_end: true,
            });
        }
        events.sort_unstable();
    }

    /// Build sorted render events into a new Vec.
    fn build_render_events(&self) -> Vec<RenderEvent> {
        let mut events = Vec::new();
        self.build_render_events_into(&mut events);
        events
    }

    /// Render to ANSI-styled string.
    /// If `pretty` is true, syntax markers are hidden.
    /// Returns the rendered string and a source map for copy-paste support.
    pub fn render_ansi(&mut self, pretty: bool) -> (String, SourceMap) {
        let events = self.build_render_events();

        // Apply force transforms in place over a copy of `self.text` so the ANSI path picks them up without restructuring `push`
        // See `Transform::force` for the byte-length invariant
        let text_owned: Option<String> = if self.buffers.transforms.iter().any(|t| t.force) {
            let mut bytes = self.text.as_bytes().to_vec();
            for t in &self.buffers.transforms {
                if !t.force {
                    continue;
                }
                debug_assert_eq!(
                    t.to.len(),
                    t.range.end - t.range.start,
                    "force transforms must preserve byte length",
                );
                debug_assert!(
                    self.text.is_char_boundary(t.range.start)
                        && self.text.is_char_boundary(t.range.end),
                    "force transform range must align with char boundaries",
                );
                if let Some(slot) = bytes.get_mut(t.range.clone()) {
                    slot.copy_from_slice(t.to.as_bytes());
                }
            }
            Some(String::from_utf8(bytes).expect("force transforms preserve UTF-8"))
        } else {
            None
        };
        let view_text: &str = text_owned.as_deref().unwrap_or(self.text);

        let mut out = String::with_capacity(view_text.len() * 2);
        let mut source_map = SourceMap::new();
        let mut rendered_offset = 0;
        let mut hl_ids = BTreeSet::<usize>::new();
        let mut last_pos = 0;
        let mut replace: Option<usize> = None;
        let mut table_replace: Option<usize> = None;
        let mut mermaid_replace: Option<usize> = None;
        let mut current = (0..0, Style::new());

        fn push(
            out: &mut String,
            current: &mut (Range<usize>, Style),
            text: &str,
            range: Range<usize>,
            style: Style,
            source_map: &mut SourceMap,
            rendered_offset: &mut usize,
        ) {
            let (crange, cstyle) = current;
            let ctext = text.get(crange.clone()).unwrap_or("");
            if !range.is_empty() && style == *cstyle {
                if !ctext.is_empty() {
                    debug_assert_eq!(crange.end, range.start);
                }
                crange.end = range.end;
                return;
            }
            if !ctext.is_empty() {
                source_map.add(*rendered_offset, crange.clone());
                *rendered_offset += ctext.len();

                if cstyle.is_plain() {
                    out.push_str(ctext);
                } else {
                    out.push_str(&ctext.astyle(*cstyle).to_string());
                }
            }
            *crange = range;
            *cstyle = style;
        }

        for ev in &events {
            if replace.is_none()
                && table_replace.is_none()
                && mermaid_replace.is_none()
                && ev.pos > last_pos
            {
                let should_skip = pretty
                    && all_hidden(
                        hl_ids
                            .iter()
                            .filter_map(|&i| self.buffers.highlights.get(i).map(|h| h.style)),
                    );

                if should_skip {
                    push(
                        &mut out,
                        &mut current,
                        view_text,
                        ev.pos..ev.pos,
                        Style::new(),
                        &mut source_map,
                        &mut rendered_offset,
                    );
                } else {
                    let mut style = merge_styles(
                        hl_ids
                            .iter()
                            .filter_map(|&i| self.buffers.highlights.get(i).map(|h| h.style)),
                    );
                    let text = view_text.get(last_pos..ev.pos).unwrap_or("");
                    let is_invert = style.get_effects().contains(Effects::INVERT);
                    if text.as_bytes().iter().all(|&ch| ch == b'\n')
                        || (text.as_bytes().iter().all(u8::is_ascii_whitespace)
                            && ((!is_invert && style.get_bg_color().is_none())
                                || (is_invert && style.get_fg_color().is_none())))
                    {
                        style = Style::new();
                    }
                    push(
                        &mut out,
                        &mut current,
                        view_text,
                        last_pos..ev.pos,
                        style,
                        &mut source_map,
                        &mut rendered_offset,
                    );
                }
                last_pos = ev.pos;
            }

            match ev.kind {
                RenderEventKind::Replace => {
                    if ev.is_end && replace == Some(ev.index) {
                        replace = None;
                        out.push_str(&Reset.to_string());
                    } else if !ev.is_end && replace.is_none() && table_replace.is_none() {
                        let Some(repl) = self.buffers.replaces.get(ev.index) else {
                            continue;
                        };
                        replace = Some(ev.index);
                        push(
                            &mut out,
                            &mut current,
                            view_text,
                            ev.pos..ev.pos,
                            Style::new(),
                            &mut source_map,
                            &mut rendered_offset,
                        );
                        out.push_str(&Reset.to_string());

                        let ansi_content = render_replace_ansi(&repl.highlighted);

                        let replace_text_len: usize = repl
                            .highlighted
                            .iter()
                            .flat_map(|line| line.iter().map(|(_, t)| t.len()))
                            .sum();
                        source_map.add(rendered_offset, repl.range.clone());
                        rendered_offset += replace_text_len;

                        out.push_str(&ansi_content);
                        last_pos = repl.range.end;
                    }
                }
                RenderEventKind::Table => {
                    if ev.is_end && table_replace == Some(ev.index) {
                        table_replace = None;
                    } else if !ev.is_end && table_replace.is_none() && pretty {
                        let Some(trepl) = self.buffers.table_replaces.get(ev.index) else {
                            continue;
                        };
                        table_replace = Some(ev.index);
                        push(
                            &mut out,
                            &mut current,
                            view_text,
                            ev.pos..ev.pos,
                            Style::new(),
                            &mut source_map,
                            &mut rendered_offset,
                        );
                        // Block lines must start at a line boundary; a display-math replacement can occur mid-paragraph
                        // Styled chunks end with a reset sequence after the newline, so check both forms
                        let at_line_start =
                            out.is_empty() || out.ends_with('\n') || out.ends_with("\n\x1b[0m");
                        if !at_line_start {
                            out.push('\n');
                            rendered_offset += 1;
                        }
                        for line in &trepl.lines {
                            out.push_str(line);
                            out.push('\n');
                            rendered_offset += line.len() + 1;
                        }
                        last_pos = trepl.range.end;
                        // Advance `current` past the table so trailing text doesn't merge back to the pre-table position
                        current.0 = trepl.range.end..trepl.range.end;
                    }
                }
                RenderEventKind::Mermaid => {
                    if ev.is_end && mermaid_replace == Some(ev.index) {
                        mermaid_replace = None;
                    } else if !ev.is_end && mermaid_replace.is_none() && pretty {
                        let Some(mrepl) = self.buffers.mermaid_replaces.get(ev.index) else {
                            continue;
                        };
                        mermaid_replace = Some(ev.index);
                        push(
                            &mut out,
                            &mut current,
                            view_text,
                            ev.pos..ev.pos,
                            Style::new(),
                            &mut source_map,
                            &mut rendered_offset,
                        );

                        for line in &mrepl.lines {
                            out.push_str(line);
                            out.push('\n');
                            rendered_offset += line.len() + 1;
                        }
                        last_pos = mrepl.range.end;
                        current.0 = mrepl.range.end..mrepl.range.end;
                    }
                }
                RenderEventKind::Highlight => {
                    if ev.is_end {
                        hl_ids.remove(&ev.index);
                    } else {
                        hl_ids.insert(ev.index);
                    }
                }
            }
        }

        let len = view_text.len();
        if last_pos < len {
            push(
                &mut out,
                &mut current,
                view_text,
                last_pos..len,
                Style::new(),
                &mut source_map,
                &mut rendered_offset,
            );
        }
        push(
            &mut out,
            &mut current,
            view_text,
            len..len,
            Style::new(),
            &mut source_map,
            &mut rendered_offset,
        );
        (out, source_map)
    }

    /// Render to ratatui Lines.
    /// If `pretty` is true, syntax markers are hidden.
    /// Returns rendered lines, line source map, and optional checkpoint.
    pub fn render_ratatui(&mut self, pretty: bool) -> (MarkdownRenderOutput, Option<Checkpoint>) {
        let render_events = self.build_render_events();

        self.buffers.current_spans.clear();
        self.buffers.active_highlights.clear();

        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut line_source_map: Vec<usize> = Vec::new();
        let mut hyperlinks: Vec<HyperlinkTarget> = Vec::new();
        let mut tables: Vec<TableCopyMeta> = Vec::new();

        let mut last_pos = 0;
        let mut replace: Option<usize> = None;
        let mut table_replace: Option<usize> = None;
        let mut mermaid_replace: Option<usize> = None;
        let mut skip_leading_newline = false;
        let mut in_hidden_code_block = false;
        let mut next_link_idx: usize = 0;
        // Running display-column tracker for the in-progress line.
        let mut cur_col_in_line: usize = 0;

        let checkpoint_info = self.last_checkpoint;
        let mut checkpoint_output_lines: Option<usize> = None;

        // Style already adapted, no need to call adapt_style again
        let code_bg_style: ratatui::style::Style = self.ms.code_background.style_into();

        let in_untagged_code = |pos: usize, buffers: &MarkdownBuffers| -> bool {
            buffers
                .untagged_code_ranges
                .iter()
                .any(|range| pos >= range.start && pos < range.end)
        };

        let mut current_source_line = 0usize;
        let mut last_line_count_pos = 0usize;
        let mut pending_line_is_code = false;

        let count_newlines_in_range = |from: usize, to: usize, text: &str| -> usize {
            if to <= from {
                return 0;
            }
            let to = to.min(text.len());
            let from = from.min(to);
            // Use as_bytes() to avoid panicking on non-char-boundary offsets.
            // This is safe because '\n' (0x0A) is a single-byte ASCII value that can never appear as a UTF-8 continuation byte (0x80..0xBF)
            text.as_bytes()
                .get(from..to)
                .map(|s| s.iter().filter(|&&b| b == b'\n').count())
                .unwrap_or(0)
        };

        for ev in &render_events {
            if replace.is_none()
                && table_replace.is_none()
                && mermaid_replace.is_none()
                && ev.pos > last_pos
            {
                // Split text processing at the checkpoint boundary when last_pos < cp_byte <= ev.pos:
                // process [last_pos..cp_byte], capture lines.len(), then process [cp_byte..ev.pos]
                let split_at_checkpoint = checkpoint_output_lines.is_none()
                    && checkpoint_info
                        .map(|(_, cp_byte)| last_pos < cp_byte && cp_byte <= ev.pos)
                        .unwrap_or(false);

                let cp_byte = checkpoint_info.map(|(_, cp)| cp).unwrap_or(0);

                // Snap cp_byte to the nearest char boundary
                // Checkpoint byte offsets come from pulldown-cmark event ranges, which should always be char-aligned
                // Snapping forward is safe: it only affects where we split the text for line counting
                let cp_byte = {
                    let mut b = cp_byte;
                    while b < self.text.len() && !self.text.is_char_boundary(b) {
                        b += 1;
                    }
                    b
                };

                let ranges: &[(usize, usize)] = if split_at_checkpoint {
                    // Process in two parts, capturing checkpoint between them
                    &[(last_pos, cp_byte), (cp_byte, ev.pos)]
                } else {
                    // Process as single range
                    &[(last_pos, ev.pos)]
                };

                for (range_idx, &(range_start, range_end)) in ranges.iter().enumerate() {
                    // After processing the first range when splitting, capture checkpoint.
                    // Without this flush, the checkpoint's `output_lines` count would be too low, causing the line to vanish on re-render
                    if split_at_checkpoint && range_idx == 1 {
                        if !self.buffers.current_spans.is_empty() {
                            line_source_map.push(current_source_line);
                            let line = Line::from(std::mem::take(&mut self.buffers.current_spans));
                            lines.push(line);
                            cur_col_in_line = 0;
                        }
                        checkpoint_output_lines = Some(lines.len());
                    }

                    if range_end <= range_start {
                        continue;
                    }

                    // Update source line counter
                    if range_start > last_line_count_pos {
                        current_source_line +=
                            count_newlines_in_range(last_line_count_pos, range_start, self.text);
                        last_line_count_pos = range_start;
                    }

                    let is_hidden = pretty
                        && all_hidden(
                            self.buffers
                                .active_highlights
                                .iter()
                                .filter_map(|&i| self.buffers.highlights.get(i).map(|h| h.style)),
                        );

                    if is_hidden {
                        let at_line_start = range_start == 0
                            || self.text.as_bytes().get(range_start - 1) == Some(&b'\n');
                        if at_line_start {
                            // Check if this hidden block is a code fence (``` or ~~~).
                            // Only code fences need separator handling; heading markers (#) are also hidden at line start but are unpaired
                            let hidden_text = self
                                .text
                                .get(range_start..range_end)
                                .unwrap_or("")
                                .trim_start();
                            let is_code_fence =
                                hidden_text.starts_with("```") || hidden_text.starts_with("~~~");

                            if is_code_fence {
                                // Emit a blank separator before an OPENING fence (not closing)
                                // Otherwise adjacent blocks (a list then a code block) collapse together once pretty mode hides the fence markers
                                if !in_hidden_code_block
                                    && lines.last().is_some_and(|l| l.width() > 0)
                                {
                                    line_source_map.push(current_source_line);
                                    lines.push(Line::default());
                                    cur_col_in_line = 0;
                                }
                                in_hidden_code_block = !in_hidden_code_block;
                            }
                            skip_leading_newline = true;
                        }
                    } else {
                        let Some(mut text) = self.text.get(range_start..range_end) else {
                            continue;
                        };
                        let mut text_start = range_start;

                        if skip_leading_newline && let Some(rest) = text.strip_prefix('\n') {
                            text = rest;
                            text_start += 1;
                        }
                        skip_leading_newline = false;

                        if !text.is_empty() {
                            let style =
                                merge_styles(self.buffers.active_highlights.iter().filter_map(
                                    |&i| self.buffers.highlights.get(i).map(|h| h.style),
                                ));

                            let transformed = self.apply_transforms(text, range_start, pretty);
                            let ratatui_style: ratatui::style::Style = style.style_into();

                            let chunk_src_start = text_start;
                            let chunk_src_end = text_start + text.len();

                            // Advance the cursor past links that ended before this chunk starts, then check if any remaining link overlaps it
                            // When none does, skip all hyperlink bookkeeping so the common no-link chunk does no extra work
                            while self
                                .buffers
                                .link_targets
                                .get(next_link_idx)
                                .is_some_and(|t| t.source_range.end <= chunk_src_start)
                            {
                                next_link_idx += 1;
                            }
                            let chunk_has_links = self
                                .buffers
                                .link_targets
                                .get(next_link_idx)
                                .is_some_and(|t| t.source_range.start < chunk_src_end);

                            let chunk_links: Vec<ChunkLinkRange> = if chunk_has_links {
                                chunk_link_offsets(
                                    &self.buffers.link_targets,
                                    next_link_idx,
                                    chunk_src_start,
                                    chunk_src_end,
                                    pretty,
                                    &self.buffers.transforms,
                                )
                            } else {
                                Vec::new()
                            };

                            let mut byte_offset = text_start;
                            let mut seg_x_offset: usize = 0;
                            let is_in_code = in_untagged_code(text_start, self.buffers);
                            pending_line_is_code = is_in_code;
                            for (idx, segment) in transformed.split('\n').enumerate() {
                                if idx > 0 {
                                    line_source_map.push(current_source_line);
                                    let line =
                                        Line::from(std::mem::take(&mut self.buffers.current_spans));
                                    lines.push(if is_in_code {
                                        line.style(code_bg_style)
                                    } else {
                                        line
                                    });
                                    if byte_offset > last_line_count_pos {
                                        current_source_line += count_newlines_in_range(
                                            last_line_count_pos,
                                            byte_offset,
                                            self.text,
                                        );
                                        last_line_count_pos = byte_offset;
                                    }
                                    cur_col_in_line = 0;
                                }

                                if !chunk_links.is_empty() {
                                    emit_segment_hyperlinks(
                                        &chunk_links,
                                        &self.buffers.link_targets,
                                        segment,
                                        seg_x_offset,
                                        cur_col_in_line,
                                        lines.len(),
                                        &mut hyperlinks,
                                    );
                                }

                                if !segment.is_empty() {
                                    self.buffers
                                        .current_spans
                                        .push(Span::styled(segment.to_string(), ratatui_style));
                                    cur_col_in_line += unicode_display_width(segment);
                                }
                                byte_offset += segment.len() + 1;
                                seg_x_offset += segment.len() + 1;
                            }
                        }
                    }
                }
                last_pos = ev.pos;
            }

            match ev.kind {
                RenderEventKind::Replace => {
                    if ev.is_end && replace == Some(ev.index) {
                        replace = None;
                    } else if !ev.is_end && replace.is_none() && table_replace.is_none() {
                        let Some(repl) = self.buffers.replaces.get(ev.index) else {
                            continue;
                        };
                        replace = Some(ev.index);

                        // Update source line to code start
                        if repl.range.start > last_line_count_pos {
                            current_source_line += count_newlines_in_range(
                                last_line_count_pos,
                                repl.range.start,
                                self.text,
                            );
                        }
                        let code_start_source_line = current_source_line;

                        for (line_idx, line_spans) in repl.highlighted.iter().enumerate() {
                            current_source_line = code_start_source_line + line_idx;

                            for (syn_style, text) in line_spans {
                                let full_style = super::syntax::to_anstyle(*syn_style);
                                let with_bg =
                                    full_style.bg_color(self.ms.code_background.get_bg_color());
                                // This is the only legitimate inline adapt_style call for a dynamically created syntect+background combo
                                let adapted = adapt_style(with_bg);
                                let ratatui_style: ratatui::style::Style = adapted.style_into();

                                for (idx, segment) in text.split('\n').enumerate() {
                                    if idx > 0 {
                                        line_source_map.push(current_source_line);
                                        let line = Line::from(std::mem::take(
                                            &mut self.buffers.current_spans,
                                        ))
                                        .style(code_bg_style);
                                        lines.push(line);
                                        current_source_line += 1;
                                        cur_col_in_line = 0;
                                    }
                                    if !segment.is_empty() {
                                        self.buffers
                                            .current_spans
                                            .push(Span::styled(segment.to_string(), ratatui_style));
                                        cur_col_in_line += unicode_display_width(segment);
                                    }
                                }
                            }

                            if !self.buffers.current_spans.is_empty() {
                                line_source_map.push(current_source_line);
                                let line =
                                    Line::from(std::mem::take(&mut self.buffers.current_spans))
                                        .style(code_bg_style);
                                lines.push(line);
                                cur_col_in_line = 0;
                            }
                        }

                        last_pos = repl.range.end;
                        let newlines_in_code =
                            count_newlines_in_range(repl.range.start, repl.range.end, self.text);
                        current_source_line = code_start_source_line + newlines_in_code;
                        last_line_count_pos = repl.range.end;

                        if checkpoint_output_lines.is_none()
                            && let Some((_, cp_byte)) = checkpoint_info
                            && last_pos >= cp_byte
                        {
                            checkpoint_output_lines = Some(lines.len());
                        }
                    }
                }
                RenderEventKind::Table => {
                    if ev.is_end && table_replace == Some(ev.index) {
                        table_replace = None;
                    } else if !ev.is_end && table_replace.is_none() && pretty {
                        let Some(trepl) = self.buffers.table_replaces.get(ev.index) else {
                            continue;
                        };
                        table_replace = Some(ev.index);

                        // Tables always start at a line boundary, so for them this is a no-op
                        // A display-math block replacement can occur mid-paragraph (`text $$x$$ more`)
                        // Without the flush, the pending "text " spans would be emitted AFTER the block lines
                        if !self.buffers.current_spans.is_empty() {
                            line_source_map.push(current_source_line);
                            lines.push(Line::from(std::mem::take(&mut self.buffers.current_spans)));
                            // cur_col_in_line is reset unconditionally after the block lines are emitted below
                        }

                        // Update source line to table start
                        if trepl.range.start > last_line_count_pos {
                            current_source_line += count_newlines_in_range(
                                last_line_count_pos,
                                trepl.range.start,
                                self.text,
                            );
                        }
                        let table_start_source_line = current_source_line;
                        let table_base_line = lines.len();

                        for (line_idx, styled_line) in trepl.styled_lines.iter().enumerate() {
                            let offset = trepl
                                .line_source_offsets
                                .get(line_idx)
                                .copied()
                                .unwrap_or(0);
                            current_source_line = table_start_source_line + offset;
                            line_source_map.push(current_source_line);
                            lines.push(styled_line.clone());
                        }
                        // Translate table-local hyperlink coordinates into absolute line indices and append to the global list
                        for link in &trepl.hyperlinks {
                            hyperlinks.push(HyperlinkTarget {
                                line_index: table_base_line + link.line_offset,
                                column_range: link.column_range.clone(),
                                url: link.url.clone(),
                                id: link.id,
                            });
                        }
                        if trepl.n_cols > 0 {
                            tables.push(TableCopyMeta {
                                line_index: table_base_line,
                                line_count: trepl.styled_lines.len(),
                                n_cols: trepl.n_cols,
                                cells: trepl.cell_copies.clone(),
                            });
                        }
                        // Table emits whole pre-rendered lines; reset col so any subsequent inline content starts at column 0
                        cur_col_in_line = 0;

                        last_pos = trepl.range.end;
                        let newlines_in_table =
                            count_newlines_in_range(trepl.range.start, trepl.range.end, self.text);
                        current_source_line = table_start_source_line + newlines_in_table;
                        last_line_count_pos = trepl.range.end;

                        if checkpoint_output_lines.is_none()
                            && let Some((_, cp_byte)) = checkpoint_info
                            && last_pos >= cp_byte
                        {
                            checkpoint_output_lines = Some(lines.len());
                        }
                    }
                }
                RenderEventKind::Mermaid => {
                    if ev.is_end && mermaid_replace == Some(ev.index) {
                        mermaid_replace = None;
                    } else if !ev.is_end && mermaid_replace.is_none() && pretty {
                        let Some(mrepl) = self.buffers.mermaid_replaces.get(ev.index) else {
                            continue;
                        };
                        mermaid_replace = Some(ev.index);

                        if mrepl.range.start > last_line_count_pos {
                            current_source_line += count_newlines_in_range(
                                last_line_count_pos,
                                mrepl.range.start,
                                self.text,
                            );
                        }
                        let start_source_line = current_source_line;

                        for styled_line in &mrepl.styled_lines {
                            line_source_map.push(start_source_line);
                            lines.push(styled_line.clone());
                        }
                        cur_col_in_line = 0;

                        last_pos = mrepl.range.end;
                        let newlines =
                            count_newlines_in_range(mrepl.range.start, mrepl.range.end, self.text);
                        current_source_line = start_source_line + newlines;
                        last_line_count_pos = mrepl.range.end;

                        if checkpoint_output_lines.is_none()
                            && let Some((_, cp_byte)) = checkpoint_info
                            && last_pos >= cp_byte
                        {
                            checkpoint_output_lines = Some(lines.len());
                        }
                    }
                }
                RenderEventKind::Highlight => {
                    if ev.is_end {
                        self.buffers.active_highlights.retain(|&x| x != ev.index);
                    } else {
                        self.buffers.active_highlights.push(ev.index);
                    }
                }
            }
        }

        // Handle remaining text
        let len = self.text.len();
        if last_pos < len {
            // Apply force transforms only; non-force transforms have never been applied in this trailing path
            // Force transforms preserve byte length, so source offsets below stay valid
            let raw = self.text.get(last_pos..len).unwrap_or("");
            let transformed = self.apply_transforms(raw, last_pos, false);
            debug_assert_eq!(transformed.len(), raw.len());
            let text: &str = &transformed;
            let is_only_whitespace = text.as_bytes().iter().all(u8::is_ascii_whitespace);

            if !(pretty && is_only_whitespace) {
                if last_pos > last_line_count_pos {
                    current_source_line +=
                        count_newlines_in_range(last_line_count_pos, last_pos, self.text);
                    last_line_count_pos = last_pos;
                }
                let chunk_src_start = last_pos;
                let chunk_src_end = last_pos + text.len();

                // Same cursor-skip pattern as the main path: a no-link chunk skips all hyperlink bookkeeping
                while self
                    .buffers
                    .link_targets
                    .get(next_link_idx)
                    .is_some_and(|t| t.source_range.end <= chunk_src_start)
                {
                    next_link_idx += 1;
                }
                let chunk_has_links = self
                    .buffers
                    .link_targets
                    .get(next_link_idx)
                    .is_some_and(|t| t.source_range.start < chunk_src_end);

                // Trailing text bypasses apply_transforms (it's emitted raw), so transformed offsets equal source offsets within the chunk
                let chunk_links: Vec<ChunkLinkRange> = if chunk_has_links {
                    chunk_link_offsets(
                        &self.buffers.link_targets,
                        next_link_idx,
                        chunk_src_start,
                        chunk_src_end,
                        false,
                        &[],
                    )
                } else {
                    Vec::new()
                };

                let mut byte_offset = last_pos;
                let mut seg_x_offset: usize = 0;
                let is_in_code = in_untagged_code(last_pos, self.buffers);
                pending_line_is_code = is_in_code;

                for (idx, segment) in text.split('\n').enumerate() {
                    if idx > 0 {
                        line_source_map.push(current_source_line);
                        let line = Line::from(std::mem::take(&mut self.buffers.current_spans));
                        lines.push(if is_in_code {
                            line.style(code_bg_style)
                        } else {
                            line
                        });
                        if byte_offset > last_line_count_pos {
                            current_source_line += count_newlines_in_range(
                                last_line_count_pos,
                                byte_offset,
                                self.text,
                            );
                            last_line_count_pos = byte_offset;
                        }
                        cur_col_in_line = 0;
                    }

                    if !chunk_links.is_empty() {
                        emit_segment_hyperlinks(
                            &chunk_links,
                            &self.buffers.link_targets,
                            segment,
                            seg_x_offset,
                            cur_col_in_line,
                            lines.len(),
                            &mut hyperlinks,
                        );
                    }

                    if !segment.is_empty() {
                        self.buffers
                            .current_spans
                            .push(Span::raw(segment.to_string()));
                        cur_col_in_line += unicode_display_width(segment);
                    }
                    byte_offset += segment.len() + 1;
                    seg_x_offset += segment.len() + 1;
                }
            }
        }

        // Emit final line. Use the code-block membership of the chunk that produced these spans, not a recomputed one.
        // An unterminated bare fence ends its range exactly at last_pos (EOF); the end-exclusive range check there would drop the code bg
        if !self.buffers.current_spans.is_empty() {
            line_source_map.push(current_source_line);
            let final_is_code = pending_line_is_code;
            let line = Line::from(std::mem::take(&mut self.buffers.current_spans));
            lines.push(if final_is_code {
                line.style(code_bg_style)
            } else {
                line
            });
        }

        // If checkpoint wasn't captured during event processing, compute it based on the number of newlines in the text up to checkpoint byte
        // This handles cases where there are no events past the checkpoint (e.g., incomplete list items).
        if checkpoint_output_lines.is_none()
            && let Some((_, cp_byte)) = checkpoint_info
        {
            // The checkpoint is at the start of the NEXT block, so the frozen content must not include anything at or after cp_byte
            // line_source_map[i] is the source line at which output line i was created; source_line_at_cp is the source line containing cp_byte

            let source_line_at_cp = self
                .text
                .get(..cp_byte.min(self.text.len()))
                .unwrap_or("")
                .bytes()
                .filter(|&b| b == b'\n')
                .count();

            // When the checkpoint is at or past the end of the text, ALL output lines are frozen (the checkpointed block consumed the entire input)
            // Otherwise, output lines created at source lines strictly before the checkpoint source line are frozen
            let complete_lines = if cp_byte >= self.text.len() {
                lines.len()
            } else {
                line_source_map
                    .iter()
                    .take_while(|&&src_line| src_line < source_line_at_cp)
                    .count()
            };

            checkpoint_output_lines = Some(complete_lines.min(lines.len()));
        }

        let checkpoint = match (checkpoint_info, checkpoint_output_lines) {
            (Some((kind, source_bytes)), Some(output_lines)) => Some(Checkpoint {
                source_bytes,
                output_lines,
                kind,
            }),
            _ => None,
        };

        // Now that `line_source_map` is final, map each parsed code block's body onto its rendered (pre-wrap) line range
        let text = self.text;
        let code_blocks = crate::engine::markdown::output::build_code_block_spans(
            text,
            &line_source_map,
            std::mem::take(&mut self.buffers.code_blocks),
        );

        (
            MarkdownRenderOutput {
                lines,
                line_source_map,
                hyperlinks,
                code_blocks,
                tables,
            },
            checkpoint,
        )
    }
}
