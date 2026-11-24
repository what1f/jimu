// Adapted from the Grok Build pager (Apache-2.0). Copyright 2023-2026 SpaceXAI.

//! Hero box component: the logo and menu sit side by side inside a bordered box.

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Flex, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, BorderType, Borders, Widget};

use crate::engine::theme::Theme;

use super::{WelcomeLayout, WelcomeLayoutInput};

/// Minimum terminal width for the side-by-side hero box layout.
pub(super) const HERO_BOX_MIN_WIDTH: u16 = 90;

/// Vertical padding (rows) between the box border and its inner content.
const V_PAD: u16 = 1;

/// Horizontal inset (cols) between the right column's content and the box border; also the collapsed left-column width when the logo is hidden.
const H_INSET: u16 = 2;

/// Horizontal gap (cols) between the logo and the right column inside the box.
const LOGO_H_PAD: u16 = 3;

/// Rows the promo upgrade CTA reserves in the info slot: a spacer row above the `[label]` button row.
/// Reserved on top of the announcement text rows so the message never paints over the button.
const UPGRADE_CTA_ROWS: u16 = 2;

use super::logo::LogoTier;
use super::{PROMPT_HEIGHT, VERSION_GAP};

/// Rows the "thanks" subtitle occupies.
/// Hidden when the in-box info slot (changelog or announcement) is shown, to keep the box compact.
fn subtitle_rows(info_height: u16) -> u16 {
    if info_height > 0 { 0 } else { 1 }
}

/// Height of the hero box's right column: version + optional subtitle + optional info block + the gap before the menu + the menu itself.
fn right_col_height(menu_height: u16, info_height: u16) -> u16 {
    let info_gap = if info_height > 0 { 1u16 } else { 0 };
    // version(1) + subtitle + [info_gap + info] + gap-before-menu(1) + menu
    1 + subtitle_rows(info_height) + info_gap + info_height + 1 + menu_height
}

/// Minimum content-area height the hero box needs to render without truncating.
/// That covers the optional error row, the box, a one-row flex gap, and the fixed rows below (tip + prompt + version).
/// The box always shows the full-height logo, so a terminal shorter than this falls back to the stacked layout instead of overflowing.
pub(super) fn min_content_height(
    input: &WelcomeLayoutInput<'_>,
    info_height: u16,
    prompt_height: u16,
) -> u16 {
    let inner = super::logo::full_logo_line_count(input.logo)
        .max(right_col_height(input.menu_height, info_height));
    let hero_box_height = 2 + V_PAD * 2 + inner;
    let gap_after_error = if input.error_height > 0 { 1u16 } else { 0 };
    gap_after_error
        + input.error_height
        + hero_box_height
        + 1
        + WelcomeLayout::fixed_below(input.tip_height, prompt_height)
}

/// Largest in-box info-slot height, at most `desired`, for which the hero box still fits beside a one-line prompt.
/// Lets the expanded announcement grow without ever pushing the box past the fit gate.
/// The renderer trails a `…` for whatever tail still doesn't fit, so the fallback never overflows.
fn clamp_info_height(desired: u16, input: &WelcomeLayoutInput<'_>, one_line_prompt: u16) -> u16 {
    (0..=desired)
        .rev()
        .find(|&h| input.content_area.height >= min_content_height(input, h, one_line_prompt))
        .unwrap_or(0)
}

/// Width (cols) of the hero box's left (logo) column, including padding.
/// Collapses to a small inset when the logo is hidden.
fn left_col_width(logo: super::WelcomeLogo<'_>) -> u16 {
    let logo_width = super::logo::full_logo_visual_width(logo);
    if logo_width == 0 {
        H_INSET
    } else {
        logo_width + LOGO_H_PAD.saturating_sub(1) + LOGO_H_PAD
    }
}

