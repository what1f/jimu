// Adapted from the Grok Build pager (Apache-2.0). Copyright 2023-2026 SpaceXAI.

//! Dropdown renderer for shell command completion suggestions.
//!
//! Mirrors the `slash_dropdown.rs` layout: aligned label column, description, selection highlight, and mouse hover.
//! A scrollbar appears when items exceed `MAX_VISIBLE_ROWS`.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

use crate::engine::render::SafeBuf;
use crate::engine::render::line_utils::truncate_str;
use crate::engine::render::scrollbar::render_scrollbar_styled;
use crate::engine::theme::Theme;
use crate::ui::views::suggestion_controller::{CompletionDropdownState, CompletionItemParsed};

pub const MAX_VISIBLE_ROWS: u16 = 6;

/// Hard cap on label column width.
const LABEL_CAP: usize = 40;

const LABEL_DESC_GAP: usize = 2;

/// Prefix width (`"❯ "` or `"  "`).
const PREFIX_W: usize = 2;

/// Height needed for the dropdown (separator + items), or 0 when hidden.
pub fn dropdown_height(state: &CompletionDropdownState) -> u16 {
    if !state.open || state.items.is_empty() {
        return 0;
    }
    let item_rows = (state.items.len() as u16).min(MAX_VISIBLE_ROWS);
    1 + item_rows // separator + items
}

/// Compute the scroll offset so the selected row stays centred.
pub fn scroll_offset(state: &CompletionDropdownState) -> usize {
    scroll_offset_for(state, MAX_VISIBLE_ROWS)
}

pub fn scroll_offset_for(state: &CompletionDropdownState, rows: u16) -> usize {
    let total = state.items.len();
    let visible = usize::from(rows.max(1));
    let selected = state.selected.min(total.saturating_sub(1));
    if total <= visible || selected < visible / 2 {
        0
    } else if selected + visible / 2 >= total {
        total.saturating_sub(visible)
    } else {
        selected.saturating_sub(visible / 2)
    }
}

fn compute_label_column_w(items: &[CompletionItemParsed], content_w: usize) -> usize {
    let budget = (content_w * 3 / 5).min(LABEL_CAP);
    let max_w = items
        .iter()
        .map(|r| r.display.width())
        .filter(|&w| w <= LABEL_CAP)
        .max()
        .unwrap_or(0);
    max_w.min(budget)
}

/// Render completion dropdown items into `area`; the caller draws the borders.
pub fn render_dropdown(
    buf: &mut Buffer,
    area: Rect,
    state: &CompletionDropdownState,
    theme: &Theme,
) {
    if area.height == 0 || area.width < 4 || !state.open {
        return;
    }

    let items = &state.items;
    let selected = state.selected.min(items.len().saturating_sub(1));
    let hovered = state.hovered;

    let content_w = area.width as usize;
    let visible_rows = area.height as usize;
    let needs_scrollbar = items.len() > visible_rows;
    let row_w = if needs_scrollbar {
        content_w.saturating_sub(2)
    } else {
        content_w
    };

    let label_col_w = compute_label_column_w(items, row_w.saturating_sub(PREFIX_W));

    let scroll = scroll_offset_for(state, area.height);

    for vis_row in 0..visible_rows {
        let item_idx = scroll + vis_row;
        if item_idx >= items.len() {
            break;
        }
        let Some(item) = items.get(item_idx) else {
            break;
        };
        let y = area.y + vis_row as u16;
        let is_selected = item_idx == selected;
        let is_hovered = hovered == Some(item_idx) && !is_selected;

        let row_bg = match crate::ui::views::modal_window::embedded_row_style(theme, is_selected) {
            Some(e) => e.bg,
            None if is_selected => theme.bg_visual,
            None if is_hovered => theme.bg_hover,
            None => theme.bg_light,
        };

        let line = build_item_line(item, is_selected, label_col_w, row_w, row_bg, theme);

        // Skip rows that fall outside the buffer (resize race).
        if y < buf.area.y || y >= buf.area.bottom() || area.x >= buf.area.right() {
            continue;
        }
        let clamped_w = row_w.min(buf.area.right().saturating_sub(area.x) as usize) as u16;
        let row_rect = Rect {
            x: area.x,
            y,
            width: clamped_w,
            height: 1,
        };
        buf.set_style(row_rect, Style::default().bg(row_bg));
        buf.set_line_safe(area.x, y, &line, row_w as u16);
        // Terminal theme (Reset bands): reverse video; no-op on RGB themes.
        if crate::ui::views::modal_window::embedded_row_style(theme, is_selected).is_none() {
            if is_selected {
                buf.set_style(row_rect, theme.selection_overlay());
            } else if is_hovered {
                buf.set_style(row_rect, theme.hover_overlay());
            }
        }
    }

    if needs_scrollbar {
        let sb_x = area.x + area.width.saturating_sub(1);
        let sb_y = area.y.max(buf.area.y);
        let sb_bottom = (area.y.saturating_add(area.height)).min(buf.area.bottom());
        if sb_x < buf.area.right() && sb_bottom > sb_y {
            let sb_area = Rect {
                x: sb_x,
                y: sb_y,
                width: 1,
                height: sb_bottom - sb_y,
            };
            let track = Style::default().bg(theme.bg_dark);
            let thumb = Style::default().fg(theme.gray_dim).bg(theme.bg_dark);
            render_scrollbar_styled(
                buf,
                Some(sb_area),
                items.len() as u16,
                sb_area.height,
                scroll as u16,
                track,
                thumb,
            );
        }
    }
}

