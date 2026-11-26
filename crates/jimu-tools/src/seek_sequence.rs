// Matching order follows OpenAI Codex's Apache-2.0 apply-patch/seek_sequence.rs.

pub(crate) enum Search {
    First,
    Unique,
    EndOfFile,
}

pub(crate) fn split_bom(text: &str) -> (&str, &str) {
    match text.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest),
        None => ("", text),
    }
}

/// Match at the strongest applicable level. For unique searches, report a tie at that level.
pub(crate) fn seek_sequence(
    lines: &[&str],
    pattern: &[&str],
    search: Search,
) -> Result<Option<usize>, ()> {
    if pattern.is_empty() {
        return Ok(Some(0));
    }
    let start = match search {
        Search::EndOfFile => lines.len().saturating_sub(pattern.len()),
        _ => 0,
    };
    let comparators: [fn(&str, &str) -> bool; 4] = [
        |a, b| a == b,
        |a, b| a.trim_end() == b.trim_end(),
        |a, b| a.trim() == b.trim(),
        |a, b| {
            a.trim()
                .chars()
                .map(normalize_char)
                .eq(b.trim().chars().map(normalize_char))
        },
    ];
    for equal in comparators {
        let mut matches = lines[start..]
            .windows(pattern.len())
            .enumerate()
            .filter_map(|(i, window)| {
                window
                    .iter()
                    .zip(pattern)
                    .all(|(a, b)| equal(a, b))
                    .then_some(start + i)
            });
        if let Some(first) = matches.next() {
            return if matches!(search, Search::Unique) && matches.next().is_some() {
                Err(())
            } else {
                Ok(Some(first))
            };
        }
    }
    Ok(None)
}

fn normalize_char(ch: char) -> char {
    match ch {
        '‐'..='―' | '−' => '-',
        '‘'..='‛' => '\'',
        '“'..='‟' => '"',
        '\u{00A0}'
        | '\u{1680}'
        | '\u{2000}'..='\u{200A}'
        | '\u{202F}'
        | '\u{205F}'
        | '\u{3000}' => ' ',
        _ => ch,
    }
}
