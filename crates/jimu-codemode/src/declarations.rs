//! Compact TypeScript declaration rendering for model-facing tool prompts.

use crate::identifier::{is_js_identifier, to_codemode_identifier};
use crate::types::CodemodeTool;
use jimu_schema::Schema;
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::sync::Arc;

const INDENT: &str = "  ";
const MAX_REF_EXPANSIONS: usize = 32;
pub const DEFAULT_INPUT_SCHEMA_MAX_CHARS: usize = 16_000;

/// Renders `declare const tools: { ... };` from the registered sandbox tools.
pub fn render_declarations(tools: &[Arc<dyn CodemodeTool>]) -> String {
    if tools.is_empty() {
        return String::new();
    }
    let mut members = Vec::with_capacity(tools.len());
    for tool in tools {
        let doc = doc_comment(tool.description(), INDENT);
        let sig = render_tool_signature(
            tool.name(),
            tool.input_schema(),
            tool.output_schema(),
            DEFAULT_INPUT_SCHEMA_MAX_CHARS,
        );
        members.push(format!("{doc}{INDENT}{sig}"));
    }
    format!("declare const tools: {{\n{}\n}};", members.join("\n"))
}

/// Renders one tool member signature: `ident(args: Input): Promise<Output>;`.
pub fn render_tool_signature(
    name: &str,
    input_schema: Option<&Schema>,
    output_schema: Option<&Schema>,
    input_max_chars: usize,
) -> String {
    let input = match input_schema {
        Some(schema) => schema_to_type(schema, Some(input_max_chars)),
        None => "unknown".to_string(),
    };
    let output = match output_schema {
        Some(schema) => schema_to_type(schema, None),
        None => "unknown".to_string(),
    };
    format!(
        "{}(args: {input}): Promise<{output}>;",
        to_codemode_identifier(name)
    )
}

/// Renders a JSON Schema to a compact TypeScript type expression.
pub fn schema_to_type(schema: &Schema, max_chars: Option<usize>) -> String {
    let mut ctx = SchemaRenderCtx {
        root: schema,
        resolving: BTreeSet::new(),
        expansions: 0,
    };
    let rendered = to_type(schema, &mut ctx);
    if let Some(limit) = max_chars
        && rendered.len() > limit
    {
        return "unknown".to_string();
    }
    rendered
}

struct SchemaRenderCtx<'a> {
    root: &'a Value,
    resolving: BTreeSet<String>,
    expansions: usize,
}

fn to_type(schema: &Value, ctx: &mut SchemaRenderCtx<'_>) -> String {
    match schema {
        Value::Bool(true) => "unknown".to_string(),
        Value::Bool(false) => "never".to_string(),
        Value::Object(map) => to_object_schema_type(map, ctx),
        _ => "unknown".to_string(),
    }
}

fn to_object_schema_type(map: &Map<String, Value>, ctx: &mut SchemaRenderCtx<'_>) -> String {
    if let Some(ref_str) = map.get("$ref").and_then(Value::as_str) {
        if ctx.resolving.contains(ref_str) || ctx.expansions >= MAX_REF_EXPANSIONS {
            return "unknown".to_string();
        }
        let Some(target) = resolve_local_ref(ref_str, ctx.root) else {
            return "unknown".to_string();
        };
        ctx.expansions += 1;
        ctx.resolving.insert(ref_str.to_string());
        let out = to_type(target, ctx);
        ctx.resolving.remove(ref_str);
        return out;
    }

    if let Some(const_val) = map.get("const") {
        return serde_json::to_string(const_val).unwrap_or_else(|_| "unknown".to_string());
    }

    if let Some(enum_vals) = map.get("enum").and_then(Value::as_array) {
        let parts: Vec<String> = enum_vals
            .iter()
            .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "unknown".to_string()))
            .collect();
        return union_types(parts);
    }

    if let Some(variants) = map
        .get("anyOf")
        .or_else(|| map.get("oneOf"))
        .and_then(Value::as_array)
    {
        let parts: Vec<String> = variants.iter().map(|v| to_type(v, ctx)).collect();
        return union_types(parts);
    }

    if let Some(all_of) = map.get("allOf").and_then(Value::as_array) {
        let parts: Vec<String> = all_of
            .iter()
            .map(|v| to_type(v, ctx))
            .filter(|s| s != "unknown")
            .map(|s| {
                if s.contains(" | ") {
                    format!("({s})")
                } else {
                    s
                }
            })
            .collect();
        return if parts.is_empty() {
            "unknown".to_string()
        } else {
            parts.join(" & ")
        };
    }

    if let Some(types) = map.get("type").and_then(Value::as_array) {
        let parts: Vec<String> = types
            .iter()
            .map(|entry| {
                let mut clone = map.clone();
                clone.insert("type".to_string(), entry.clone());
                to_object_schema_type(&clone, ctx)
            })
            .collect();
        return union_types(parts);
    }

    match map.get("type").and_then(Value::as_str) {
        Some("string") => "string".to_string(),
        Some("number" | "integer") => "number".to_string(),
        Some("boolean") => "boolean".to_string(),
        Some("null") => "null".to_string(),
        Some("array") => array_type(map, ctx),
        Some("object") => object_type(map, ctx),
        None => {
            if map.contains_key("properties")
                || map.contains_key("additionalProperties")
                || map.contains_key("required")
            {
                object_type(map, ctx)
            } else if map.contains_key("items") || map.contains_key("prefixItems") {
                array_type(map, ctx)
            } else {
                "unknown".to_string()
            }
        }
        Some(_) => "unknown".to_string(),
    }
}

