use crate::WorkspaceToolsConfig;
use crate::find::{optional_usize, search_limit, search_result, search_root, search_walk};
use crate::util::{check_cancelled, display_path};
use grep_matcher::LineTerminator;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{
    BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkFinish, SinkMatch,
};
use ignore::overrides::OverrideBuilder;
use pikra_agent::{AgentTool, AgentToolResult, ToolProgressCallback};
use pikra_ai::{BoxFuture, CancelToken, ToolDefinition};
use pikra_schema::{
    bool_schema, int_schema, obj_schema, str_schema, string_enum, with_bounds, with_description,
};
use serde_json::{Value, json};
use std::fs::File;
use std::io::{self, Read};
use std::sync::Arc;

const MAX_CONTEXT_LINES: usize = 20;
const MAX_SNIPPET_CHARS: usize = 400;

/// Search text files with the embedded ripgrep engine.
#[derive(Clone, Debug)]
pub struct GrepTool {
    config: Arc<WorkspaceToolsConfig>,
}

impl GrepTool {
    pub fn new(config: Arc<WorkspaceToolsConfig>) -> Self {
        Self { config }
    }
}

impl AgentTool for GrepTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "grep".into(),
            description: "Search file contents by regular expression or literal text.".into(),
            parameters: obj_schema([
                (
                    "pattern",
                    with_description(str_schema(), "Regular expression or literal text to find."),
                    true,
                ),
                (
                    "path",
                    with_description(
                        str_schema(),
                        "File or directory to search. Defaults to the workspace.",
                    ),
                    false,
                ),
                (
                    "glob",
                    with_description(
                        str_schema(),
                        "Restrict paths with a glob such as '*.rs' or '!target/**'.",
                    ),
                    false,
                ),
                (
                    "case_insensitive",
                    with_description(bool_schema(), "Ignore letter case."),
                    false,
                ),
                (
                    "fixed_strings",
                    with_description(bool_schema(), "Treat the pattern as literal text."),
                    false,
                ),
                (
                    "context_lines",
                    with_description(
                        with_bounds(int_schema(), Some(0.0), Some(MAX_CONTEXT_LINES as f64)),
                        "Lines before and after each match.",
                    ),
                    false,
                ),
                (
                    "output_mode",
                    with_description(
                        string_enum(["content", "files_with_matches", "count"]),
                        "Show matching lines, file paths, or counts per file. Defaults to content.",
                    ),
                    false,
                ),
                (
                    "limit",
                    with_description(
                        with_bounds(int_schema(), Some(1.0), None),
                        "Maximum matching lines or files to return. Defaults to 200.",
                    ),
                    false,
                ),
            ]),
        }
    }

    fn execute(
        &self,
        _tool_call_id: String,
        args: Value,
        cancel: CancelToken,
        _on_update: ToolProgressCallback,
    ) -> BoxFuture<'_, Result<AgentToolResult, String>> {
        Box::pin(async move { self.search(&args, &cancel) })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    Content,
    Files,
    Count,
}

