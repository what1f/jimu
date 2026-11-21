use crate::WorkspaceToolsConfig;
use crate::seek_sequence::seek_sequence;
use crate::util::{
    check_cancelled, display_path, path_lock, read_editable_text, resolve_path,
    resolve_write_target, stage_write, truncate_output,
};
use pikra_agent::{AgentTool, AgentToolResult, ToolExecutionMode, ToolProgressCallback};
use pikra_ai::{BoxFuture, CancelToken, ToolDefinition};
use pikra_schema::{obj_schema, str_schema, with_description};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs::{self, Permissions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Apply a Codex-format patch to workspace files.
#[derive(Clone, Debug)]
pub struct ApplyPatchTool {
    config: Arc<WorkspaceToolsConfig>,
}

impl ApplyPatchTool {
    pub fn new(config: Arc<WorkspaceToolsConfig>) -> Self {
        Self { config }
    }
}

impl AgentTool for ApplyPatchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "apply_patch".into(),
            description: "Add, update, move, or delete files in one patch.".into(),
            parameters: obj_schema([(
                "patch",
                with_description(
                    str_schema(),
                    "Start with *** Begin Patch and finish with *** End Patch. Use *** Add File: <path> with + lines, *** Delete File: <path>, or *** Update File: <path> with @@ and lines prefixed by space, - or +. To move a file, place *** Move to: <path> after its update header. Use *** End of File to match the end of a file.",
                ),
                true,
            )]),
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
        Box::pin(async move {
            check_cancelled(&cancel)?;
            let patch = args["patch"]
                .as_str()
                .ok_or("Missing string argument 'patch'")?;
            apply(&self.config, parse(patch)?, &cancel)
        })
    }
}

struct FilePatch<'a> {
    path: &'a str,
    action: Action<'a>,
}

enum Action<'a> {
    Add(String),
    Delete,
    Update {
        move_to: Option<&'a str>,
        chunks: Vec<Chunk<'a>>,
    },
}

#[derive(Default)]
struct Chunk<'a> {
    anchor: Option<&'a str>,
    lines: Vec<Change<'a>>,
    eof: bool,
}

enum Change<'a> {
    Keep(&'a str),
    Remove(&'a str),
    Insert(&'a str),
}

impl<'a> Change<'a> {
    fn old_text(&self) -> Option<&'a str> {
        match *self {
            Self::Keep(line) | Self::Remove(line) => Some(line),
            Self::Insert(_) => None,
        }
    }
}

const ADD: &str = "*** Add File: ";
const DELETE: &str = "*** Delete File: ";
const UPDATE: &str = "*** Update File: ";
const MOVE: &str = "*** Move to: ";
const EOF: &str = "*** End of File";

fn file_header(line: &str) -> bool {
    [ADD, DELETE, UPDATE]
        .iter()
        .any(|prefix| line.starts_with(prefix))
}

fn parse(patch: &str) -> Result<Vec<FilePatch<'_>>, String> {
    let lines: Vec<_> = patch.trim().lines().collect();
    let (Some(&"*** Begin Patch"), Some(&"*** End Patch")) = (lines.first(), lines.last()) else {
        return Err(
            "Start the patch with '*** Begin Patch' and finish with '*** End Patch'".into(),
        );
    };
    let mut lines = lines[1..lines.len() - 1]
        .iter()
        .copied()
        .enumerate()
        .peekable();
    let mut files = Vec::new();
    while let Some((i, header)) = lines.next() {
        let body = std::iter::from_fn(|| lines.next_if(|(_, line)| !file_header(line)));
        let (path, action) = if let Some(path) = header.strip_prefix(ADD) {
            let mut text = String::new();
            for (i, line) in body {
                text.push_str(line.strip_prefix('+').ok_or_else(|| {
                    format!("Expected a line starting with '+' at patch line {}", i + 2)
                })?);
                text.push('\n');
            }
            (path, Action::Add(text))
        } else if let Some(path) = header.strip_prefix(DELETE) {
            (path, Action::Delete)
        } else if let Some(path) = header.strip_prefix(UPDATE) {
            let mut body = body.peekable();
            let move_to = body.peek().and_then(|(_, line)| line.strip_prefix(MOVE));
            if move_to.is_some() {
                body.next();
            }
            let chunks = parse_chunks(body)?;
            if chunks.is_empty() && move_to.is_none() {
                return Err(format!("No changes for '{path}'"));
            }
            (path, Action::Update { move_to, chunks })
        } else {
            return Err(format!("Expected a file header at patch line {}", i + 2));
        };
        files.push(FilePatch { path, action });
    }
    if files.is_empty() {
        return Err("Patch contains no file changes".into());
    }
    Ok(files)
}

