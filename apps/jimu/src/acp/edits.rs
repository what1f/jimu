use crate::acp::{schema::v1 as acp, tools_bridge::Bridge};
use jimu_agent::{AgentTool, AgentToolResult};
use jimu_ai::CancelToken;
use jimu_tools::{ApplyPatchTool, EditTool, WorkspaceToolsConfig, WriteTool};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::Arc,
};

pub(crate) struct Change {
    pub(crate) path: PathBuf,
    pub(crate) target: Option<PathBuf>,
    before: Option<String>,
    after: Option<String>,
}

impl Change {
    pub(crate) fn diff(&self) -> acp::ToolCallContent {
        acp::ToolCallContent::Diff(
            acp::Diff::new(&self.path, self.after.as_deref().unwrap_or_default())
                .old_text(self.before.clone()),
        )
    }

    pub(crate) fn location(&self) -> acp::ToolCallLocation {
        let matching = self
            .before
            .as_deref()
            .unwrap_or_default()
            .split_inclusive('\n')
            .zip(
                self.after
                    .as_deref()
                    .unwrap_or_default()
                    .split_inclusive('\n'),
            )
            .take_while(|(before, after)| before == after)
            .count();
        acp::ToolCallLocation::new(&self.path)
            .line(u32::try_from(matching.saturating_add(1)).unwrap_or(u32::MAX))
    }
}

/// Resolve existing components, including symlinks, before interpreting any parent components.
/// Missing suffixes are accepted for new files. Dangling links require explicit permission.
pub(crate) fn write_target(path: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let mut resolved = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            _ => {
                resolved.push(part.as_os_str());
                match std::fs::symlink_metadata(&resolved) {
                    Ok(metadata) if metadata.is_symlink() => {
                        resolved = std::fs::canonicalize(&resolved).ok()?
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == ErrorKind::NotFound => {}
                    Err(_) => return None,
                }
            }
        }
    }
    Some(resolved)
}

pub(crate) async fn preview(
    bridge: &Bridge,
    name: &str,
    args: &Value,
    cancel: &CancelToken,
) -> Result<Vec<Change>, String> {
    if name == "write" {
        let path = bridge.path(string_arg(args, "path")?);
        let before = original(bridge, &path, cancel).await?;
        return Ok(vec![Change {
            target: write_target(&path),
            path,
            before,
            after: Some(string_arg(args, "content")?.to_owned()),
        }]);
    }

    // Stage only the touched paths. The canonical tool keeps its matching, validation,
    // newline handling, and patch semantics for both the preview and the final content.
    let staging = tempfile::tempdir().map_err(|_| "Could not prepare the file changes.")?;
    let mut paths = BTreeMap::new();
    let mut staged_args = args.clone();
    let config = Arc::new(WorkspaceToolsConfig::new(staging.path()));
    let tool: Box<dyn AgentTool> = if name == "apply_patch" {
        let patch = string_arg(args, "patch")?;
        let mut rewritten = String::new();
        for line in patch.lines() {
            let header = [
                "*** Add File: ",
                "*** Delete File: ",
                "*** Update File: ",
                "*** Move to: ",
            ]
            .into_iter()
            .find_map(|prefix| line.strip_prefix(prefix).map(|path| (prefix, path)));
            if let Some((prefix, path)) = header {
                let local = bridge.path(path);
                let key = paths.len().to_string();
                let staged = paths.entry(local).or_insert(key);
                rewritten.push_str(prefix);
                rewritten.push_str(staged);
            } else {
                rewritten.push_str(line);
            }
            rewritten.push('\n');
        }
        staged_args["patch"] = Value::String(rewritten);
        Box::new(ApplyPatchTool::new(config))
    } else {
        paths.insert(bridge.path(string_arg(args, "path")?), "file".into());
        staged_args["path"] = json!("file");
        Box::new(EditTool::new(config))
    };
    let mut planned = Vec::new();
    let mut targets = BTreeSet::new();
    for (path, file) in paths {
        let target = write_target(&path);
        if let Some(path) = &target
            && !targets.insert(path.clone())
        {
            return Err("Use one path per file in the patch.".into());
        }
        let staged = staging.path().join(file);
        let before = original(bridge, &path, cancel).await?;
        if let Some(text) = &before {
            tokio::fs::write(&staged, text)
                .await
                .map_err(|_| "Could not prepare the file changes.")?;
        }
        let change = Change {
            target,
            path,
            before,
            after: None,
        };
        planned.push((change, staged));
    }
    let result = tool
        .execute(String::new(), staged_args, cancel.clone(), Arc::new(|_| {}))
        .await;
    if !result.is_ok_and(|result| !result.is_error) {
        return Err("The edit did not match the current files. Read them and try again.".into());
    }
    let mut changes = Vec::new();
    for (mut change, staged) in planned {
        change.after = read_disk(&staged).await?;
        if change.before != change.after {
            changes.push(change);
        }
    }
    // For moves, save the destination before removing the source.
    changes.sort_by_key(|change| change.after.is_none());
    Ok(changes)
}

