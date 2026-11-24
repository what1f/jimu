//! The markdown renderer rewrites each `>` quote marker to a `│` bar styled `blockquote_outer`.
//! The bar is then ordinary span content and would leak into drag-select copies.
//! The helpers here detect that prefix on a rendered row and exclude it from selection via [`Selectable::Spans`].
//! Tool headers and diff gutters exclude their decorations the same way.
//! [`MarkdownContent::output`](super::markdown_content::MarkdownContent::output) renders through these helpers.
//! So does [`ThinkingBlock`](super::ThinkingBlock)'s render path.

use std::borrow::Cow;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::engine::theme::Theme;
use crate::ui::scrollback::types::Selectable;

/// Built once per output pass: whether to strip quote bars, and the bar style the theme paints them with.
/// Raw mode skips the `Theme::current()` lookup entirely.
#[derive(Clone, Copy)]
pub(crate) struct QuoteBarStrip {
    /// `None` means stripping is disabled (raw mode shows source `>` markers).
    bar_style: Option<Style>,
}

impl QuoteBarStrip {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            bar_style: enabled.then(quote_bar_style),
        }
    }

    /// Selection metadata for one rendered row; see [`quote_prefix_selectable`].
    pub(crate) fn selectable(&self, line: &mut Line<'static>) -> Selectable {
        match self.bar_style {
            Some(bar_style) => quote_prefix_selectable(line, bar_style),
            None => Selectable::All,
        }
    }
}

/// The exact ratatui style the renderer paints parser-generated blockquote bars with. This mirrors
/// `blockquote_outer` in pager-render's theme/md_style.rs, and a comment there points back here. The end-to-end
/// tests below trip if either side drifts.
fn quote_bar_style() -> Style {
    let muted = Theme::current().md_muted;
    let style = Style::default().add_modifier(Modifier::DIM);
    if muted == Color::Reset {
        style
    } else {
        style.fg(muted)
    }
}

/// Byte length of the rendered blockquote prefix at the start of `line`. A bar-only row (blank line inside a quote:
/// `│`, `│ │`, optional trailing space) counts as all prefix. Every prefix bar must carry `bar_style`. The shape
/// must agree with the looser `blockquote_prefix_len` in pager-render's wrapping.rs.
fn rendered_quote_prefix_len(line: &Line<'_>, bar_style: Style) -> Option<usize> {
    const BAR: char = '\u{2502}';
    const BAR_LEN: usize = '\u{2502}'.len_utf8();
    let mut chars = line
        .spans
        .iter()
        .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
        .peekable();
    let mut len = 0usize;
    loop {
        match chars.next() {
            Some((BAR, style)) if style == bar_style => len += BAR_LEN,
            _ => return None,
        }
        match chars.next() {
            None => return Some(len),
            Some((' ', _)) => {
                len += 1;
                match chars.peek() {
                    None => return Some(len),
                    Some((BAR, style)) if *style == bar_style => continue,
                    Some(_) => break,
                }
            }
            Some(_) => return None,
        }
    }
    if chars.any(|(c, _)| c == BAR) {
        return None;
    }
    Some(len)
}

/// Split `line`'s spans at `byte_offset` (splitting a straddling span in two) and return the number of spans covering `0..byte_offset`.
fn split_spans_at(line: &mut Line<'static>, byte_offset: usize) -> usize {
    let mut acc = 0usize;
    for i in 0..line.spans.len() {
        let Some(span) = line.spans.get(i) else { break };
        let end = acc + span.content.len();
        if end == byte_offset {
            return i + 1;
        }
        if end > byte_offset {
            let Some(local) = byte_offset.checked_sub(acc) else {
                return line.spans.len();
            };
            let Some(span) = line.spans.get_mut(i) else {
                break;
            };
            let tail: Cow<'static, str> = match &mut span.content {
                Cow::Borrowed(s) => {
                    let (head, tail) = s.split_at(local);
                    span.content = Cow::Borrowed(head);
                    Cow::Borrowed(tail)
                }
                Cow::Owned(s) => Cow::Owned(s.split_off(local)),
            };
            let style = span.style;
            line.spans.insert(
                i + 1,
                Span {
                    content: tail,
                    style,
                },
            );
            return i + 1;
        }
        acc = end;
    }
    line.spans.len()
}

/// Selection metadata for a pretty-mode markdown row. Bar-only rows (blank quote lines) return an empty end range
/// so multi-line copies keep the blank line. The bar must be the FIRST span: a quote indented under a list item (`-
/// > quoted`, bullet span first) keeps its prefix in copies.
fn quote_prefix_selectable(line: &mut Line<'static>, bar_style: Style) -> Selectable {
    // Only parser-generated bars are a lone 1-char span carrying the blockquote_outer style
    // A literal "│ " in prose or code stays glued to its content span or carries a different style, so it is left intact
    let genuine = line
        .spans
        .first()
        .is_some_and(|s| s.content.as_ref() == "\u{2502}" && s.style == bar_style);
    if !genuine {
        return Selectable::All;
    }
    let Some(prefix_len) = rendered_quote_prefix_len(line, bar_style) else {
        return Selectable::All;
    };
    let prefix_spans = split_spans_at(line, prefix_len);
    Selectable::Spans(prefix_spans..line.spans.len())
}
