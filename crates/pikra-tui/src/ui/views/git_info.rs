//! Cached Git location information for header and welcome views.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CwdGitInfo {
    /// Empty for detached HEAD.
    pub branch: Option<String>,
    pub is_worktree: bool,
}

type GitCache = HashMap<PathBuf, Option<CwdGitInfo>>;

fn cache() -> &'static Mutex<GitCache> {
    static CACHE: OnceLock<Mutex<GitCache>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

pub fn cwd_git_info_lazy(cwd: &Path) -> Option<CwdGitInfo> {
    let mut cache = cache().lock().unwrap_or_else(|error| error.into_inner());
    cache
        .entry(cwd.to_path_buf())
        .or_insert_with(|| read(cwd))
        .clone()
}

/// Clear the cached location after a host observes a checkout or repository change.
pub fn invalidate(cwd: &Path) {
    cache()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(cwd);
}

fn read(cwd: &Path) -> Option<CwdGitInfo> {
    for root in cwd.ancestors() {
        let entry = root.join(".git");
        let git_dir = if entry.is_dir() {
            entry
        } else if let Ok(contents) = fs::read_to_string(&entry) {
            root.join(contents.trim().strip_prefix("gitdir:")?.trim())
        } else {
            continue;
        };
        let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
        let branch = head
            .trim()
            .strip_prefix("ref: ")
            .map_or_else(String::new, |reference| {
                reference
                    .strip_prefix("refs/heads/")
                    .unwrap_or(reference)
                    .to_owned()
            });
        return Some(CwdGitInfo {
            branch: Some(branch),
            is_worktree: git_dir.join("commondir").is_file(),
        });
    }
    None
}

pub fn branch_icon() -> &'static str {
    if cfg!(windows) { "≡" } else { "⎇" }
}