fn array_type(map: &Map<String, Value>, ctx: &mut SchemaRenderCtx<'_>) -> String {
    if let Some(items) = map.get("items")
        && !items.is_array()
    {
        return format!("Array<{}>", to_type(items, ctx));
    }
    if let Some(tuple) = map
        .get("prefixItems")
        .or_else(|| map.get("items"))
        .and_then(Value::as_array)
        && !tuple.is_empty()
    {
        let parts: Vec<String> = tuple.iter().map(|item| to_type(item, ctx)).collect();
        return format!("[{}]", parts.join(", "));
    }
    "unknown[]".to_string()
}

fn object_type(map: &Map<String, Value>, ctx: &mut SchemaRenderCtx<'_>) -> String {
    let required: BTreeSet<&str> = map
        .get("required")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    let props = map.get("properties").and_then(Value::as_object);
    let mut members = Vec::new();
    if let Some(props) = props {
        for (name, prop_schema) in props {
            let key = if is_js_identifier(name) {
                name.clone()
            } else {
                serde_json::to_string(name).unwrap_or_else(|_| format!("\"{name}\""))
            };
            let opt = if required.contains(name.as_str()) {
                ""
            } else {
                "?"
            };
            members.push(format!("{key}{opt}: {};", to_type(prop_schema, ctx)));
        }
    }

    match map.get("additionalProperties") {
        Some(Value::Bool(false)) => {}
        Some(Value::Bool(true)) => members.push("[key: string]: unknown;".to_string()),
        Some(schema) => members.push(format!("[key: string]: {};", to_type(schema, ctx))),
        None if members.is_empty() => members.push("[key: string]: unknown;".to_string()),
        None => {}
    }

    if members.is_empty() {
        "{}".to_string()
    } else {
        format!("{{ {} }}", members.join(" "))
    }
}

fn union_types(types: Vec<String>) -> String {
    let mut unique = Vec::new();
    for ty in types {
        if ty == "unknown" {
            return "unknown".to_string();
        }
        if !unique.contains(&ty) {
            unique.push(ty);
        }
    }
    if unique.is_empty() {
        "never".to_string()
    } else {
        unique.join(" | ")
    }
}

fn resolve_local_ref<'a>(ref_str: &str, root: &'a Value) -> Option<&'a Value> {
    if ref_str == "#" {
        return Some(root);
    }
    let path = ref_str.strip_prefix("#/")?;
    let mut current = root;
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        let key = segment.replace("~1", "/").replace("~0", "~");
        current = current.as_object()?.get(&key)?;
    }
    Some(current)
}

fn doc_comment(description: &str, indent: &str) -> String {
    let text = description.trim();
    if text.is_empty() {
        return String::new();
    }
    let sanitized = text.replace("*/", "*\\/");
    let lines: Vec<&str> = sanitized.lines().collect();
    if lines.len() == 1 {
        format!("{indent}/** {} */\n", lines[0])
    } else {
        let body = lines
            .into_iter()
            .map(|line| {
                if line.is_empty() {
                    format!("{indent} *")
                } else {
                    format!("{indent} * {line}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!("{indent}/**\n{body}\n{indent} */\n")
    }
}
