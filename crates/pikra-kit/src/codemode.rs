use crate::deferred::DeferredToolRegistry;
use futures_util::future::{Either, select};
use pikra_agent::{AgentTool, AgentToolResult, ToolExecutionMode, ToolProgressCallback};
use pikra_ai::{BoxFuture, CancelToken, ContentPart, ImageContent, TextContent, ToolDefinition};
use pikra_codemode::{
    CodemodeExecuteOptions, CodemodeOutputItem, CodemodeSandbox, CodemodeSandboxOptions,
    CodemodeTool, CodemodeToolContext,
};
use pikra_schema::{
    CompiledSchema, Schema, int_schema, obj_schema, str_schema, with_bounds, with_description,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

pub const CODEMODE_EXEC_TOOL_NAME: &str = "exec";

/// Adapter exposing any [`AgentTool`] inside [`CodemodeSandbox`].
struct AgentToolCodemodeBridge {
    definition: ToolDefinition,
    tool: Arc<dyn AgentTool>,
    middlewares: Vec<Arc<dyn pikra_agent::AgentMiddleware>>,
    outer_cancel: CancelToken,
}

impl AgentToolCodemodeBridge {
    fn new(
        tool: Arc<dyn AgentTool>,
        middlewares: Vec<Arc<dyn pikra_agent::AgentMiddleware>>,
        outer_cancel: CancelToken,
    ) -> Self {
        Self {
            definition: tool.definition(),
            tool,
            middlewares,
            outer_cancel,
        }
    }
}

impl CodemodeTool for AgentToolCodemodeBridge {
    fn name(&self) -> &str {
        &self.definition.name
    }

    fn description(&self) -> &str {
        &self.definition.description
    }

    fn input_schema(&self) -> Option<&Schema> {
        Some(&self.definition.parameters)
    }

    fn execute(
        &self,
        args: Value,
        ctx: CodemodeToolContext,
    ) -> pikra_codemode::BoxFuture<'_, Result<Value, String>> {
        Box::pin(async move {
            if ctx.is_aborted() || self.outer_cancel.is_cancelled() {
                return Err("Execution aborted".to_string());
            }

            let effective_args = if args.is_null() {
                Value::Object(serde_json::Map::new())
            } else {
                args
            };

            let noop_progress: ToolProgressCallback = Arc::new(|_| {});
            let call_id = format!("cm_{}", Uuid::now_v7());
            let result = if self.middlewares.is_empty() {
                let validated = match CompiledSchema::compile(self.definition.parameters.clone()) {
                    Ok(compiled) => {
                        compiled
                            .coerce_and_validate(&effective_args)
                            .map_err(|err| {
                                format!("Invalid arguments for `{}`:\n{err}", self.definition.name)
                            })?
                    }
                    Err(_) => effective_args,
                };
                self.tool
                    .execute(call_id, validated, self.outer_cancel.clone(), noop_progress)
                    .await?
            } else {
                let args_obj = effective_args.as_object().cloned().ok_or_else(|| {
                    format!(
                        "Invalid arguments for `{}`: expected JSON object",
                        self.definition.name
                    )
                })?;
                let dummy_model = pikra_ai::Model::new(
                    "codemode",
                    "codemode",
                    pikra_ai::ApiKind::OpenaiResponses,
                    "",
                );
                let dummy_assistant = pikra_ai::AssistantMessage {
                    content: Vec::new(),
                    api: dummy_model.api.clone(),
                    provider: dummy_model.provider.clone(),
                    model: dummy_model.id.clone(),
                    response_id: None,
                    thinking_level: None,
                    usage: pikra_ai::Usage::default(),
                    stop_reason: pikra_ai::StopReason::ToolUse,
                    error_message: None,
                    diagnostics: Vec::new(),
                    timestamp: 0,
                    duration_ms: None,
                };
                let snapshot = pikra_agent::AgentContextSnapshot {
                    system_prompt: String::new(),
                    tools: vec![self.definition.clone()],
                    messages: Vec::new(),
                    model: dummy_model,
                    thinking_level: pikra_ai::ThinkingLevel::Off,
                    fast: false,
                };
                let synthetic_tc = pikra_ai::ToolCall {
                    id: call_id,
                    name: self.definition.name.clone(),
                    arguments: args_obj,
                    item_id: None,
                    thought_signature: None,
                    namespace: None,
                    arguments_error: None,
                };
                let tools_slice = [Arc::clone(&self.tool)];
                pikra_agent::run_tool_call(
                    &tools_slice,
                    &self.middlewares,
                    &dummy_assistant,
                    &synthetic_tc,
                    &snapshot,
                    self.outer_cancel.clone(),
                    noop_progress,
                )
                .await
            };

            if result.is_error {
                let err_text = join_text_parts(&result.content);
                return Err(if err_text.is_empty() {
                    format!("tool `{}` returned an error", self.definition.name)
                } else {
                    err_text
                });
            }

            if let Some(structured) = result.structured_content {
                return Ok(structured);
            }

            let text = join_text_parts(&result.content);
            let trimmed = text.trim();
            if (trimmed.starts_with('{') || trimmed.starts_with('['))
                && let Ok(parsed) = serde_json::from_str::<Value>(trimmed)
            {
                return Ok(parsed);
            }
            Ok(Value::String(text))
        })
    }
}

