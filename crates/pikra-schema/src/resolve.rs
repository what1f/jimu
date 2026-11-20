//! Local JSON Pointer `$ref` resolution and compile-time schema structural validation.

use serde_json::Value;

use crate::SchemaCompileError;

const VALID_TYPES: &[&str] = &[
    "string", "integer", "number", "boolean", "null", "array", "object",
];

/// Resolves a local JSON Pointer (`#` or `#/...`) inside `root`.
pub(crate) fn resolve_pointer<'a>(
    root: &'a Value,
    reference: &str,
) -> Result<&'a Value, SchemaCompileError> {
    if reference == "#" {
        return Ok(root);
    }
    let Some(fragment) = reference.strip_prefix("#/") else {
        return Err(SchemaCompileError(format!(
            "unsupported $ref '{reference}': only local JSON pointer references ('#' or '#/...') are supported"
        )));
    };
    let mut current = root;
    for raw_segment in fragment.split('/') {
        let decoded = percent_decode(raw_segment);
        let segment = unescape_pointer_segment(&decoded);
        current = match current {
            Value::Object(map) => map.get(&segment).ok_or_else(|| {
                SchemaCompileError(format!("unresolved $ref '{reference}': missing segment '{segment}'"))
            })?,
            Value::Array(arr) => {
                let idx = segment.parse::<usize>().map_err(|_| {
                    SchemaCompileError(format!(
                        "unresolved $ref '{reference}': invalid array index '{segment}'"
                    ))
                })?;
                arr.get(idx).ok_or_else(|| {
                    SchemaCompileError(format!(
                        "unresolved $ref '{reference}': array index {idx} out of bounds"
                    ))
                })?
            }
            _ => {
                return Err(SchemaCompileError(format!(
                    "unresolved $ref '{reference}': cannot traverse non-container at '{segment}'"
                )));
            }
        };
    }
    if !current.is_object() && !current.is_boolean() {
        return Err(SchemaCompileError(format!(
            "$ref '{reference}' does not resolve to an object or boolean schema"
        )));
    }
    Ok(current)
}

