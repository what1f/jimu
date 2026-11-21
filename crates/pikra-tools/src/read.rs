use crate::WorkspaceToolsConfig;
use crate::util::{display_path, read_regular_file, resolve_path, truncate_output};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use pikra_agent::{AgentTool, AgentToolResult, ToolExecutionMode, ToolProgressCallback};
use pikra_ai::{BoxFuture, CancelToken, ContentPart, ImageContent, ToolDefinition};
use pikra_schema::{int_schema, obj_schema, str_schema, with_bounds, with_description};
use serde_json::{Value, json};
use std::sync::Arc;

const DEFAULT_READ_LIMIT: usize = 2_000;
const MAX_LINE_CHARS: usize = 2_000;
const BINARY_SNIFF_BYTES: usize = 8192;

/// Built-in `read` tool for reading UTF-8 text files with optional 1-based line offset & limit.
#[derive(Clone, Debug)]
pub struct ReadTool {
    config: Arc<WorkspaceToolsConfig>,
}

impl ReadTool {
    pub fn new(config: Arc<WorkspaceToolsConfig>) -> Self {
        Self { config }
    }
}

impl AgentTool for ReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read".into(),
            description: "Read a UTF-8 text file or image from the workspace with optional 1-based line offset and line limit.".into(),
            parameters: obj_schema([
                (
                    "path",
                    with_description(
                        str_schema(),
                        "Path to the file to read (relative to workspace cwd or absolute).",
                    ),
                    true,
                ),
                (
                    "offset",
                    with_description(
                        with_bounds(int_schema(), Some(1.0), None),
                        "1-based starting line number (defaults to 1).",
                    ),
                    false,
                ),
                (
                    "limit",
                    with_description(
                        with_bounds(int_schema(), Some(1.0), None),
                        "Maximum number of lines to return (defaults to 2000).",
                    ),
                    false,
                ),
            ]),
        }
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
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
            let offset = match args.get("offset") {
                None | Some(Value::Null) => 1usize,
                Some(v) => v
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .filter(|&n| n >= 1)
                    .ok_or_else(|| "offset must be an integer >= 1".to_string())?,
            };
            let limit = match args.get("limit") {
                None | Some(Value::Null) => DEFAULT_READ_LIMIT,
                Some(v) => v
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .filter(|&n| n >= 1)
                    .ok_or_else(|| "limit must be an integer >= 1".to_string())?,
            };

            let resolved = resolve_path(&config.cwd, raw_path)?;
            let shown = display_path(&config.cwd, &resolved);

            let (bytes, _) = read_regular_file(&resolved, &shown)?;

            if cancel.is_cancelled() {
                return Err("Operation aborted".into());
            }

            if let Some(mime_type) = image_mime_type(&bytes) {
                let mut result = AgentToolResult::text("");
                result.content = vec![ContentPart::Image(ImageContent {
                    data: STANDARD.encode(&bytes),
                    mime_type: mime_type.into(),
                })];
                return Ok(result);
            }

            let sniff = &bytes[..bytes.len().min(BINARY_SNIFF_BYTES)];
            if sniff.contains(&0) {
                return Err(format!(
                    "cannot read binary file '{shown}' ({} bytes)",
                    bytes.len()
                ));
            }

            let text = std::str::from_utf8(&bytes).map_err(|_| {
                format!(
                    "cannot read binary or non-UTF-8 file '{shown}' ({} bytes)",
                    bytes.len()
                )
            })?;

            if text.is_empty() {
                let mut res = AgentToolResult::text("(empty file)");
                res.details = json!({
                    "path": shown,
                    "offset": offset,
                    "limit": limit,
                    "total_lines": 0,
                    "returned_lines": 0,
                    "truncated": false,
                });
                return Ok(res);
            }

            let lines: Vec<&str> = text.lines().collect();
            let total_lines = lines.len();

            if offset > total_lines {
                let mut res = AgentToolResult::text(format!(
                    "Offset {offset} is beyond end of file '{shown}' ({total_lines} lines)."
                ));
                res.details = json!({
                    "path": shown,
                    "offset": offset,
                    "limit": limit,
                    "total_lines": total_lines,
                    "returned_lines": 0,
                    "truncated": false,
                });
                return Ok(res);
            }

            let start_idx = offset - 1;
            let effective_limit = limit.min(config.max_output_lines.max(1));
            let end_idx = start_idx.saturating_add(effective_limit).min(total_lines);
            let slice = &lines[start_idx..end_idx];

            let footer_reserve = format!(
                "\n[Showing lines {offset}-{total_lines} of {total_lines}. Use offset={} to read more.]",
                total_lines + 1
            )
            .len();
            let stream_budget = config.max_output_bytes.saturating_sub(footer_reserve);

            let mut formatted = String::new();
            let mut shown_count = 0usize;
            let mut raw_consumed_bytes = 0usize;
            let mut hit_byte_budget = false;
            let mut any_line_clipped = false;

            for (i, line) in slice.iter().enumerate() {
                let line_no = offset + i;
                let clean_line = line.strip_suffix('\r').unwrap_or(line);
                let formatted_line = if clean_line.len() > MAX_LINE_CHARS
                    && clean_line.chars().nth(MAX_LINE_CHARS).is_some()
                {
                    any_line_clipped = true;
                    let total_chars = clean_line.chars().count();
                    let cut = clean_line
                        .char_indices()
                        .nth(MAX_LINE_CHARS)
                        .map(|(idx, _)| idx)
                        .unwrap_or(clean_line.len());
                    format!(
                        "{line_no}: {} ... [line truncated, {total_chars} chars]",
                        &clean_line[..cut]
                    )
                } else {
                    format!("{line_no}: {clean_line}")
                };

                let sep = usize::from(shown_count > 0);
                if shown_count > 0
                    && stream_budget > 0
                    && formatted.len() + sep + formatted_line.len() > stream_budget
                {
                    hit_byte_budget = true;
                    break;
                }
                if shown_count > 0 {
                    formatted.push('\n');
                }
                formatted.push_str(&formatted_line);
                raw_consumed_bytes += clean_line.len() + 1;
                shown_count += 1;
            }

            let shown_end = offset + shown_count.saturating_sub(1);
            let lines_truncated = shown_end < total_lines;
            if lines_truncated && shown_count > 0 {
                let next_offset = shown_end + 1;
                formatted.push_str(&format!(
                    "\n[Showing lines {offset}-{shown_end} of {total_lines}. Use offset={next_offset} to read more.]"
                ));
            }

            let trunc = truncate_output(&formatted, config.max_output_bytes);
            let omitted_bytes = if trunc.truncated {
                trunc.omitted_bytes
            } else if lines_truncated || any_line_clipped {
                text.len().saturating_sub(raw_consumed_bytes)
            } else {
                0
            };
            let mut res = AgentToolResult::text(trunc.text);
            res.details = json!({
                "path": shown,
                "offset": offset,
                "limit": limit,
                "total_lines": total_lines,
                "returned_lines": shown_count,
                "truncated": lines_truncated || hit_byte_budget || any_line_clipped || trunc.truncated,
                "omitted_bytes": omitted_bytes,
            });
            Ok(res)
        })
    }
}

fn image_mime_type(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', b'7' | b'9', b'a', ..] => Some("image/gif"),
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => Some("image/webp"),
        _ => None,
    }
}
