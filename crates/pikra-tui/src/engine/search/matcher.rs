#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryKind {
    /// Case-insensitive substring (compiled to a regex via `regex::escape`).
    Substring,
    /// User-supplied regex pattern.
    Regex,
}

/// Both substring and regex queries compile to a `regex::Regex`.
/// Matching is smart-case: case-insensitive unless the query contains an uppercase character (Vim `smartcase` / ripgrep `--smart-case`).
#[derive(Debug, Clone)]
pub struct TextMatcher {
    regex: regex::Regex,
    query: String,
    is_error: bool,
}

impl TextMatcher {
    /// Compile `query` under the given interpretation.
    ///
    /// A regex that fails to compile sets [`is_error`](Self::is_error) and falls back to a pattern that never matches.
    pub fn new(query: impl Into<String>, kind: QueryKind) -> Self {
        let query = query.into();
        let smart_ci = !query.chars().any(|c| c.is_uppercase());
        let (regex, is_error) = match kind {
            QueryKind::Substring => {
                let escaped = regex::escape(&query);
                let re = regex::RegexBuilder::new(&escaped)
                    .case_insensitive(smart_ci)
                    .build()
                    .unwrap_or_else(|_| regex::Regex::new("(?:)").unwrap());
                (re, false)
            }
            QueryKind::Regex => match regex::RegexBuilder::new(&query)
                .case_insensitive(smart_ci)
                .build()
            {
                Ok(re) => (re, false),
                // `\z.` can never match: an end-of-text anchor followed by a char.
                Err(_) => (regex::Regex::new(r"\z.").unwrap(), true),
            },
        };
        Self {
            regex,
            query,
            is_error,
        }
    }

    /// The raw query string the user typed.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Whether the regex failed to compile (bad user regex).
    pub fn is_error(&self) -> bool {
        self.is_error
    }

    /// The compiled regex, for callers that highlight matches.
    pub fn compiled_regex(&self) -> &regex::Regex {
        &self.regex
    }

    pub fn is_match(&self, haystack: &str) -> bool {
        self.regex.is_match(haystack)
    }
}
