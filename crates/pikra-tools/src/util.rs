#[cfg(all(feature = "shell", not(target_os = "ios")))]
use std::collections::VecDeque;
#[cfg(any(
    feature = "fs",
    feature = "search",
    all(feature = "shell", not(target_os = "ios"))
))]
use std::path::{Path, PathBuf};
#[cfg(feature = "fs")]
use std::{
    collections::HashMap,
    fs::{self, Permissions},
    io::{ErrorKind, Write},
    sync::{Arc, Mutex, OnceLock, Weak},
};

#[cfg(feature = "fs")]
static PATH_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();
#[cfg(feature = "fs")]
const MAX_SYMLINK_HOPS: usize = 40;

#[cfg(any(
    feature = "fs",
    feature = "search",
    all(feature = "shell", not(target_os = "ios"))
))]
/// Resolve a tool path relative to `base_cwd` (preserving absolute paths as-is).
pub(crate) fn resolve_path(base_cwd: &Path, raw: &str) -> Result<PathBuf, String> {
    if raw.contains('\0') {
        return Err("path contains a null character".into());
    }
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

#[cfg(any(
    feature = "fs",
    feature = "search",
    all(feature = "shell", not(target_os = "ios"))
))]
/// Format a path relative to `base_cwd` when possible, using forward slashes.
pub(crate) fn display_path(base_cwd: &Path, path: &Path) -> String {
    let shown = path.strip_prefix(base_cwd).unwrap_or(path);
    shown.to_string_lossy().replace('\\', "/")
}

