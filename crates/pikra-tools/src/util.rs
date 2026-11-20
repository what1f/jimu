use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Resolve a tool path relative to `base_cwd` (preserving absolute paths as-is).
pub(crate) fn resolve_path(base_cwd: &Path, raw: &str) -> Result<PathBuf, String> {
    if raw.is_empty() {
        return Err("path must not be empty".to_string());
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(base_cwd.join(path))
    }
}

/// Format a path relative to `base_cwd` when possible, using forward slashes.
pub(crate) fn display_path(base_cwd: &Path, path: &Path) -> String {
    let shown = path.strip_prefix(base_cwd).unwrap_or(path);
    shown.to_string_lossy().replace('\\', "/")
}

/// Atomically write `bytes` to `target`, creating parent directories automatically.
pub(crate) async fn atomic_write_file(target: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Ok(meta) = tokio::fs::metadata(target).await
        && meta.is_dir()
    {
        return Err(format!(
            "cannot write to '{}': path is a directory",
            target.display()
        ));
    }

    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|e| format!("failed to create parent directories for '{}': {e}", target.display()))?;

    let existing_perms = tokio::fs::metadata(target)
        .await
        .ok()
        .map(|m| m.permissions());

    let seq = TMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let file_name = target
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_else(|| std::borrow::Cow::Borrowed("file"));
    let tmp_path = parent.join(format!(".pikra-tmp-{pid}-{seq}-{file_name}"));

    if let Err(err) = write_and_rename(&tmp_path, target, bytes, existing_perms).await {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(err);
    }

    Ok(())
}

async fn write_and_rename(
    tmp_path: &Path,
    target: &Path,
    bytes: &[u8],
    existing_perms: Option<std::fs::Permissions>,
) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;

    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp_path)
        .await
        .map_err(|e| format!("failed to create temporary file for '{}': {e}", target.display()))?;

    file.write_all(bytes)
        .await
        .map_err(|e| format!("failed to write '{}': {e}", target.display()))?;
    file.flush()
        .await
        .map_err(|e| format!("failed to flush '{}': {e}", target.display()))?;
    drop(file);

    if let Some(perms) = existing_perms {
        let _ = tokio::fs::set_permissions(tmp_path, perms).await;
    }

    tokio::fs::rename(tmp_path, target)
        .await
        .map_err(|e| format!("failed to commit '{}': {e}", target.display()))
}

/// Result of UTF-8-safe head+tail truncation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncatedOutput {
    pub text: String,
    pub truncated: bool,
    pub original_bytes: usize,
    pub omitted_bytes: usize,
}

fn floor_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let mut i = idx;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let mut i = idx;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Truncate UTF-8 text to at most `max_bytes`, keeping head + tail with a
/// `[... truncated N bytes ...]` marker while strictly respecting UTF-8 char boundaries.
pub fn truncate_output(text: &str, max_bytes: usize) -> TruncatedOutput {
    let original_bytes = text.len();
    if original_bytes <= max_bytes {
        return TruncatedOutput {
            text: text.to_string(),
            truncated: false,
            original_bytes,
            omitted_bytes: 0,
        };
    }

    if max_bytes == 0 {
        return TruncatedOutput {
            text: String::new(),
            truncated: true,
            original_bytes,
            omitted_bytes: original_bytes,
        };
    }

    let full_marker_max = format!("\n[... truncated {original_bytes} bytes ...]\n").len();
    let compact_marker_max = format!("[... truncated {original_bytes} bytes ...]").len();

    let (marker_budget, padded) = if max_bytes > full_marker_max {
        (full_marker_max, true)
    } else if max_bytes >= compact_marker_max {
        (compact_marker_max, false)
    } else {
        let end = floor_char_boundary(text, max_bytes);
        return TruncatedOutput {
            text: text[..end].to_string(),
            truncated: true,
            original_bytes,
            omitted_bytes: original_bytes - end,
        };
    };

    let avail = max_bytes - marker_budget;
    let head_budget = avail / 2;
    let tail_budget = avail - head_budget;

    let head_end = floor_char_boundary(text, head_budget);
    let tail_start = ceil_char_boundary(text, original_bytes.saturating_sub(tail_budget)).max(head_end);
    let omitted_bytes = tail_start.saturating_sub(head_end);

    let marker = if padded {
        format!("\n[... truncated {omitted_bytes} bytes ...]\n")
    } else {
        format!("[... truncated {omitted_bytes} bytes ...]")
    };

    let mut out = String::with_capacity(head_end + marker.len() + (original_bytes - tail_start));
    out.push_str(&text[..head_end]);
    out.push_str(&marker);
    out.push_str(&text[tail_start..]);

    TruncatedOutput {
        text: out,
        truncated: true,
        original_bytes,
        omitted_bytes,
    }
}

/// Memory-bounded streaming output collector preserving head + tail bytes up to `max_bytes`.
pub(crate) struct BoundedOutputBuffer {
    max_bytes: usize,
    head_cap: usize,
    tail_cap: usize,
    head: Vec<u8>,
    tail: VecDeque<u8>,
    overflowed: bool,
    total_bytes: usize,
}