fn build_item_line(
    item: &CompletionItemParsed,
    is_selected: bool,
    label_col_w: usize,
    total_w: usize,
    row_bg: ratatui::style::Color,
    theme: &Theme,
) -> Line<'static> {
    let bold = if is_selected {
        Modifier::BOLD
    } else {
        Modifier::empty()
    };
    let embed = crate::ui::views::modal_window::embedded_row_style(theme, is_selected);
    let primary_fg = embed.map_or(theme.text_primary, |e| e.fg(theme.text_primary));
    let normal = Style::default()
        .fg(primary_fg)
        .bg(row_bg)
        .add_modifier(bold);
    // muted(): DIM on the terminal theme, where `gray` is Reset and the
    // description would render as heavy as the label.
    let desc_style = match embed {
        Some(e) => Style::default().fg(e.fg(theme.gray)).bg(row_bg),
        None => theme.muted().bg(row_bg),
    };
    let bg_style = Style::default().bg(row_bg);

    let prefix = if is_selected {
        crate::engine::render::glyphs::prompt_arrow()
    } else {
        "  "
    };
    let prefix_span = Span::styled(
        prefix.to_string(),
        if is_selected { normal } else { bg_style },
    );

    let label = truncate_str(&item.display, label_col_w);
    let label_w = label.width();
    let padding = label_col_w.saturating_sub(label_w);

    let label_span = Span::styled(label, normal);

    let desc_indent = PREFIX_W + label_col_w + LABEL_DESC_GAP;
    let desc_w = total_w.saturating_sub(desc_indent).max(1);
    let desc = truncate_str(&item.description, desc_w);

    let mut spans = vec![prefix_span, label_span];
    if padding > 0 {
        spans.push(Span::styled(" ".repeat(padding), bg_style));
    }
    if !desc.is_empty() {
        spans.push(Span::styled(" ".to_string(), bg_style));
        spans.push(Span::styled(desc, desc_style));
    }

    Line::from(spans).style(bg_style)
}
/// Render dropdown chrome (borders, count hint) anchored to the prompt and return the inner items area. Returns `None` when geometry doesn't fit.
/// `below = false` anchors the panel *above* the prompt (full-TUI default); `below = true` anchors it *below* the prompt (minimal mode, common CLI style, to reduce layout shift).
/// Shared by slash dropdown and completion dropdown to avoid duplicated chrome code.
#[allow(clippy::too_many_arguments)]
pub fn render_dropdown_chrome(
    buf: &mut Buffer,
    item_count: usize,
    item_rows: u16,
    inline_prompt_area: Option<Rect>,
    layout_prompt: Rect,
    area: Rect,
    horizontal_padding: (u16, u16),
    embedded_footer: &str,
    below: bool,
    theme: &Theme,
) -> Option<DropdownChrome> {
    let mut panel_height = item_rows + 2;
    let (top_border_y, bottom_border_y) = if below {
        let anchor = inline_prompt_area.unwrap_or(layout_prompt);
        let top = anchor.y + anchor.height;
        (top, top + panel_height - 1)
    } else {
        let bottom = if let Some(ipa) = inline_prompt_area {
            ipa.y.saturating_sub(1)
        } else {
            layout_prompt.y.saturating_sub(1)
        };
        let avail = bottom.saturating_sub(area.y).saturating_add(1);
        panel_height = panel_height.min(avail);
        if panel_height < 3 {
            return None;
        }
        (bottom.saturating_sub(panel_height - 1), bottom)
    };
    let embedded = crate::ui::views::modal_window::embedded();
    let (hpad_left, hpad_right) = if embedded { (0, 0) } else { horizontal_padding };
    let panel_x = area.x + hpad_left;
    let panel_width = area.width.saturating_sub(hpad_left + hpad_right);
    if top_border_y >= bottom_border_y || panel_width <= 4 {
        return None;
    }
    if below && bottom_border_y > area.y + area.height.saturating_sub(1) {
        return None;
    }
    let panel_area = Rect {
        x: panel_x,
        y: top_border_y,
        width: panel_width,
        height: panel_height,
    };
    if panel_area.bottom() > buf.area.bottom()
        || panel_area.right() > buf.area.right()
        || panel_area.y < buf.area.y
    {
        return None;
    }
    ratatui::widgets::Clear.render(panel_area, buf);
    if embedded {
        let reset = ratatui::style::Color::Reset;
        let divider_style = Style::default().fg(theme.gray_dim).bg(reset);
        let divider = Line::styled("\u{2500}".repeat(panel_width as usize), divider_style);
        buf.set_line_safe(panel_x, top_border_y, &divider, panel_width);
        let footer_line = Line::styled(
            embedded_footer,
            Style::default().fg(theme.gray_dim).bg(reset),
        );
        buf.set_line_safe(
            panel_x + 1,
            bottom_border_y,
            &footer_line,
            panel_width.saturating_sub(1),
        );
    } else {
        buf.set_style(
            panel_area,
            Style::default().fg(theme.text_primary).bg(theme.bg_light),
        );
        let border_style = Style::default()
            .fg(theme.panel_border_fg())
            .bg(theme.bg_base);
        let border_line = Line::styled("\u{2500}".repeat(panel_width as usize), border_style);
        buf.set_line_safe(panel_x, top_border_y, &border_line, panel_width);
        buf.set_line_safe(panel_x, bottom_border_y, &border_line, panel_width);
        let right_x = panel_area.right().saturating_sub(1);
        for (y, left, right) in [(top_border_y, '╭', '╮'), (bottom_border_y, '╰', '╯')] {
            if let Some(cell) = buf.cell_mut((panel_x, y)) {
                cell.set_char(left);
            }
            if let Some(cell) = buf.cell_mut((right_x, y)) {
                cell.set_char(right);
            }
        }
        for y in top_border_y + 1..bottom_border_y {
            for x in [panel_x, right_x] {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_char('│');
                    cell.set_style(border_style);
                }
            }
        }
        let hint = format!("{}", item_count);
        let hint_w = hint.len() as u16;
        if hint_w + 2 <= panel_width {
            let hint_x = panel_x + panel_width - hint_w - 1;
            let hint_line = Line::styled(hint, Style::default().fg(theme.gray).bg(theme.bg_base));
            buf.set_line_safe(hint_x, top_border_y, &hint_line, hint_w);
        }
    }
    let content_inset = dropdown_content_inset();
    let items_x = (layout_prompt.x + content_inset).max(panel_area.x + u16::from(!embedded));
    let items_right = layout_prompt
        .right()
        .min(panel_area.right().saturating_sub(u16::from(!embedded)));
    let items_width = items_right.saturating_sub(items_x);
    Some(DropdownChrome {
        items: Rect {
            x: items_x,
            y: top_border_y + 1,
            height: panel_height - 2,
            width: items_width,
        },
        panel: panel_area,
    })
}
/// Left inset of dropdown item rows inside the panel (see the comment in [`render_dropdown_chrome`]).
/// [`render_dropdown_chrome`]).
pub fn dropdown_content_inset() -> u16 {
    if crate::ui::views::modal_window::embedded() {
        0
    } else {
        2
    }
}
/// Width of the dropdown item rows [`render_dropdown_chrome`] will produce for `layout_prompt`, for sizing the row count *before* drawing the chrome.
/// `layout_prompt`, for sizing the row count *before* drawing the chrome.
pub fn dropdown_items_width(layout_prompt: Rect) -> u16 {
    layout_prompt.width.saturating_sub(dropdown_content_inset())
}
/// Geometry returned by [`render_dropdown_chrome`]: the inset `items` area for rendering rows and the full `panel` rect (borders + padding) used as an occluder for hyperlink hit-testing.
/// rendering rows and the full `panel` rect (borders + padding) used as an
/// occluder for hyperlink hit-testing.
pub struct DropdownChrome {
    pub items: Rect,
    pub panel: Rect,
}

