//! Clipboard image probing, local image file-path paste detection, validation/resizing,
//! and `[Image #N]` transcript placeholder formatting.

#[cfg(feature = "clipboard-image")]
use crate::{
    engine::clipboard::{
        self,
        resize::{FilterType, ReEncodeParams, re_encode_under_limit},
        validate::validate_image_bytes,
    },
    ui::views::prompt_widget::{PastedImage, PromptWidget},
};
#[cfg(feature = "clipboard-image")]
use std::path::{Path, PathBuf};

/// Max raw encoded image size sent to model APIs (5 MiB).
#[cfg(feature = "clipboard-image")]
const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
/// Max file size read from disk before decoding (50 MiB) to guard against OOM.
#[cfg(feature = "clipboard-image")]
const MAX_FILE_READ_BYTES: u64 = 50 * 1024 * 1024;

#[cfg(feature = "clipboard-image")]
const REENCODE_PARAMS: ReEncodeParams = ReEncodeParams {
    max_bytes: MAX_IMAGE_BYTES,
    max_side_px: 2048,
    max_pixels: 4_000_000,
    min_side_px: 256,
    quality_steps: &[90, 82, 74, 65, 55],
    filter: FilterType::CatmullRom,
};

/// Build user-visible transcript text that preserves `[Image #1]`..`[Image #N]` placeholders
/// for attached images without mutating the clean text sent to the model.
pub(super) fn format_user_display(text: &str, image_count: usize) -> String {
    if image_count == 0 {
        return text.to_owned();
    }
    let chips: String = (1..=image_count)
        .map(|n| format!("[Image #{n}]"))
        .collect::<Vec<_>>()
        .join(" ");
    let trimmed = text.trim();
    if trimmed.is_empty() {
        chips
    } else {
        format!("{chips} {trimmed}")
    }
}

/// Strip leading `[Image #N]` placeholder tokens without altering newlines, internal whitespace,
/// or literal `[Image #N]` tokens inside the user's message body.
pub(super) fn strip_leading_image_chips(text: &str) -> &str {
    let mut rest = text;
    let mut stripped_any = false;
    loop {
        let candidate = if stripped_any {
            rest.strip_prefix(' ').unwrap_or(rest)
        } else {
            rest
        };
        let Some(after_prefix) = candidate.strip_prefix("[Image #") else {
            break;
        };
        let digits = after_prefix
            .bytes()
            .take_while(|b| b.is_ascii_digit())
            .count();
        if digits == 0 {
            break;
        }
        let Some(after_chip) = after_prefix[digits..].strip_prefix(']') else {
            break;
        };
        rest = after_chip;
        stripped_any = true;
    }
    if stripped_any {
        rest.strip_prefix(' ').unwrap_or(rest)
    } else {
        text
    }
}

/// Remove `[Image #N]` placeholder tokens while preserving newlines and internal whitespace
/// for pending-bubble deduplication.
pub(super) fn strip_image_chips_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let needle = b"[Image #";
    let mut pos = 0;
    while pos < bytes.len() {
        if bytes[pos..].starts_with(needle) {
            let mut idx = pos + needle.len();
            while idx < bytes.len() && bytes[idx].is_ascii_digit() {
                idx += 1;
            }
            if idx > pos + needle.len() && idx < bytes.len() && bytes[idx] == b']' {
                pos = idx + 1;
                if pos < bytes.len() && bytes[pos] == b' ' {
                    pos += 1;
                }
                continue;
            }
        }
        let ch = text[pos..].chars().next().unwrap();
        out.push(ch);
        pos += ch.len_utf8();
    }
    out.trim().to_owned()
}

#[cfg(feature = "clipboard-image")]
fn has_image_extension(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff"
    )
}

#[cfg(feature = "clipboard-image")]
fn parse_file_uri(token: &str) -> Option<PathBuf> {
    if !token.starts_with("file://") {
        return None;
    }
    let url = url::Url::parse(token).ok()?;
    url.to_file_path().ok()
}

/// Split a shell-escaped or newline-delimited paste payload into candidate path tokens.
/// Supports `'...'`, `"..."`, and backslash-escaped spaces (`\ `).
#[cfg(feature = "clipboard-image")]
fn split_path_tokens(raw: &str, base_dir: Option<&Path>) -> Vec<String> {
    let mut tokens = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Fast path: the entire trimmed line is directly an existing image path (e.g. unescaped spaces on a single line).
        if let Some(direct) = resolve_candidate_path(trimmed, base_dir)
            && has_image_extension(&direct)
            && direct.is_file()
        {
            tokens.push(trimmed.to_owned());
            continue;
        }
        let mut current = String::new();
        let mut chars = trimmed.chars().peekable();
        let mut quote: Option<char> = None;
        while let Some(ch) = chars.next() {
            match quote {
                Some(q) if ch == q => quote = None,
                Some('"') if ch == '\\' => {
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                }
                Some(_) => current.push(ch),
                None => match ch {
                    '\'' | '"' => quote = Some(ch),
                    '\\' => {
                        if let Some(next) = chars.next() {
                            current.push(next);
                        }
                    }
                    c if c.is_whitespace() => {
                        if !current.is_empty() {
                            tokens.push(std::mem::take(&mut current));
                        }
                    }
                    _ => current.push(ch),
                },
            }
        }
        if !current.is_empty() {
            tokens.push(current);
        }
    }
    tokens
}

