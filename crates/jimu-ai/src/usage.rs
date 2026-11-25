//! Usage cost calculation, prompt-cache key clamping, endpoint URL joining, and tool JSON parsing.

use crate::{ModelCost, Usage};
use serde_json::{Map, Value};

pub const OPENAI_PROMPT_CACHE_KEY_MAX_CHARS: usize = 64;

/// Compute `usage.cost_total` from `ModelCost` (USD per 1M tokens) unless the provider
/// already supplied an authoritative billed cost (`cost_from_provider == true`).
pub fn calculate_cost(cost: &ModelCost, usage: &mut Usage) {
    if usage.cost_from_provider {
        return;
    }
    usage.cost_total = (usage.input as f64 * cost.input
        + usage.output as f64 * cost.output
        + usage.cache_read as f64 * cost.cache_read
        + usage.cache_write as f64 * cost.cache_write)
        / 1_000_000.0;
}

/// Clamp OpenAI `prompt_cache_key` to at most 64 Unicode scalar values.
pub fn clamp_prompt_cache_key(key: &str) -> String {
    key.chars()
        .take(OPENAI_PROMPT_CACHE_KEY_MAX_CHARS)
        .collect()
}

/// Build `{base_url}/responses` without duplicating `/v1` or `/responses`.
pub fn openai_responses_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/responses") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/responses")
    }
}

/// Build `{base_url}/chat/completions` without duplicating the suffix.
pub fn openai_completions_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/chat/completions") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/chat/completions")
    }
}

/// Build `{base_url}/v1/messages`, avoiding double `/v1` when `base_url` already ends with `/v1`.
pub fn anthropic_messages_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/messages") {
        trimmed.to_string()
    } else if trimmed.ends_with("/v1") {
        format!("{trimmed}/messages")
    } else {
        format!("{trimmed}/v1/messages")
    }
}

/// Build `{base_url}/models/{model_id}:streamGenerateContent?alt=sse`.
pub fn google_stream_url(base_url: &str, model_id: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    format!("{trimmed}/models/{model_id}:streamGenerateContent?alt=sse")
}

/// Repair unescaped control characters and invalid escape sequences inside JSON string literals.
fn repair_json_strings(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;

    while let Some(ch) = chars.next() {
        if !in_string {
            out.push(ch);
            if ch == '"' {
                in_string = true;
            }
            continue;
        }
        match ch {
            '"' => {
                out.push('"');
                in_string = false;
            }
            '\\' => match chars.peek().copied() {
                Some('"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't') => {
                    out.push('\\');
                    if let Some(next) = chars.next() {
                        out.push(next);
                    }
                }
                Some('u') => {
                    out.push('\\');
                    out.push(chars.next().unwrap());
                }
                Some(_) => {
                    // Double invalid backslash so JSON parse succeeds.
                    out.push_str("\\\\");
                }
                None => out.push_str("\\\\"),
            },
            '\u{0000}'..='\u{001f}' => match ch {
                '\u{0008}' => out.push_str("\\b"),
                '\u{000c}' => out.push_str("\\f"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                other => {
                    use std::fmt::Write;
                    let _ = write!(&mut out, "\\u{:04x}", other as u32);
                }
            },
            other => out.push(other),
        }
    }
    out
}

/// Parse final streamed tool call JSON arguments.
/// Never panics: if the payload is malformed or not a JSON object, returns an empty `Map`
/// alongside `Some(error_description)`.
pub fn parse_tool_call_arguments(raw: &str) -> (Map<String, Value>, Option<String>) {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return (Map::new(), None);
    }

    let parsed = match serde_json::from_str::<Value>(trimmed) {
        Ok(v) => Ok(v),
        Err(first_err) => {
            let repaired = repair_json_strings(trimmed);
            if repaired != trimmed {
                serde_json::from_str::<Value>(&repaired).map_err(|_| first_err)
            } else {
                Err(first_err)
            }
        }
    };

    match parsed {
        Ok(Value::Object(map)) => (map, None),
        Ok(_) => (
            Map::new(),
            Some(format!(
                "Tool arguments must be a JSON object\n\nReceived arguments:\n{raw}"
            )),
        ),
        Err(err) => (
            Map::new(),
            Some(format!("{err}\n\nReceived arguments:\n{raw}")),
        ),
    }
}

/// Merge HTTP header layers case-insensitively (`None` removes a header).
pub fn merge_headers(
    base: Vec<(String, String)>,
    model_headers: &[(String, String)],
    option_headers: &[(String, Option<String>)],
) -> Vec<(String, String)> {
    let mut headers = base;
    let upsert = |list: &mut Vec<(String, String)>, key: &str, val: Option<&str>| {
        let lower = key.to_ascii_lowercase();
        if let Some(pos) = list
            .iter()
            .position(|(k, _)| k.to_ascii_lowercase() == lower)
        {
            match val {
                Some(v) => list[pos] = (key.to_string(), v.to_string()),
                None => {
                    list.remove(pos);
                }
            }
        } else if let Some(v) = val {
            list.push((key.to_string(), v.to_string()));
        }
    };

    for (k, v) in model_headers {
        upsert(&mut headers, k, Some(v));
    }
    for (k, v) in option_headers {
        upsert(&mut headers, k, v.as_deref());
    }
    headers
}

pub fn has_header(headers: &[(String, String)], name: &str) -> bool {
    let expected = name.to_ascii_lowercase();
    headers
        .iter()
        .any(|(k, v)| k.to_ascii_lowercase() == expected && !v.trim().is_empty())
}