/// A taller draft never reflows the slot: when the box no longer fits beside it, this returns
/// `None` and the caller falls back to the stacked layout.
pub(super) fn compute_hero_box(input: &WelcomeLayoutInput<'_>) -> Option<WelcomeLayout> {
    let content_area = input.content_area;
    let error_height = input.error_height;
    let menu_height = input.menu_height;
    let tip_height = input.tip_height;
    let prompt_height = input.prompt_height.unwrap_or(PROMPT_HEIGHT);
    let one_line_prompt = prompt_height.min(PROMPT_HEIGHT);
    let zero = Rect::default();
    let tip_gap = if tip_height > 0 { 1u16 } else { 0 };
    let fixed_below = WelcomeLayout::fixed_below(tip_height, prompt_height);

    // Column widths are height-independent, so derive them once and reuse for both the measurement and the rects
    // `hero_info.width == info_slot_width`, so the measured width is the drawn width
    let box_width = content_area.width.saturating_sub(6).min(120);
    let inner_width = box_width.saturating_sub(2);
    let left_col_width = left_col_width(input.logo);
    let right_width = inner_width.saturating_sub(left_col_width);
    let info_slot_width = right_width.saturating_sub(H_INSET);
    let info_height = match input.announcement {
        Some(ann) => clamp_info_height(
            announcement_desired_rows(ann, info_slot_width, input.expanded, input.has_upgrade_cta),
            input,
            one_line_prompt,
        ),
        None => input.changelog_height,
    };
    if content_area.height < min_content_height(input, info_height, prompt_height) {
        return None;
    }

    let logo_rows = super::logo::full_logo_line_count(input.logo);
    let info_gap = if info_height > 0 { 1u16 } else { 0 };
    let inner_height = logo_rows.max(right_col_height(menu_height, info_height));
    let hero_box_height = 2 + V_PAD * 2 + inner_height;

    let gap_after_error = if error_height > 0 { 1 } else { 0 };
    let fixed_above = gap_after_error + error_height;

    // Top padding for vertical centering (use the default menu height and the one-line prompt so the logo position stays constant regardless of picker/focus state or draft length)
    let default_menu_height = 4u16;
    let default_inner = logo_rows.max(right_col_height(default_menu_height, info_height));
    let default_hero = 2 + V_PAD * 2 + default_inner;
    let remaining = content_area.height.saturating_sub(fixed_above);
    let top_pad = remaining
        .saturating_sub(default_hero)
        .saturating_sub(WelcomeLayout::fixed_below(tip_height, one_line_prompt))
        / 3;
    // Centering derives top_pad from the default-menu box, but the fit gate (min_content_height) sizes for the actual box with no pad
    // Clamp to the real slack so a taller menu or draft can't push the rows below the box off the bottom
    let top_pad = top_pad.min(
        content_area
            .height
            .saturating_sub(fixed_above + hero_box_height + 1 + fixed_below),
    );

    let [
        _,
        _,
        error,
        hero_box_slot,
        _,
        tip,
        _,
        prompt,
        _,
        version_slot,
    ] = Layout::vertical([
        Constraint::Length(top_pad),
        Constraint::Length(gap_after_error),
        Constraint::Length(error_height),
        Constraint::Length(hero_box_height),
        Constraint::Min(1), // flex gap
        Constraint::Length(tip_height),
        Constraint::Length(tip_gap),
        Constraint::Length(prompt_height),
        Constraint::Length(VERSION_GAP),
        Constraint::Length(1),
    ])
    .areas(content_area);

    // Horizontally center the hero box (`box_width` derived above).
    let [_, hero_box, _] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(box_width),
        Constraint::Min(0),
    ])
    .flex(Flex::Center)
    .areas(hero_box_slot);

    // Inner area inside the border and the vertical pad
    // Widths reuse the values above; only x and y come from the laid-out box
    let inner = Rect {
        x: hero_box.x + 1,
        y: hero_box.y + 1 + V_PAD,
        width: inner_width,
        height: inner_height,
    };

    // Left column: balanced padding around the logo; collapses to a small inset when the logo is hidden
    let logo_width = super::logo::full_logo_visual_width(input.logo);
    // Logo body leans right; shave a column off the left pad to optically center.
    let logo_left_pad = LOGO_H_PAD.saturating_sub(1);

    // The logo is top-aligned and horizontally centered within the left column
    let hero_logo = Rect {
        x: inner.x + logo_left_pad,
        y: inner.y,
        width: logo_width.min(inner.width.saturating_sub(logo_left_pad)),
        height: logo_rows.min(inner.height),
    };

    // The right column takes the rest of the inner width after the left column
    let right_x = inner.x + left_col_width;

    // Version line at top of right column.
    let hero_version = Rect {
        x: right_x,
        y: inner.y,
        width: right_width,
        height: 1,
    };

    // Subtitle line below the version; hidden when the info slot is shown
    let hero_subtitle = if subtitle_rows(info_height) > 0 {
        Rect {
            x: right_x,
            y: inner.y + 1,
            width: right_width,
            height: 1,
        }
    } else {
        zero
    };

    // Info block (announcement or changelog) below the version and optional subtitle
    let info_y = inner.y + 1 + subtitle_rows(info_height) + info_gap;
    let hero_info = if info_height > 0 {
        Rect {
            x: right_x,
            y: info_y,
            width: info_slot_width,
            height: info_height,
        }
    } else {
        zero
    };

    // version + subtitle + info_gap + info + gap-before-menu
    let right_header_rows = 1 + subtitle_rows(info_height) + info_gap + info_height + 1;

    // The menu sits below the header rows, left-aligned in the right column
    let hero_menu = Rect {
        x: right_x,
        y: inner.y + right_header_rows,
        width: info_slot_width,
        height: menu_height.min(inner.height.saturating_sub(right_header_rows)),
    };

    Some(WelcomeLayout {
        logo: zero,
        error,
        menu: zero,
        changelog: zero,
        tip,
        prompt,
        version: version_slot,
        hero_box,
        hero_logo,
        hero_version,
        hero_subtitle,
        hero_info,
        hero_menu,
        // The box paints the full logo through `render_hero_box`; the stacked `logo` rect is empty
        logo_tier: LogoTier::Hidden,
    })
}

