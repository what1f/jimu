//! Paths and geometry for media referenced in scrollback.

use std::path::PathBuf;

/// Metadata for inline media rendering in the scrollback.
/// Returned by blocks that want to display media inline.
#[derive(Debug, Clone)]
pub struct InlineMediaInfo {
    /// Path to the media file.
    pub path: PathBuf,
    /// Pixel dimensions of the media.
    pub width: u32,
    pub height: u32,
    /// Whether this is a video (poster frame) vs. a static image.
    pub is_video: bool,
    /// Alt text / description from the markdown `![alt](path)` syntax.
    pub alt_text: String,
}

const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "tiff", "tif"];

fn strip_verbatim_prefix(path: &std::path::Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

/// Decode and validate the preview when an image codec is available.
#[cfg(any(feature = "inline-image", feature = "clipboard-image"))]
pub fn decode_image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    use std::io::Cursor;
    let decoded = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    Some((decoded.width(), decoded.height()))
}

#[cfg(not(any(feature = "inline-image", feature = "clipboard-image")))]
pub fn decode_image_dimensions(_bytes: &[u8]) -> Option<(u32, u32)> {
    None
}

/// An image file referenced in scrollback content via `![alt](path)` markdown or a bare absolute path.
/// Validated on construction: the path must exist, have a recognized image extension, and decode successfully.
#[derive(Debug, Clone)]
pub struct ScrollbackImageRef {
    /// Absolute path to the image file on disk.
    pub path: PathBuf,
    /// Pixel dimensions `(width, height)`, decoded on construction.
    pub dimensions: Option<(u32, u32)>,
    /// Alt text from the `![alt](path)` markdown syntax (empty for bare paths).
    pub alt_text: String,
}

impl ScrollbackImageRef {
    /// Construct from a file path.
    /// Returns `None` if the path doesn't exist, isn't a file, lacks a recognized image extension, or can't be decoded as an image.
    pub fn from_path(path: impl Into<PathBuf>) -> Option<Self> {
        Self::from_path_with_alt(path, String::new())
    }

    /// Construct with alt text from the markdown `![alt](path)` syntax.
    pub fn from_path_with_alt(path: impl Into<PathBuf>, alt_text: String) -> Option<Self> {
        let path = strip_verbatim_prefix(&path.into());
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        if !IMAGE_EXTENSIONS.contains(&ext.as_str()) {
            return None;
        }
        if !path.is_file() {
            return None;
        }
        if !cfg!(any(feature = "inline-image", feature = "clipboard-image")) {
            return None;
        }
        let bytes = std::fs::read(&path).ok()?;
        let dimensions = Some(decode_image_dimensions(&bytes)?);
        Some(Self {
            path,
            dimensions,
            alt_text,
        })
    }
}

/// Regex pattern for `![alt](path)`: captures alt text (group 1) and path (group 2).
const MARKDOWN_IMAGE_REF_PATTERN: &str = r"!\[([^\]]*)\]\(([^)\s]+)\)";

/// Unresolved or undecodable paths are not counted, so a broken ref cannot look media-only.
pub fn is_media_only_markdown(text: &str, resolved_ref_count: usize) -> bool {
    use std::sync::LazyLock;

    if resolved_ref_count == 0 {
        return false;
    }

    static ONLY_MD_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(&format!(r"^\s*(?:{}\s*)+$", MARKDOWN_IMAGE_REF_PATTERN)).unwrap()
    });
    static MD_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(MARKDOWN_IMAGE_REF_PATTERN).unwrap());

    if !ONLY_MD_RE.is_match(text) {
        return false;
    }

    let unique_ref_count = MD_RE
        .captures_iter(text)
        .filter_map(|cap| cap.get(2).map(|m| m.as_str()))
        .collect::<std::collections::HashSet<_>>()
        .len();
    unique_ref_count == resolved_ref_count
}

/// Markdown or bare absolute paths that exist and decode as an image.
pub fn extract_image_refs(text: &str) -> Vec<ScrollbackImageRef> {
    use std::sync::LazyLock;

    static MD_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(MARKDOWN_IMAGE_REF_PATTERN).unwrap());

    static PATH_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        let exts = IMAGE_EXTENSIONS.join("|");
        // Unix absolute paths (/...) and Windows absolute paths (C:\..., C:/..., or \\... UNC). Drive letters accept either separator.
        regex::Regex::new(&format!(
            r"(?:^|[\s,])((?:/|[A-Za-z]:[\\/]|\\\\)[^\s,]+\.(?:{exts}))(?:[\s,.(]|$)"
        ))
        .unwrap()
    });

    let mut refs = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for cap in MD_RE.captures_iter(text) {
        if let Some(m) = cap.get(2) {
            let path_str = m.as_str();
            let alt_text = cap
                .get(1)
                .map(|a| a.as_str().to_owned())
                .unwrap_or_default();
            if seen.insert(path_str.to_owned())
                && let Some(r) = ScrollbackImageRef::from_path_with_alt(path_str, alt_text)
            {
                refs.push(r);
            }
        }
    }

    for cap in PATH_RE.captures_iter(text) {
        if let Some(m) = cap.get(1) {
            let path_str = m.as_str();
            if seen.insert(path_str.to_owned())
                && let Some(r) = ScrollbackImageRef::from_path(path_str)
            {
                refs.push(r);
            }
        }
    }

    refs
}

// -------------------------------------------------------------------------
// Scrollback video references
// -------------------------------------------------------------------------

const VIDEO_EXTENSIONS: &[&str] = &["mp4", "webm", "mov", "avi", "mkv"];

