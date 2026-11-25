//! Unified Jimu CLI: interactive TUI (default), one-shot `-p` CLI, JSONL RPC server, and ACP v1 server.

pub mod acp;
pub mod docs;
pub mod host;
pub mod mcp;
pub mod prompt;
pub mod providers;
pub mod settings;
pub mod tools;
#[cfg(feature = "tui")]
pub mod tui;

use jimu_session::{JsonlSessionStore, SessionEntryPayload, SessionStore};
use std::{
    fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

/// Find the most recently active non-empty JSONL session recorded for `cwd` under `sessions_dir`.
pub fn find_latest_session_for_cwd(sessions_dir: &Path, cwd: &Path) -> Option<(String, PathBuf)> {
    let entries = fs::read_dir(sessions_dir).ok()?;
    let target_cwd = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut best: Option<(u64, String, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(id) = name.strip_suffix(".jsonl") else {
            continue;
        };
        let store = JsonlSessionStore::new(&path);
        let Ok((header, items)) = store.load() else {
            continue;
        };
        let Some(header_cwd) = header.and_then(|h| h.cwd) else {
            continue;
        };
        let candidate_cwd =
            fs::canonicalize(&header_cwd).unwrap_or_else(|_| PathBuf::from(&header_cwd));
        if candidate_cwd != target_cwd {
            continue;
        }
        let has_messages = items
            .iter()
            .any(|item| matches!(item.payload, SessionEntryPayload::Message { .. }));
        if !has_messages {
            continue;
        }
        let last_ts = items.iter().map(|item| item.timestamp).max().unwrap_or(0);
        let sort_ms = if last_ts > 0 {
            last_ts
        } else {
            fs::metadata(&path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
        };
        if best.as_ref().is_none_or(|(prev, _, _)| sort_ms > *prev) {
            best = Some((sort_ms, id.to_owned(), path));
        }
    }
    best.map(|(_, id, path)| (id, path))
}
