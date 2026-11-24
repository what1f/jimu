//! Formats shell commands for tool and background-task blocks.

use std::ops::Range;

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::engine::theme::Theme;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Quote {
    Single,
    Double,
    Backtick,
    Ansi,
}

impl Quote {
    fn opening(bytes: &[u8]) -> Option<(Self, usize)> {
        match bytes {
            [b'\'', ..] => Some((Self::Single, 1)),
            [b'"', ..] => Some((Self::Double, 1)),
            [b'`', ..] => Some((Self::Backtick, 1)),
            [b'$', b'\'', ..] => Some((Self::Ansi, 2)),
            [b'$', b'"', ..] => Some((Self::Double, 2)),
            _ => None,
        }
    }

    fn closes(self, byte: u8) -> bool {
        byte == match self {
            Self::Single | Self::Ansi => b'\'',
            Self::Double => b'"',
            Self::Backtick => b'`',
        }
    }

    fn escapes(self, next: u8) -> bool {
        match self {
            Self::Single => false,
            Self::Double => matches!(next, b'$' | b'`' | b'"' | b'\\' | b'\n'),
            Self::Backtick => matches!(next, b'$' | b'`' | b'\\' | b'\n'),
            Self::Ansi => true,
        }
    }
}

struct HereDocument {
    delimiter: Vec<u8>,
    strip_tabs: bool,
    quoted: bool,
}

impl HereDocument {
    fn read(mut bytes: &[u8], strip_tabs: bool) -> Option<(Self, usize)> {
        let len = bytes.len();
        while let [b' ' | b'\t', rest @ ..] | [b'\\', b'\n', rest @ ..] = bytes {
            bytes = rest;
        }
        let start = bytes.len();
        let mut quote = None::<Quote>;
        let mut quoted = false;
        let mut delimiter = Vec::new();
        while let [byte, rest @ ..] = bytes {
            if let Some(open) = quote {
                if open.closes(*byte) {
                    quote = None;
                    bytes = rest;
                    continue;
                }
            } else if let Some((open, n)) = Quote::opening(bytes) {
                quote = Some(open);
                quoted = true;
                bytes = &bytes[n..];
                continue;
            } else if matches!(
                byte,
                b' ' | b'\t' | b'\n' | b'|' | b'&' | b';' | b'(' | b')' | b'<' | b'>'
            ) || (*byte == b'#' && bytes.len() == start)
            {
                break;
            }

            if *byte == b'\\'
                && let [next, tail @ ..] = rest
                && quote.is_none_or(|open| open.escapes(*next))
            {
                if *next != b'\n' {
                    quoted = true;
                    delimiter.push(*next);
                }
                bytes = tail;
            } else {
                delimiter.push(*byte);
                bytes = rest;
            }
        }
        (bytes.len() < start && quote.is_none()).then_some((
            Self {
                delimiter,
                strip_tabs,
                quoted,
            },
            len - bytes.len(),
        ))
    }

    /// Consume body and terminator together so neither influences command wrapping.
    fn consumed_bytes(&self, body: &str) -> usize {
        let mut consumed = 0;
        let mut continued = Vec::new();
        for physical in body.split_inclusive('\n') {
            consumed += physical.len();
            let line = physical.strip_suffix('\n').unwrap_or(physical);
            let line = if self.strip_tabs {
                line.trim_start_matches('\t')
            } else {
                line
            };
            let bytes = line.as_bytes();
            if !self.quoted
                && physical.ends_with('\n')
                && bytes.iter().rev().take_while(|&&b| b == b'\\').count() % 2 == 1
            {
                continued.extend_from_slice(&bytes[..bytes.len() - 1]);
                continue;
            }
            if continued.is_empty() {
                if bytes == self.delimiter {
                    break;
                }
            } else {
                continued.extend_from_slice(bytes);
                if continued == self.delimiter {
                    break;
                }
                continued.clear();
            }
        }
        consumed
    }
}

#[derive(Default)]
struct CommandBreaks {
    operators: Vec<usize>,
    words: Vec<usize>,
    heredocs: Vec<Range<usize>>,
}