/// A video file referenced in scrollback content via `![alt](path.mp4)` markdown or a bare absolute path.
/// Validated on construction: the path must exist and have a recognized video extension.
#[derive(Debug, Clone)]
pub struct ScrollbackVideoRef {
    /// Absolute path to the video file on disk.
    pub path: PathBuf,
    /// Alt text from the `![alt](path)` markdown syntax (empty for bare paths).
    pub alt_text: String,
}

impl ScrollbackVideoRef {
    /// Validate that `path` exists and has a recognized video extension.
    pub fn from_path(path: impl Into<PathBuf>) -> Option<Self> {
        Self::from_path_with_alt(path, String::new())
    }

    /// Construct with alt text from the markdown `![alt](path)` syntax.
    pub fn from_path_with_alt(path: impl Into<PathBuf>, alt_text: String) -> Option<Self> {
        let path = strip_verbatim_prefix(&path.into());
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        if !VIDEO_EXTENSIONS.contains(&ext.as_str()) || !path.is_file() {
            return None;
        }
        Some(Self { path, alt_text })
    }
}

/// Extract video references from text (markdown `![](path)` and bare paths).
pub fn extract_video_refs(text: &str) -> Vec<ScrollbackVideoRef> {
    use std::sync::LazyLock;

    // Reuse the markdown image ref pattern; video_gen uses ![prompt](path.mp4)
    static MD_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(MARKDOWN_IMAGE_REF_PATTERN).unwrap());

    static VIDEO_PATH_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        let exts = VIDEO_EXTENSIONS.join("|");
        // Unix absolute paths (/...) and Windows absolute paths (C:\..., C:/..., or \\... UNC). Drive letters accept either separator.
        regex::Regex::new(&format!(
            r"(?:^|[\s,])((?:/|[A-Za-z]:[\\/]|\\\\)[^\s,]+\.(?:{exts}))(?:[\s,.(]|$)"
        ))
        .unwrap()
    });

    let mut refs = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // MD_RE captures the alt text (group 1) and the path (group 2)
    for cap in MD_RE.captures_iter(text) {
        if let Some(m) = cap.get(2) {
            let path_str = m.as_str();
            let alt_text = cap
                .get(1)
                .map(|a| a.as_str().to_owned())
                .unwrap_or_default();
            if seen.insert(path_str.to_owned())
                && let Some(r) = ScrollbackVideoRef::from_path_with_alt(path_str, alt_text)
            {
                refs.push(r);
            }
        }
    }

    // VIDEO_PATH_RE captures only the path (group 1); there is no alt text
    for cap in VIDEO_PATH_RE.captures_iter(text) {
        if let Some(m) = cap.get(1) {
            let path_str = m.as_str();
            if seen.insert(path_str.to_owned())
                && let Some(r) = ScrollbackVideoRef::from_path(path_str)
            {
                refs.push(r);
            }
        }
    }

    refs
}

pub(crate) fn scrollback_inline_overlay_active() -> bool {
    cfg!(feature = "inline-image")
        && crate::engine::terminal::image::scrollback_inline_overlay_active()
}

pub fn ffmpeg_available() -> bool {
    if !cfg!(feature = "inline-image") {
        return false;
    }
    use std::sync::atomic::{AtomicBool, Ordering};

    static FOUND: AtomicBool = AtomicBool::new(false);
    if FOUND.load(Ordering::Relaxed) {
        return true;
    }

    let available = is_command_available("ffmpeg");
    if available {
        FOUND.store(true, Ordering::Relaxed);
    }
    available
}

fn ffmpeg_install_candidates() -> &'static [(&'static str, &'static str)] {
    if cfg!(target_os = "macos") {
        &[("brew", "! brew install ffmpeg")]
    } else if cfg!(target_os = "windows") {
        &[
            ("winget", "! winget install ffmpeg"),
            ("choco", "! choco install ffmpeg"),
            ("scoop", "! scoop install ffmpeg"),
        ]
    } else {
        &[
            ("apt", "! sudo apt install ffmpeg"),
            ("apt-get", "! sudo apt-get install ffmpeg"),
            ("dnf", "! sudo dnf install ffmpeg"),
            ("pacman", "! sudo pacman -S ffmpeg"),
            ("zypper", "! sudo zypper install ffmpeg"),
            ("apk", "! sudo apk add ffmpeg"),
        ]
    }
}

pub fn ffmpeg_install_cmd() -> Option<&'static str> {
    use std::sync::OnceLock;
    static FOUND: OnceLock<&'static str> = OnceLock::new();
    if let Some(cmd) = FOUND.get() {
        return Some(*cmd);
    }
    for (manager, cmd) in ffmpeg_install_candidates() {
        if is_command_available(manager) {
            let _ = FOUND.set(*cmd);
            return Some(*cmd);
        }
    }
    None
}

fn ffmpeg_hint_banner_rows() -> u16 {
    if ffmpeg_install_cmd().is_some() { 2 } else { 1 }
}

pub fn inline_media_reserved_rows(info: &InlineMediaInfo, content_width: u16) -> (u16, u16) {
    use crate::engine::terminal::image::fit_image_to_cells;

    if info.is_video && !ffmpeg_available() {
        let banner_rows = ffmpeg_hint_banner_rows();
        return (banner_rows, banner_rows + 1);
    }
    let max_cols = content_width.saturating_sub(2);
    let max_rows = (content_width / 2).clamp(4, 20);
    let (_cols, rows) = fit_image_to_cells(info.width, info.height, max_cols, max_rows);
    (rows, rows + 3)
}

fn is_command_available(name: &str) -> bool {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .any(|path| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                path.join(name)
                    .metadata()
                    .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            }
            #[cfg(windows)]
            {
                path.join(format!("{name}.exe")).is_file()
            }
            #[cfg(not(any(unix, windows)))]
            {
                path.join(name).is_file()
            }
        })
}
