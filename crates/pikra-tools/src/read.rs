use crate::WorkspaceToolsConfig;
use crate::util::{display_path, read_regular_file, resolve_path, truncate_output};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use pikra_agent::{AgentTool, AgentToolResult, ToolExecutionMode, ToolProgressCallback};
use pikra_ai::{BoxFuture, CancelToken, ContentPart, ImageContent, ToolDefinition};
use pikra_schema::{int_schema, obj_schema, str_schema, with_bounds, with_description};
use serde_json::{Value, json};
use std::sync::Arc;

const DEFAULT_READ_LIMIT: usize = 800;
const BINARY_SNIFF_BYTES: usize = 8192;

/// Built-in `read` tool for reading UTF-8 text files with optional 1-based line offset & limit.
#[derive(Clone, Debug)]
pub struct ReadTool {
    config: Arc<WorkspaceToolsConfig>,
    plain_text: bool,
}

impl ReadTool {
    pub fn new(config: Arc<WorkspaceToolsConfig>) -> Self {
        Self {
            config,
            plain_text: false,
        }
    }

    /// Return file text verbatim, or return the requested line slice without annotations.
    pub fn with_plain_text(mut self) -> Self {
        self.plain_text = true;
        self
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
                        "Maximum number of lines to return (defaults to 800).",
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
        let plain_text = self.plain_text;
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

            if plain_text {
                if args["offset"].is_null() && args["limit"].is_null() {
                    return Ok(AgentToolResult::text(text));
                }
                let limit = if args["limit"].is_null() {
                    usize::MAX
                } else {
                    limit
                };
                let lines = text
                    .split_inclusive('\n')
                    .map(|line| match line.strip_suffix('\n') {
                        Some(line) => line.strip_suffix('\r').unwrap_or(line),
                        None => line,
                    })
                    .chain(text.ends_with('\n').then_some(""));
                let slice = lines.skip(offset - 1).take(limit).collect::<Vec<_>>();
                return Ok(AgentToolResult::text(slice.join("\n")));
            }

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
            let end_idx = start_idx.saturating_add(limit).min(total_lines);
            let slice = &lines[start_idx..end_idx];

            let mut formatted = String::new();
            for (i, line) in slice.iter().enumerate() {
                if i > 0 {
                    formatted.push('\n');
                }
                let line_no = offset + i;
                let clean_line = line.strip_suffix('\r').unwrap_or(line);
                formatted.push_str(&format!("{line_no}: {clean_line}"));
            }

            let lines_truncated = end_idx < total_lines;
            if lines_truncated {
                let next_offset = end_idx + 1;
                formatted.push_str(&format!(
                    "\n[Showing lines {offset}-{end_idx} of {total_lines}. Use offset={next_offset} to read more.]"
                ));
            }

            let trunc = truncate_output(&formatted, config.max_output_bytes);
            let mut res = AgentToolResult::text(trunc.text);
            res.details = json!({
                "path": shown,
                "offset": offset,
                "limit": limit,
                "total_lines": total_lines,
                "returned_lines": slice.len(),
                "truncated": lines_truncated || trunc.truncated,
                "omitted_bytes": trunc.omitted_bytes,
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
