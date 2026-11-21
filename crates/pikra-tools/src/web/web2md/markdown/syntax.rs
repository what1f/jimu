use std::fmt::Write;

pub(in crate::web::web2md) fn escape(text: &str) -> String {
    escape_text(text, true)
}

pub(in crate::web::web2md) fn escape_text(text: &str, at_line_start: bool) -> String {
    let mut output = String::with_capacity(text.len());
    for (index, line) in text.split('\n').enumerate() {
        if index != 0 {
            output.push('\n');
        }
        let marker_allowed = at_line_start || index != 0;
        let trimmed = line.trim_start();
        let ordered_marker = trimmed
            .find(|c: char| !c.is_ascii_digit())
            .filter(|offset| marker_allowed && *offset > 0 && *offset <= 9)
            .and_then(|offset| {
                let after = &trimmed[offset..];
                (after.starts_with(". ") || after.starts_with(") "))
                    .then_some(line.len() - trimmed.len() + offset)
            });
        let first = line.len() - trimmed.len();
        let mut chars = line.char_indices().peekable();
        let mut previous = ' ';
        while let Some((offset, c)) = chars.next() {
            let intraword_underscore = c == '_'
                && previous.is_alphanumeric()
                && chars.peek().is_some_and(|(_, next)| next.is_alphanumeric());
            if matches!(c, '\\' | '`' | '*' | '[' | ']' | '<' | '>' | '~')
                || (c == '_' && !intraword_underscore)
                || (marker_allowed && offset == first && matches!(c, '#' | '+' | '-' | '='))
                || Some(offset) == ordered_marker
            {
                output.push('\\');
            }
            output.push(c);
            previous = c;
        }
    }
    output
}

fn ticks(text: &str) -> usize {
    text.split(|c| c != '`')
        .map(str::len)
        .max()
        .unwrap_or_default()
}

pub(in crate::web::web2md) fn inline_code(code: &str) -> String {
    let marker = "`".repeat(ticks(code) + 1);
    let code = code.replace(['\n', '\r'], " ");
    if code.starts_with(['`', ' ']) || code.ends_with(['`', ' ']) {
        format!("{marker} {code} {marker}")
    } else {
        format!("{marker}{code}{marker}")
    }
}

pub(in crate::web::web2md) fn fence(code: &str, language: &str) -> String {
    let marker = "`".repeat((ticks(code) + 1).max(3));
    format!(
        "{marker}{language}\n{}\n{marker}",
        code.trim_end_matches(['\n', '\r'])
    )
}

pub(in crate::web::web2md) fn destination(url: &str) -> String {
    let mut output = String::new();
    for c in url.chars() {
        match c {
            '(' | ')' | '<' | '>' | '\\' | ' ' | '\n' | '\r' | '\t' => {
                let _ = write!(output, "%{:02X}", u32::from(c));
            }
            _ => output.push(c),
        }
    }
    output
}

pub(in crate::web::web2md) fn link(label: &str, url: &str) -> String {
    if url.is_empty() {
        return label.to_owned();
    }
    format!("[{label}]({})", destination(url))
}

pub(in crate::web::web2md) fn image(alt: &str, url: &str) -> String {
    if url.is_empty() {
        return String::new();
    }
    format!(
        "![{}]({})",
        escape(&alt.replace(['\n', '\r'], " ")),
        destination(url)
    )
}

pub(in crate::web::web2md) fn quote(body: &str) -> String {
    body.split('\n')
        .map(|line| {
            if line.is_empty() {
                ">".to_owned()
            } else {
                format!("> {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
