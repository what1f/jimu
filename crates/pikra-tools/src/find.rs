use crate::WorkspaceToolsConfig;
use crate::util::{display_path, resolve_path, truncate_output};
use pikra_agent::{AgentTool, AgentToolResult, ToolExecutionMode, ToolProgressCallback};
use pikra_ai::{BoxFuture, CancelToken, ToolDefinition};
use pikra_schema::{int_schema, obj_schema, str_schema, with_bounds, with_description};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const DEFAULT_FIND_LIMIT: usize = 200;
const MAX_SEARCH_FILE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_LINE_SNIPPET_CHARS: usize = 400;

/// Built-in pure-Rust `find` tool for recursive workspace glob filename and substring content search.
#[derive(Clone, Debug)]
pub struct FindTool {
    config: Arc<WorkspaceToolsConfig>,
}

impl FindTool {
    pub fn new(config: Arc<WorkspaceToolsConfig>) -> Self {
        Self { config }
    }
}

impl AgentTool for FindTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "find".into(),
            description: "Recursively search workspace files by glob filename pattern or substring content match (skipping .git, node_modules, target).".into(),
            parameters: obj_schema([
                (
                    "pattern",
                    with_description(
                        str_schema(),
                        "Glob filename pattern (e.g. '*.rs', 'src/**/*.ts') or literal substring to search in file paths and contents.",
                    ),
                    true,
                ),
                (
                    "path",
                    with_description(
                        str_schema(),
                        "Directory or file path to search within (defaults to workspace cwd).",
                    ),
                    false,
                ),
                (
                    "limit",
                    with_description(
                        with_bounds(int_schema(), Some(1.0), None),
                        "Maximum number of matches to return (defaults to 200).",
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

            let pattern = args
                .get("pattern")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing required string argument: pattern".to_string())?
                .to_string();
            if pattern.is_empty() {
                return Err("pattern must not be empty".to_string());
            }

            let search_root = match args.get("path").and_then(Value::as_str) {
                Some(p) => resolve_path(&config.cwd, p)?,
                None => config.cwd.clone(),
            };
            let limit = match args.get("limit") {
                None | Some(Value::Null) => DEFAULT_FIND_LIMIT,
                Some(v) => v
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .filter(|&n| n >= 1)
                    .ok_or_else(|| "limit must be an integer >= 1".to_string())?,
            };

            let shown_root = display_path(&config.cwd, &search_root);
            let cwd = config.cwd.clone();
            let ignore_dirs: HashSet<String> = config.ignore_dirs.iter().cloned().collect();
            let max_output_bytes = config.max_output_bytes;
            let cancel_clone = cancel.clone();

            let outcome = tokio::task::spawn_blocking(move || {
                run_find(&cwd, &search_root, &pattern, limit, &ignore_dirs, &cancel_clone)
            })
            .await
            .map_err(|e| format!("find task failed: {e}"))??;
            let formatted = if outcome.matches.is_empty() {
                "No matches found.".to_string()
            } else {
                let mut text = outcome.matches.join("\n");
                if outcome.hit_limit {
                    text.push_str(&format!("\n[Reached limit of {limit} matches.]"));
                }
                text
            };

            let trunc = truncate_output(&formatted, max_output_bytes);
            let mut res = AgentToolResult::text(trunc.text);
            res.details = json!({
                "path": shown_root,
                "mode": if outcome.is_glob { "glob" } else { "content" },
                "matches": outcome.matches.len(),
                "hit_limit": outcome.hit_limit,
                "truncated": trunc.truncated,
                "omitted_bytes": trunc.omitted_bytes,
            });
            Ok(res)
        })
    }
}

struct FindOutcome {
    matches: Vec<String>,
    hit_limit: bool,
    is_glob: bool,
}

fn is_glob_pattern(pattern: &str) -> bool {
    pattern.contains('*') || pattern.contains('?') || pattern.contains('[')
}

