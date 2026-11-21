use pikra_agent::{AgentTool, AgentToolResult, ToolExecutionMode, ToolProgressCallback};
use pikra_ai::{BoxFuture, CancelToken, ToolDefinition};
use pikra_schema::{int_schema, obj_schema, str_schema, with_bounds, with_description, CompiledSchema};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

pub const TOOL_SEARCH_NAME: &str = "tool_search";
pub const CALL_TOOL_NAME: &str = "call_tool";

const DEFAULT_SEARCH_LIMIT: usize = 5;
const MAX_SEARCH_LIMIT: usize = 20;

#[derive(Default)]
struct DeferredRegistryState {
    /// Ordered tool names for deterministic catalog serialization and tie-breaking.
    order: Vec<String>,
    tools: HashMap<String, Arc<dyn AgentTool>>,
    /// Tool names whose full schemas were already returned by `tool_search` since the last reset.
    seen_schemas: HashSet<String>,
}

/// Thread-safe registry for deferred tools exposed via `tool_search` and `call_tool`.
///
/// Deferred tools NEVER enter the top-level resident `tools` array (preserving prompt cache bytes).
#[derive(Default)]
pub struct DeferredToolRegistry {
    state: Mutex<DeferredRegistryState>,
}

impl DeferredToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace deferred tools. Any updated tool clears its `seen_schemas` entry so a
    /// subsequent `tool_search` can return its refreshed schema.
    pub fn upsert(&self, tools: impl IntoIterator<Item = Arc<dyn AgentTool>>) {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        for tool in tools {
            let name = tool.definition().name;
            if !guard.tools.contains_key(&name) {
                guard.order.push(name.clone());
            }
            guard.seen_schemas.remove(&name);
            guard.tools.insert(name, tool);
        }
    }

    /// Remove deferred tools by name, returning how many were actually removed.
    pub fn remove(&self, names: &[&str]) -> usize {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut removed = 0;
        for &name in names {
            if guard.tools.remove(name).is_some() {
                guard.order.retain(|n| n != name);
                guard.seen_schemas.remove(name);
                removed += 1;
            }
        }
        removed
    }

    /// Reset schema deduplication tracking (e.g., after compaction or branch fork).
    pub fn clear_seen_schemas(&self) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .seen_schemas
            .clear();
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn AgentTool>> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .tools
            .get(name)
            .cloned()
    }

    pub fn list_tools(&self) -> Vec<Arc<dyn AgentTool>> {
        let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .order
            .iter()
            .filter_map(|name| guard.tools.get(name).cloned())
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .tools
            .is_empty()
    }

    /// Render a compact `name: one-line description` catalog for `session.set_context("deferred_tools", ...)`.
    pub fn format_catalog(&self) -> Option<String> {
        let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if guard.order.is_empty() {
            return None;
        }
        let mut lines = Vec::with_capacity(guard.order.len() + 2);
        lines.push(
            "Deferred tools available via `tool_search` (for full schemas) and `call_tool` (direct invocation):"
                .to_string(),
        );
        for name in &guard.order {
            if let Some(tool) = guard.tools.get(name) {
                let def = tool.definition();
                let one_line = first_line(&def.description);
                if one_line.is_empty() {
                    lines.push(format!("- {}", def.name));
                } else {
                    lines.push(format!("- {}: {}", def.name, one_line));
                }
            }
        }
        Some(lines.join("\n"))
    }

    /// Search registered deferred tools by keyword query or exact `"select:name1,name2"`.
    /// Returns `(newly_loaded_definitions, already_loaded_names, missing_selected_names)`.
    pub fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> (Vec<ToolDefinition>, Vec<String>, Vec<String>) {
        let clamped_limit = limit.clamp(1, MAX_SEARCH_LIMIT);
        let trimmed = query.trim();

        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut matched_names: Vec<String> = Vec::new();
        let mut missing: Vec<String> = Vec::new();

        if let Some(rest) = strip_select_prefix(trimmed) {
            let mut local_seen = HashSet::new();
            for raw_name in rest.split(',') {
                let name = raw_name.trim();
                if name.is_empty() || !local_seen.insert(name.to_string()) {
                    continue;
                }
                if guard.tools.contains_key(name) {
                    matched_names.push(name.to_string());
                    if matched_names.len() >= clamped_limit {
                        break;
                    }
                } else {
                    missing.push(name.to_string());
                }
            }
        } else {
            let tokens: Vec<String> = trimmed
                .split_whitespace()
                .map(|t| t.to_ascii_lowercase())
                .filter(|t| !t.is_empty())
                .collect();

            if tokens.is_empty() {
                matched_names = guard.order.iter().take(clamped_limit).cloned().collect();
            } else {
                let mut scored: Vec<(usize, usize, String)> = Vec::new();
                for (order_idx, name) in guard.order.iter().enumerate() {
                    let Some(tool) = guard.tools.get(name) else {
                        continue;
                    };
                    let def = tool.definition();
                    let name_lc = def.name.to_ascii_lowercase();
                    let desc_lc = def.description.to_ascii_lowercase();
                    let mut score = 0usize;
                    for tok in &tokens {
                        if name_lc == *tok {
                            score += 10;
                        } else if name_lc.contains(tok.as_str()) {
                            score += 5;
                        }
                        if desc_lc.contains(tok.as_str()) {
                            score += 2;
                        }
                    }
                    if score > 0 {
                        scored.push((score, order_idx, name.clone()));
                    }
                }
                scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
                matched_names = scored
                    .into_iter()
                    .take(clamped_limit)
                    .map(|(_, _, name)| name)
                    .collect();
            }
        }

        let mut newly_loaded = Vec::new();
        let mut already_loaded = Vec::new();

        for name in matched_names {
            if let Some(tool) = guard.tools.get(&name) {
                let def = tool.definition();
                if guard.seen_schemas.insert(name.clone()) {
                    newly_loaded.push(def);
                } else {
                    already_loaded.push(name);
                }
            }
        }

        (newly_loaded, already_loaded, missing)
    }
}