#[cfg(feature = "clipboard-image")]
fn resolve_candidate_path(token: &str, base_dir: Option<&Path>) -> Option<PathBuf> {
    let unquoted = token
        .trim()
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .or_else(|| {
            token
                .trim()
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
        })
        .unwrap_or_else(|| token.trim());
    if unquoted.is_empty() {
        return None;
    }
    if let Some(uri_path) = parse_file_uri(unquoted) {
        return Some(uri_path);
    }
    let expanded = shellexpand::tilde(unquoted);
    let path = PathBuf::from(expanded.as_ref());
    if path.is_relative()
        && let Some(dir) = base_dir
    {
        return Some(dir.join(path));
    }
    Some(path)
}

#[cfg(feature = "clipboard-image")]
fn normalize_image_bytes(
    bytes: Vec<u8>,
    source_path: Option<PathBuf>,
) -> Result<PastedImage, String> {
    let (w, h, mime) = validate_image_bytes(&bytes).map_err(|e| format!("Invalid image: {e}"))?;
    let needs_reencode = bytes.len() > MAX_IMAGE_BYTES
        || REENCODE_PARAMS.exceeds_dimension_caps(w, h)
        || matches!(mime, "image/bmp" | "image/tiff");
    let (final_bytes, final_w, final_h, final_mime) = if needs_reencode {
        let decoded =
            image::load_from_memory(&bytes).map_err(|e| format!("Failed to decode image: {e}"))?;
        let (encoded, rw, rh, rmime) = re_encode_under_limit(&decoded, &REENCODE_PARAMS)
            .map_err(|e| format!("Image too large to attach: {e}"))?;
        (encoded, rw, rh, rmime)
    } else {
        (bytes, w, h, mime)
    };
    Ok(PastedImage::new(
        final_mime,
        final_bytes,
        Some((final_w, final_h)),
        source_path,
    ))
}

/// Result of loading a batch of pasted image files or clipboard attachments.
#[cfg(feature = "clipboard-image")]
pub(super) struct PastedImageBatch {
    pub images: Vec<PastedImage>,
    pub warning: Option<String>,
}

/// Attempt to interpret a pasted text payload purely as 1 or more existing local image file paths.
///
/// Relative paths are resolved against `base_dir` (when provided). Pre-truncates the batch to
/// `remaining_cap` before decoding files and preserves valid images when a later file in the same
/// batch is corrupt.
///
/// Returns `None` if the payload is not purely image file paths (caller falls back to normal text
/// paste).
#[cfg(feature = "clipboard-image")]
pub(super) fn try_load_pasted_image_paths(
    raw: &str,
    base_dir: Option<&Path>,
    remaining_cap: usize,
) -> Option<PastedImageBatch> {
    let tokens = split_path_tokens(raw, base_dir);
    if tokens.is_empty() {
        return None;
    }
    let mut paths = Vec::with_capacity(tokens.len());
    for token in &tokens {
        let path = resolve_candidate_path(token, base_dir)?;
        if !has_image_extension(&path) || !path.is_file() {
            return None;
        }
        paths.push(path);
    }

    if remaining_cap == 0 {
        return Some(PastedImageBatch {
            images: Vec::new(),
            warning: Some(PromptWidget::cap_reached_toast()),
        });
    }
    let mut warning: Option<String> = None;
    if paths.len() > remaining_cap {
        let skipped = paths.len() - remaining_cap;
        paths.truncate(remaining_cap);
        warning = Some(format!(
            "{} ({skipped} skipped)",
            PromptWidget::cap_reached_toast()
        ));
    }

    let mut loaded = Vec::with_capacity(paths.len());
    for path in paths {
        let load_one = || -> Result<PastedImage, String> {
            let meta =
                std::fs::metadata(&path).map_err(|e| format!("Failed to read image: {e}"))?;
            if meta.len() > MAX_FILE_READ_BYTES {
                return Err("Image file is too large (max 50 MB)".into());
            }
            let bytes = std::fs::read(&path).map_err(|e| format!("Failed to read image: {e}"))?;
            normalize_image_bytes(bytes, Some(path.clone()))
        };
        match load_one() {
            Ok(img) => loaded.push(img),
            Err(err) => {
                if warning.is_none() {
                    warning = Some(err);
                }
            }
        }
    }
    Some(PastedImageBatch {
        images: loaded,
        warning,
    })
}

/// Probe the system clipboard for raster image data or copied image file URLs.
///
/// Returns `None` when no image is present so the caller can fall back to text paste.
#[cfg(feature = "clipboard-image")]
pub(super) fn probe_clipboard_images(
    clipboard_text: Option<&str>,
    base_dir: Option<&Path>,
    remaining_cap: usize,
) -> Option<PastedImageBatch> {
    if let Some(text) = clipboard_text
        && let Some(loaded) = try_load_pasted_image_paths(text, base_dir, remaining_cap)
    {
        return Some(loaded);
    }
    match clipboard::system_clipboard_probe_attachments(clipboard_text) {
        Ok((Some(image_data), _)) => {
            if remaining_cap == 0 {
                return Some(PastedImageBatch {
                    images: Vec::new(),
                    warning: Some(PromptWidget::cap_reached_toast()),
                });
            }
            Some(match normalize_image_bytes(image_data.data, None) {
                Ok(img) => PastedImageBatch {
                    images: vec![img],
                    warning: None,
                },
                Err(err) => PastedImageBatch {
                    images: Vec::new(),
                    warning: Some(err),
                },
            })
        }
        Ok((None, Some(file_urls))) => {
            try_load_pasted_image_paths(&file_urls, base_dir, remaining_cap)
        }
        Ok((None, None)) => None,
        Err(reason) => {
            if clipboard_text.is_some_and(|t| !t.is_empty()) {
                None
            } else {
                Some(PastedImageBatch {
                    images: Vec::new(),
                    warning: Some(format!("Clipboard image read failed ({reason:?})")),
                })
            }
        }
    }
}
