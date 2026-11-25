// Adapted from the Grok Build pager (Apache-2.0). Copyright 2023-2026 SpaceXAI.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::ui::views::prompt_widget::{PromptInfo, PromptStyle, PromptWidget};

use super::WelcomePromptFocus;

pub fn prompt_inset(compact: bool) -> u16 {
    if compact { 0 } else { 2 }
}

fn prompt_area_inset(compact: bool) -> u16 {
    if compact {
        super::PROMPT_GUTTER
    } else {
        prompt_inset(false)
    }
}

const CHROME_PAD: u16 = 2;

fn prompt_style(
    focus: WelcomePromptFocus,
    compact: bool,
    placeholder: Option<&'static str>,
) -> PromptStyle {
    PromptStyle {
        focused: focus == WelcomePromptFocus::Focused,
        show_prefix: true,
        vpad_top: 1,
        compact,
        chrome: true,
        chrome_pad_left: CHROME_PAD,
        chrome_pad_right: CHROME_PAD,
        placeholder_override: placeholder,
        ..PromptStyle::default()
    }
}

/// Measured with the style [`render_prompt`] draws with, so the layout reserves the rows the draft will paint into.
pub fn desired_prompt_height(
    prompt: &PromptWidget,
    content_width: u16,
    compact: bool,
    max_height: u16,
) -> u16 {
    let inset = prompt_area_inset(compact);
    // Focus only tints, never changes the row count
    let style = prompt_style(WelcomePromptFocus::Focused, compact, None);
    prompt.desired_height(
        content_width.saturating_sub(inset * 2),
        &style,
        true,
        max_height,
    )
}

/// Returns the cursor position and the post-flush output that carries terminal-overlay ownership.
pub fn render_prompt(
    area: Rect,
    buf: &mut Buffer,
    focus: WelcomePromptFocus,
    prompt: &mut PromptWidget,
    info: &PromptInfo<'_>,
    compact: bool,
    placeholder: Option<&'static str>,
) -> (
    Option<(u16, u16)>,
    Option<crate::ui::views::prompt_widget::PromptPostFlush>,
) {
    let style = prompt_style(focus, compact, placeholder);

    let inset = prompt_area_inset(compact);
    let inset_area = Rect {
        x: area.x + inset,
        y: area.y,
        width: area.width.saturating_sub(inset * 2),
        height: area.height,
    };

    let result = prompt.draw(buf, inset_area, None, &style, Some(info), None);

    (result.cursor_pos, result.post_flush_escapes.map(Into::into))
}
