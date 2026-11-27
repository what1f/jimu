use std::collections::VecDeque;
#[cfg(any(
    feature = "fs",
    feature = "search",
    all(feature = "shell", not(target_os = "ios"))
))]
use std::path::Path;
use std::path::PathBuf;
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

#[cfg(all(
    windows,
    any(
        feature = "fs",
        feature = "search",
        all(feature = "shell", not(target_os = "ios"))
    )
))]
fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if let Some(rest) = raw.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = raw.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

#[cfg(any(
    feature = "fs",
    feature = "search",
    all(feature = "shell", not(target_os = "ios"))
))]
/// Format a path relative to `base_cwd` when possible, using forward slashes.
pub(crate) fn display_path(base_cwd: &Path, path: &Path) -> String {
    #[cfg(windows)]
    {
        if let Ok(rel) = path.strip_prefix(base_cwd) {
            return rel.to_string_lossy().replace('\\', "/");
        }
        let clean_base = strip_verbatim_prefix(base_cwd);
        let clean_path = strip_verbatim_prefix(path);
        let shown = clean_path.strip_prefix(&clean_base).unwrap_or(&clean_path);
        shown.to_string_lossy().replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        let shown = path.strip_prefix(base_cwd).unwrap_or(path);
        shown.to_string_lossy().replace('\\', "/")
    }
}

