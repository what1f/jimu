//! Persistent and in-memory prompt history management.
//!
//! Entries are stored in memory newest-first (index 0 is most recent) so both
//! in-place Up/Down recall and `history_search` share one canonical ordering.
//! On disk each line is a compact JSON object (`{"text":"..."}`) appended oldest-to-newest.

use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::Path,
};

#[derive(Serialize, Deserialize)]
struct PersistedPrompt {
    text: String,
}

/// Load persisted prompts from a JSONL file, returning them newest-first and deduplicated.
pub(super) fn load_newest_first(path: &Path, limit: Option<usize>) -> Vec<String> {
    let Ok(file) = fs::File::open(path) else {
        return Vec::new();
    };
    let reader = BufReader::new(file);
    let mut oldest_first = Vec::new();
    for line in reader.lines().map_while(Result::ok) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(record) = serde_json::from_str::<PersistedPrompt>(trimmed)
            && !record.text.trim().is_empty()
        {
            oldest_first.push(record.text);
        }
    }
    dedupe_newest_first(oldest_first.into_iter().rev(), limit)
}

/// Append one prompt entry to the JSONL history file, creating parent directories if needed.
pub(super) fn append_entry(path: &Path, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        let _ = fs::create_dir_all(parent);
    }
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    if let Ok(line) = serde_json::to_string(&PersistedPrompt {
        text: text.to_owned(),
    }) {
        let _ = writeln!(file, "{line}");
    }
}

/// Record a newly submitted entry at index 0 (newest-first), moving any duplicate to the front.
pub(super) fn push_newest(entries: &mut Vec<String>, text: String, limit: Option<usize>) {
    if text.trim().is_empty() {
        return;
    }
    if entries.first().is_some_and(|first| first == &text) {
        return;
    }
    entries.retain(|existing| existing != &text);
    entries.insert(0, text);
    if let Some(cap) = limit {
        entries.truncate(cap);
    }
}

/// Merge session history (oldest-first) on top of persisted history (newest-first),
/// keeping the most recent occurrence of each unique entry at the front.
pub(super) fn merge_newest_first(
    session_oldest_first: impl DoubleEndedIterator<Item = String>,
    persisted_newest_first: Vec<String>,
    limit: Option<usize>,
) -> Vec<String> {
    let combined = session_oldest_first.rev().chain(persisted_newest_first);
    dedupe_newest_first(combined, limit)
}

fn dedupe_newest_first(
    newest_first: impl Iterator<Item = String>,
    limit: Option<usize>,
) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for text in newest_first {
        if text.trim().is_empty() {
            continue;
        }
        if seen.insert(text.clone()) {
            out.push(text);
            if let Some(cap) = limit
                && out.len() >= cap
            {
                break;
            }
        }
    }
    out
}
