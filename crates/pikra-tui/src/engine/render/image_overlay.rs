//! Image preview overlay for prompt image chips.
//!
//! Renders a bordered popup when the cursor is on (or right after) an image chip, or when the chip is hovered.
//! Content follows a 2×2 matrix:
//!
//! |                    | Has filepath              | No filepath                |
//! |--------------------|---------------------------|----------------------------|
//! | **Pixels available** | Image + path footer       | Image only                 |
//! | **Pixels unavailable** | Metadata + path         | Metadata only              |
//!
//! The prompt bar chip itself never shows the path (`[Image #N]`); paths appear only here.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget, Wrap};

pub mod image;
pub mod overlay;
use content::{
    build_meta_line, format_bytes, format_mime, paint_path_line, truncate_path_for_overlay,
};
use image as terminal_image;

#[derive(Debug)]
pub enum Preview {
    Pending,
    Failed,
    Ready(Vec<u8>),
}

#[derive(Debug)]
pub struct PastedImage {
    pub display_number: usize,
    pub mime_type: String,
    pub source_path: Option<std::path::PathBuf>,
    pub byte_len: usize,
    pub dimensions: Option<(u32, u32)>,
    pub preview: Preview,
    owner_id: u64,
}

impl PastedImage {
    /// Allocate a placement identity for one newly pasted image. Keep it across redraws.
    pub fn new(
        display_number: usize,
        mime_type: String,
        source_path: Option<std::path::PathBuf>,
        byte_len: usize,
        dimensions: Option<(u32, u32)>,
        preview: Preview,
    ) -> Self {
        Self {
            display_number,
            mime_type,
            source_path,
            byte_len,
            dimensions,
            preview,
            owner_id: overlay::next_owner_id(),
        }
    }
}

/// Labels are supplied by the host in the user's language.
pub struct ImageLabels<'a> {
    pub image: &'a str,
    pub format: &'a str,
    pub dimensions: &'a str,
    pub size: &'a str,
    pub path: &'a str,
    pub unavailable: &'a str,
    pub pending: &'a str,
    pub loading: &'a str,
}

mod content;
mod geometry;

use geometry::{
    MIN_BOX_WIDTH, MIN_META_BOX_HEIGHT, MIN_PIXEL_BOX_HEIGHT, overlay_geometry, plan_image_preview,
};

/// Render an image preview overlay and return any post-flush pixel escapes.
pub fn render_image_overlay(
    buf: &mut Buffer,
    area: Rect,
    image: &PastedImage,
    bg: Color,
    text_fg: Color,
    border_fg: Color,
    labels: &ImageLabels<'_>,
) -> Option<overlay::Escapes> {
    if area.width < MIN_BOX_WIDTH {
        return None;
    }

    let theme = crate::engine::theme::Theme::current();
    let protocol = terminal_image::prompt_preview_graphics_protocol();
    let plan = plan_image_preview(image, protocol);
    let min_height = if plan.show_pixels {
        MIN_PIXEL_BOX_HEIGHT
    } else {
        MIN_META_BOX_HEIGHT
    };
    if area.height < min_height {
        return None;
    }
    let geometry = overlay_geometry(
        area,
        plan.show_pixels,
        plan.display_path.is_some(),
        image.dimensions.unwrap_or((640, 480)),
    )?;
    let overlay_rect = geometry.overlay_rect;

    crate::engine::render::color::dim_area(buf, area, theme.bg_base, 0.5);

    ratatui::widgets::Clear.render(overlay_rect, buf);
    buf.set_style(overlay_rect, Style::default().fg(text_fg).bg(bg));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_fg).bg(bg))
        .style(Style::default().bg(bg));
    let inner = block.inner(overlay_rect);
    block.render(overlay_rect, buf);

    let title_text = format!(" {} #{} ", labels.image, image.display_number);
    let meta = build_meta_line(image, plan.display_path);
    let full_title = if meta.len() + title_text.len() + 6 < overlay_rect.width as usize {
        format!("{}\u{2500} {} ", title_text, meta)
    } else {
        title_text.clone()
    };
    let title_style = Style::default()
        .fg(text_fg)
        .bg(bg)
        .add_modifier(ratatui::style::Modifier::BOLD);
    let title_width = unicode_width::UnicodeWidthStr::width(full_title.as_str())
        .min(usize::from(u16::MAX)) as u16;
    let title_x = overlay_rect.x + (overlay_rect.width.saturating_sub(title_width)) / 2;
    buf.set_span(
        title_x,
        overlay_rect.y,
        &Span::styled(&full_title, title_style),
        title_width,
    );

    if inner.width == 0 || inner.height == 0 {
        return None;
    }

    // Reserve the footer so a pixel placement cannot cover the path.
    let path_footer = plan.display_path.filter(|_| inner.height >= 2);
    let image_inner = if let Some(path) = path_footer {
        let footer_y = inner.y + inner.height - 1;
        paint_path_line(
            buf,
            inner.x,
            footer_y,
            inner.width,
            path,
            Style::default().fg(text_fg).bg(bg),
            labels.path,
        );
        Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: inner.height.saturating_sub(1),
        }
    } else {
        inner
    };

    if !plan.show_pixels {
        let mut lines = Vec::new();
        lines.push(Line::from(format!(
            "{}: {}",
            labels.format,
            format_mime(&image.mime_type)
        )));
        if let Some((w, h)) = image.dimensions {
            lines.push(Line::from(format!("{}: {} x {}", labels.dimensions, w, h)));
        }
        let status = if matches!(image.preview, Preview::Failed) {
            Some(labels.unavailable)
        } else if matches!(image.preview, Preview::Pending) && protocol.supports_images() {
            Some(labels.pending)
        } else {
            None
        };
        lines.push(Line::from(status.map(str::to_owned).unwrap_or_else(|| {
            format!("{}: {}", labels.size, format_bytes(image.byte_len as u64))
        })));
        // Short boxes need the path in the body because no footer fits.
        if path_footer.is_none()
            && let Some(path) = plan.display_path
        {
            lines.push(Line::from(format!(
                "{}: {}",
                labels.path,
                truncate_path_for_overlay(&path.display().to_string(), inner.width as usize)
            )));
        }

        let body = if path_footer.is_some() {
            image_inner
        } else {
            inner
        };
        let paragraph = Paragraph::new(lines)
            .style(Style::default().fg(text_fg).bg(bg))
            .wrap(Wrap { trim: false });
        paragraph.render(body, buf);

        return None;
    }

    if image_inner.width > 0 && image_inner.height > 0 {
        use crate::engine::render::SafeBuf;
        let loading = labels.loading;
        let lw = unicode_width::UnicodeWidthStr::width(loading).min(usize::from(u16::MAX)) as u16;
        let lx = image_inner.x + image_inner.width.saturating_sub(lw) / 2;
        let ly = image_inner.y + image_inner.height / 2;
        buf.set_span_safe(
            lx,
            ly,
            &Span::styled(loading, Style::default().fg(text_fg).bg(bg)),
            lw,
        );
    }

    geometry.image_placement.and_then(|placement| {
        let Preview::Ready(bytes) = &image.preview else {
            return None;
        };
        overlay::static_image_for_protocol(
            protocol,
            bytes,
            placement.cols,
            placement.rows,
            placement.x,
            placement.y,
            image.owner_id,
        )
    })
}