impl CommandBreaks {
    fn scan(script: &str) -> Self {
        let mut breaks = Self::default();
        let bytes = script.as_bytes();
        let mut quote = None::<Quote>;
        // Treat expansions as part of a word, including when they contain nested quotes.
        let mut scopes = Vec::new();
        let mut heredocs: Vec<HereDocument> = Vec::new();
        let mut word_start = true;
        let mut i = 0;
        while let Some(&byte) = bytes.get(i) {
            let rest = &bytes[i..];
            if (quote.is_none() || quote == Some(Quote::Double))
                && let Some((end, n)) = match rest {
                    [b'$', b'(', ..] => Some((b')', 2)),
                    [b'$', b'{', ..] => Some((b'}', 2)),
                    [b'(', b'(', ..] if quote.is_none() => Some((b')', 1)),
                    [b'(', ..] if quote.is_none() && !scopes.is_empty() => Some((b')', 1)),
                    _ => None,
                }
            {
                scopes.push((end, quote.take()));
                i += n;
                word_start = false;
                continue;
            }
            if byte == b'\\'
                && let Some(&next) = rest.get(1)
                && quote.is_none_or(|open| open.escapes(next))
            {
                i += 2;
                if next != b'\n' {
                    word_start = false;
                }
                continue;
            }
            if let Some(open) = quote {
                if open.closes(byte) {
                    quote = None;
                }
                i += 1;
                continue;
            }
            if let Some((open, n)) = Quote::opening(rest) {
                quote = Some(open);
                i += n;
                word_start = false;
                continue;
            }
            if let Some(&(end, outer_quote)) = scopes.last() {
                if byte == end {
                    scopes.pop();
                    quote = outer_quote;
                }
                i += 1;
                continue;
            }

            match rest {
                [b' ' | b'\t', ..] => {
                    breaks.words.push(i);
                    i += rest
                        .iter()
                        .take_while(|&&b| matches!(b, b' ' | b'\t'))
                        .count();
                    word_start = true;
                }
                [b'#', ..] if word_start => {
                    i += rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
                }
                [b'\n', ..] => {
                    i += 1;
                    for document in heredocs.drain(..) {
                        let end = i + document.consumed_bytes(&script[i..]);
                        breaks.heredocs.push(i..end);
                        i = end;
                    }
                    word_start = true;
                }
                [b'<', b'<', b'<', ..] => {
                    i += 3;
                    word_start = true;
                }
                [b'<', b'<', after @ ..] => {
                    let strip_tabs = after.first() == Some(&b'-');
                    i += 2 + usize::from(strip_tabs);
                    if let Some((document, n)) = HereDocument::read(&bytes[i..], strip_tabs) {
                        heredocs.push(document);
                        i += n;
                    }
                    word_start = true;
                }
                // These operators contain shell metacharacters but do not separate commands.
                [b'>', b'|' | b'&', ..] | [b'<', b'&', ..] | [b'&', b'>', ..] => {
                    i += 2;
                    word_start = true;
                }
                [b';', b';', b'&', ..] => {
                    i += 3;
                    word_start = true;
                }
                [b';', b';' | b'&', ..] => {
                    i += 2;
                    word_start = true;
                }
                [b'&', b'&', ..] | [b'|', b'|' | b'&', ..] => {
                    i += 2;
                    breaks.operators.push(i);
                    word_start = true;
                }
                [b'|' | b';', ..] => {
                    i += 1;
                    breaks.operators.push(i);
                    word_start = true;
                }
                _ => {
                    i += 1;
                    word_start = matches!(byte, b'(' | b')' | b'<' | b'>' | b'&');
                }
            }
        }
        breaks
    }

    fn in_heredoc(&self, line_start: usize) -> bool {
        let index = self
            .heredocs
            .partition_point(|range| range.end <= line_start);
        self.heredocs
            .get(index)
            .is_some_and(|range| range.contains(&line_start))
    }
}

#[cfg(feature = "syntax")]
pub(crate) fn highlight_bash_command(command: &str) -> Vec<Span<'static>> {
    let syntect = crate::engine::render::syntax::get_syntect();
    let grammar = if cfg!(windows) { "powershell" } else { "bash" };
    let Some(mut hl) = syntect
        .highlight_lines_for_token(grammar)
        .or_else(|| syntect.highlight_lines_for_token("bash"))
    else {
        let theme = Theme::current();
        return vec![Span::styled(
            command.to_string(),
            Style::default().fg(theme.command),
        )];
    };
    let line = format!("{command}\n");
    match hl.highlight_line(&line, &syntect.syntax_set) {
        Ok(ranges) => {
            let mut spans = Vec::new();
            for (style, segment) in ranges {
                let mut text = segment.to_owned();
                while text.ends_with('\n') || text.ends_with('\r') {
                    text.pop();
                }
                if text.is_empty() {
                    continue;
                }
                spans.push(Span::styled(
                    text,
                    crate::engine::render::syntax::syntect_to_ratatui_fg(style),
                ));
            }
            if spans.is_empty() {
                let theme = Theme::current();
                vec![Span::styled(
                    command.to_string(),
                    Style::default().fg(theme.command),
                )]
            } else {
                spans
            }
        }
        Err(_) => {
            let theme = Theme::current();
            vec![Span::styled(
                command.to_string(),
                Style::default().fg(theme.command),
            )]
        }
    }
}