fn join_text_parts(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text(t) => Some(t.text.as_str()),
            ContentPart::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Built-in resident `exec` tool backed by `pikra_codemode::CodemodeSandbox`.
///
/// Dynamically bridges all active resident & deferred tools (excluding `exec` itself to prevent
/// recursion) into an isolated QuickJS async VM.
pub struct CodemodeExecTool {
    resident_tools: Arc<Mutex<Vec<Arc<dyn AgentTool>>>>,
    deferred_registry: Arc<DeferredToolRegistry>,
    middlewares: Vec<Arc<dyn pikra_agent::AgentMiddleware>>,
    sandbox_options: CodemodeSandboxOptions,
    store: Arc<Mutex<BTreeMap<String, Value>>>,
}

impl CodemodeExecTool {
    pub fn new(
        resident_tools: Arc<Mutex<Vec<Arc<dyn AgentTool>>>>,
        deferred_registry: Arc<DeferredToolRegistry>,
        sandbox_options: CodemodeSandboxOptions,
    ) -> Self {
        Self {
            resident_tools,
            deferred_registry,
            middlewares: Vec::new(),
            sandbox_options,
            store: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn with_middlewares(
        mut self,
        middlewares: Vec<Arc<dyn pikra_agent::AgentMiddleware>>,
    ) -> Self {
        self.middlewares = middlewares;
        self
    }

    pub fn store_snapshot(&self) -> BTreeMap<String, Value> {
        self.store.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl AgentTool for CodemodeExecTool {
    fn definition(&self) -> ToolDefinition {
        let code_schema = with_description(
            str_schema(),
            "Async JavaScript function body. Call tools via `await tools.<name>(args)`, inspect `ALL_TOOLS`, emit output via `text(...)`/`image(...)` or `return` a value, and persist values with `store(k, v)` / `load(k)`.",
        );
        let timeout_schema = with_description(
            with_bounds(int_schema(), Some(1.0), Some(300_000.0)),
            "Optional execution timeout override in milliseconds.",
        );
        ToolDefinition {
            name: CODEMODE_EXEC_TOOL_NAME.to_string(),
            description: "Execute JavaScript in a restricted QuickJS sandbox with async access to all active resident and deferred tools under `tools.<name>(args)`."
                .to_string(),
            parameters: obj_schema([
                ("code", code_schema, true),
                ("timeout_ms", timeout_schema, false),
            ]),
        }
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    fn execute(
        &self,
        _tool_call_id: String,
        args: Value,
        cancel: CancelToken,
        _on_update: ToolProgressCallback,
    ) -> BoxFuture<'_, Result<AgentToolResult, String>> {
        Box::pin(async move {
            let code = args
                .get("code")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing required string argument `code`".to_string())?
                .to_string();
            let timeout_ms = args.get("timeout_ms").and_then(Value::as_u64);

            // Snapshot resident + deferred tools, strictly excluding `exec` itself.
            let mut bridged: Vec<Arc<dyn CodemodeTool>> = Vec::new();
            let mut seen_names: HashSet<String> = HashSet::new();
            seen_names.insert(CODEMODE_EXEC_TOOL_NAME.to_string());

            let resident_snapshot = self
                .resident_tools
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            for tool in resident_snapshot {
                let name = tool.definition().name;
                if seen_names.insert(name) {
                    bridged.push(Arc::new(AgentToolCodemodeBridge::new(
                        tool,
                        self.middlewares.clone(),
                        cancel.clone(),
                    )));
                }
            }
            for tool in self.deferred_registry.list_tools() {
                let name = tool.definition().name;
                if seen_names.insert(name) {
                    bridged.push(Arc::new(AgentToolCodemodeBridge::new(
                        tool,
                        self.middlewares.clone(),
                        cancel.clone(),
                    )));
                }
            }

            let sandbox = CodemodeSandbox::with_tools(self.sandbox_options.clone(), bridged)
                .map_err(|err| format!("failed to initialize codemode sandbox: {err}"))?;

            let abort_flag = Arc::new(AtomicBool::new(cancel.is_cancelled()));
            let store_snapshot = self.store.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let exec_opts = CodemodeExecuteOptions {
                timeout_ms,
                abort: Some(abort_flag.clone()),
                store: store_snapshot,
            };

            let mut exec_fut = Box::pin(sandbox.execute(&code, exec_opts));
            let cancel_fut = Box::pin(cancel.cancelled());
            let result = match select(exec_fut.as_mut(), cancel_fut).await {
                Either::Left((res, _)) => res,
                Either::Right(((), _)) => {
                    abort_flag.store(true, Ordering::Release);
                    exec_fut.await
                }
            };

            if result.ok && !result.store_writes.is_empty() {
                let mut store_guard = self.store.lock().unwrap_or_else(|e| e.into_inner());
                result.store_writes.apply_to(&mut store_guard);
            }

            let mut content: Vec<ContentPart> = Vec::new();
            for item in &result.output {
                match item {
                    CodemodeOutputItem::Text { text, .. } => {
                        content.push(ContentPart::Text(TextContent::plain(text.clone())));
                    }
                    CodemodeOutputItem::Image { data, mime_type } => {
                        content.push(ContentPart::Image(ImageContent {
                            data: data.clone(),
                            mime_type: mime_type.clone(),
                        }));
                    }
                }
            }

            if result.ok {
                if let Some(val) = &result.value
                    && !val.is_null()
                {
                    let rendered = match val {
                        Value::String(s) => s.clone(),
                        other => serde_json::to_string_pretty(other)
                            .unwrap_or_else(|_| other.to_string()),
                    };
                    content.push(ContentPart::Text(TextContent::plain(rendered)));
                }
                if content.is_empty() {
                    content.push(ContentPart::Text(TextContent::plain("(ok)")));
                }
                Ok(AgentToolResult {
                    content,
                    details: json!({ "calls": result.calls }),
                    structured_content: Some(json!({
                        "ok": true,
                        "value": result.value,
                        "calls": result.calls,
                    })),
                    is_error: false,
                    terminate: false,
                })
            } else {
                let err_msg = result
                    .error
                    .as_ref()
                    .map(|e| match &e.name {
                        Some(name) => format!("{name}: {}", e.message),
                        None => e.message.clone(),
                    })
                    .unwrap_or_else(|| "codemode execution failed".to_string());
                content.push(ContentPart::Text(TextContent::plain(err_msg)));
                Ok(AgentToolResult {
                    content,
                    details: json!({
                        "error": result.error,
                        "calls": result.calls,
                    }),
                    structured_content: Some(json!({
                        "ok": false,
                        "error": result.error,
                        "calls": result.calls,
                    })),
                    is_error: true,
                    terminate: false,
                })
            }
        })
    }
}
