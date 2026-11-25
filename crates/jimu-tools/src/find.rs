use crate::WorkspaceToolsConfig;
use crate::util::{check_cancelled, display_path, resolve_path, truncate_head};
use globset::{GlobBuilder, GlobMatcher};
use ignore::WalkBuilder;
use jimu_agent::{AgentTool, AgentToolResult, ToolProgressCallback};
use jimu_ai::{BoxFuture, CancelToken, ToolDefinition};
use jimu_schema::{bool_schema, int_schema, obj_schema, str_schema, with_bounds, with_description};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const DEFAULT_LIMIT: usize = 200;

/// Find file and directory paths by glob or substring.
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
            description: "Find file and directory paths by glob or substring.".into(),
            parameters: obj_schema([
                (
                    "pattern",
                    with_description(str_schema(), "Path glob (such as '**/*.rs') or substring."),
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
                    "limit",
                    with_description(
                        with_bounds(int_schema(), Some(1.0), None),
                        "Maximum paths to return. Defaults to 200.",
                    ),
                    false,
                ),
                (
                    "hidden",
                    with_description(bool_schema(), "Include hidden paths."),
                    false,
                ),
                (
                    "max_depth",
                    with_description(
                        with_bounds(int_schema(), Some(0.0), None),
                        "Maximum depth below the search path.",
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
        Box::pin(async move {
            check_cancelled(&cancel)?;
            let root = search_root(&self.config, &args)?;
            let limit = search_limit(&args)?;
            let pattern = args["pattern"]
                .as_str()
                .filter(|pattern| !pattern.is_empty())
                .ok_or("pattern must be a non-empty string")?;
            let glob = path_glob(pattern)?;
            let mut walk = search_walk(&self.config, &root, &cancel);
            walk.hidden(!args["hidden"].as_bool().unwrap_or(false))
                .max_depth(optional_usize(&args, "max_depth")?);

            let mut paths = BTreeSet::new();
            let mut hit_limit = false;
            for entry in walk.build() {
                check_cancelled(&cancel)?;
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(err) => {
                        if err.depth() == Some(0) {
                            return Err(format!("Could not search paths: {err}"));
                        }
                        continue;
                    }
                };
                if !entry
                    .file_type()
                    .is_some_and(|kind| kind.is_file() || kind.is_dir())
                    || (entry.depth() == 0 && root.is_dir())
                {
                    continue;
                }
                let path = entry.path();
                let shown = display_path(&self.config.cwd, path);
                let relative = path
                    .strip_prefix(&root)
                    .ok()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or_else(|| path.file_name().map(Path::new).unwrap_or(path));
                let is_match = match &glob {
                    Some(glob) => glob.is_match(relative) || glob.is_match(&shown),
                    None => shown.contains(pattern) || relative.to_string_lossy().contains(pattern),
                };
                if is_match && paths.insert(shown) && paths.len() > limit {
                    paths.pop_last();
                    hit_limit = true;
                }
            }
            check_cancelled(&cancel)?;
            let rows: Vec<_> = paths.into_iter().collect();
            Ok(search_result(
                &self.config,
                &root,
                if glob.is_some() { "glob" } else { "substring" },
                &rows,
                hit_limit,
            ))
        })
    }
}

fn path_glob(pattern: &str) -> Result<Option<GlobMatcher>, String> {
    if !pattern.contains(['*', '?', '[', '{']) {
        return Ok(None);
    }
    let full_pattern = if pattern.contains('/') {
        pattern.to_owned()
    } else {
        format!("**/{pattern}")
    };
    GlobBuilder::new(&full_pattern)
        .literal_separator(true)
        .build()
        .map(|glob| Some(glob.compile_matcher()))
        .map_err(|err| format!("Invalid path glob: {err}"))
}

pub(crate) fn search_root(config: &WorkspaceToolsConfig, args: &Value) -> Result<PathBuf, String> {
    let path = match args.get("path") {
        Some(path) => resolve_path(&config.cwd, path.as_str().ok_or("path must be a string")?)?,
        None => config.cwd.clone(),
    };
    let meta = path
        .metadata()
        .map_err(|err| format!("Could not access '{}': {err}", path.display()))?;
    if meta.is_file() || meta.is_dir() {
        Ok(path)
    } else {
        Err(format!(
            "Expected a file or directory: '{}'",
            path.display()
        ))
    }
}

pub(crate) fn optional_usize(args: &Value, name: &str) -> Result<Option<usize>, String> {
    args.get(name)
        .map(|value| {
            value
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| format!("{name} must be a non-negative integer"))
        })
        .transpose()
}

pub(crate) fn search_limit(args: &Value) -> Result<usize, String> {
    let limit = optional_usize(args, "limit")?.unwrap_or(DEFAULT_LIMIT);
    if limit == 0 {
        return Err("limit must be at least 1".into());
    }
    Ok(limit)
}

pub(crate) fn search_walk(
    config: &WorkspaceToolsConfig,
    root: &Path,
    cancel: &CancelToken,
) -> WalkBuilder {
    let ignored: std::collections::HashSet<_> = config.ignore_dirs.iter().cloned().collect();
    let cancel = cancel.clone();
    let mut walk = WalkBuilder::new(root);
    walk.require_git(false)
        .add_custom_ignore_filename(".rgignore")
        .sort_by_file_path(Path::cmp)
        .filter_entry(move |entry| {
            if cancel.is_cancelled() {
                return false;
            }
            let name = entry.file_name();
            name != ".git"
                && (!entry.file_type().is_some_and(|kind| kind.is_dir())
                    || !name.to_str().is_some_and(|name| ignored.contains(name)))
        });
    walk
}

pub(crate) fn search_result(
    config: &WorkspaceToolsConfig,
    root: &Path,
    mode: &str,
    rows: &[String],
    hit_limit: bool,
) -> AgentToolResult {
    let text = if rows.is_empty() {
        "No matches found.".to_owned()
    } else {
        rows.join("\n")
    };
    let trunc = truncate_head(&text, config.max_output_bytes, config.max_output_lines);
    let mut result = AgentToolResult::text(trunc.text);
    result.details = json!({
        "path": display_path(&config.cwd, root),
        "mode": mode,
        "matches": rows.len(),
        "hit_limit": hit_limit,
        "truncated": trunc.truncated,
        "omitted_bytes": trunc.omitted_bytes,
    });
    result
}
