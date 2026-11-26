use crate::WorkspaceToolsConfig;
use crate::image_util::{image_format, process_image_bytes};
use crate::util::{
    check_cancelled, display_path, open_regular_file, read_bounded_bytes, resolve_path,
};
use jimu_agent::{AgentTool, AgentToolResult, ToolExecutionMode, ToolProgressCallback};
use jimu_ai::{BoxFuture, CancelToken, ContentPart, ToolDefinition};
use jimu_schema::{int_schema, obj_schema, str_schema, with_bounds, with_description};
use serde_json::{Value, json};
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;
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
    /// Probe a local regular file before routing a read to a text-only client.
    pub fn is_image_file(path: &Path) -> bool {
        let mut header = [0; 12];
        open_regular_file(path, &path.to_string_lossy()).is_ok_and(|(mut file, _)| {
            file.read(&mut header)
                .is_ok_and(|n| image_format(&header[..n]).is_some())
        })
    }

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

            let (file, meta) = open_regular_file(&resolved, &shown)?;
            let mut reader = BufReader::with_capacity(BINARY_SNIFF_BYTES, file);
            let sniff = reader
                .fill_buf()
                .map_err(|err| format!("Could not read '{shown}': {err}"))?;
            if image_format(sniff).is_some() {
                let bytes = read_bounded_bytes(reader, meta.len(), &shown)?;
                check_cancelled(&cancel)?;
                let image = process_image_bytes(&bytes, &config.image)?;
                let mut result = AgentToolResult::text(image.note());
                result.details = json!({
                    "path": shown, "mime_type": image.content.mime_type,
                    "width": image.size.0, "height": image.size.1,
                    "original_width": image.original.0, "original_height": image.original.1,
                    "resized": image.size != image.original,
                });
                result.content.push(ContentPart::Image(image.content));
                return Ok(result);
            }
            if sniff.contains(&0) {
                return Err(format!("Cannot read binary file '{shown}'"));
            }
            read_text(reader, &shown, offset, limit, &config, &cancel)
        })
    }
}

// Read even a newline-free file in bounded pieces. Keep only the requested line prefix.
fn read_line_prefix(
    reader: &mut impl BufRead,
    chunk: &mut Vec<u8>,
    prefix: &mut String,
    capture: bool,
    cancel: &CancelToken,
) -> io::Result<Option<(usize, usize)>> {
    prefix.clear();
    chunk.clear();
    let mut bytes = 0;
    let mut chars = 0;
    let mut last_cr = false;
    loop {
        if cancel.is_cancelled() {
            return Err(io::Error::other("Operation aborted"));
        }
        let count = reader
            .take(BINARY_SNIFF_BYTES as u64)
            .read_until(b'\n', chunk)?;
        bytes += count;
        if bytes == 0 {
            return Ok(None);
        }
        let newline = chunk.last() == Some(&b'\n');
        if newline {
            chunk.pop();
        }
        let valid = match std::str::from_utf8(chunk) {
            Ok(text) => text,
            Err(e) if e.error_len().is_none() && !newline && count != 0 => {
                std::str::from_utf8(&chunk[..e.valid_up_to()]).unwrap()
            }
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "file is not UTF-8",
                ));
            }
        };
        if !valid.is_empty() {
            last_cr = valid.ends_with('\r');
        }
        if capture && chars < MAX_LINE_CHARS {
            prefix.extend(valid.chars().take(MAX_LINE_CHARS - chars));
        }
        chars += valid.chars().count();
        if newline || count == 0 {
            if last_cr {
                if capture && chars <= MAX_LINE_CHARS {
                    prefix.pop();
                }
                chars -= 1;
            }
            return Ok(Some((bytes, chars)));
        }
        chunk.drain(..valid.len());
    }
}

fn read_text(
    mut reader: impl BufRead,
    shown: &str,
    offset: usize,
    limit: usize,
    config: &WorkspaceToolsConfig,
    cancel: &CancelToken,
) -> Result<AgentToolResult, String> {
    let max = config.max_output_bytes;
    let max_lines = limit.min(config.max_output_lines.max(1));
    let mut output = String::new();
    let mut ends = Vec::new();
    let mut total_bytes = 0;
    let mut total_lines = 0;
    let mut clipped = false;
    let mut full = false;
    let mut chunk = Vec::new();
    let mut prefix = String::new();
    loop {
        let line_no = total_lines + 1;
        let capture = line_no >= offset && ends.len() < max_lines && !full;
        let Some((bytes, chars)) =
            read_line_prefix(&mut reader, &mut chunk, &mut prefix, capture, cancel)
                .map_err(|err| format!("Could not read '{shown}': {err}"))?
        else {
            break;
        };
        total_lines = line_no;
        total_bytes += bytes;
        if !capture {
            continue;
        }
        let mut line = format!("{line_no}: {prefix}");
        let long = chars > MAX_LINE_CHARS;
        if long {
            line.push_str(&format!(" ... [line truncated, {chars} chars]"));
        }
        let separator = usize::from(!ends.is_empty());
        if line.len() + separator > max.saturating_sub(output.len()) {
            full = true;
            if !ends.is_empty() {
                continue;
            }
        }
        if separator != 0 {
            output.push('\n');
        }
        output.push_str(&line);
        ends.push((output.len(), if long { prefix.len() } else { bytes }));
        clipped |= long;
    }

    let mut omitted = total_bytes;
    if ends.is_empty() {
        output = if total_lines == 0 {
            "(empty file)".into()
        } else {
            format!("Offset {offset} is beyond end of file '{shown}' ({total_lines} lines).")
        };
    } else {
        // Only now do we know the exact footer length. Remove whole lines first.
        loop {
            let last = offset + (ends.len() - 1);
            let footer = if last < total_lines {
                format!(
                    "\n[Showing lines {offset}-{last} of {total_lines}. Use offset={} to read more.]",
                    last + 1
                )
            } else {
                String::new()
            };
            if output.len() + footer.len() <= max {
                output.push_str(&footer);
                break;
            }
            if ends.len() > 1 {
                ends.pop();
                output.truncate(ends.last().unwrap().0);
                continue;
            }
            // Preserve a whole first line when the continuation note alone would overflow.
            if output.len() <= max {
                break;
            }
            let hint = format!(" [{max}-byte limit]");
            let number_len = offset.to_string().len() + 2;
            let hint = if number_len + hint.len() < max {
                hint.as_str()
            } else {
                ""
            };
            let footer = if number_len + footer.len() + hint.len() < max {
                &footer
            } else {
                ""
            };
            let cut = output.floor_char_boundary(max - footer.len() - hint.len());
            output.truncate(cut);
            ends[0].1 = ends[0].1.min(cut.saturating_sub(number_len));
            output.push_str(hint);
            output.push_str(footer);
            clipped = true;
            break;
        }
        omitted = omitted.saturating_sub(ends.iter().map(|&(_, bytes)| bytes).sum());
    }
    let returned = ends.len();
    let truncated = clipped || (!ends.is_empty() && offset + (returned - 1) < total_lines);
    let mut result = AgentToolResult::text(output);
    result.details = json!({ "path": shown, "offset": offset, "limit": limit,
        "total_lines": total_lines, "returned_lines": returned, "truncated": truncated,
        "omitted_bytes": if truncated { omitted } else { 0 } });
    if truncated {
        result.details["max_output_bytes"] = json!(max);
        if offset + (returned - 1) < total_lines {
            result.details["next_offset"] = json!(offset + returned);
        }
    }
    Ok(result)
}
