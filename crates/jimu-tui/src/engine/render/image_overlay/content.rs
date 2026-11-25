use std::path::Path;

use ratatui::buffer::Buffer;
use ratatui::style::Style;
use ratatui::text::Span;

use super::PastedImage;
use crate::engine::render::SafeBuf;

pub(super) fn paint_path_line(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    path: &Path,
    style: Style,
    path_label: &str,
) {
    let raw = path.display().to_string();
    let label = format!(
        "{}: {}",
        path_label,
        truncate_path_for_overlay(
            &raw,
            (width as usize).saturating_sub(unicode_width::UnicodeWidthStr::width(path_label) + 2)
        )
    );
    let clipped = crate::engine::render::line_utils::truncate_str(&label, width as usize);
    buf.set_span_safe(x, y, &Span::styled(clipped, style), width);
}

pub(super) fn build_meta_line(image: &PastedImage, display_path: Option<&Path>) -> String {
    let mut parts = Vec::with_capacity(4);
    parts.push(format_mime(&image.mime_type));
    if let Some((width, height)) = image.dimensions {
        parts.push(format!("{}x{}", width, height));
    }
    parts.push(format_bytes(image.byte_len as u64));
    if let Some(path) = display_path
        && let Some(name) = path.file_name()
    {
        parts.push(name.to_string_lossy().into_owned());
    }
    parts.join(" \u{00b7} ")
}

pub(super) fn format_mime(mime: &str) -> String {
    match mime {
        "image/png" => "PNG".into(),
        "image/jpeg" => "JPEG".into(),
        "image/tiff" => "TIFF".into(),
        "image/gif" => "GIF".into(),
        "image/webp" => "WebP".into(),
        "image/bmp" => "BMP".into(),
        other => other.into(),
    }
}

pub(super) fn truncate_path_for_overlay(path: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let char_count = path.chars().count();
    if char_count <= max_chars {
        return path.to_owned();
    }
    if max_chars <= 3 {
        return path.chars().take(max_chars).collect();
    }
    let keep = max_chars.saturating_sub(3) / 2;
    let end_keep = max_chars.saturating_sub(3) - keep;
    let chars: Vec<char> = path.chars().collect();
    let head: String = chars.iter().take(keep).collect();
    let tail: String = match chars.len().checked_sub(end_keep) {
        Some(start) => chars.iter().skip(start).collect(),
        None => String::new(),
    };
    format!("{head}...{tail}")
}
pub fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    const UNITS: &[&str] = &["KB", "MB", "GB", "TB", "PB"];
    let mut val = bytes as f64 / 1024.0;
    for unit in UNITS {
        if val < 1023.95 {
            return format!("{val:.1} {unit}");
        }
        val /= 1024.0;
    }
    format!("{val:.1} EB")
}
