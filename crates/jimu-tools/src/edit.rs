use crate::WorkspaceToolsConfig;
use crate::seek_sequence::seek_sequence;
use crate::util::{
    atomic_write_file_resolved, check_cancelled, display_path, path_lock, read_editable_text,
    resolve_path, resolve_write_target, uses_crlf,
};
use jimu_agent::{AgentTool, AgentToolResult, ToolExecutionMode, ToolProgressCallback};
use jimu_ai::{BoxFuture, CancelToken, ToolDefinition};
use jimu_schema::{arr_schema, obj_schema, str_schema, with_description, with_items_bounds};
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

fn parse_hunks(args: &Value) -> Result<Vec<(&str, &str)>, String> {
    fn parse(item: &Value) -> Result<(&str, &str), String> {
        let old = item
            .get("oldText")
            .and_then(Value::as_str)
            .ok_or_else(|| "Missing string argument 'oldText'".to_owned())?;
        let new = item
            .get("newText")
            .and_then(Value::as_str)
            .ok_or_else(|| "Missing string argument 'newText'".to_owned())?;
        if old.is_empty() {
            return Err("oldText must not be empty".to_owned());
        }
        Ok((old, new))
    }
    let Some(edits) = args.get("edits") else {
        return Ok(vec![parse(args)?]);
    };
    if ["oldText", "newText"]
        .iter()
        .any(|key| args.get(key).is_some())
    {
        return Err("Use one replacement or an edits list".into());
    }
    let items = edits
        .as_array()
        .filter(|items| !items.is_empty())
        .ok_or("edits must contain at least one edit")?;
    items
        .iter()
        .enumerate()
        .map(|(i, item)| parse(item).map_err(|err| format!("Edit #{}: {err}", i + 1)))
        .collect()
}

fn replacement_range(text: &str, old: &str) -> Result<std::ops::Range<usize>, String> {
    if let Some(start) = text.find(old) {
        if text[start + old.ceil_char_boundary(1)..].contains(old) {
            return Err(
                "oldText matches multiple locations. Include more surrounding text.".into(),
            );
        }
        return Ok(start..start + old.len());
    }

    let lines: Vec<_> = text.lines().collect();
    let pattern: Vec<_> = old.lines().collect();
    let index = seek_sequence(&lines, &pattern, true)
        .map_err(|()| "oldText matches multiple locations. Include more surrounding text.")?
        .ok_or("oldText not found")?;
    let first = lines[index].as_ptr() as usize - text.as_ptr() as usize;
    let last = lines[index + pattern.len() - 1];
    let mut end = last.as_ptr() as usize - text.as_ptr() as usize + last.len();
    if old.ends_with('\n') {
        let suffix = &text[end..];
        end += if suffix.starts_with("\r\n") {
            2
        } else {
            usize::from(suffix.starts_with('\n'))
        };
    }
    Ok(first..end)
}

impl AgentTool for EditTool {
    fn definition(&self) -> ToolDefinition {
        let edit_item_schema = obj_schema([
            (
                "oldText",
                with_description(str_schema(), "Text to find and replace."),
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
            description: "Replace text in a UTF-8 file. Include enough surrounding text to identify each edit.".into(),
            parameters: obj_schema([
                ("path", with_description(str_schema(), "Path to the file to edit."), true),
                (
                    "oldText",
                    with_description(str_schema(), "Text to find and replace."),
                    false,
                ),
                (
                    "newText",
                    with_description(str_schema(), "Replacement text."),
                    false,
                ),
                (
                    "edits",
                    with_description(
                        with_items_bounds(arr_schema(edit_item_schema), Some(1), None),
                        "Apply all of these replacements as one edit.",
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
            let target = resolve_write_target(&resolved)?;
            let lock = path_lock(&target);
            let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            check_cancelled(&cancel)?;
            if resolve_write_target(&resolved)? != target {
                return Err(format!(
                    "'{shown}' changed during the operation. Try again."
                ));
            }

            let (mut working, _) = read_editable_text(&target, &shown)?;
            let is_crlf = uses_crlf(&working);
            for (idx, (old, new)) in hunks.iter().enumerate() {
                check_cancelled(&cancel)?;
                let (old, new) = if is_crlf {
                    (
                        old.replace("\r\n", "\n").replace('\n', "\r\n"),
                        new.replace("\r\n", "\n").replace('\n', "\r\n"),
                    )
                } else {
                    (old.to_string(), new.replace("\r\n", "\n"))
                };
                let range = replacement_range(&working, &old)
                    .map_err(|err| format!("Edit #{} in '{shown}': {err}", idx + 1))?;
                working.replace_range(range, &new);
            }
            check_cancelled(&cancel)?;
            atomic_write_file_resolved(&target, working.as_bytes())?;

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
