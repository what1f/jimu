//! Tool name to JavaScript identifier conversion.

/// Converts a tool name to a valid ASCII JavaScript identifier for `tools.<ident>`.
///
/// Characters that are not valid in a JavaScript identifier become `_`.
/// For example, `my-tool` becomes `my_tool` and `mcp__docs__search` stays unchanged.
pub fn to_codemode_identifier(name: &str) -> String {
    if name.is_empty() {
        return "_".to_string();
    }
    let mut out = String::with_capacity(name.len());
    for (idx, ch) in name.chars().enumerate() {
        let valid = if idx == 0 {
            ch.is_ascii_alphabetic() || ch == '_' || ch == '$'
        } else {
            ch.is_ascii_alphanumeric() || ch == '_' || ch == '$'
        };
        out.push(if valid { ch } else { '_' });
    }
    out
}

pub(crate) fn is_js_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_' || first == '$') {
        return false;
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '$')
}
