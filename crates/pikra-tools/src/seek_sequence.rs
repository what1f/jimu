// Matching order follows OpenAI Codex's Apache-2.0 apply-patch/seek_sequence.rs.

/// Match at the strongest applicable level. For unique searches, report a tie at that level.
pub(crate) fn seek_sequence(
    lines: &[&str],
    pattern: &[&str],
    unique: bool,
) -> Result<Option<usize>, ()> {
    if pattern.is_empty() {
        return Ok(Some(0));
    }
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
        let mut matches = lines
            .windows(pattern.len())
            .enumerate()
            .filter_map(|(i, window)| {
                window
                    .iter()
                    .zip(pattern)
                    .all(|(a, b)| equal(a, b))
                    .then_some(i)
            });
        if let Some(first) = matches.next() {
            return if unique && matches.next().is_some() {
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