fn parse_chunks<'a>(
    lines: impl Iterator<Item = (usize, &'a str)>,
) -> Result<Vec<Chunk<'a>>, String> {
    let mut chunks = Vec::new();
    let mut current: Option<Chunk> = None;
    for (i, line) in lines {
        let invalid = || format!("Invalid update at patch line {}", i + 2);
        if line == "@@" || line.starts_with("@@ ") {
            if let Some(chunk) = current.take() {
                if chunk.lines.is_empty() {
                    return Err(invalid());
                }
                chunks.push(chunk);
            }
            current = Some(Chunk {
                anchor: line.strip_prefix("@@ "),
                ..Chunk::default()
            });
            continue;
        }

        let chunk = current.get_or_insert_with(Chunk::default);
        if chunk.eof {
            return Err(invalid());
        }
        match line.as_bytes().first() {
            _ if line == EOF && !chunk.lines.is_empty() => chunk.eof = true,
            Some(b' ') => chunk.lines.push(Change::Keep(&line[1..])),
            Some(b'-') => chunk.lines.push(Change::Remove(&line[1..])),
            Some(b'+') => chunk.lines.push(Change::Insert(&line[1..])),
            None => chunk.lines.push(Change::Keep("")),
            _ => return Err(invalid()),
        }
    }
    if let Some(chunk) = current {
        if chunk.lines.is_empty() {
            return Err("An update section contains no lines".into());
        }
        chunks.push(chunk);
    }
    Ok(chunks)
}

