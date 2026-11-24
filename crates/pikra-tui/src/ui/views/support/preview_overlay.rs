// Adapted from the Grok Build pager (Apache-2.0). Copyright 2023-2026 SpaceXAI.

//! Renders a bordered popup showing a preview of multiline content.
//! Shows first N and last N lines with a `⋮` separator when content exceeds the preview limit.
//!
//! Used for:
//! - Paste element previews in the prompt widget
//! - Queue item previews in the queue pane

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Widget};

use crate::engine::render::line_utils::{truncate_line, truncate_str};
use crate::engine::render::safe_buf::SafeBuf;

// ---------------------------------------------------------------------------
// PreviewStyle — configurable colors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct PreviewStyle {
    /// Background color for the entire overlay box.
    pub bg: Color,
    /// Foreground color for content text.
    pub text_fg: Color,
    /// Foreground color for the border and dots separator.
    pub border_fg: Color,
}

impl PreviewStyle {
    pub fn new(bg: Color, text_fg: Color, border_fg: Color) -> Self {
        Self {
            bg,
            text_fg,
            border_fg,
        }
    }
}

// ---------------------------------------------------------------------------
// PreviewConfig — layout configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PreviewConfig {
    /// Number of lines to show from the top and bottom when truncating.
    /// If content has more than `preview_lines * 2` lines, shows first N, dots separator, and last N lines.
    pub preview_lines: usize,

    /// Width of the overlay as a fraction of the available width (0.0 to 1.0).
    /// Default: 0.75 (3/4 of available width).
    pub width_ratio: f32,

    /// Vertical gap between the overlay's bottom border and the anchor point.
    /// At 0 the overlay sits flush against the anchor.
    pub bottom_gap: u16,

    /// Minimum width for the overlay. Below this, the overlay won't render.
    pub min_width: u16,

    /// Minimum height for the overlay area. Below this, the overlay won't render.
    pub min_height: u16,

    /// Optional one-line hint painted into the bottom border row, e.g. `╰─ enter to expand ────╯`.
    /// It costs no content row and is skipped when the box is too narrow to fit readable text.
    /// `None` (the default) leaves the plain border.
    pub hint: Option<Line<'static>>,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        Self {
            preview_lines: 3,
            width_ratio: 0.75,
            bottom_gap: 0,
            min_width: 20,
            min_height: 5,
            hint: None,
        }
    }
}

// ---------------------------------------------------------------------------
// render_preview_overlay — main rendering function
// ---------------------------------------------------------------------------

/// Bottom-anchored preview. Over `preview_lines * 2` lines, show head, an elision, and tail.
/// `None` when the area is too small or content is empty.
pub fn render_preview_overlay(
    buf: &mut Buffer,
    area: Rect,
    content: &str,
    style: PreviewStyle,
    config: PreviewConfig,
) -> Option<Rect> {
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();

    if total == 0 || area.height < config.min_height || area.width < config.min_width {
        return None;
    }

    // Calculate content layout
    let needs_dots = total > config.preview_lines * 2;
    let content_lines: usize = if needs_dots {
        config.preview_lines * 2 + 1 // top + dots + bottom
    } else {
        total
    };

    // Box dimensions: border(1) + content + border(1)
    let box_height = (content_lines as u16 + 2).min(area.height);
    let box_width = ((area.width as f32) * config.width_ratio) as u16;

    // Anchor at bottom of area
    let anchor_bottom = area.y + area.height - config.bottom_gap;
    let box_x = area.x + (area.width.saturating_sub(box_width)) / 2;
    let box_y = anchor_bottom.saturating_sub(box_height);

    let box_area = Rect {
        x: box_x,
        y: box_y,
        width: box_width,
        height: box_height,
    };

    // Build the bordered block
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(style.border_fg))
        .style(Style::default().bg(style.bg));
    let inner = block.inner(box_area);

    // Clear background: fill every cell so underlying content doesn't bleed through
    Clear.render(box_area, buf);
    buf.set_style(box_area, Style::default().bg(style.bg));

    block.render(box_area, buf);

    // Render content
    let text_style = Style::default().fg(style.text_fg).bg(style.bg);
    let dots_style = Style::default().fg(style.border_fg).bg(style.bg);

    render_content_lines(
        buf,
        inner,
        &lines,
        needs_dots,
        config.preview_lines,
        text_style,
        dots_style,
    );

    // The hint lives in the bottom border row, so it costs no content row
    // The border interruption reads as a label even when a theme aliases the hint palette to the border/content colors
    if let Some(hint) = &config.hint {
        render_border_hint(buf, box_area, hint, style.bg);
    }

    Some(box_area)
}

fn render_content_lines(
    buf: &mut Buffer,
    inner: Rect,
    lines: &[&str],
    needs_dots: bool,
    preview_lines: usize,
    text_style: Style,
    dots_style: Style,
) {
    let total = lines.len();
    let mut row = 0u16;
    let max_rows = inner.height;

    if needs_dots {
        // Top lines
        for line in lines.iter().take(preview_lines) {
            if row >= max_rows {
                break;
            }
            render_line(buf, inner.x, inner.y + row, inner.width, line, text_style);
            row += 1;
        }

        // Dots separator
        if row < max_rows {
            let omitted = total - preview_lines * 2;
            let dots_text = format!("⋮ ({omitted} more lines)");
            buf.set_span_safe(
                inner.x,
                inner.y + row,
                &Span::styled(dots_text, dots_style),
                inner.width,
            );
            row += 1;
        }

        // Bottom lines
        let start = total.saturating_sub(preview_lines);
        for line in lines.iter().skip(start) {
            if row >= max_rows {
                break;
            }
            render_line(buf, inner.x, inner.y + row, inner.width, line, text_style);
            row += 1;
        }
    } else {
        // Show all lines
        for line in lines {
            if row >= max_rows {
                break;
            }
            render_line(buf, inner.x, inner.y + row, inner.width, line, text_style);
            row += 1;
        }
    }
}

#[inline]
fn render_line(buf: &mut Buffer, x: u16, y: u16, width: u16, line: &str, style: Style) {
    let truncated = truncate_str(line, width as usize);
    buf.set_span_safe(x, y, &Span::styled(truncated, style), width);
}

/// Hint sits in the bottom border after the corner and one dash. Corners and one dash per side are never overwritten.
/// Skipped when the box is too narrow for readable text.
fn render_border_hint(buf: &mut Buffer, box_area: Rect, hint: &Line<'static>, bg: Color) {
    // Chrome around the text: corners (2) + one dash each side (2) + pads (2).
    const CHROME: u16 = 6;
    // Below this the truncated text is noise; keep the plain border
    const MIN_TEXT_WIDTH: u16 = 8;
    let text_width = box_area.width.saturating_sub(CHROME);
    if text_width < MIN_TEXT_WIDTH {
        return;
    }

    let mut line = truncate_line(hint.clone(), text_width as usize);
    // The box bg wins so the hint sits on the border row fill.
    for span in &mut line.spans {
        span.style = span.style.bg(bg);
    }
    let pad = Span::styled(" ", Style::default().bg(bg));
    let mut spans = vec![pad.clone()];
    spans.append(&mut line.spans);
    spans.push(pad);

    let y = box_area.y + box_area.height - 1;
    buf.set_line_safe(box_area.x + 2, y, &Line::from(spans), box_area.width - 4);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
