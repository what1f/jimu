use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub use super::tool_paths::{path_basename, path_for_tool_header, shorten_path};
pub use crate::engine::tty::is_unsafe_display_char;
use std::borrow::Cow;
use unicode_width::UnicodeWidthChar;

/// Clone a borrowed ratatui `Line` into an owned `'static` line.
pub fn line_to_static(line: &Line<'_>) -> Line<'static> {
    Line {
        style: line.style,
        alignment: line.alignment,
        spans: line
            .spans
            .iter()
            .map(|s| Span {
                style: s.style,
                content: std::borrow::Cow::Owned(s.content.to_string()),
            })
            .collect(),
    }
}

/// Append owned copies of borrowed lines to `out`.
pub fn push_owned_lines(src: &[Line<'_>], out: &mut Vec<Line<'static>>) {
    for l in src {
        out.push(line_to_static(l));
    }
}

/// Polyfill for nightly-only [`str::floor_char_boundary`].
/// Snaps a byte index down to the nearest char boundary.
pub fn floor_char_boundary(s: &str, index: usize) -> usize {
    let index = index.min(s.len());
    let mut i = index;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Like [`truncate_to_width`] but returns an owned `String`.
pub fn truncate_str(s: &str, max_width: usize) -> String {
    truncate_to_width(s, max_width).into_owned()
}

/// Truncate on the span where the budget runs out and append `…` in that span's style. Unchanged if it already fits.
pub fn truncate_line(line: Line<'static>, max_width: usize) -> Line<'static> {
    if max_width == 0 {
        return Line::from(vec![]);
    }

    let total: usize = line.spans.iter().map(|s| s.content.width()).sum();
    if total <= max_width {
        return line;
    }

    // Need room for the ellipsis (1 column).
    let budget = max_width.saturating_sub(1);
    let mut used = 0usize;
    let mut out: Vec<Span<'static>> = Vec::new();

    for span in line.spans {
        let sw = span.content.width();
        if used + sw <= budget {
            // Entire span fits.
            used += sw;
            out.push(span);
        } else {
            // Partial fit: truncate this span
            let remaining = budget - used;
            if remaining > 0 {
                let truncated = take_width(&span.content, remaining);
                out.push(Span::styled(truncated, span.style));
            }
            // Append ellipsis with the same style as the last span.
            let ellipsis_style = out.last().map(|s| s.style).unwrap_or_default();
            out.push(Span::styled("\u{2026}", ellipsis_style));
            return Line::from(out);
        }
    }

    // Shouldn't reach here (total > max_width checked above), but be safe.
    Line::from(out)
}

/// Clip on grapheme boundaries (no ellipsis) or pad with spaces so every column is a real cell.
/// Otherwise a wider glyph strands a stale cell (ghost glyphs beside tables). `width` must be bounded: the pad allocates `width - total` spaces.
pub fn fit_line_to_width<'a>(line: Line<'a>, width: usize) -> Line<'a> {
    let total: usize = line.spans.iter().map(|s| s.content.width()).sum();
    if total == width {
        return line;
    }

    let Line {
        style,
        alignment,
        mut spans,
    } = line;

    if total < width {
        spans.push(Span::raw(" ".repeat(width - total)));
        return Line {
            style,
            alignment,
            spans,
        };
    }

    // Wider than width: clip on grapheme boundaries, no ellipsis.
    let mut out: Vec<Span<'a>> = Vec::new();
    let mut used = 0usize;
    for span in spans {
        let sw = span.content.width();
        if used + sw <= width {
            used += sw;
            out.push(span);
            if used == width {
                break;
            }
            continue;
        }
        // This span straddles the boundary: take whole graphemes that fit
        let remaining = width - used;
        let mut taken = String::new();
        let mut taken_width = 0usize;
        for g in span.content.graphemes(true) {
            let gw = g.width();
            if taken_width + gw > remaining {
                break;
            }
            taken_width += gw;
            taken.push_str(g);
        }
        if !taken.is_empty() {
            out.push(Span::styled(taken, span.style));
            used += taken_width;
        }
        // A straddling wide grapheme leaves a 1-column gap; pad it.
        if used < width {
            out.push(Span::raw(" ".repeat(width - used)));
        }
        break;
    }

    Line {
        style,
        alignment,
        spans: out,
    }
}

/// Take the first `n` display columns from a string.
fn take_width(s: &str, n: usize) -> String {
    let Some(prefix) = s.get(..byte_offset_at_width(s, n)) else {
        return String::new();
    };
    prefix.to_owned()
}

/// Fit `(type, description, activity, meta)` in `avail`. Drop description first, then meta, then truncate activity, then type.
pub fn cascade_truncate(
    avail: usize,
    type_text: &str,
    description: &str,
    activity_text: &str,
    meta_text: &str,
) -> (String, String, String, String) {
    let overhead = type_text.width() + activity_text.width() + meta_text.width();
    if overhead <= avail {
        let desc_max = avail - overhead;
        (
            type_text.to_string(),
            truncate_str(description, desc_max),
            activity_text.to_string(),
            meta_text.to_string(),
        )
    } else {
        let mut budget = avail;
        let td = if type_text.width() <= budget {
            budget -= type_text.width();
            type_text.to_string()
        } else {
            let s = truncate_str(type_text, budget);
            budget = 0;
            s
        };
        let ad = if budget == 0 {
            String::new()
        } else if activity_text.width() <= budget {
            budget -= activity_text.width();
            activity_text.to_string()
        } else {
            let s = truncate_str(activity_text, budget);
            budget = 0;
            s
        };
        let md = if budget == 0 {
            String::new()
        } else if meta_text.width() <= budget {
            meta_text.to_string()
        } else {
            truncate_str(meta_text, budget)
        };
        (td, String::new(), ad, md)
    }
}

/// Truncate to at most `max_width` display columns (CJK counts 2), ending with `…` when cut; a zero budget yields an empty string.
pub fn truncate_to_width(s: &str, max_width: usize) -> Cow<'_, str> {
    if byte_offset_at_width(s, max_width) == s.len() {
        return Cow::Borrowed(s);
    }
    if max_width == 0 {
        return Cow::Borrowed("");
    }
    let end = byte_offset_at_width(s, max_width - 1);
    let Some(prefix) = s.get(..end) else {
        return Cow::Borrowed(s);
    };
    Cow::Owned(format!("{prefix}…"))
}

/// Byte offset at which display width would exceed `max_width`, or `s.len()`.
pub fn byte_offset_at_width(s: &str, max_width: usize) -> usize {
    let mut width = 0;
    for (i, ch) in s.char_indices() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + cw > max_width {
            return i;
        }
        width += cw;
    }
    s.len()
}