impl BoundedOutputBuffer {
    pub(crate) fn new(max_bytes: usize) -> Self {
        let head_cap = max_bytes / 2;
        let tail_cap = max_bytes.saturating_sub(head_cap);
        Self {
            max_bytes,
            head_cap,
            tail_cap,
            head: Vec::new(),
            tail: VecDeque::new(),
            overflowed: false,
            total_bytes: 0,
        }
    }

    pub(crate) fn push(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        self.total_bytes = self.total_bytes.saturating_add(chunk.len());

        if self.max_bytes == 0 {
            self.overflowed = true;
            return;
        }

        if !self.overflowed {
            if self.head.len() + chunk.len() <= self.max_bytes {
                self.head.extend_from_slice(chunk);
                return;
            }
            // Transition to bounded head + rolling tail mode.
            self.overflowed = true;
            let excess = if self.head.len() > self.head_cap {
                self.head.split_off(self.head_cap)
            } else {
                Vec::new()
            };
            self.push_tail(&excess);
            self.push_tail(chunk);
        } else {
            self.push_tail(chunk);
        }
    }

    fn push_tail(&mut self, bytes: &[u8]) {
        if self.tail_cap == 0 || bytes.is_empty() {
            return;
        }
        if bytes.len() >= self.tail_cap {
            self.tail.clear();
            let start = bytes.len() - self.tail_cap;
            self.tail.extend(&bytes[start..]);
            return;
        }
        let needed = self.tail.len() + bytes.len();
        if needed > self.tail_cap {
            let drop_count = needed - self.tail_cap;
            self.tail.drain(..drop_count);
        }
        self.tail.extend(bytes);
    }

    /// Render captured bytes into UTF-8 text bounded by `budget` (at most `self.max_bytes`).
    pub(crate) fn render(&self, budget: usize) -> TruncatedOutput {
        let effective_max = budget.min(self.max_bytes);
        if !self.overflowed {
            let raw = String::from_utf8_lossy(&self.head);
            return truncate_output(&raw, effective_max);
        }

        if effective_max == 0 {
            return TruncatedOutput {
                text: String::new(),
                truncated: true,
                original_bytes: self.total_bytes,
                omitted_bytes: self.total_bytes,
            };
        }

        let head_clean = trim_incomplete_utf8_tail(&self.head);
        let tail_vec: Vec<u8> = self.tail.iter().copied().collect();
        let tail_clean = trim_incomplete_utf8_head(&tail_vec);

        let head_str = String::from_utf8_lossy(head_clean);
        let tail_str = String::from_utf8_lossy(tail_clean);

        let full_marker_max = format!("\n[... truncated {} bytes ...]\n", self.total_bytes).len();
        let compact_marker_max = format!("[... truncated {} bytes ...]", self.total_bytes).len();

        let (marker_budget, padded) = if effective_max > full_marker_max {
            (full_marker_max, true)
        } else if effective_max >= compact_marker_max {
            (compact_marker_max, false)
        } else {
            let end = floor_char_boundary(&head_str, effective_max);
            return TruncatedOutput {
                text: head_str[..end].to_string(),
                truncated: true,
                original_bytes: self.total_bytes,
                omitted_bytes: self.total_bytes.saturating_sub(end),
            };
        };

        let avail = effective_max - marker_budget;
        let head_budget = avail / 2;
        let tail_budget = avail - head_budget;

        let head_end = floor_char_boundary(&head_str, head_budget);
        let tail_start = ceil_char_boundary(&tail_str, tail_str.len().saturating_sub(tail_budget));

        let head_part = &head_str[..head_end];
        let tail_part = &tail_str[tail_start..];
        let kept_bytes = head_part.len() + tail_part.len();
        let omitted_bytes = self.total_bytes.saturating_sub(kept_bytes);

        let marker = if padded {
            format!("\n[... truncated {omitted_bytes} bytes ...]\n")
        } else {
            format!("[... truncated {omitted_bytes} bytes ...]")
        };

        let mut out = String::with_capacity(head_part.len() + marker.len() + tail_part.len());
        out.push_str(head_part);
        out.push_str(&marker);
        out.push_str(tail_part);

        TruncatedOutput {
            text: out,
            truncated: true,
            original_bytes: self.total_bytes,
            omitted_bytes,
        }
    }
}

/// Strip an incomplete UTF-8 multi-byte sequence from the end of a byte slice (up to 3 bytes).
fn trim_incomplete_utf8_tail(bytes: &[u8]) -> &[u8] {
    if bytes.is_empty() {
        return bytes;
    }
    let check_len = bytes.len().min(4);
    let tail = &bytes[bytes.len() - check_len..];
    match std::str::from_utf8(tail) {
        Ok(_) => bytes,
        Err(err) => {
            if err.error_len().is_none() {
                // Incomplete sequence at the very end; trim it cleanly.
                &bytes[..bytes.len() - check_len + err.valid_up_to()]
            } else {
                bytes
            }
        }
    }
}

/// Strip leading UTF-8 continuation bytes (`0b10xx_xxxx`) from the start of a byte slice.
fn trim_incomplete_utf8_head(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < bytes.len() && start < 4 && (bytes[start] & 0b1100_0000) == 0b1000_0000 {
        start += 1;
    }
    &bytes[start..]
}
