//! Codemode source header parsing (`// @options: {...}`).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Prefix of the optional first-line options directive.
pub const CODEMODE_OPTIONS_PREFIX: &str = "// @options:";

/// Maximum `timeout_ms` accepted in `@options` (matches 32-bit signed ms timer limit).
pub const MAX_OPTIONS_TIMEOUT_MS: u64 = 2_147_483_647;

/// Lark grammar for providers that support grammar-constrained tool input.
pub const CODEMODE_SOURCE_GRAMMAR: &str = r#"
start: options_source | plain_source
options_source: OPTIONS_LINE NEWLINE SOURCE
plain_source: SOURCE

OPTIONS_LINE: /[ \t]*\/\/ @options:[^\r\n]*/
NEWLINE: /\r?\n/
SOURCE: /[\s\S]+/
"#;

const SUPPORTED_FIELDS_TEXT: &str = "`max_output_tokens` and `timeout_ms`";

/// Parsed directive fields from a leading `// @options: {...}` line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodemodeSourceOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// Output of [`parse_codemode_source`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCodemodeSource {
    /// Script body with the `@options` line replaced by an empty line (`""`) so stack trace
    /// line numbers match the original input.
    pub code: String,
    pub options: CodemodeSourceOptions,
}

/// Validation failure returned by [`parse_codemode_source`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct CodemodeSourceError {
    pub message: String,
}

impl CodemodeSourceError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Splits an optional first-line `// @options: {...}` directive from JavaScript source.
///
/// Rejects empty input, invalid JSON, non-object JSON, unknown option keys, invalid values,
/// an `@options` line with no following code, or an `@options` directive on any line other
/// than line 1.
///
/// When present, the `@options` line is replaced with an empty line (`""`) so line numbers in
/// QuickJS syntax errors and runtime stack traces match the original input.
pub fn parse_codemode_source(input: &str) -> Result<ParsedCodemodeSource, CodemodeSourceError> {
    if input.trim().is_empty() {
        return Err(CodemodeSourceError::new(
            "Expected JavaScript source text (non-empty). Provide JS only, optionally with a first line `// @options: {\"max_output_tokens\": 1000}`.",
        ));
    }

    for (index, line) in input.split('\n').enumerate().skip(1) {
        if line.trim_start().starts_with(CODEMODE_OPTIONS_PREFIX) {
            return Err(CodemodeSourceError::new(format!(
                "The {CODEMODE_OPTIONS_PREFIX} line must be the first line of the input; found one on line {}",
                index + 1
            )));
        }
    }

    let newline = input.find('\n');
    let first_line = match newline {
        Some(pos) => input[..pos].trim_end_matches('\r'),
        None => input.trim_end_matches('\r'),
    };
    let trimmed = first_line.trim_start();
    if !trimmed.starts_with(CODEMODE_OPTIONS_PREFIX) {
        return Ok(ParsedCodemodeSource {
            code: input.to_string(),
            options: CodemodeSourceOptions::default(),
        });
    }

    let code = match newline {
        Some(pos) => &input[pos..],
        None => "",
    };
    if code.trim().is_empty() {
        return Err(CodemodeSourceError::new(
            "The @options line must be followed by JavaScript source on subsequent lines",
        ));
    }

    let directive = trimmed[CODEMODE_OPTIONS_PREFIX.len()..].trim();
    let options = parse_options_json(directive)?;

    Ok(ParsedCodemodeSource {
        code: code.to_string(),
        options,
    })
}

fn parse_options_json(directive: &str) -> Result<CodemodeSourceOptions, CodemodeSourceError> {
    if directive.is_empty() {
        return Err(CodemodeSourceError::new(format!(
            "@options must be a JSON object with supported fields {SUPPORTED_FIELDS_TEXT}"
        )));
    }

    let value: Value = serde_json::from_str(directive).map_err(|err| {
        CodemodeSourceError::new(format!(
            "@options must be valid JSON with supported fields {SUPPORTED_FIELDS_TEXT}: {err}"
        ))
    })?;

    let Some(map) = value.as_object() else {
        return Err(CodemodeSourceError::new(format!(
            "@options must be a JSON object with supported fields {SUPPORTED_FIELDS_TEXT}"
        )));
    };

    for key in map.keys() {
        if key != "max_output_tokens" && key != "timeout_ms" {
            return Err(CodemodeSourceError::new(format!(
                "@options only supports {SUPPORTED_FIELDS_TEXT}; got `{key}`"
            )));
        }
    }

    let mut options = CodemodeSourceOptions::default();

    if let Some(raw) = map.get("max_output_tokens") {
        let Some(n) = raw.as_u64() else {
            return Err(CodemodeSourceError::new(
                "@options field `max_output_tokens` must be a non-negative safe integer",
            ));
        };
        if n > MAX_SAFE_JS_INT {
            return Err(CodemodeSourceError::new(
                "@options field `max_output_tokens` must be a non-negative safe integer",
            ));
        }
        options.max_output_tokens = Some(n);
    }

    if let Some(raw) = map.get("timeout_ms") {
        let Some(n) = raw.as_u64() else {
            return Err(CodemodeSourceError::new(format!(
                "@options field `timeout_ms` must be a positive integer up to {MAX_OPTIONS_TIMEOUT_MS}"
            )));
        };
        if n == 0 || n > MAX_OPTIONS_TIMEOUT_MS {
            return Err(CodemodeSourceError::new(format!(
                "@options field `timeout_ms` must be a positive integer up to {MAX_OPTIONS_TIMEOUT_MS}"
            )));
        }
        options.timeout_ms = Some(n);
    }

    Ok(options)
}

const MAX_SAFE_JS_INT: u64 = (1u64 << 53) - 1;