/// Hit-test rects produced by [`render_hero_box`].
pub(super) struct HeroBoxRects {
    /// Hit-test rect per menu item row (for clicks and hover).
    pub(super) menu_rects: Vec<Rect>,
    /// Clickable changelog info block, if drawn.
    pub(super) changelog_cta_rect: Option<Rect>,
    /// Whether the announcement overflowed (the "expandable" signal).
    pub(super) announcement_truncated: bool,
    /// Full announcement block area (clickable anywhere to toggle), if shown.
    pub(super) announcement_rect: Option<Rect>,
    /// Promo upgrade CTA `[label]` button rect (a click opens it), if drawn.
    pub(super) upgrade_cta_rect: Option<Rect>,
}

/// Render the bordered hero box with the logo on the left and the version, subtitle, and menu on the right.
#[allow(clippy::too_many_arguments)]
pub(super) fn render_hero_box(
    layout: &WelcomeLayout,
    buf: &mut Buffer,
    theme: &Theme,
    menu_items: &[(&str, &str)],
    selected: Option<usize>,
    mouse_pos: Option<(u16, u16)>,
    announcement: Option<&super::WelcomeAnnouncement>,
    announcement_expanded: bool,
    config: &super::WelcomeConfig<'_>,
    upgrade_cta: Option<&str>,
) -> HeroBoxRects {
    // Dim the box border toward the background for a softer, dimmer gray.
    let border_color =
        crate::engine::render::color::blend_color(theme.bg_base, theme.gray_dim, 0.45)
            .unwrap_or(theme.gray_dim);
    let border_block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color));
    border_block.render(layout.hero_box, buf);

    super::logo::render_full_logo(layout.hero_logo, buf, theme, config.logo);

    super::render_version_badge(layout.hero_version, buf, theme, config, true);
    // Subtitle line below the version.
    if layout.hero_subtitle.height > 0
        && let Some(subtitle) = config.subtitle
    {
        let subtitle_style = Style::default().fg(theme.gray);
        buf.set_span(
            layout.hero_subtitle.x,
            layout.hero_subtitle.y,
            &Span::styled(subtitle, subtitle_style),
            layout.hero_subtitle.width,
        );
    }

    // In-box info slot: the announcement takes priority over the changelog, and only one is ever shown, always in this same position
    let mut changelog_cta_rect = None;
    let mut announcement_truncated = false;
    let mut announcement_rect = None;
    let mut upgrade_cta_rect = None;
    if layout.hero_info.height > 0 {
        if let Some(ann) = announcement {
            let (text_area, truncated, cta_rect) = render_announcement_with_upgrade_cta(
                buf,
                theme,
                layout.hero_info,
                ann,
                announcement_expanded,
                mouse_pos,
                upgrade_cta,
            );
            announcement_rect = Some(text_area);
            announcement_truncated = truncated;
            upgrade_cta_rect = cta_rect;
        } else if !config.notes.is_empty() {
            changelog_cta_rect = render_hero_changelog(
                buf,
                theme,
                layout.hero_info,
                config.notes_title,
                config.notes,
                config.notes_clickable,
                mouse_pos,
            );
        }
    }

    let menu_area = layout.hero_menu;

    let menu_rects = super::menu::render_menu(
        menu_area,
        buf,
        theme,
        menu_items,
        selected,
        mouse_pos,
        menu_area.width,
        config.logo,
    );
    HeroBoxRects {
        menu_rects,
        changelog_cta_rect,
        announcement_truncated,
        announcement_rect,
        upgrade_cta_rect,
    }
}

