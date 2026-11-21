use crate::WorkspaceToolsConfig;
use crate::util::{atomic_write_file, display_path, resolve_path};
use pikra_agent::{AgentTool, AgentToolResult, ToolExecutionMode, ToolProgressCallback};
use pikra_ai::{BoxFuture, CancelToken, ToolDefinition};
use pikra_schema::{arr_schema, obj_schema, str_schema, with_description, with_items_bounds};
use serde_json::{Value, json};
use std::sync::Arc;

/// Built-in `edit` tool supporting single (`oldText`/`newText`) and atomic multi-hunk (`edits`) replacements.
#[derive(Clone, Debug)]
pub struct EditTool {
    config: Arc<WorkspaceToolsConfig>,
}

impl EditTool {
    pub fn new(config: Arc<WorkspaceToolsConfig>) -> Self {
        Self { config }
    }
}

struct EditHunk {
    old_text: String,
    new_text: String,
}

fn parse_hunks(args: &Value) -> Result<Vec<EditHunk>, String> {
    let old_text = args.get("oldText").and_then(Value::as_str);
    let new_text = args.get("newText").and_then(Value::as_str);
    let edits_arr = args.get("edits").and_then(Value::as_array);

    let has_single = old_text.is_some() || new_text.is_some();
    let has_multi = edits_arr.is_some();

    if has_single && has_multi {
        return Err("provide either oldText/newText or edits, not both".to_string());
    }

    if let Some(items) = edits_arr {
        if items.is_empty() {
            return Err("edits must contain at least one edit".to_string());
        }
        let mut hunks = Vec::with_capacity(items.len());
        for (idx, item) in items.iter().enumerate() {
            let old = item
                .get("oldText")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("edits[{idx}].oldText must be a string"))?;
            let new = item
                .get("newText")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("edits[{idx}].newText must be a string"))?;
            if old.is_empty() {
                return Err(format!("edits[{idx}].oldText must not be empty"));
            }
            hunks.push(EditHunk {
                old_text: old.to_string(),
                new_text: new.to_string(),
            });
        }
        return Ok(hunks);
    }

    match (old_text, new_text) {
        (Some(old), Some(new)) => {
            if old.is_empty() {
                return Err("oldText must not be empty".to_string());
            }
            Ok(vec![EditHunk {
                old_text: old.to_string(),
                new_text: new.to_string(),
            }])
        }
        (Some(_), None) => Err("missing required string argument: newText".to_string()),
        (None, Some(_)) => Err("missing required string argument: oldText".to_string()),
        (None, None) => Err("provide either oldText and newText, or edits".to_string()),
    }
}

fn has_bare_lf(s: &str) -> bool {
    let bytes = s.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' && (i == 0 || bytes[i - 1] != b'\r') {
            return true;
        }
    }
    false
}

impl AgentTool for EditTool {
    fn definition(&self) -> ToolDefinition {
        let edit_item_schema = obj_schema([
            (
                "oldText",
                with_description(str_schema(), "Exact text to find and replace."),
                true,
            ),
            (
                "newText",
                with_description(str_schema(), "Replacement text."),
                true,
            ),
        ]);

        ToolDefinition {
            name: "edit".into(),
            description: "Apply exact text replacement(s) to an existing UTF-8 file atomically while preserving line endings.".into(),
            parameters: obj_schema([
                (
                    "path",
                    with_description(
                        str_schema(),
                        "Path to the file to edit (relative to workspace cwd or absolute).",
                    ),
                    true,
                ),
                (
                    "oldText",
                    with_description(
                        str_schema(),
                        "Exact text to find and replace (for single-hunk edit).",
                    ),
                    false,
                ),
                (
                    "newText",
                    with_description(
                        str_schema(),
                        "Replacement text (for single-hunk edit).",
                    ),
                    false,
                ),
                (
                    "edits",
                    with_description(
                        with_items_bounds(arr_schema(edit_item_schema), Some(1), None),
                        "Ordered list of atomic exact-match edits to apply to the file.",
                    ),
                    false,
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
            let hunks = parse_hunks(&args)?;

            let resolved = resolve_path(&config.cwd, raw_path)?;
            let shown = display_path(&config.cwd, &resolved);

            let meta = tokio::fs::metadata(&resolved)
                .await
                .map_err(|e| format!("failed to stat '{shown}': {e}"))?;
            if meta.is_dir() {
                return Err(format!("cannot edit '{shown}': path is a directory"));
            }
            if !meta.is_file() {
                return Err(format!("cannot edit '{shown}': path is not a regular file"));
            }
            if meta.len() > 16 * 1024 * 1024 {
                return Err(format!(
                    "cannot edit '{shown}': file size ({} bytes) exceeds 16 MiB limit",
                    meta.len()
                ));
            }

            let bytes = tokio::fs::read(&resolved)
                .await
                .map_err(|e| format!("failed to read '{shown}': {e}"))?;
            if bytes.iter().take(8192).any(|&b| b == 0) {
                return Err(format!("cannot edit binary file '{shown}'"));
            }
            let original = String::from_utf8(bytes)
                .map_err(|_| format!("cannot edit non-UTF-8 file '{shown}'"))?;

            let is_crlf = original.contains("\r\n") && !has_bare_lf(&original);
            let mut working = if is_crlf {
                original.replace("\r\n", "\n")
            } else {
                original.clone()
            };

            for (idx, hunk) in hunks.iter().enumerate() {
                let edit_num = idx + 1;
                let (old_norm, new_norm) = if is_crlf {
                    (
                        hunk.old_text.replace("\r\n", "\n"),
                        hunk.new_text.replace("\r\n", "\n"),
                    )
                } else {
                    (hunk.old_text.clone(), hunk.new_text.clone())
                };

                let matches = working.matches(&old_norm).count();
                if matches == 0 {
                    return Err(format!(
                        "edit #{edit_num} failed: oldText not found in '{shown}'. Ensure oldText matches the file exactly, including whitespace."
                    ));
                }
                if matches > 1 {
                    return Err(format!(
                        "edit #{edit_num} failed: oldText matched {matches} times in '{shown}'. Include more surrounding context to make the match unique."
                    ));
                }

                working = working.replacen(&old_norm, &new_norm, 1);
            }

            let final_content = if is_crlf {
                working.replace('\n', "\r\n")
            } else {
                working
            };

            atomic_write_file(&resolved, final_content.as_bytes()).await?;

            let count = hunks.len();
            let mut res = AgentToolResult::text(format!(
                "Applied {count} {} to {shown}",
                if count == 1 { "edit" } else { "edits" }
            ));
            res.details = json!({
                "path": shown,
                "edits_applied": count,
                "line_ending": if is_crlf { "crlf" } else { "lf" },
            });
            Ok(res)
        })
    }
}