pub fn render_floating_dropdown(
    buf: &mut Buffer,
    area: Rect,
    prompt: Rect,
    state: &CompletionDropdownState,
    theme: &Theme,
) -> Option<DropdownChrome> {
    let height = dropdown_height(state);
    if height == 0 {
        return None;
    }
    let chrome = render_dropdown_chrome(
        buf,
        state.items.len(),
        height.saturating_sub(1),
        None,
        prompt,
        area,
        (0, 0),
        "",
        false,
        theme,
    )?;
    render_dropdown(buf, chrome.items, state, theme);
    Some(chrome)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropdownMouse {
    Hover(Option<usize>),
    Select(usize),
    Scroll(isize),
    Unhandled,
}

impl DropdownChrome {
    pub fn handle_mouse(
        &self,
        mouse: &crossterm::event::MouseEvent,
        item_count: usize,
        offset: usize,
    ) -> DropdownMouse {
        use crossterm::event::{MouseButton, MouseEventKind};
        let point = (mouse.column, mouse.row).into();
        let hit = self
            .items
            .contains(point)
            .then(|| offset + usize::from(mouse.row.saturating_sub(self.items.y)))
            .filter(|&index| index < item_count);
        match mouse.kind {
            MouseEventKind::Moved => DropdownMouse::Hover(hit),
            MouseEventKind::Down(MouseButton::Left) => {
                hit.map_or(DropdownMouse::Unhandled, DropdownMouse::Select)
            }
            MouseEventKind::ScrollUp if self.panel.contains(point) => DropdownMouse::Scroll(-1),
            MouseEventKind::ScrollDown if self.panel.contains(point) => DropdownMouse::Scroll(1),
            _ => DropdownMouse::Unhandled,
        }
    }
}