/// The CTA rows are reserved at the bottom so a long or expanded message never overpaints the
/// button.
#[allow(clippy::too_many_arguments)]
pub(super) fn render_announcement_with_upgrade_cta(
    buf: &mut Buffer,
    theme: &Theme,
    area: Rect,
    ann: &super::WelcomeAnnouncement,
    expanded: bool,
    mouse_pos: Option<(u16, u16)>,
    upgrade_cta: Option<&str>,
) -> (Rect, bool, Option<Rect>) {
    let cta_rows = if upgrade_cta.is_some() {
        UPGRADE_CTA_ROWS
    } else {
        0
    };
    let text_area = Rect {
        height: area.height.saturating_sub(cta_rows),
        ..area
    };
    let truncated = render_announcement_block(buf, theme, text_area, ann, expanded, mouse_pos);
    let mut cta_rect = None;
    if let Some(label) = upgrade_cta {
        let text_rows =
            announcement_text_rows(ann, text_area.width, expanded).min(text_area.height);
        let cta_y = area.y + text_rows + 1;
        if cta_y < area.y + area.height {
            let label = format!("[{label}]");
            let label =
                crate::engine::render::line_utils::truncate_str(&label, usize::from(area.width));
            let width = unicode_width::UnicodeWidthStr::width(label.as_str()) as u16;
            let hit = Rect::new(area.x, cta_y, width, 1);
            let hovered = mouse_pos.is_some_and(|(x, y)| hit.contains(Position::new(x, y)));
            let style = super::hover_style(theme, hovered, theme.muted());
            buf.set_span(hit.x, hit.y, &Span::styled(label, style), hit.width);
            cta_rect = Some(hit);
        }
    }
    (text_area, truncated, cta_rect)
}

/// The block brightens while hovered, but only when it's interactive (overflowing or already
/// expanded).
pub(super) fn render_announcement_block(
    buf: &mut Buffer,
    theme: &Theme,
    area: Rect,
    ann: &super::WelcomeAnnouncement,
    expanded: bool,
    mouse_pos: Option<(u16, u16)>,
) -> bool {
    let over = mouse_pos.is_some_and(|(mx, my)| area.contains(Position::new(mx, my)));
    let mut row = area.y;
    let max_w = area.width as usize;
    if let Some(title) = ann.title.as_deref() {
        let title_color = match ann.severity.as_deref() {
            Some("critical") => theme.accent_error,
            _ => theme.warning,
        };
        let title_style = Style::default()
            .fg(title_color)
            .add_modifier(Modifier::BOLD);
        let display = crate::engine::render::line_utils::truncate_str(title, max_w);
        buf.set_span(area.x, row, &Span::styled(display, title_style), area.width);
        row += 1;
    }
    if let Some(msg) = ann.message.as_deref() {
        let remaining_rows = (area.y + area.height).saturating_sub(row) as usize;
        let max_lines = if expanded {
            remaining_rows
        } else {
            remaining_rows.min(2)
        };
        // Only brighten when there's something to toggle (an overflowing message or the already-expanded state)
        // A short message that fits isn't clickable, so it must not look interactive
        let interactive = expanded || wrapped_line_count(msg, area.width) as usize > max_lines;
        let hovered = over && interactive;
        let msg_style = super::hover_style(theme, hovered, Style::default().fg(theme.gray));
        // Dim `…` affordance unless hovered.
        let ell_style = super::hover_style(
            theme,
            hovered,
            Style::default()
                .fg(theme.gray_bright)
                .add_modifier(Modifier::DIM),
        );
        return render_wrapped_text(
            buf, area.x, row, area.width, msg, msg_style, ell_style, max_lines,
        );
    }
    false
}