// Locate all chunks in the original text in order. Copy context lines verbatim.
fn update_text(source: &str, chunks: &[Chunk<'_>], shown: &str) -> Result<String, String> {
    if chunks.is_empty() {
        return Ok(source.to_owned());
    }
    let raw: Vec<_> = source.split_inclusive('\n').collect();
    let lines: Vec<_> = raw
        .iter()
        .map(|line| {
            if let Some(text) = line.strip_suffix('\n') {
                text.strip_suffix('\r').unwrap_or(text)
            } else {
                line
            }
        })
        .collect();
    let ending = if raw
        .iter()
        .find(|line| line.ends_with('\n'))
        .is_some_and(|line| line.ends_with("\r\n"))
    {
        "\r\n"
    } else {
        "\n"
    };

    let mut output = String::with_capacity(source.len());
    let mut cursor = 0;
    for chunk in chunks {
        let mut start = cursor;
        if let Some(anchor) = chunk.anchor {
            let offset = seek_sequence(&lines[start..], &[anchor], false)
                .ok()
                .flatten()
                .ok_or_else(|| format!("Could not find context in '{shown}': {anchor}"))?;
            start += offset + 1;
        }
        let expected: Vec<_> = chunk.lines.iter().filter_map(Change::old_text).collect();
        let index = if expected.is_empty() {
            lines.len()
        } else {
            if chunk.eof {
                start = lines.len().saturating_sub(expected.len()).max(start);
            }
            let offset = seek_sequence(&lines[start..], &expected, false)
                .ok()
                .flatten()
                .ok_or_else(|| format!("Could not find the expected lines in '{shown}'"))?;
            start + offset
        };

        for line in &raw[cursor..index] {
            ensure_line_break(&mut output, ending);
            output.push_str(line);
        }
        cursor = index;
        for change in &chunk.lines {
            match change {
                Change::Keep(_) => {
                    ensure_line_break(&mut output, ending);
                    output.push_str(raw[cursor]);
                    cursor += 1;
                }
                Change::Remove(_) => cursor += 1,
                Change::Insert(line) => {
                    ensure_line_break(&mut output, ending);
                    output.push_str(line);
                    output.push_str(ending);
                }
            }
        }
    }
    for line in &raw[cursor..] {
        ensure_line_break(&mut output, ending);
        output.push_str(line);
    }
    Ok(output)
}

fn ensure_line_break(output: &mut String, ending: &str) {
    if !output.is_empty() && !output.ends_with('\n') {
        output.push_str(ending);
    }
}

#[derive(PartialEq, Eq)]
struct PatchPath {
    shown: String,
    entry: PathBuf,
    target: PathBuf,
}

impl PatchPath {
    fn resolve(cwd: &Path, raw: &str) -> Result<Self, String> {
        let path = resolve_path(cwd, raw)?;
        let name = path
            .file_name()
            .ok_or_else(|| format!("Expected a file path: '{raw}'"))?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        Ok(Self {
            shown: display_path(cwd, &path),
            entry: resolve_write_target(parent)?.join(name),
            target: resolve_write_target(&path)?,
        })
    }

    fn require_absent(&self) -> Result<(), String> {
        match fs::symlink_metadata(&self.entry) {
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
            Ok(_) => Err(format!("'{}' already exists", self.shown)),
            Err(err) => Err(format!("Could not access '{}': {err}", self.shown)),
        }
    }

    fn require_removable(&self) -> Result<(), String> {
        let meta = fs::symlink_metadata(&self.entry)
            .map_err(|err| format!("Could not access '{}': {err}", self.shown))?;
        if meta.is_file() || meta.is_symlink() {
            Ok(())
        } else {
            Err(format!("Cannot remove '{}': expected a file", self.shown))
        }
    }
}

enum Prepared<'a> {
    Write {
        path: &'a PatchPath,
        text: String,
        permissions: Option<Permissions>,
        source: Option<&'a PatchPath>,
        create: bool,
    },
    Delete(&'a PatchPath),
}

enum Staged<'a> {
    Write {
        path: &'a PatchPath,
        file: tempfile::NamedTempFile,
        source: Option<&'a PatchPath>,
        create: bool,
    },
    Delete(&'a PatchPath),
}

impl<'a> Prepared<'a> {
    fn stage(self) -> Result<Staged<'a>, String> {
        match self {
            Self::Delete(path) => Ok(Staged::Delete(path)),
            Self::Write {
                path,
                text,
                permissions,
                source,
                create,
            } => {
                let target = if create { &path.entry } else { &path.target };
                let file = stage_write(target, text.as_bytes(), permissions)?;
                Ok(Staged::Write {
                    path,
                    file,
                    source,
                    create,
                })
            }
        }
    }
}

#[derive(Default)]
struct Summary {
    added: Vec<String>,
    modified: Vec<String>,
    deleted: Vec<String>,
    lines: Vec<String>,
}

impl Summary {
    fn commit(&mut self, staged: Staged<'_>) -> Result<(), String> {
        let remove = |path: &PatchPath| {
            fs::remove_file(&path.entry)
                .map_err(|err| format!("Could not remove '{}': {err}", path.shown))
        };
        match staged {
            Staged::Delete(path) => {
                remove(path)?;
                self.deleted.push(path.shown.clone());
                self.lines.push(format!("Deleted {}", path.shown));
            }
            Staged::Write {
                path,
                file,
                source,
                create,
            } => {
                let outcome = if create {
                    file.persist_noclobber(&path.entry)
                } else {
                    file.persist(&path.target)
                };
                outcome
                    .map_err(|err| format!("Could not write '{}': {}", path.shown, err.error))?;
                if let Some(source) = source {
                    if let Err(err) = remove(source) {
                        return match remove(path) {
                            Ok(()) => Err(err),
                            Err(cleanup) => Err(format!("{err}\n{cleanup}")),
                        };
                    }
                    self.added.push(path.shown.clone());
                    self.deleted.push(source.shown.clone());
                    self.lines
                        .push(format!("Moved {} to {}", source.shown, path.shown));
                } else if create {
                    self.added.push(path.shown.clone());
                    self.lines.push(format!("Added {}", path.shown));
                } else {
                    self.modified.push(path.shown.clone());
                    self.lines.push(format!("Modified {}", path.shown));
                }
            }
        }
        Ok(())
    }
}

