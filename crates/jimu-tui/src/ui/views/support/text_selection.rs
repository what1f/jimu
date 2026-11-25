// Adapted from the Grok Build pager (Apache-2.0). Copyright 2023-2026 SpaceXAI.

use crate::engine::theme::Theme;
use ratatui::{
    style::{Color, Modifier},
    text::Line,
};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Slice `text` to the graphemes overlapping the display-column range `[start, end)`.
/// A wide grapheme is kept whole when the range covers any of its cells.
/// Graphemes that only touch a boundary (start at `end` or end at `start`) stay excluded.
pub fn slice_display_cols(text: &str, start: u16, end: u16) -> String {
    if start >= end || text.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    let mut col = 0u16;

    for grapheme in text.graphemes(true) {
        let width = grapheme_width(grapheme) as u16;
        let next_col = col.saturating_add(width);

        if width == 0 {
            if col >= start && col < end {
                out.push_str(grapheme);
            }
            continue;
        }
        if next_col <= start {
            col = next_col;
            continue;
        }
        if col >= end {
            break;
        }
        out.push_str(grapheme);
        col = next_col;
    }

    out
}

/// Display-cell range of the grapheme covering `col`, or `None` when `col` is past the last grapheme.
/// Zero-width graphemes occupy no cell and never advance the column, matching `slice_display_cols` and the paint path.
pub fn grapheme_cells_at(text: &str, col: u16) -> Option<std::ops::Range<u16>> {
    let mut c = 0u16;

    for grapheme in text.graphemes(true) {
        let width = grapheme_width(grapheme) as u16;
        if width == 0 {
            continue;
        }

        let next = c.saturating_add(width);
        if next > col {
            return Some(c..next);
        }
        c = next;
    }

    None
}

/// Exclusive display column just past the grapheme occupying `col`, or the text's total painted width when `col` is past the last one.
pub fn col_past_grapheme(text: &str, col: u16) -> u16 {
    match grapheme_cells_at(text, col) {
        Some(cells) => cells.end,
        // Per-grapheme total (matches `grapheme_cells_at`), so a ligature row's last cell isn't dropped when `col` snaps to the row end
        None => u16::try_from(str_display_cells(text)).unwrap_or(u16::MAX),
    }
}

pub fn line_plain_text(line: &Line) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}
fn grapheme_width(grapheme: &str) -> usize {
    UnicodeWidthStr::width(grapheme)
}
pub(crate) fn str_display_cells(s: &str) -> usize {
    s.graphemes(true).map(grapheme_width).sum()
}
pub(crate) fn apply_selection_highlight(theme: &Theme, cell: &mut ratatui::buffer::Cell) {
    let band = theme.text_primary;
    if band == Color::Reset || theme.bg_base == Color::Reset {
        cell.modifier.insert(Modifier::REVERSED);
        return;
    }
    // A search-match highlight painted earlier in the frame sets REVERSED; left in place it would swap the band right back out
    cell.modifier.remove(Modifier::REVERSED);
    cell.set_fg(theme.bg_base);
    cell.set_bg(band);
}
/// All printable ASCII punctuation except underscore, matching tmux's `word-separators` default from `options-table.c`.
pub const DEFAULT_WORD_SEPARATORS: &str = "!\"#$%&'()*+,-./:;<=>?@[\\]^`{|}~";

/// Three-class partition for word boundary detection (whitespace / separator / word).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Word,
    Whitespace,
    Separator,
}

fn classify_grapheme(grapheme: &str, separators: &str) -> CharClass {
    match grapheme.chars().next() {
        Some(c) if c.is_whitespace() => CharClass::Whitespace,
        Some(c) if separators.contains(c) => CharClass::Separator,
        _ => CharClass::Word,
    }
}

/// Find word boundaries around a display column using tmux-style three-class grouping.
/// Returns the range in display columns.
pub fn word_boundaries_at_col(text: &str, col: u16, separators: &str) -> Range<u16> {
    let mut segments: Vec<(u16, u16, CharClass)> = Vec::new();
    let mut current_col: u16 = 0;

    for grapheme in text.graphemes(true) {
        let width = UnicodeWidthStr::width(grapheme) as u16;
        if width == 0 {
            continue;
        }
        let next_col = current_col.saturating_add(width);
        segments.push((
            current_col,
            next_col,
            classify_grapheme(grapheme, separators),
        ));
        current_col = next_col;
    }

    if segments.is_empty() {
        return 0..0;
    }

    // If col is past the end, clamp to the last segment.
    let target_idx = segments
        .iter()
        .position(|(start, end, _)| col >= *start && col < *end)
        .unwrap_or(segments.len() - 1);

    let Some(&(_, _, target_class)) = segments.get(target_idx) else {
        return 0..0;
    };

    let mut left = target_idx;
    while left > 0
        && left
            .checked_sub(1)
            .and_then(|j| segments.get(j))
            .is_some_and(|seg| seg.2 == target_class)
    {
        left -= 1;
    }

    let mut right = target_idx;
    while right + 1 < segments.len()
        && segments
            .get(right + 1)
            .is_some_and(|seg| seg.2 == target_class)
    {
        right += 1;
    }

    match (segments.get(left), segments.get(right)) {
        (Some(&(start, _, _)), Some(&(_, end, _))) => start..end,
        _ => 0..0,
    }
}