/// Render the changelog block (header and bullets) in the info slot.
/// When `clickable` (full notes exist), the whole block opens the notes on click and brightens while hovered; returns that clickable rect.
fn render_hero_changelog(
    buf: &mut Buffer,
    theme: &Theme,
    area: Rect,
    title: &str,
    bullets: &[String],
    clickable: bool,
    mouse_pos: Option<(u16, u16)>,
) -> Option<Rect> {
    if area.width == 0 || area.height == 0 {
        return None;
    }

    let hovered =
        clickable && mouse_pos.is_some_and(|(mx, my)| area.contains(Position::new(mx, my)));

    let header_style = super::hover_style(
        theme,
        hovered,
        Style::default()
            .fg(theme.gray_bright)
            .add_modifier(Modifier::DIM),
    );
    buf.set_span(
        area.x,
        area.y,
        &Span::styled(title, header_style),
        area.width,
    );

    // Bullets start 2 rows down (header row plus a blank), matching the height budget
    let bullet_style = super::hover_style(theme, hovered, Style::default().fg(theme.gray_bright));
    let max_text_width = area.width.saturating_sub(4) as usize; // " • " prefix plus pad
    for (i, bullet) in bullets.iter().enumerate() {
        let row = area.y + 2 + i as u16;
        if row >= area.y + area.height {
            break;
        }
        let truncated = crate::engine::render::line_utils::truncate_str(bullet, max_text_width);
        let text = format!(" \u{2022} {truncated}");
        buf.set_span(area.x, row, &Span::styled(text, bullet_style), area.width);
    }

    clickable.then_some(area)
}

/// Word-wrap `text` into lines no wider than `width` columns.
/// A single word longer than `width` becomes its own (over-wide) line; the renderer clips it.
fn wrap_lines(text: &str, width: u16) -> Vec<String> {
    use unicode_width::UnicodeWidthStr;

    let w = width as usize;
    let mut lines: Vec<String> = Vec::new();
    if w == 0 {
        return lines;
    }
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current = word.to_string();
        } else if current.width() + 1 + word.width() <= w {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            current = word.to_string();
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Number of rows `text` occupies when word-wrapped to `width` columns.
/// Shared by the layout height pre-pass and the renderer so they can't drift.
pub(super) fn wrapped_line_count(text: &str, width: u16) -> u16 {
    wrap_lines(text, width).len() as u16
}

/// Rows the announcement TEXT wants at `width`: title and message, the message capped at 2 wrapped lines unless `expanded`.
/// Shared with the renderer so the upgrade CTA is placed right after the drawn text (the reserved rows match the drawn rows).
pub(super) fn announcement_text_rows(
    ann: &super::WelcomeAnnouncement,
    width: u16,
    expanded: bool,
) -> u16 {
    let title_rows = if ann.title.is_some() { 1u16 } else { 0 };
    let msg_rows = ann.message.as_deref().map_or(0, |msg| {
        let wrapped = wrapped_line_count(msg, width);
        if expanded { wrapped } else { wrapped.min(2) }
    });
    title_rows + msg_rows
}

/// Rows the announcement info slot wants at `width`.
/// That is the text rows plus, when a promo upgrade CTA is shown, the spacer row and the `[label]` button row (`UPGRADE_CTA_ROWS`).
/// Shared with the renderer (the reserved rows match the drawn rows).
pub(super) fn announcement_desired_rows(
    ann: &super::WelcomeAnnouncement,
    width: u16,
    expanded: bool,
    has_upgrade_cta: bool,
) -> u16 {
    announcement_text_rows(ann, width, expanded)
        + if has_upgrade_cta { UPGRADE_CTA_ROWS } else { 0 }
}

/// Word-wrap `text` into at most `max_lines` rows at (`x`, `y`).
/// Overflow ends the last row with a `…` painted in `ell_style`.
/// Returns whether the text was truncated.
#[allow(clippy::too_many_arguments)]
fn render_wrapped_text(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    text: &str,
    style: Style,
    ell_style: Style,
    max_lines: usize,
) -> bool {
    use unicode_width::UnicodeWidthStr;

    if max_lines == 0 || width == 0 {
        return false;
    }
    let w = width as usize;
    let lines = wrap_lines(text, width);
    let truncated = lines.len() > max_lines;
    let visible = max_lines.min(lines.len());

    for (i, line) in lines.iter().take(visible).enumerate() {
        let row = y + i as u16;
        if i + 1 == visible && truncated {
            // Hard-cut the text and append our own styled `…` (no built-in one).
            let (head, ell_x) = if line.width() < w {
                (line.as_str(), x + line.width() as u16)
            } else {
                let cut = crate::engine::render::line_utils::byte_offset_at_width(
                    line,
                    w.saturating_sub(1),
                );
                let Some(head) = line.get(..cut) else {
                    continue;
                };
                (head, x + head.width() as u16)
            };
            buf.set_span(x, row, &Span::styled(head, style), width);
            buf.set_span(ell_x, row, &Span::styled("…", ell_style), 1);
        } else {
            buf.set_span(x, row, &Span::styled(line.as_str(), style), width);
        }
    }
    truncated
}