fn strip_select_prefix(query: &str) -> Option<&str> {
    if query.len() >= 7 && query[..7].eq_ignore_ascii_case("select:") {
        Some(&query[7..])
    } else {
        None
    }
}

fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
}

/// Built-in resident `tool_search` tool for discovering full JSON Schemas of deferred tools.
pub struct ToolSearchTool {
    registry: Arc<DeferredToolRegistry>,
}

impl ToolSearchTool {
    pub fn new(registry: Arc<DeferredToolRegistry>) -> Self {
        Self { registry }
    }
}

impl AgentTool for ToolSearchTool {
    fn definition(&self) -> ToolDefinition {
        let query_schema = with_description(
            str_schema(),
            "Keyword query (e.g. \"weather forecast\") or exact selection \"select:name1,name2\".",
        );
        let limit_schema = with_description(
            with_bounds(int_schema(), Some(1.0), Some(MAX_SEARCH_LIMIT as f64)),
            "Maximum number of tools to return (default 5, max 20).",
        );
        ToolDefinition {
            name: TOOL_SEARCH_NAME.to_string(),
            description: "Search deferred tools by keyword or exact \"select:name1,name2\" to inspect their full parameter schemas. Schemas are deduplicated across repeated searches."
                .to_string(),
            parameters: obj_schema([
                ("query", query_schema, true),
                ("limit", limit_schema, false),
            ]),
        }
    }

    fn execute(
        &self,
        _tool_call_id: String,
        args: Value,
        _cancel: CancelToken,
        _on_update: ToolProgressCallback,
    ) -> BoxFuture<'_, Result<AgentToolResult, String>> {
        Box::pin(async move {
            let query = args
                .get("query")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing required string argument `query`".to_string())?;
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .map(|n| n as usize)
                .unwrap_or(DEFAULT_SEARCH_LIMIT);

            let (newly_loaded, already_loaded, missing) = self.registry.search(query, limit);

            let tools_json: Vec<Value> = newly_loaded
                .iter()
                .map(|def| {
                    json!({
                        "name": def.name,
                        "description": def.description,
                        "parameters": def.parameters,
                    })
                })
                .collect();

            let mut payload = serde_json::Map::new();
            payload.insert("tools".into(), Value::Array(tools_json));
            if !already_loaded.is_empty() {
                payload.insert("already_loaded".into(), json!(already_loaded));
            }
            if !missing.is_empty() {
                payload.insert("not_found".into(), json!(missing));
            }

            let structured = Value::Object(payload);
            let text = serde_json::to_string_pretty(&structured)
                .unwrap_or_else(|_| structured.to_string());

            Ok(AgentToolResult {
                content: vec![pikra_ai::ContentPart::Text(pikra_ai::TextContent::plain(
                    text,
                ))],
                details: Value::Null,
                structured_content: Some(structured),
                is_error: false,
                terminate: false,
            })
        })
    }
}

/// Built-in resident `call_tool` gateway invoking any registered deferred tool directly by name.
pub struct CallToolGateway {
    registry: Arc<DeferredToolRegistry>,
}

impl CallToolGateway {
    pub fn new(registry: Arc<DeferredToolRegistry>) -> Self {
        Self { registry }
    }
}

impl AgentTool for CallToolGateway {
    fn definition(&self) -> ToolDefinition {
        let name_schema = with_description(str_schema(), "Exact name of the deferred tool to call.");
        let args_schema = with_description(
            json!({ "type": "object" }),
            "JSON object of arguments to pass to the deferred tool.",
        );
        ToolDefinition {
            name: CALL_TOOL_NAME.to_string(),
            description: "Invoke any registered deferred tool by name with its JSON arguments."
                .to_string(),
            parameters: obj_schema([
                ("name", name_schema, true),
                ("arguments", args_schema, false),
            ]),
        }
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    fn execute(
        &self,
        tool_call_id: String,
        args: Value,
        cancel: CancelToken,
        on_update: ToolProgressCallback,
    ) -> BoxFuture<'_, Result<AgentToolResult, String>> {
        Box::pin(async move {
            let name = args
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing required string argument `name`".to_string())?;
            let raw_tool_args = args
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

            let Some(tool) = self.registry.get(name) else {
                return Ok(AgentToolResult::error(format!(
                    "Unknown deferred tool: {name}"
                )));
            };

            let validated_args = match CompiledSchema::compile(tool.definition().parameters) {
                Ok(compiled) => match compiled.coerce_and_validate(&raw_tool_args) {
                    Ok(v) => v,
                    Err(err) => {
                        return Ok(AgentToolResult::error(format!(
                            "Invalid arguments for deferred tool `{name}`:\n{err}"
                        )));
                    }
                },
                Err(_) => raw_tool_args,
            };

            tool.execute(tool_call_id, validated_args, cancel, on_update)
                .await
        })
    }
}