/// Validates schema syntax and checks all local `$ref` targets exist at compile time.
pub(crate) fn validate_schema_tree(
    root: &Value,
    node: &Value,
    loc: &str,
) -> Result<(), SchemaCompileError> {
    match node {
        Value::Bool(_) => Ok(()),
        Value::Object(map) => {
            if let Some(ty) = map.get("type") {
                match ty {
                    Value::String(s) => {
                        if !VALID_TYPES.contains(&s.as_str()) {
                            return Err(SchemaCompileError(format!(
                                "{loc}: unknown schema type '{s}'"
                            )));
                        }
                    }
                    Value::Array(arr) => {
                        if arr.is_empty() {
                            return Err(SchemaCompileError(format!(
                                "{loc}: 'type' array must not be empty"
                            )));
                        }
                        for item in arr {
                            let Some(s) = item.as_str() else {
                                return Err(SchemaCompileError(format!(
                                    "{loc}: 'type' array entries must be strings"
                                )));
                            };
                            if !VALID_TYPES.contains(&s) {
                                return Err(SchemaCompileError(format!(
                                    "{loc}: unknown schema type '{s}'"
                                )));
                            }
                        }
                    }
                    _ => {
                        return Err(SchemaCompileError(format!(
                            "{loc}: 'type' must be a string or array of strings"
                        )));
                    }
                }
            }

            if let Some(reference) = map.get("$ref") {
                let Some(ref_str) = reference.as_str() else {
                    return Err(SchemaCompileError(format!("{loc}: '$ref' must be a string")));
                };
                resolve_pointer(root, ref_str)?;
            }

            if let Some(enum_val) = map.get("enum")
                && !enum_val.is_array()
            {
                return Err(SchemaCompileError(format!("{loc}: 'enum' must be an array")));
            }

            if let Some(req) = map.get("required") {
                let Some(arr) = req.as_array() else {
                    return Err(SchemaCompileError(format!("{loc}: 'required' must be an array")));
                };
                if !arr.iter().all(Value::is_string) {
                    return Err(SchemaCompileError(format!(
                        "{loc}: 'required' entries must be strings"
                    )));
                }
            }

            for kw in ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"] {
                if let Some(v) = map.get(kw)
                    && !v.is_number()
                {
                    return Err(SchemaCompileError(format!("{loc}: '{kw}' must be a number")));
                }
            }

            if let Some(v) = map.get("multipleOf") {
                let Some(n) = v.as_f64() else {
                    return Err(SchemaCompileError(format!(
                        "{loc}: 'multipleOf' must be a positive number"
                    )));
                };
                if !n.is_finite() || n <= 0.0 {
                    return Err(SchemaCompileError(format!(
                        "{loc}: 'multipleOf' must be > 0"
                    )));
                }
            }

            for kw in [
                "minLength",
                "maxLength",
                "minItems",
                "maxItems",
                "minProperties",
                "maxProperties",
            ] {
                if let Some(v) = map.get(kw)
                    && !is_non_negative_int(v)
                {
                    return Err(SchemaCompileError(format!(
                        "{loc}: '{kw}' must be a non-negative integer"
                    )));
                }
            }

            if let Some(v) = map.get("uniqueItems")
                && !v.is_boolean()
            {
                return Err(SchemaCompileError(format!(
                    "{loc}: 'uniqueItems' must be a boolean"
                )));
            }

            if let Some(v) = map.get("format")
                && !v.is_string()
            {
                return Err(SchemaCompileError(format!(
                    "{loc}: 'format' must be a string"
                )));
            }

            for kw in ["properties", "$defs", "definitions"] {
                if let Some(v) = map.get(kw) {
                    let Some(submap) = v.as_object() else {
                        return Err(SchemaCompileError(format!("{loc}: '{kw}' must be an object")));
                    };
                    for (k, sub) in submap {
                        validate_schema_tree(root, sub, &format!("{loc}/{kw}/{k}"))?;
                    }
                }
            }

            if let Some(addl) = map.get("additionalProperties") {
                validate_schema_tree(root, addl, &format!("{loc}/additionalProperties"))?;
            }

            if let Some(items) = map.get("items") {
                match items {
                    Value::Array(arr) => {
                        for (i, sub) in arr.iter().enumerate() {
                            validate_schema_tree(root, sub, &format!("{loc}/items/{i}"))?;
                        }
                    }
                    _ => validate_schema_tree(root, items, &format!("{loc}/items"))?,
                }
            }

            if let Some(addl_items) = map.get("additionalItems") {
                validate_schema_tree(root, addl_items, &format!("{loc}/additionalItems"))?;
            }

            for kw in ["prefixItems", "anyOf", "oneOf", "allOf"] {
                if let Some(v) = map.get(kw) {
                    let Some(arr) = v.as_array() else {
                        return Err(SchemaCompileError(format!("{loc}: '{kw}' must be an array")));
                    };
                    for (i, sub) in arr.iter().enumerate() {
                        validate_schema_tree(root, sub, &format!("{loc}/{kw}/{i}"))?;
                    }
                }
            }

            if let Some(not) = map.get("not") {
                validate_schema_tree(root, not, &format!("{loc}/not"))?;
            }

            Ok(())
        }
        _ => Err(SchemaCompileError(format!(
            "{loc}: schema must be an object or boolean"
        ))),
    }
}

fn is_non_negative_int(v: &Value) -> bool {
    match v {
        Value::Number(n) => {
            if n.is_u64() {
                true
            } else if let Some(i) = n.as_i64() {
                i >= 0
            } else if let Some(f) = n.as_f64() {
                f.is_finite() && f >= 0.0 && f.fract() == 0.0
            } else {
                false
            }
        }
        _ => false,
    }
}

fn unescape_pointer_segment(s: &str) -> String {
    if !s.contains('~') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '~' {
            match chars.next() {
                Some('1') => out.push('/'),
                Some('0') => out.push('~'),
                Some(other) => {
                    out.push('~');
                    out.push(other);
                }
                None => out.push('~'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