fn run_find(
    cwd: &Path,
    search_root: &Path,
    pattern: &str,
    limit: usize,
    ignore_dirs: &HashSet<String>,
    cancel: &CancelToken,
) -> Result<FindOutcome, String> {
    if cancel.is_cancelled() {
        return Err("Operation aborted".into());
    }

    let meta = fs::metadata(search_root).map_err(|e| {
        format!(
            "failed to access search path '{}': {e}",
            display_path(cwd, search_root)
        )
    })?;

    let is_glob = is_glob_pattern(pattern);
    let mut matches = Vec::new();
    let mut hit_limit = false;

    if meta.is_file() {
        inspect_file(
            cwd,
            search_root,
            search_root,
            pattern,
            is_glob,
            limit,
            &mut matches,
            &mut hit_limit,
            cancel,
        )?;
        return Ok(FindOutcome {
            matches,
            hit_limit,
            is_glob,
        });
    }

    let mut stack: Vec<PathBuf> = vec![search_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if cancel.is_cancelled() {
            return Err("Operation aborted".into());
        }
        if matches.len() >= limit {
            hit_limit = true;
            break;
        }

        let read_dir = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };

        let mut entries: Vec<PathBuf> = read_dir
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .collect();
        // Sort ascending for deterministic traversal; subdirectories are pushed in reverse onto LIFO stack.
        entries.sort();

        let mut subdirs = Vec::new();
        for entry_path in entries {
            if cancel.is_cancelled() {
                return Err("Operation aborted".into());
            }
            let Ok(ft) = fs::symlink_metadata(&entry_path).map(|m| m.file_type()) else {
                continue;
            };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                if let Some(name) = entry_path.file_name().and_then(|n| n.to_str())
                    && ignore_dirs.contains(name)
                {
                    continue;
                }
                subdirs.push(entry_path);
            } else if ft.is_file() {
                inspect_file(
                    cwd,
                    search_root,
                    &entry_path,
                    pattern,
                    is_glob,
                    limit,
                    &mut matches,
                    &mut hit_limit,
                    cancel,
                )?;
                if hit_limit {
                    break;
                }
            }
        }

        if hit_limit {
            break;
        }
        for sub in subdirs.into_iter().rev() {
            stack.push(sub);
        }
    }

    Ok(FindOutcome {
        matches,
        hit_limit,
        is_glob,
    })
}

#[allow(clippy::too_many_arguments)]
fn inspect_file(
    cwd: &Path,
    search_root: &Path,
    file_path: &Path,
    pattern: &str,
    is_glob: bool,
    limit: usize,
    matches: &mut Vec<String>,
    hit_limit: &mut bool,
    cancel: &CancelToken,
) -> Result<(), String> {
    if cancel.is_cancelled() {
        return Err("Operation aborted".into());
    }

    let display = display_path(cwd, file_path);
    let rel_to_root = file_path
        .strip_prefix(search_root)
        .unwrap_or(file_path)
        .to_string_lossy()
        .replace('\\', "/");
    let file_name = file_path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();

    if is_glob {
        let matched = if pattern.contains('/') {
            glob_match_path(pattern, &rel_to_root) || glob_match_path(pattern, &display)
        } else {
            glob_match_segment(pattern, &file_name)
        };
        if matched {
            if matches.len() >= limit {
                *hit_limit = true;
                return Ok(());
            }
            matches.push(display);
        }
        return Ok(());
    }

    // Literal search: match filename/path and/or file contents.
    let mut matched_content = false;
    let file_len = fs::metadata(file_path).map(|m| m.len()).unwrap_or(u64::MAX);
    if file_len <= MAX_SEARCH_FILE_BYTES
        && let Ok(file) = File::open(file_path)
    {
        let mut reader = BufReader::new(file);
        let mut buf = Vec::new();
        let mut line_no = 0usize;
        let mut checked_binary = false;

        loop {
            if cancel.is_cancelled() {
                return Err("Operation aborted".into());
            }
            buf.clear();
            let n = match reader.read_until(b'\n', &mut buf) {
                Ok(n) => n,
                Err(_) => break,
            };
            if n == 0 {
                break;
            }
            if !checked_binary {
                checked_binary = true;
                if buf.contains(&0) {
                    break;
                }
            } else if buf.contains(&0) {
                break;
            }
            line_no += 1;
            let Ok(line) = std::str::from_utf8(&buf) else {
                break;
            };
            if line.contains(pattern) {
                matched_content = true;
                if matches.len() >= limit {
                    *hit_limit = true;
                    return Ok(());
                }
                let trimmed = line.trim_end_matches(['\r', '\n']);
                let snippet = clip_snippet(trimmed, MAX_LINE_SNIPPET_CHARS);
                matches.push(format!("{display}:{line_no}: {snippet}"));
            }
        }
    }

    if !matched_content && (file_name.contains(pattern) || rel_to_root.contains(pattern)) {
        if matches.len() >= limit {
            *hit_limit = true;
            return Ok(());
        }
        matches.push(display);
    }

    Ok(())
}