impl GrepTool {
    fn search(&self, args: &Value, cancel: &CancelToken) -> Result<AgentToolResult, String> {
        check_cancelled(cancel)?;
        let root = search_root(&self.config, args)?;
        let mut walk = search_walk(&self.config, &root, cancel);
        let overrides = args
            .get("glob")
            .map(|glob| {
                let base = if root.is_dir() {
                    &root
                } else {
                    root.parent().unwrap_or(&root)
                };
                OverrideBuilder::new(base)
                    .add(glob.as_str().ok_or("glob must be a string")?)
                    .and_then(|builder| builder.build())
                    .map_err(|err| format!("Invalid glob: {err}"))
            })
            .transpose()?;
        if let Some(overrides) = &overrides {
            walk.overrides(overrides.clone());
        }
        let limit = search_limit(args)?;
        let context = optional_usize(args, "context_lines")?.unwrap_or(0);
        if context > MAX_CONTEXT_LINES {
            return Err("context_lines must be between 0 and 20".into());
        }
        let mode_name = args
            .get("output_mode")
            .map(|value| value.as_str().ok_or("output_mode must be a string"))
            .transpose()?
            .unwrap_or("content");
        let mode = match mode_name {
            "content" => OutputMode::Content,
            "files_with_matches" => OutputMode::Files,
            "count" => OutputMode::Count,
            _ => {
                return Err(
                    "Unknown output_mode. Use content, files_with_matches, or count.".into(),
                );
            }
        };
        let context = if mode == OutputMode::Content {
            context
        } else {
            0
        };
        let pattern = args["pattern"]
            .as_str()
            .ok_or("Missing string argument 'pattern'")?;
        let matcher = RegexMatcherBuilder::new()
            .case_insensitive(args["case_insensitive"].as_bool().unwrap_or(false))
            .fixed_strings(args["fixed_strings"].as_bool().unwrap_or(false))
            .crlf(true)
            .ban_byte(Some(b'\0'))
            .build(pattern)
            .map_err(|err| format!("Invalid search pattern: {err}"))?;
        let mut searcher = SearcherBuilder::new()
            .binary_detection(BinaryDetection::quit(b'\0'))
            .line_terminator(LineTerminator::crlf())
            .before_context(context)
            .after_context(context)
            .build();

        let mut rows = Vec::new();
        let mut matches = 0;
        let mut files = 0;
        let mut hit_limit = false;
        for entry in walk.build() {
            check_cancelled(cancel)?;
            let entry = entry.map_err(|err| format!("Could not search files: {err}"))?;
            let path = entry.path();
            if !entry.file_type().is_some_and(|kind| kind.is_file())
                || (entry.depth() == 0
                    && overrides
                        .as_ref()
                        .is_some_and(|globs| globs.matched(path, false).is_ignore()))
            {
                continue;
            }

            let shown = display_path(&self.config.cwd, path);
            let mut sink = FileMatches {
                cancel,
                shown: &shown,
                mode,
                remaining: limit - matches.min(limit),
                matches: 0,
                rows: Vec::new(),
                binary: false,
                hit_limit: false,
            };
            let read = File::open(path)
                .map(|file| CancelReader { file, cancel })
                .and_then(|reader| searcher.search_reader(&matcher, reader, &mut sink));
            check_cancelled(cancel)?;
            read.map_err(|err| format!("Could not search '{shown}': {err}"))?;
            if sink.binary || (sink.matches == 0 && !sink.hit_limit) {
                continue;
            }
            if mode != OutputMode::Content && files == limit {
                hit_limit = true;
                break;
            }

            files += usize::from(sink.matches != 0);
            matches += sink.matches;
            let file_hit_limit = sink.hit_limit;
            match mode {
                OutputMode::Content => rows.extend(sink.rows),
                OutputMode::Files => rows.push(shown),
                OutputMode::Count => rows.push(format!("{shown}:{}", sink.matches)),
            }
            if file_hit_limit {
                hit_limit = true;
                break;
            }
        }
        check_cancelled(cancel)?;
        let mut result = search_result(&self.config, &root, mode_name, &rows, hit_limit);
        result.details["matches"] = json!(matches);
        result.details["files"] = json!(files);
        Ok(result)
    }
}

struct CancelReader<'a> {
    file: File,
    cancel: &'a CancelToken,
}

impl Read for CancelReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancel.is_cancelled() {
            return Err(io::Error::other("Operation aborted"));
        }
        self.file.read(buf)
    }
}

struct FileMatches<'a> {
    cancel: &'a CancelToken,
    shown: &'a str,
    mode: OutputMode,
    remaining: usize,
    matches: usize,
    rows: Vec<String>,
    binary: bool,
    hit_limit: bool,
}

impl FileMatches<'_> {
    fn push(&mut self, line_number: Option<u64>, bytes: &[u8], separator: char) {
        let line_number = line_number.expect("line numbers are enabled");
        let snippet = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_SNIPPET_CHARS * 4 + 1)]);
        let snippet = snippet.trim_end_matches(['\r', '\n']);
        let mut row = format!("{}{separator}{line_number}{separator}", self.shown);
        if let Some((end, _)) = snippet.char_indices().nth(MAX_SNIPPET_CHARS) {
            row.push_str(&snippet[..end]);
            row.push('…');
        } else {
            row.push_str(snippet);
        }
        self.rows.push(row);
    }
}

impl Sink for FileMatches<'_> {
    type Error = io::Error;

    fn matched(&mut self, _: &Searcher, matched: &SinkMatch<'_>) -> io::Result<bool> {
        if self.cancel.is_cancelled() {
            return Ok(false);
        }
        if self.mode == OutputMode::Content {
            if self.matches == self.remaining {
                self.hit_limit = true;
                return Ok(false);
            }
            self.push(matched.line_number(), matched.bytes(), ':');
        }
        self.matches += 1;
        Ok(self.mode != OutputMode::Files)
    }

    fn context(&mut self, _: &Searcher, context: &SinkContext<'_>) -> io::Result<bool> {
        if self.cancel.is_cancelled() {
            return Ok(false);
        }
        if self.matches < self.remaining
            || (self.matches > 0 && matches!(context.kind(), grep_searcher::SinkContextKind::After))
        {
            self.push(context.line_number(), context.bytes(), '-');
        }
        Ok(true)
    }

    fn finish(&mut self, _: &Searcher, finish: &SinkFinish) -> io::Result<()> {
        self.binary = finish.binary_byte_offset().is_some();
        Ok(())
    }
}