#[cfg(feature = "fs")]
/// Canonicalize the nearest existing ancestor of `path` and re-attach remaining components
/// so per-path write locks serialize equivalent paths consistently.
fn normalize_nonexistent_path(path: &Path) -> PathBuf {
    let mut missing = Vec::new();
    let mut cur = path;
    loop {
        if let Ok(canon) = fs::canonicalize(cur) {
            let mut out = canon;
            for part in missing.into_iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (cur.file_name(), cur.parent()) {
            (Some(name), Some(parent)) if !parent.as_os_str().is_empty() => {
                missing.push(name.to_os_string());
                cur = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

#[cfg(feature = "fs")]
/// Resolve the real target file path before acquiring a write lock or renaming a temp file.
///
/// If `target` is an existing symlink, resolves it to the real target path so atomic
/// temp-file + `rename` writes into the resolved target's parent directory and preserves
/// the symlink intact.
pub(crate) fn resolve_write_target(target: &Path) -> Result<PathBuf, String> {
    match fs::symlink_metadata(target) {
        Ok(meta) if meta.file_type().is_symlink() => match fs::canonicalize(target) {
            Ok(real) => Ok(real),
            Err(err) if err.kind() == ErrorKind::NotFound => {
                // Dangling symlink: walk symlink hops so `write` creates the linked target
                // while preserving the symlink (and `edit` still reports missing target).
                let mut cur = target.to_path_buf();
                for _ in 0..MAX_SYMLINK_HOPS {
                    match fs::symlink_metadata(&cur) {
                        Ok(m) if m.file_type().is_symlink() => {
                            let link = fs::read_link(&cur).map_err(|e| {
                                format!("failed to read symlink '{}': {e}", cur.display())
                            })?;
                            cur = if link.is_absolute() {
                                link
                            } else {
                                cur.parent()
                                    .filter(|p| !p.as_os_str().is_empty())
                                    .unwrap_or_else(|| Path::new("."))
                                    .join(link)
                            };
                        }
                        Ok(_) => {
                            return fs::canonicalize(&cur).map_err(|e| {
                                format!("failed to resolve symlink '{}': {e}", target.display())
                            });
                        }
                        Err(e) if e.kind() == ErrorKind::NotFound => {
                            return Ok(normalize_nonexistent_path(&cur));
                        }
                        Err(e) => {
                            return Err(format!(
                                "failed to resolve symlink '{}': {e}",
                                target.display()
                            ));
                        }
                    }
                }
                Err(format!(
                    "failed to resolve symlink '{}': too many levels of symbolic links",
                    target.display()
                ))
            }
            Err(err) => Err(format!(
                "failed to resolve symlink '{}': {err}",
                target.display()
            )),
        },
        Ok(_) => fs::canonicalize(target)
            .map_err(|err| format!("Could not resolve '{}': {err}", target.display())),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(normalize_nonexistent_path(target)),
        Err(err) => Err(format!("Could not resolve '{}': {err}", target.display())),
    }
}

/// Return the shared lock for a resolved file path. Writers hold it through the final rename.
#[cfg(feature = "fs")]
pub(crate) fn path_lock(path: &Path) -> Arc<Mutex<()>> {
    let mut locks = PATH_LOCKS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    locks.retain(|_, weak| weak.strong_count() > 0);
    if let Some(existing) = locks.get(path).and_then(Weak::upgrade) {
        return existing;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(path.to_path_buf(), Arc::downgrade(&lock));
    lock
}

/// Atomically write to an already-resolved and locked target.
#[cfg(feature = "fs")]
pub(crate) fn atomic_write_file_resolved(target: &Path, bytes: &[u8]) -> Result<(), String> {
    let permissions = match fs::metadata(target) {
        Ok(meta) if meta.is_file() => Some(meta.permissions()),
        Ok(_) => {
            return Err(format!(
                "Cannot write to '{}': expected a regular file",
                target.display()
            ));
        }
        Err(err) if err.kind() == ErrorKind::NotFound => None,
        Err(err) => return Err(format!("Could not access '{}': {err}", target.display())),
    };
    stage_write(target, bytes, permissions)?
        .persist(target)
        .map(|_| ())
        .map_err(|err| format!("Could not write '{}': {}", target.display(), err.error))
}

#[cfg(feature = "fs")]
pub(crate) fn stage_write(
    target: &Path,
    bytes: &[u8],
    permissions: Option<Permissions>,
) -> Result<tempfile::NamedTempFile, String> {
    let parent = target
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let stage = || -> std::io::Result<_> {
        fs::create_dir_all(parent)?;
        let mut file = tempfile::Builder::new()
            .prefix(".agent-tmp-")
            .tempfile_in(parent)?;
        file.write_all(bytes)?;
        if let Some(permissions) = permissions {
            file.as_file().set_permissions(permissions)?;
        }
        Ok(file)
    };
    stage().map_err(|err| format!("Could not prepare '{}': {err}", target.display()))
}

#[cfg(any(feature = "fs", feature = "search"))]
pub(crate) fn check_cancelled(cancel: &pikra_ai::CancelToken) -> Result<(), String> {
    if cancel.is_cancelled() {
        Err("Operation aborted".into())
    } else {
        Ok(())
    }
}

/// Detect a file that uses CRLF consistently.
#[cfg(feature = "patch")]
pub(crate) fn uses_crlf(text: &str) -> bool {
    text.contains("\r\n")
        && text
            .match_indices('\n')
            .all(|(i, _)| i > 0 && text.as_bytes()[i - 1] == b'\r')
}

#[cfg(feature = "fs")]
pub(crate) fn read_regular_file(
    path: &Path,
    shown: &str,
) -> Result<(Vec<u8>, fs::Metadata), String> {
    use std::io::Read as _;

    const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|err| format!("Could not open '{shown}': {err}"))?;
    let meta = file
        .metadata()
        .map_err(|err| format!("Could not access '{shown}': {err}"))?;
    if !meta.is_file() {
        return Err(format!("Cannot read '{shown}': expected a regular file"));
    }
    let size_error = || format!("Cannot read '{shown}': file exceeds 16 MiB");
    if meta.len() > MAX_FILE_BYTES {
        return Err(size_error());
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| format!("Could not read '{shown}': {err}"))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(size_error());
    }
    Ok((bytes, meta))
}

#[cfg(feature = "patch")]
pub(crate) fn read_editable_text(
    path: &Path,
    shown: &str,
) -> Result<(String, Permissions), String> {
    let (bytes, meta) = read_regular_file(path, shown)?;
    if bytes.contains(&0) {
        return Err(format!("Cannot edit binary file '{shown}'"));
    }
    let text =
        String::from_utf8(bytes).map_err(|_| format!("Cannot edit non-UTF-8 file '{shown}'"))?;
    Ok((text, meta.permissions()))
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
    let tail_start =
        ceil_char_boundary(text, original_bytes.saturating_sub(tail_budget)).max(head_end);
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
#[cfg(all(feature = "shell", not(target_os = "ios")))]
pub(crate) struct BoundedOutputBuffer {
    max_bytes: usize,
    head_cap: usize,
    tail_cap: usize,
    head: Vec<u8>,
    tail: VecDeque<u8>,
    overflowed: bool,
    total_bytes: usize,
}

#[cfg(all(feature = "shell", not(target_os = "ios")))]
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
            // Transition to bounded head + rolling tail mode while filling `head` up to
            // `max_bytes` so small-budget renders (< compact marker length) still have a
            // full prefix available.
            self.overflowed = true;
            let need = self.max_bytes.saturating_sub(self.head.len());
            let take = need.min(chunk.len());
            self.head.extend_from_slice(&chunk[..take]);
            if self.head.len() > self.head_cap {
                Self::push_tail(&mut self.tail, self.tail_cap, &self.head[self.head_cap..]);
            }
            Self::push_tail(&mut self.tail, self.tail_cap, &chunk[take..]);
        } else {
            Self::push_tail(&mut self.tail, self.tail_cap, chunk);
        }
    }

    fn push_tail(tail: &mut VecDeque<u8>, tail_cap: usize, bytes: &[u8]) {
        if tail_cap == 0 || bytes.is_empty() {
            return;
        }
        if bytes.len() >= tail_cap {
            tail.clear();
            let start = bytes.len() - tail_cap;
            tail.extend(&bytes[start..]);
            return;
        }
        let needed = tail.len() + bytes.len();
        if needed > tail_cap {
            let drop_count = needed - tail_cap;
            tail.drain(..drop_count);
        }
        tail.extend(bytes);
    }

    pub(crate) fn prefix(&self) -> (std::borrow::Cow<'_, str>, bool) {
        let bytes = if self.overflowed {
            trim_incomplete_utf8_tail(&self.head)
        } else {
            &self.head
        };
        (String::from_utf8_lossy(bytes), self.overflowed)
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
#[cfg(all(feature = "shell", not(target_os = "ios")))]
fn trim_incomplete_utf8_tail(bytes: &[u8]) -> &[u8] {
    if bytes.is_empty() {
        return bytes;
    }
    let mut start = bytes.len() - 1;
    let floor = bytes.len().saturating_sub(4);
    while start > floor && (bytes[start] & 0b1100_0000) == 0b1000_0000 {
        start -= 1;
    }
    match std::str::from_utf8(&bytes[start..]) {
        Ok(_) => bytes,
        Err(err) if err.error_len().is_none() => &bytes[..start],
        Err(_) => bytes,
    }
}

/// Strip leading UTF-8 continuation bytes (`0b10xx_xxxx`) from the start of a byte slice.
#[cfg(all(feature = "shell", not(target_os = "ios")))]
fn trim_incomplete_utf8_head(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < bytes.len() && start < 4 && (bytes[start] & 0b1100_0000) == 0b1000_0000 {
        start += 1;
    }
    &bytes[start..]
}