fn clip_snippet(line: &str, max_chars: usize) -> String {
    if line.chars().count() <= max_chars {
        return line.to_string();
    }
    let mut out: String = line.chars().take(max_chars).collect();
    out.push_str("...");
    out
}

/// Match a glob pattern containing path separators (`/`), supporting `**` across zero or more segments.
fn glob_match_path(pattern: &str, path: &str) -> bool {
    let pat_segs: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let path_segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    let m = pat_segs.len();
    let n = path_segs.len();
    let mut dp = vec![vec![false; n + 1]; m + 1];
    dp[0][0] = true;

    for i in 1..=m {
        if pat_segs[i - 1] == "**" {
            dp[i][0] = dp[i - 1][0];
            for j in 1..=n {
                dp[i][j] = dp[i - 1][j] || dp[i][j - 1];
            }
        } else {
            for j in 1..=n {
                if dp[i - 1][j - 1] && glob_match_segment(pat_segs[i - 1], path_segs[j - 1]) {
                    dp[i][j] = true;
                }
            }
        }
    }
    dp[m][n]
}

/// Match a single path segment against a glob pattern (`*`, `?`, `[abc]`, `[a-z]`, `[!a-z]`).
fn glob_match_segment(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let m = p.len();
    let n = t.len();

    // Parse pattern into lightweight tokens so DP is O(tokens * text.len()).
    enum Tok {
        Star,
        Any,
        Literal(char),
        Class { negated: bool, ranges: Vec<(char, char)> },
    }

    let mut tokens = Vec::with_capacity(m);
    let mut idx = 0;
    while idx < m {
        match p[idx] {
            '*' => {
                if !matches!(tokens.last(), Some(Tok::Star)) {
                    tokens.push(Tok::Star);
                }
                idx += 1;
            }
            '?' => {
                tokens.push(Tok::Any);
                idx += 1;
            }
            '[' => {
                if let Some(close_rel) = p[idx + 1..].iter().position(|&c| c == ']') {
                    let close = idx + 1 + close_rel;
                    let mut inner_i = idx + 1;
                    let negated = if inner_i < close && (p[inner_i] == '!' || p[inner_i] == '^') {
                        inner_i += 1;
                        true
                    } else {
                        false
                    };
                    let mut ranges = Vec::new();
                    while inner_i < close {
                        if inner_i + 2 < close && p[inner_i + 1] == '-' {
                            ranges.push((p[inner_i], p[inner_i + 2]));
                            inner_i += 3;
                        } else {
                            ranges.push((p[inner_i], p[inner_i]));
                            inner_i += 1;
                        }
                    }
                    tokens.push(Tok::Class { negated, ranges });
                    idx = close + 1;
                } else {
                    tokens.push(Tok::Literal('['));
                    idx += 1;
                }
            }
            c => {
                tokens.push(Tok::Literal(c));
                idx += 1;
            }
        }
    }

    let mut prev = vec![false; n + 1];
    let mut curr = vec![false; n + 1];
    prev[0] = true;

    for tok in &tokens {
        curr.fill(false);
        match tok {
            Tok::Star => {
                curr[0] = prev[0];
                for j in 1..=n {
                    curr[j] = prev[j] || curr[j - 1];
                }
            }
            Tok::Any => {
                curr[1..=n].copy_from_slice(&prev[..n]);
            }
            Tok::Literal(ch) => {
                for j in 1..=n {
                    curr[j] = prev[j - 1] && t[j - 1] == *ch;
                }
            }
            Tok::Class { negated, ranges } => {
                for j in 1..=n {
                    let c = t[j - 1];
                    let in_set = ranges.iter().any(|&(lo, hi)| c >= lo && c <= hi);
                    let ok = if *negated { !in_set } else { in_set };
                    curr[j] = prev[j - 1] && ok;
                }
            }
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[n]
}