#[cfg(not(feature = "syntax"))]
pub(crate) fn highlight_bash_command(command: &str) -> Vec<Span<'static>> {
    vec![Span::styled(
        command.to_owned(),
        Style::default().fg(Theme::current().command),
    )]
}

pub(crate) fn render_bash_command_display_lines(
    command: &str,
    content_width: usize,
) -> Vec<Line<'static>> {
    build_raw_bash_lines(command, content_width, usize::MAX)
}

fn soft_wrap_row_texts<'a>(
    line: &'a str,
    line_start: usize,
    breaks: &CommandBreaks,
    content_width: usize,
    max_rows: usize,
) -> Vec<&'a str> {
    if max_rows == 0 {
        return Vec::new();
    }
    if content_width == 0 {
        return vec![line];
    }

    if line.len() <= content_width || breaks.in_heredoc(line_start) {
        return vec![line];
    }

    let line_end = line_start + line.len();
    let first_inside = breaks.operators.partition_point(|&b| b <= line_start);
    let mut bounds = breaks
        .operators
        .get(first_inside..)
        .unwrap_or(&[])
        .iter()
        .copied()
        .take_while(|&b| b < line_end)
        .map(|b| b - line_start)
        .filter(|&b| line.is_char_boundary(b))
        .chain(std::iter::once(line.len()))
        .peekable();

    if bounds.peek().copied() == Some(line.len()) {
        return bash_quote_aware_wrap(line, line_start, &breaks.words, content_width, max_rows);
    }

    let mut out: Vec<&'a str> = Vec::new();
    let mut pos = 0usize;
    let mut first_row = true;
    while pos < line.len() && out.len() < max_rows {
        let mut start = pos;
        if !first_row {
            while start < line.len()
                && line
                    .as_bytes()
                    .get(start)
                    .is_some_and(|b| b.is_ascii_whitespace())
            {
                start += 1;
            }
            while bounds.peek().is_some_and(|&b| b <= start) {
                bounds.next();
            }
            if start >= line.len() {
                break;
            }
        }
        first_row = false;

        let Some(mut end) = bounds.next() else {
            break;
        };
        if UnicodeWidthStr::width(line.get(start..end).unwrap_or("")) <= content_width {
            while let Some(&next_end) = bounds.peek() {
                if UnicodeWidthStr::width(line.get(start..next_end).unwrap_or("")) <= content_width
                {
                    end = next_end;
                    bounds.next();
                } else {
                    break;
                }
            }
            out.push(line.get(start..end).unwrap_or("").trim_end());
        } else {
            let row = line.get(start..end).unwrap_or("").trim_end();
            if UnicodeWidthStr::width(row) <= content_width {
                out.push(row);
            } else {
                out.extend(bash_quote_aware_wrap(
                    row,
                    line_start + start,
                    &breaks.words,
                    content_width,
                    max_rows - out.len(),
                ));
            }
        }
        pos = end;
    }
    out
}

fn display_width_end(s: &str, start: usize, limit: usize, width: usize) -> usize {
    let Some(rest) = s.get(start..limit.min(s.len())) else {
        return start;
    };
    let mut used = 0usize;
    let mut end = start;
    for ch in rest.chars() {
        let ch_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_w > width && end > start {
            break;
        }
        used += ch_w;
        end += ch.len_utf8();
        if used >= width {
            break;
        }
    }
    end
}

fn extend_display_width_rows<'a>(
    rows: &mut Vec<&'a str>,
    line: &'a str,
    start: usize,
    end: usize,
    width: usize,
    max_rows: usize,
) {
    let end = end.min(line.len());
    let mut pos = start.min(end);
    while pos < end && rows.len() < max_rows {
        let chunk_end = display_width_end(line, pos, end, width);
        if chunk_end <= pos {
            break;
        }
        match line.get(pos..chunk_end) {
            Some(row) => rows.push(row),
            None => break,
        }
        pos = chunk_end;
    }
}