#[cfg(feature = "fs")]
fn is_symlink_or_junction(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt as _;
        let ft = meta.file_type();
        ft.is_symlink() || ft.is_symlink_dir()
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
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
        Ok(meta) if is_symlink_or_junction(&meta) => match fs::canonicalize(target) {
            Ok(real) => Ok(real),
            Err(err) if err.kind() == ErrorKind::NotFound => {
                // Dangling symlink: walk symlink hops so `write` creates the linked target
                // while preserving the symlink (and `edit` still reports missing target).
                let mut cur = target.to_path_buf();
                for _ in 0..MAX_SYMLINK_HOPS {
                    match fs::symlink_metadata(&cur) {
                        Ok(m) if is_symlink_or_junction(&m) => {
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
pub(crate) fn check_cancelled(cancel: &jimu_ai::CancelToken) -> Result<(), String> {
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
pub(crate) fn open_regular_file(
    path: &Path,
    shown: &str,
) -> Result<(fs::File, fs::Metadata), String> {
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
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: file owns the open descriptor throughout both calls.
        unsafe {
            let fd = file.as_raw_fd();
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) < 0 {
                return Err(format!(
                    "Could not open '{shown}': {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
    }
    Ok((file, meta))
}

#[cfg(feature = "fs")]
pub(crate) fn read_bounded_bytes(
    reader: impl std::io::Read,
    size: u64,
    shown: &str,
) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
    let size_error = || format!("Cannot read '{shown}': file exceeds 16 MiB");
    if size > MAX_FILE_BYTES {
        return Err(size_error());
    }
    let mut bytes = Vec::new();
    reader
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| format!("Could not read '{shown}': {err}"))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(size_error());
    }
    Ok(bytes)
}

#[cfg(feature = "patch")]
pub(crate) fn read_regular_file(
    path: &Path,
    shown: &str,
) -> Result<(Vec<u8>, fs::Metadata), String> {
    let (file, meta) = open_regular_file(path, shown)?;
    Ok((read_bounded_bytes(file, meta.len(), shown)?, meta))
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

/// Result of UTF-8-safe tool output truncation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncatedOutput {
    pub text: String,
    pub truncated: bool,
    pub original_bytes: usize,
    pub omitted_bytes: usize,
    pub full_output_path: Option<PathBuf>,
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

fn format_byte_limit(bytes: usize) -> String {
    if bytes >= 1024 && bytes.is_multiple_of(1024) {
        format!("{}KB", bytes / 1024)
    } else {
        format!("{bytes}B")
    }
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
            full_output_path: None,
        };
    }

    if max_bytes == 0 {
        return TruncatedOutput {
            text: String::new(),
            truncated: true,
            original_bytes,
            omitted_bytes: original_bytes,
            full_output_path: None,
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
            full_output_path: None,
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
        full_output_path: None,
    }
}

/// Truncate UTF-8 text from the head by both `max_lines` and `max_bytes` (whichever hits first).
pub fn truncate_head(text: &str, max_bytes: usize, max_lines: usize) -> TruncatedOutput {
    let original_bytes = text.len();
    if original_bytes == 0 {
        return TruncatedOutput {
            text: String::new(),
            truncated: false,
            original_bytes: 0,
            omitted_bytes: 0,
            full_output_path: None,
        };
    }
    if max_bytes == 0 || max_lines == 0 {
        return TruncatedOutput {
            text: String::new(),
            truncated: true,
            original_bytes,
            omitted_bytes: original_bytes,
            full_output_path: None,
        };
    }

    let total_lines = text.lines().count().max(1);
    if original_bytes <= max_bytes && total_lines <= max_lines {
        return TruncatedOutput {
            text: text.to_string(),
            truncated: false,
            original_bytes,
            omitted_bytes: 0,
            full_output_path: None,
        };
    }

    let limit_label = format_byte_limit(max_bytes);
    let notice_max = format!(
        "\n[Showing first {total_lines} of {total_lines} lines ({limit_label} limit, {original_bytes} bytes omitted)]"
    )
    .len();
    if max_bytes <= notice_max + 16 {
        return truncate_output(text, max_bytes);
    }

    let content_budget = max_bytes - notice_max;
    let mut kept_end = 0usize;
    let mut kept_lines = 0usize;
    for (idx, seg) in text.split_inclusive('\n').enumerate() {
        if idx >= max_lines {
            break;
        }
        if kept_end + seg.len() > content_budget {
            if kept_lines == 0 {
                kept_end = floor_char_boundary(seg, content_budget);
                kept_lines = usize::from(kept_end > 0);
            }
            break;
        }
        kept_end += seg.len();
        kept_lines += 1;
    }

    let head = text[..kept_end].trim_end_matches('\n');
    let omitted_bytes = original_bytes.saturating_sub(head.len());
    let notice = format!(
        "\n[Showing first {kept_lines} of {total_lines} lines ({limit_label} limit, {omitted_bytes} bytes omitted)]"
    );
    let mut out = String::with_capacity(head.len() + notice.len());
    out.push_str(head);
    out.push_str(&notice);

    TruncatedOutput {
        text: out,
        truncated: true,
        original_bytes,
        omitted_bytes,
        full_output_path: None,
    }
}

/// Truncate UTF-8 text keeping the tail (`max_lines` / `max_bytes`) and spilling the full text
/// to a `0600` temporary file when truncated (matching Pi's shell output behavior).
pub fn truncate_tail_spillover(text: &str, max_bytes: usize, max_lines: usize) -> TruncatedOutput {
    let mut buf = TailSpilloverBuffer::with_structured_cap(max_bytes, max_lines, 0);
    buf.push(text.as_bytes());
    buf.render(max_bytes)
}

const MAX_SPILL_FILE_BYTES: usize = 64 * 1024 * 1024;
#[cfg(not(target_os = "ios"))]
static NEXT_SPILL_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn open_spill_temp_file() -> Option<(std::fs::File, PathBuf)> {
    #[cfg(target_os = "ios")]
    {
        None
    }
    #[cfg(not(target_os = "ios"))]
    {
        use std::io::Write as _;
        use std::sync::atomic::Ordering;
        use std::time::{SystemTime, UNIX_EPOCH};

        #[cfg(unix)]
        let dir_name = format!("agent-tool-outputs-{}", unsafe { libc::geteuid() });
        #[cfg(not(unix))]
        let dir_name = "agent-tool-outputs".to_string();

        let dir = std::env::temp_dir().join(dir_name);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder.create(&dir).ok()?;
        }
        #[cfg(not(unix))]
        {
            std::fs::create_dir_all(&dir).ok()?;
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let seq = NEXT_SPILL_ID.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("output-{}-{now}-{seq}.log", std::process::id()));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&path).ok()?;
        let _ = file.flush();
        Some((file, path))
    }
}

/// Streaming shell output accumulator keeping a rolling tail for model context (`max_bytes` / `max_lines`),
/// a bounded structured prefix (`structured_cap`, default 1 MiB) for programmatic callers (codemode),
/// and spilling the full untruncated stream to a `0600` temp file when truncated.
pub(crate) struct TailSpilloverBuffer {
    max_bytes: usize,
    max_lines: usize,
    structured_cap: usize,
    structured_head: Vec<u8>,
    structured_overflowed: bool,
    initial: Vec<u8>,
    tail_cap: usize,
    tail: VecDeque<u8>,
    total_bytes: usize,
    newline_count: usize,
    ended_with_newline: bool,
    spilling: bool,
    spill_file: Option<std::fs::File>,
    spill_path: Option<PathBuf>,
    spilled_bytes: usize,
}

impl TailSpilloverBuffer {
    #[cfg(all(feature = "shell", not(target_os = "ios")))]
    pub(crate) fn new(max_bytes: usize, max_lines: usize) -> Self {
        Self::with_structured_cap(max_bytes, max_lines, crate::STRUCTURED_OUTPUT_MAX_BYTES)
    }

    pub(crate) fn with_structured_cap(
        max_bytes: usize,
        max_lines: usize,
        structured_cap: usize,
    ) -> Self {
        Self {
            max_bytes,
            max_lines,
            structured_cap,
            structured_head: Vec::new(),
            structured_overflowed: false,
            initial: Vec::new(),
            tail_cap: max_bytes.saturating_mul(2).max(4096),
            tail: VecDeque::new(),
            total_bytes: 0,
            newline_count: 0,
            ended_with_newline: false,
            spilling: false,
            spill_file: None,
            spill_path: None,
            spilled_bytes: 0,
        }
    }

    fn logical_lines(&self) -> usize {
        if self.total_bytes == 0 {
            0
        } else if self.ended_with_newline {
            self.newline_count
        } else {
            self.newline_count + 1
        }
    }

    pub(crate) fn push(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        self.total_bytes = self.total_bytes.saturating_add(chunk.len());
        self.newline_count = self
            .newline_count
            .saturating_add(chunk.iter().filter(|&&b| b == b'\n').count());
        self.ended_with_newline = chunk.last() == Some(&b'\n');

        if self.structured_cap > 0 && !self.structured_overflowed {
            let avail = self
                .structured_cap
                .saturating_sub(self.structured_head.len());
            let take = avail.min(chunk.len());
            self.structured_head.extend_from_slice(&chunk[..take]);
            if take < chunk.len() {
                self.structured_overflowed = true;
            }
        } else if self.structured_cap == 0 {
            self.structured_overflowed = true;
        }

        Self::push_tail(&mut self.tail, self.tail_cap, chunk);

        if !self.spilling {
            if self.max_bytes > 0
                && self.max_lines > 0
                && self.total_bytes <= self.max_bytes
                && self.logical_lines() <= self.max_lines
            {
                self.initial.extend_from_slice(chunk);
                return;
            }
            self.spilling = true;
            if let Some((file, path)) = open_spill_temp_file() {
                self.spill_file = Some(file);
                self.spill_path = Some(path);
                let prev = std::mem::take(&mut self.initial);
                self.write_spill(&prev);
                self.write_spill(chunk);
            } else {
                self.initial.clear();
            }
        } else {
            self.write_spill(chunk);
        }
    }

    fn write_spill(&mut self, bytes: &[u8]) {
        use std::io::Write as _;
        if bytes.is_empty() || self.spilled_bytes >= MAX_SPILL_FILE_BYTES {
            return;
        }
        if let Some(file) = &mut self.spill_file {
            let room = MAX_SPILL_FILE_BYTES - self.spilled_bytes;
            let slice = &bytes[..bytes.len().min(room)];
            if file.write_all(slice).is_ok() {
                self.spilled_bytes += slice.len();
            } else {
                self.spill_file = None;
            }
        }
    }

    fn push_tail(tail: &mut VecDeque<u8>, tail_cap: usize, bytes: &[u8]) {
        if tail_cap == 0 || bytes.is_empty() {
            return;
        }
        if bytes.len() >= tail_cap {
            tail.clear();
            tail.extend(&bytes[bytes.len() - tail_cap..]);
            return;
        }
        let needed = tail.len() + bytes.len();
        if needed > tail_cap {
            tail.drain(..(needed - tail_cap));
        }
        tail.extend(bytes);
    }

    #[cfg(all(feature = "shell", not(target_os = "ios")))]
    pub(crate) fn structured_prefix(&self) -> (std::borrow::Cow<'_, str>, bool) {
        if !self.spilling && !self.structured_overflowed {
            (String::from_utf8_lossy(&self.initial), false)
        } else {
            let bytes = trim_incomplete_utf8_tail(&self.structured_head);
            (
                String::from_utf8_lossy(bytes),
                self.structured_overflowed || self.total_bytes > self.structured_head.len(),
            )
        }
    }

    pub(crate) fn render(&self, budget: usize) -> TruncatedOutput {
        let effective_max = budget.min(self.max_bytes);
        let total_lines = self.logical_lines();

        if let Some(file) = &self.spill_file {
            use std::io::Write as _;
            let _ = (&mut &*file).flush();
        }

        if !self.spilling && self.total_bytes <= effective_max && total_lines <= self.max_lines {
            return TruncatedOutput {
                text: String::from_utf8_lossy(&self.initial).into_owned(),
                truncated: false,
                original_bytes: self.total_bytes,
                omitted_bytes: 0,
                full_output_path: None,
            };
        }

        let spill_path = self.spill_path.clone().or_else(|| {
            if !self.initial.is_empty() {
                let (mut file, path) = open_spill_temp_file()?;
                use std::io::Write as _;
                file.write_all(&self.initial).ok()?;
                file.flush().ok()?;
                Some(path)
            } else {
                None
            }
        });

        if effective_max == 0 || self.max_lines == 0 {
            return TruncatedOutput {
                text: String::new(),
                truncated: true,
                original_bytes: self.total_bytes,
                omitted_bytes: self.total_bytes,
                full_output_path: spill_path,
            };
        }

        let tail_vec: Vec<u8> = self.tail.iter().copied().collect();
        let tail_clean = trim_incomplete_utf8_head(&tail_vec);
        let tail_str = String::from_utf8_lossy(tail_clean);

        if let Some(path) = &spill_path {
            let limit_label = format_byte_limit(effective_max);
            let worst_notice = format!(
                "\n\n[Showing lines {total_lines}-{total_lines} of {total_lines} ({limit_label} limit). Full output: {}]",
                path.display()
            );
            if effective_max > worst_notice.len() + 24 {
                let content_budget = effective_max - worst_notice.len();
                let tail_slice =
                    take_tail_lines_and_bytes(&tail_str, content_budget, self.max_lines);
                let shown_lines = tail_slice.lines().count().max(1);
                let start_line = total_lines.saturating_sub(shown_lines) + 1;
                let notice = format!(
                    "\n\n[Showing lines {start_line}-{total_lines} of {total_lines} ({limit_label} limit). Full output: {}]",
                    path.display()
                );
                let omitted_bytes = self.total_bytes.saturating_sub(tail_slice.len());
                let mut out = String::with_capacity(tail_slice.len() + notice.len());
                out.push_str(tail_slice);
                out.push_str(&notice);
                return TruncatedOutput {
                    text: out,
                    truncated: true,
                    original_bytes: self.total_bytes,
                    omitted_bytes,
                    full_output_path: spill_path,
                };
            }
        }

        // Small-budget fallback (e.g. tight byte budgets where full temp path won't fit):
        // strictly honor `effective_max` while preserving the UTF-8 tail.
        let full_marker_max = format!("[... truncated {} bytes ...]\n", self.total_bytes).len();
        if effective_max > full_marker_max {
            let tail_budget = effective_max - full_marker_max;
            let tail_start =
                ceil_char_boundary(&tail_str, tail_str.len().saturating_sub(tail_budget));
            let tail_part = &tail_str[tail_start..];
            let omitted_bytes = self.total_bytes.saturating_sub(tail_part.len());
            let marker = format!("[... truncated {omitted_bytes} bytes ...]\n");
            let mut out = String::with_capacity(marker.len() + tail_part.len());
            out.push_str(&marker);
            out.push_str(tail_part);
            TruncatedOutput {
                text: out,
                truncated: true,
                original_bytes: self.total_bytes,
                omitted_bytes,
                full_output_path: spill_path,
            }
        } else {
            let tail_start =
                ceil_char_boundary(&tail_str, tail_str.len().saturating_sub(effective_max));
            let tail_part = &tail_str[tail_start..];
            let omitted_bytes = self.total_bytes.saturating_sub(tail_part.len());
            TruncatedOutput {
                text: tail_part.to_string(),
                truncated: true,
                original_bytes: self.total_bytes,
                omitted_bytes,
                full_output_path: spill_path,
            }
        }
    }
}

fn take_tail_lines_and_bytes(text: &str, max_bytes: usize, max_lines: usize) -> &str {
    if text.is_empty() || max_bytes == 0 || max_lines == 0 {
        return "";
    }
    let byte_floor = ceil_char_boundary(text, text.len().saturating_sub(max_bytes));
    let bounded = &text[byte_floor..];

    // If bounded had to cut mid-line and contains a newline with meaningful trailing content,
    // snap to the next newline so the first displayed line isn't a broken fragment.
    let snapped = if byte_floor > 0
        && text.as_bytes().get(byte_floor - 1) != Some(&b'\n')
        && let Some(nl) = bounded.find('\n')
        && !bounded[nl + 1..].trim().is_empty()
    {
        &bounded[nl + 1..]
    } else {
        bounded
    };

    // Enforce `max_lines` from the end.
    let bytes = snapped.as_bytes();
    let end = if bytes.last() == Some(&b'\n') {
        bytes.len().saturating_sub(1)
    } else {
        bytes.len()
    };
    let mut seen_lines = 0usize;
    for i in (0..end).rev() {
        if bytes[i] == b'\n' {
            seen_lines += 1;
            if seen_lines >= max_lines {
                return &snapped[i + 1..];
            }
        }
    }
    snapped
}

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

fn trim_incomplete_utf8_head(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < bytes.len() && start < 4 && (bytes[start] & 0b1100_0000) == 0b1000_0000 {
        start += 1;
    }
    &bytes[start..]
}