pub(crate) async fn apply(
    bridge: &Bridge,
    changes: &[Change],
    cancel: &CancelToken,
) -> Result<AgentToolResult, String> {
    for change in changes {
        if cancel.is_cancelled() {
            return Err("Operation cancelled.".into());
        }
        if write_target(&change.path) != change.target
            || original(bridge, &change.path, cancel).await? != change.before
        {
            return Err(
                "The file changed while the edit was pending. Read it and try again.".into(),
            );
        }
    }
    let mut applied: Vec<&Change> = Vec::new();
    for change in changes {
        let step_result = if cancel.is_cancelled() {
            Err("Operation cancelled.".into())
        } else if write_target(&change.path) != change.target
            || original(bridge, &change.path, cancel).await? != change.before
        {
            Err("The file changed while the edit was pending. Read it and try again.".into())
        } else {
            write_change(bridge, change, change.after.as_deref(), cancel).await
        };
        if let Err(error) = step_result {
            let undo = CancelToken::new();
            let mut restored = true;
            for prior in applied.into_iter().rev() {
                if write_target(&prior.path) != prior.target
                    || original(bridge, &prior.path, &undo).await.as_ref().ok()
                        != Some(&prior.after)
                    || write_change(bridge, prior, prior.before.as_deref(), &undo)
                        .await
                        .is_err()
                {
                    restored = false;
                }
            }
            return Err(if restored {
                error
            } else {
                "The edit stopped before all files were updated. Check the changed files.".into()
            });
        }
        applied.push(change);
    }
    Ok(AgentToolResult::text(if changes.is_empty() {
        "No changes."
    } else {
        "Applied the file changes."
    }))
}

async fn write_change(
    bridge: &Bridge,
    change: &Change,
    content: Option<&str>,
    cancel: &CancelToken,
) -> Result<(), String> {
    if cancel.is_cancelled() {
        return Err("Operation cancelled.".into());
    }
    if write_target(&change.path) != change.target {
        return Err("The file location changed. Read it and try again.".into());
    }
    let Some(content) = content else {
        // Delete the resolved file, as the canonical patch tool does for symlinks.
        let path = change.target.as_ref().unwrap_or(&change.path);
        return tokio::fs::remove_file(path)
            .await
            .map_err(|_| "Could not remove the file.".into());
    };
    if bridge.capabilities.fs.write_text_file {
        bridge
            .peer
            .request::<acp::WriteTextFileResponse>(
                "fs/write_text_file",
                acp::WriteTextFileRequest::new(bridge.id.clone(), &change.path, content),
                cancel,
            )
            .await
            .map_err(|_| "The editor could not save the file.".to_string())?;
    } else {
        let tool = WriteTool::new(Arc::new(WorkspaceToolsConfig::new(&bridge.cwd)));
        let path = change.target.as_ref().unwrap_or(&change.path);
        tool.execute(
            String::new(),
            json!({"path":path,"content":content}),
            cancel.clone(),
            Arc::new(|_| {}),
        )
        .await
        .map_err(|_| "Could not save the file.".to_string())?;
    }
    Ok(())
}

async fn original(
    bridge: &Bridge,
    path: &Path,
    cancel: &CancelToken,
) -> Result<Option<String>, String> {
    if let Some(result) = bridge.read_ide(path, None, None, cancel).await {
        return match result {
            Ok(text) => Ok(Some(text)),
            Err(err) if err.code == acp::ErrorCode::ResourceNotFound => Ok(None),
            Err(_) if cancel.is_cancelled() => Err("Operation cancelled.".into()),
            Err(err) => Err(err.message),
        };
    }
    let target = write_target(path);
    read_disk(target.as_deref().unwrap_or(path)).await
}

async fn read_disk(path: &Path) -> Result<Option<String>, String> {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(_) => Err("Could not read the file as text.".into()),
    }
}

pub(crate) fn string_arg<'a>(args: &'a Value, name: &str) -> Result<&'a str, String> {
    args.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("Expected text for {name}."))
}