fn apply(
    config: &WorkspaceToolsConfig,
    files: Vec<FilePatch<'_>>,
    cancel: &CancelToken,
) -> Result<AgentToolResult, String> {
    let mut paths = BTreeSet::new();
    let resolved = files
        .iter()
        .map(|file| {
            check_cancelled(cancel)?;
            let path = PatchPath::resolve(&config.cwd, file.path)?;
            let dest = match file.action {
                Action::Update {
                    move_to: Some(dest),
                    ..
                } => Some(PatchPath::resolve(&config.cwd, dest)?),
                _ => None,
            };
            for path in std::iter::once(&path).chain(dest.iter()) {
                if paths.contains(&path.target) || paths.contains(&path.entry) {
                    return Err(format!("Patch uses '{}' more than once", path.shown));
                }
                paths.insert(path.entry.clone());
                paths.insert(path.target.clone());
            }
            Ok((path, dest))
        })
        .collect::<Result<Vec<_>, String>>()?;
    if let Some(path) = paths
        .iter()
        .find(|path| path.ancestors().skip(1).any(|p| paths.contains(p)))
    {
        return Err(format!(
            "Conflicting paths in patch near '{}'",
            display_path(&config.cwd, path)
        ));
    }
    let locks: Vec<_> = paths.iter().map(|path| path_lock(path)).collect();
    let _guards: Vec<_> = locks
        .iter()
        .map(|lock| lock.lock().unwrap_or_else(|e| e.into_inner()))
        .collect();

    let mut prepared = Vec::new();
    for (file, (path, dest)) in files.into_iter().zip(&resolved) {
        check_cancelled(cancel)?;
        if PatchPath::resolve(&config.cwd, file.path)? != *path {
            return Err(format!(
                "'{}' changed during the operation. Try again.",
                path.shown
            ));
        }
        let change = match file.action {
            Action::Add(text) => {
                path.require_absent()?;
                Prepared::Write {
                    path,
                    text,
                    permissions: None,
                    source: None,
                    create: true,
                }
            }
            Action::Delete => {
                path.require_removable()?;
                Prepared::Delete(path)
            }
            Action::Update { chunks, move_to } => {
                if let Some((dest, raw)) = dest.as_ref().zip(move_to) {
                    if PatchPath::resolve(&config.cwd, raw)? != *dest {
                        return Err(format!(
                            "'{}' changed during the operation. Try again.",
                            dest.shown
                        ));
                    }
                    path.require_removable()?;
                    dest.require_absent()?;
                }
                let (original, permissions) = read_editable_text(&path.target, &path.shown)?;
                let text = update_text(&original, &chunks, &path.shown)?;
                Prepared::Write {
                    path: dest.as_ref().unwrap_or(path),
                    text,
                    permissions: Some(permissions),
                    source: dest.as_ref().map(|_| path),
                    create: dest.is_some(),
                }
            }
        };
        prepared.push(change);
    }
    let staged = prepared
        .into_iter()
        .map(|change| {
            check_cancelled(cancel)?;
            change.stage()
        })
        .collect::<Result<Vec<_>, _>>()?;
    check_cancelled(cancel)?;

    let mut summary = Summary::default();
    for change in staged {
        if let Err(error) = summary.commit(change) {
            if summary.lines.is_empty() {
                return Err(error);
            }
            let mut result = AgentToolResult::error(
                truncate_output(
                    &format!("{error}\n{}", summary.lines.join("\n")),
                    config.max_output_bytes,
                )
                .text,
            );
            result.details = json!({"added": summary.added, "modified": summary.modified, "deleted": summary.deleted});
            return Ok(result);
        }
    }
    let mut result = AgentToolResult::text(
        truncate_output(&summary.lines.join("\n"), config.max_output_bytes).text,
    );
    result.details =
        json!({"added": summary.added, "modified": summary.modified, "deleted": summary.deleted});
    Ok(result)
}
