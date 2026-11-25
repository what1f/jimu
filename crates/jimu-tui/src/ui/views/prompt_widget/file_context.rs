// Adapted from the Grok Build pager (Apache-2.0). Copyright 2023-2026 SpaceXAI.

//! @-context detection: parses `@query` tokens from prompt text and cursor position.
//!
//! Given the prompt text and cursor position, determines whether the cursor is
//! inside an `@`-token and extracts the query string for fuzzy matching.
//!
//! ## Rules
//!
//! - The `@` must NOT be preceded by an alphanumeric character or underscore
//!   (avoids triggering on email addresses like `user@example.com`).
//! - The token extends from `@` to the first whitespace, comma, or semicolon.
//! - The cursor must be within the token range.
//! - The query is the text between `@` (exclusive) and the cursor.
//!
//! ## Special modes
//!
//! - **Dir mode**: a query ending with `/` restricts matches to directories only.
//! - **Hidden mode**: a query starting with `!` shows hidden/gitignored files.

use std::ops::Range;

/// Context for the current @-completion token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtContext {
    /// Byte range in the input text (includes the `@` as the first character).
    pub range: Range<usize>,
    /// Cursor byte position within the input text.
    pub cursor: usize,
    /// Query string: text after `@` (and after `!` if hidden mode) up to cursor.
    pub query: String,
}

impl AtContext {
    /// Whether the query requests directory-only results (ends with `/`).
    pub fn is_dir_mode(&self) -> bool {
        self.query.ends_with('/')
    }

    /// Whether the query requests hidden/gitignored files (starts with `!`).
    pub fn is_hidden_mode(&self) -> bool {
        self.query.starts_with('!')
    }

    /// The effective query for the fuzzy matcher (strips leading `!`).
    pub fn matcher_query(&self) -> &str {
        self.query.strip_prefix('!').unwrap_or(&self.query)
    }

    /// Byte range covering only the path portion of the @-token.
    pub fn path_range(&self) -> Range<usize> {
        let prefix = 1 + if self.is_hidden_mode() { 1 } else { 0 };
        self.range.start + prefix..self.range.end
    }
}

/// Detect an @-completion context from prompt text and cursor position. Returns `None` if the
/// cursor is not inside an @-token, or if the `@` is preceded by an alphanumeric/underscore
/// character (e.g., `email@`).
pub fn detect(text: &str, cursor: usize) -> Option<AtContext> {
    detect_with_drill(text, cursor, None)
}

/// Like [`detect`], but treats whitespace *inside* `drill_prefix` (the path of the directory being drilled into) as part of the @-token.
/// `@my dir/` thus stays one token.
/// Once the text after `@` stops matching the prefix, it stops applying and whitespace terminates the token again.
pub fn detect_with_drill(
    text: &str,
    cursor: usize,
    drill_prefix: Option<&str>,
) -> Option<AtContext> {
    // Cursor must be within text bounds and on a char boundary.
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return None;
    }

    // Find the rightmost `@` before the cursor.
    let at_idx = text.get(..cursor)?.rfind('@')?;

    // Reject if `@` is preceded by alphanumeric or underscore (email-like).
    if let Some(ch) = text.get(..at_idx)?.chars().next_back()
        && (ch.is_alphanumeric() || ch == '_')
    {
        return None;
    }

    // Path content starts after `@` (+ optional `!` hidden-mode marker).
    let content_start = at_idx + 1;
    let after_bang = if text.get(content_start..)?.starts_with('!') {
        content_start + 1
    } else {
        content_start
    };
    // Whitespace inside the drilled prefix is path content, not a terminator.
    let internal_until = drill_prefix.and_then(|prefix| {
        text.get(after_bang..)
            .filter(|rest| rest.starts_with(prefix))
            .map(|_| after_bang + prefix.len())
    });

    // Find the end of the @-token: first whitespace, comma, or semicolon after `@`.
    let token_end = text
        .get(at_idx + 1..)?
        .char_indices()
        .find_map(|(offset, ch)| {
            let abs = at_idx + 1 + offset;
            if (ch.is_whitespace() || matches!(ch, ',' | ';'))
                && internal_until.is_none_or(|until| abs >= until)
            {
                Some(abs)
            } else {
                None
            }
        })
        .unwrap_or(text.len());

    // Cursor must be within the @-token.
    if cursor > token_end {
        return None;
    }

    Some(AtContext {
        range: at_idx..token_end,
        cursor,
        query: text.get(at_idx + 1..cursor)?.to_owned(),
    })
}

/// Normalize a display path (strip leading `./`).
pub fn normalize_display_path(path: &str) -> &str {
    path.strip_prefix("./").unwrap_or(path)
}
