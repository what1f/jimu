// Adapted from the Grok Build pager (Apache-2.0). Copyright 2023-2026 SpaceXAI.

//! Single-row welcome toast overlay (above the prompt when present).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use crate::engine::render::SafeBuf;
use crate::engine::render::line_utils::truncate_to_width;
use crate::engine::theme::Theme;

/// Prefer one row above the prompt, right-aligned to it.
/// With no prompt (login / gate), paint the last row of `area`.
/// Stacked welcome layouts put the version badge there, so the toast may overlay it briefly.
pub fn paint_welcome_toast(buf: &mut Buffer, area: Rect, msg: &str, prompt_rect: Option<Rect>) {
    let theme = Theme::current();
    let max_msg = (area.width as usize).saturating_sub(4);
    if max_msg == 0 || area.height == 0 {
        return;
    }
    let body = if UnicodeWidthStr::width(msg) <= max_msg {
        std::borrow::Cow::Borrowed(msg)
    } else {
        truncate_to_width(msg, max_msg)
    };
    let toast = format!(" {body} ");
    let w = UnicodeWidthStr::width(toast.as_str()) as u16;
    let (x, y) = if let Some(prompt) = prompt_rect.filter(|r| r.width > 0 && r.y > area.y) {
        let max_x = area.right().saturating_sub(w).max(area.x);
        let x = prompt.right().saturating_sub(w + 1).clamp(area.x, max_x);
        (x, prompt.y.saturating_sub(1))
    } else {
        (
            area.right().saturating_sub(w.saturating_add(1)),
            area.bottom().saturating_sub(1),
        )
    };
    let style = Style::default()
        .fg(theme.accent_user)
        .bg(theme.bg_base)
        .add_modifier(Modifier::BOLD);
    buf.set_string_safe(x, y, &toast, style);
}
