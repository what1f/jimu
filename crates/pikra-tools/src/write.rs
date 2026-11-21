use crate::WorkspaceToolsConfig;
use crate::util::{
    atomic_write_file_resolved, check_cancelled, display_path, path_lock, resolve_path,
    resolve_write_target,
};
use pikra_agent::{AgentTool, AgentToolResult, ToolExecutionMode, ToolProgressCallback};
use pikra_ai::{BoxFuture, CancelToken, ToolDefinition};
use pikra_schema::{obj_schema, str_schema, with_description};
use serde_json::{Value, json};
use std::sync::Arc;

/// Built-in `write` tool that creates parent directories and writes files atomically.
#[derive(Clone, Debug)]
pub struct WriteTool {
    config: Arc<WorkspaceToolsConfig>,
}

impl WriteTool {
    pub fn new(config: Arc<WorkspaceToolsConfig>) -> Self {
        Self { config }
    }
}

impl AgentTool for WriteTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write".into(),
            description:
                "Write full UTF-8 content to a file, creating parent directories automatically."
                    .into(),
            parameters: obj_schema([
                (
                    "path",
                    with_description(
                        str_schema(),
                        "Path to the file to write (relative to workspace cwd or absolute).",
                    ),
                    true,
                ),
                (
                    "content",
                    with_description(str_schema(), "Full text content to write to the file."),
                    true,
                ),
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
        let config = Arc::clone(&self.config);
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err("Operation aborted".into());
            }

            let raw_path = args
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing required string argument: path".to_string())?;
            let content = args
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing required string argument: content".to_string())?;

            let resolved = resolve_path(&config.cwd, raw_path)?;
            let shown = display_path(&config.cwd, &resolved);
            let target = resolve_write_target(&resolved)?;
            let lock = path_lock(&target);
            let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            check_cancelled(&cancel)?;
            if resolve_write_target(&resolved)? != target {
                return Err(format!(
                    "'{shown}' changed during the operation. Try again."
                ));
            }
            atomic_write_file_resolved(&target, content.as_bytes())?;

            let mut res =
                AgentToolResult::text(format!("Wrote {} bytes to {shown}", content.len()));
            res.details = json!({
                "path": shown,
                "bytes": content.len(),
            });
            Ok(res)
        })
    }
}