fn bash_quote_aware_wrap<'a>(
    line: &'a str,
    line_start: usize,
    word_breaks: &[usize],
    width: usize,
    max_rows: usize,
) -> Vec<&'a str> {
    if max_rows == 0 {
        return Vec::new();
    }
    if width == 0 || line.len() <= width {
        return vec![line];
    }

    let mut rows: Vec<&str> = Vec::new();
    let mut row_start = 0usize;
    let mut last_break = 0usize;

    let first = word_breaks.partition_point(|&b| b <= line_start);
    let candidates = word_breaks[first..]
        .iter()
        .copied()
        .take_while(|&b| b < line_start + line.len())
        .map(|b| b - line_start)
        .chain(std::iter::once(line.len()));

    for b in candidates {
        if b <= row_start {
            continue;
        }
        let candidate = line.get(row_start..b).unwrap_or("").trim_end();
        if UnicodeWidthStr::width(candidate) <= width {
            last_break = b;
            continue;
        }
        if last_break > row_start {
            let row = line.get(row_start..last_break).unwrap_or("").trim_end();
            if !row.is_empty() {
                rows.push(row);
                if rows.len() >= max_rows {
                    return rows;
                }
            }
            row_start = last_break;
            while row_start < line.len()
                && line
                    .as_bytes()
                    .get(row_start)
                    .is_some_and(|b| b.is_ascii_whitespace())
            {
                row_start += 1;
            }
            last_break = row_start;
            if b > row_start {
                let candidate = line.get(row_start..b).unwrap_or("").trim_end();
                if UnicodeWidthStr::width(candidate) <= width {
                    last_break = b;
                } else {
                    extend_display_width_rows(&mut rows, line, row_start, b, width, max_rows);
                    if rows.len() >= max_rows {
                        return rows;
                    }
                    row_start = b;
                    while row_start < line.len()
                        && line
                            .as_bytes()
                            .get(row_start)
                            .is_some_and(|b| b.is_ascii_whitespace())
                    {
                        row_start += 1;
                    }
                    last_break = row_start;
                }
            }
        } else {
            extend_display_width_rows(&mut rows, line, row_start, b, width, max_rows);
            if rows.len() >= max_rows {
                return rows;
            }
            row_start = b;
            while row_start < line.len()
                && line
                    .as_bytes()
                    .get(row_start)
                    .is_some_and(|b| b.is_ascii_whitespace())
            {
                row_start += 1;
            }
            last_break = row_start;
        }
    }
    if row_start < line.len() && rows.len() < max_rows {
        let row = line.get(row_start..).unwrap_or("").trim_end();
        if !row.is_empty() {
            rows.push(row);
        }
    }
    if rows.is_empty() { vec![line] } else { rows }
}

fn build_raw_bash_lines(
    command: &str,
    content_width: usize,
    max_rows: usize,
) -> Vec<Line<'static>> {
    let text = command.replace("\r\n", "\n").replace('\r', "\n");
    if max_rows == 0 {
        return Vec::new();
    }
    let breaks = CommandBreaks::scan(&text);
    let end = breaks
        .heredocs
        .last()
        .map_or(0, |range| range.end)
        .max(text.trim_end().len());
    let text = &text[..end];

    #[cfg(feature = "syntax")]
    let syntect = crate::engine::render::syntax::get_syntect();
    let fallback = Style::default().fg(Theme::current().command);
    #[cfg(feature = "syntax")]
    let grammar = if cfg!(windows) { "powershell" } else { "bash" };
    #[cfg(feature = "syntax")]
    let mut hl = syntect
        .highlight_lines_for_token(grammar)
        .or_else(|| syntect.highlight_lines_for_token("bash"));

    let mut out = Vec::new();
    let mut offset = 0usize;
    for raw_line in text.split_terminator('\n') {
        if out.len() >= max_rows {
            break;
        }
        let line_start = offset;
        offset += raw_line.len() + 1;
        let physical = if breaks.in_heredoc(line_start) {
            raw_line
        } else {
            raw_line.trim_end()
        };
        #[cfg(feature = "syntax")]
        let spans =
            crate::engine::render::syntax::highlight_line(physical, &mut hl, syntect, fallback);
        #[cfg(not(feature = "syntax"))]
        let spans = [Span::styled(physical.to_owned(), fallback)];
        if physical.is_empty() {
            out.push(Line::default());
            continue;
        }
        debug_assert_eq!(
            spans.iter().map(|s| s.content.as_ref()).collect::<String>(),
            physical,
            "highlight spans must flatten back to the physical line"
        );
        for row in soft_wrap_row_texts(
            physical,
            line_start,
            &breaks,
            content_width,
            max_rows - out.len(),
        ) {
            let start = (row.as_ptr() as usize) - (physical.as_ptr() as usize);
            out.push(Line::from(slice_highlighted_spans(
                &spans,
                start,
                start + row.len(),
            )));
        }
    }
    out
}
fn slice_highlighted_spans(
    spans: &[Span<'static>],
    start: usize,
    end: usize,
) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    for span in spans {
        let span_start = pos;
        let span_end = pos + span.content.len();
        pos = span_end;
        if span_end <= start {
            continue;
        }
        if span_start >= end {
            break;
        }
        let lo = start.max(span_start) - span_start;
        let hi = end.min(span_end) - span_start;
        if lo >= hi {
            continue;
        }
        let Some(slice) = span.content.get(lo..hi) else {
            debug_assert!(false, "row boundary off a char boundary");
            continue;
        };
        out.push(Span::styled(slice.to_owned(), span.style));
    }
    out
}
