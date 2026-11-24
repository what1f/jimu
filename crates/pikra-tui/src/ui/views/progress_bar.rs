// Adapted from the Grok Build pager (Apache-2.0). Copyright 2023-2026 SpaceXAI.

//! Unicode block progress bar at 1/8th-cell resolution via the LEFT fractional blocks `▏▎▍▌▋▊▉█`.
//!
//! Consolas (the default ConHost font) is missing the narrow ones (U+258F..=U+2589, microsoft/terminal#387).
//! On legacy ConHost we substitute the shade glyphs `░▒▓` from CP437 instead.
//! Same eighth-resolution input; the cell just reads as a density pattern rather than a true left-justified bar.
//!
//! ```ignore
//! render_progress_bar(buf, x, y, 5, 0.42, fg_color, bg_color);
//! ```

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Style};
use ratatui::text::Span;

/// LEFT-fractional block glyphs, indexed 0..=8 (0 is empty, 8 is full).
const BLOCKS: [&str; 9] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"];

/// Shade substitutes used on hosts that can't render the LEFT-fractional blocks.
/// Same index domain as [`BLOCKS`] so call sites stay uniform.
const SHADES: [&str; 9] = ["", "░", "░", "░", "▒", "▒", "▓", "▓", "█"];

/// Per-cell partial-fill glyph table: `BLOCKS` everywhere except legacy ConHost, where we substitute `SHADES`.
fn partial_blocks() -> &'static [&'static str; 9] {
    if crate::engine::render::glyphs::is_legacy_windows_console() {
        &SHADES
    } else {
        &BLOCKS
    }
}

/// Split a fill fraction into (whole cells, remainder eighths).
fn cell_breakdown(width: u16, value: f32) -> (u16, usize) {
    let value = value.clamp(0.0, 1.0);
    let total_eighths = (value * width as f32 * 8.0).round() as u16;
    let full = (total_eighths / 8).min(width);
    let remainder = (total_eighths % 8) as usize;
    (full, remainder)
}

/// Per-cell `(symbol, is_filled)` for a `width`-cell bar at `value` fill.
/// Single source of truth for both renderers below.
fn bar_cells(width: u16, value: f32) -> impl Iterator<Item = (&'static str, /* filled */ bool)> {
    let (full, remainder) = cell_breakdown(width, value);
    let glyphs = partial_blocks();
    (0..width).map(move |i| {
        if i < full {
            (glyphs.last().copied().unwrap_or("█"), true)
        } else if i == full && remainder > 0 {
            (glyphs.get(remainder).copied().unwrap_or(" "), true)
        } else {
            (" ", false)
        }
    })
}

/// Build a progress bar as styled spans (one per cell).
///
/// Each span has `fg` on `bg`, suitable for composing into a `Line`.
pub fn progress_bar_spans(width: u16, value: f32, fg: Color, bg: Color) -> Vec<Span<'static>> {
    let fg_style = Style::default().fg(fg).bg(bg);
    let bg_style = Style::default().bg(bg);
    bar_cells(width, value)
        .map(|(symbol, filled)| {
            let style = if filled { fg_style } else { bg_style };
            Span::styled(symbol.to_string(), style)
        })
        .collect()
}

/// Render a progress bar into the buffer at the given position. `width`: number of character cells
/// for the bar. `value`: fill fraction in `0.0..=1.0` (clamped). `fg`: color for the filled
/// portion. `bg`: background color for the track (filled + empty cells).
pub fn render_progress_bar(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    value: f32,
    fg: Color,
    bg: Color,
) {
    let fg_style = Style::default().fg(fg).bg(bg);
    let bg_style = Style::default().bg(bg);
    for (i, (symbol, filled)) in bar_cells(width, value).enumerate() {
        let Some(cell) = buf.cell_mut((x + i as u16, y)) else {
            continue;
        };
        cell.set_symbol(symbol);
        cell.set_style(if filled { fg_style } else { bg_style });
    }
}
