use crate::WorkspaceToolsConfig;
use crate::seek_sequence::{Search, seek_sequence, split_bom};
use crate::util::{
    atomic_write_file_resolved, check_cancelled, display_path, path_lock, read_editable_text,
    resolve_path, resolve_write_target, truncate_output, uses_crlf,
};
use jimu_agent::{AgentTool, AgentToolResult, ToolExecutionMode, ToolProgressCallback};
use jimu_ai::{BoxFuture, CancelToken, ToolDefinition};
use jimu_schema::{arr_schema, obj_schema, str_schema, with_description, with_items_bounds};
use serde_json::{Value, json};
use std::fmt::Write;
use std::ops::Range;
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

fn replacement_range(text: &str, old: &str) -> Result<Range<usize>, String> {
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
    let index = seek_sequence(&lines, &pattern, Search::Unique)
        .map_err(|()| "oldText matches multiple locations. Include more surrounding text.")?
        .ok_or("oldText not found")?;
    let first = lines[index].as_ptr() as usize - text.as_ptr() as usize;
    let last = lines[index + pattern.len() - 1];
    let mut end = last.as_ptr() as usize - text.as_ptr() as usize + last.len();
    if old.ends_with('\n') && text[end..].starts_with('\n') {
        end += 1;
    }
    Ok(first..end)
}

// Leave identical context on disk, including its original line endings.
fn changed_parts<'a>(mut old: &'a str, mut new: &'a str) -> (usize, &'a str, &'a str) {
    let prefix: usize = old
        .split_inclusive('\n')
        .zip(new.split_inclusive('\n'))
        .take_while(|(a, b)| a == b)
        .map(|(line, _)| line.len())
        .sum();
    old = &old[prefix..];
    new = &new[prefix..];
    let suffix: usize = old
        .split_inclusive('\n')
        .rev()
        .zip(new.split_inclusive('\n').rev())
        .take_while(|(a, b)| a == b)
        .map(|(line, _)| line.len())
        .sum();
    (
        prefix,
        &old[..old.len() - suffix],
        &new[..new.len() - suffix],
    )
}

fn push_replacement(output: &mut String, raw: &str, range: Range<usize>, new: &str) {
    if !new.contains('\n') {
        output.push_str(new);
        return;
    }
    let start = range.start;
    let nearest_lf = raw[start..]
        .find('\n')
        .map(|i| start + i)
        .or_else(|| raw[..start].rfind('\n'));
    let local_crlf = nearest_lf.is_some_and(|lf| raw[..lf].ends_with('\r'));

    // Reuse the replaced line endings in order. Extra lines inherit the local style.
    let mut endings = raw[range].match_indices('\n');
    for part in new.split_inclusive('\n') {
        if let Some(body) = part.strip_suffix('\n') {
            output.push_str(body);
            let crlf = endings
                .next()
                .map_or(local_crlf, |(lf, _)| raw[..start + lf].ends_with('\r'));
            output.push_str(if crlf { "\r\n" } else { "\n" });
        } else {
            output.push_str(part);
        }
    }
}

fn apply_edits(
    source: &str,
    hunks: &[(&str, &str)],
    shown: &str,
    cancel: &CancelToken,
) -> Result<(String, String), String> {
    let (bom, raw) = split_bom(source);
    let text = raw.replace("\r\n", "\n");
    let mut edits = Vec::with_capacity(hunks.len());
    for (i, (old, new)) in hunks.iter().enumerate() {
        check_cancelled(cancel)?;
        let old = old.replace("\r\n", "\n");
        let new = new.replace("\r\n", "\n");
        if old == new {
            return Err(format!(
                "No change in edit #{}: oldText and newText are identical",
                i + 1
            ));
        }
        let range = replacement_range(&text, &old)
            .map_err(|err| format!("Edit #{} in '{shown}': {err}", i + 1))?;
        edits.push((range, new));
    }
    edits.sort_unstable_by_key(|(range, _)| range.start);
    if edits.windows(2).any(|pair| pair[0].0.end > pair[1].0.start) {
        return Err(format!(
            "Edits overlap in '{shown}'. Merge them into one edit."
        ));
    }

    // Each CRLF before a normalized offset adds one extra byte in the source.
    let mut crlfs = raw.match_indices("\r\n").peekable();
    let mut extra = 0;
    let mut raw_offset = |offset| {
        while crlfs.next_if(|(i, _)| *i < offset + extra).is_some() {
            extra += 1;
        }
        offset + extra
    };
    let mut output = String::with_capacity(source.len());
    output.push_str(bom);
    let mut diff = String::new();
    let mut cursor = 0;
    let mut line = 1;
    for (range, new) in edits {
        let (prefix, old, new) = changed_parts(&text[range.clone()], &new);
        let start = raw_offset(range.start + prefix);
        let end = raw_offset(range.start + prefix + old.len());
        output.push_str(&raw[cursor..start]);
        line += raw[cursor..start].matches('\n').count();
        if !old.is_empty() || !new.is_empty() {
            writeln!(diff, "@@ line {line} @@").unwrap();
            for (sign, part) in [('-', old), ('+', new)] {
                for fragment in part.lines() {
                    writeln!(diff, "{sign}{fragment}").unwrap();
                }
                if end == raw.len() && !part.is_empty() && !part.ends_with('\n') {
                    diff.push_str("\\ No newline at end of file\n");
                }
            }
        }

        push_replacement(&mut output, raw, start..end, new);
        line += old.matches('\n').count();
        cursor = end;
    }
    output.push_str(&raw[cursor..]);
    if output == source {
        return Err(format!("No changes made to '{shown}'"));
    }
    Ok((output, diff))
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
                        "Replace non-overlapping regions of the original file together.",
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

            let (original, _) = read_editable_text(&target, &shown)?;
            let (working, diff) = apply_edits(&original, &hunks, &shown, &cancel)?;
            let first_changed_line = original
                .split_inclusive('\n')
                .zip(working.split_inclusive('\n'))
                .take_while(|(a, b)| a == b)
                .count()
                + 1;
            check_cancelled(&cancel)?;
            atomic_write_file_resolved(&target, working.as_bytes())?;

            let count = hunks.len();
            let summary = format!(
                "Applied {count} {} to {shown}",
                if count == 1 { "edit" } else { "edits" }
            );
            let budget = config.max_output_bytes;
            let diff = truncate_output(
                diff.trim_end_matches('\n'),
                budget.saturating_sub(summary.len() + 1),
            )
            .text;
            let mut content = truncate_output(&summary, budget).text;
            if !diff.is_empty() {
                content.push('\n');
                content.push_str(&diff);
            }
            let mut res = AgentToolResult::text(content);
            res.details = json!({
                "path": shown,
                "edits_applied": count,
                "line_ending": if uses_crlf(&original) { "crlf" } else if original.contains("\r\n") { "mixed" } else { "lf" },
                "first_changed_line": first_changed_line,
                "diff": diff,
            });
            Ok(res)
        })
    }
}
