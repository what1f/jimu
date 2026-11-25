// Adapted from the Grok Build pager (Apache-2.0). Copyright 2023-2026 SpaceXAI.

//! Welcome screen with a wide hero card and a stacked layout for small terminals.

use super::prompt_widget::{PromptInfo, PromptWidget};
use crate::engine::{
    render::{SafeBuf, line_utils::truncate_line},
    theme::Theme,
};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Flex, Layout, Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use std::path::Path;

mod hero_box;
mod logo;
mod menu;
mod prompt;
mod toast;
mod top_bar;

use hero_box::HERO_BOX_MIN_WIDTH;
use logo::{LogoTier, render_logo_tier};
pub use logo::{WelcomeLogo, shimmer_frame};
pub use toast::paint_welcome_toast;

const PROMPT_GUTTER: u16 = 1;
const MENU_MIN_WIDTH: u16 = 51;
const H_MARGIN: u16 = 2;
const H_MARGIN_COMPACT: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WelcomePromptFocus {
    #[default]
    Unfocused,
    Focused,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WelcomeMenuItem<'a> {
    pub label: &'a str,
    pub shortcut: &'a str,
}

pub struct WelcomeConfig<'a> {
    pub title: &'a str,
    pub version: &'a str,
    pub logo: WelcomeLogo<'a>,
    pub subtitle: Option<&'a str>,
    pub notes_title: &'a str,
    pub notes: &'a [String],
    pub notes_clickable: bool,
    pub menu: &'a [WelcomeMenuItem<'a>],
    pub footer_hint: Option<&'a str>,
    pub placeholder: Option<&'static str>,
}

impl Default for WelcomeConfig<'_> {
    fn default() -> Self {
        Self {
            title: "Terminal",
            version: "",
            logo: WelcomeLogo::default(),
            subtitle: None,
            notes_title: "What's new",
            notes: &[],
            notes_clickable: false,
            menu: &[],
            footer_hint: None,
            placeholder: Some("Type a message..."),
        }
    }
}

#[derive(Default)]
pub struct WelcomeAnnouncement {
    pub title: Option<String>,
    pub message: Option<String>,
    pub severity: Option<String>,
    pub dismissible: bool,
}

pub struct WelcomeRenderParams<'a> {
    pub config: &'a WelcomeConfig<'a>,
    pub cwd: &'a Path,
    pub prompt_info: PromptInfo<'a>,
    pub prompt_focus: WelcomePromptFocus,
    pub compact: bool,
    pub selected: Option<usize>,
    pub mouse_pos: Option<(u16, u16)>,
    pub notice: Option<&'a WelcomeAnnouncement>,
    pub notice_expanded: bool,
    pub notice_action: Option<&'a str>,
    pub tip: Option<&'a str>,
    pub error: Option<&'a str>,
}

#[derive(Default)]
pub struct WelcomeRenderResult {
    pub cursor_pos: Option<(u16, u16)>,
    pub post_flush_escapes: Option<crate::ui::views::prompt_widget::PromptPostFlush>,
    pub menu_rects: Vec<Rect>,
    pub prompt_rect: Option<Rect>,
    pub notes_rect: Option<Rect>,
    pub notice_rect: Option<Rect>,
    pub notice_truncated: bool,
    pub notice_action_rect: Option<Rect>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WelcomeTarget {
    Menu(usize),
    Prompt,
    Notes,
    Notice,
    NoticeAction,
}

impl WelcomeRenderResult {
    pub fn hit_test(&self, col: u16, row: u16) -> Option<WelcomeTarget> {
        let pos = Position::new(col, row);
        [
            (self.prompt_rect, WelcomeTarget::Prompt),
            (self.notes_rect, WelcomeTarget::Notes),
            (self.notice_action_rect, WelcomeTarget::NoticeAction),
            (
                self.notice_rect.filter(|_| self.notice_truncated),
                WelcomeTarget::Notice,
            ),
        ]
        .into_iter()
        .find_map(|(rect, target)| rect.filter(|r| r.contains(pos)).map(|_| target))
        .or_else(|| {
            self.menu_rects
                .iter()
                .position(|rect| rect.contains(pos))
                .map(WelcomeTarget::Menu)
        })
    }

    pub fn handle_mouse(&self, mouse: &MouseEvent) -> Option<WelcomeTarget> {
        matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            .then(|| self.hit_test(mouse.column, mouse.row))
            .flatten()
    }
}

/// Shared by the announcement and changelog renderers.
pub(super) fn hover_style(theme: &Theme, hovered: bool, base: Style) -> Style {
    if hovered {
        // Brighten to the primary fg; on the terminal palette the base fgs
        // and text_primary are all Reset, so bold carries the cue instead.
        if theme.is_bandless() {
            Style::default()
                .fg(theme.text_primary)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text_primary)
        }
    } else {
        base
    }
}

/// Prompt input height for a one-line draft (shared across hero and stacked layout paths).
const PROMPT_HEIGHT: u16 = 3;

/// Half the content area, as in the session view, but never more than the logo-less stacked column leaves beside the menu, tip, version and announcement rows.
/// The draft can therefore push the logo out, never the announcement.
fn prompt_max_height(input: &WelcomeLayoutInput<'_>) -> u16 {
    let content_height = input.content_area.height;
    let column = StackedColumn::new(input, 0);
    let info = stacked_announcement_rows(input);
    let info_gap = if info > 0 { 1u16 } else { 0 };
    let column_fit = content_height.saturating_sub(
        column.fixed_above(LogoTier::Hidden)
            + input.menu_height
            + info_gap
            + info
            + 1
            + column.fixed_below,
    );
    (content_height / 2).min(column_fit).max(PROMPT_HEIGHT)
}

/// Rows the stacked info slot gives the announcement: its desired rows at the stacked slot width, clamped to what fits
/// beside a one-line prompt with the logo hidden. Independent of the draft, so a growing draft never reflows it.
/// Zero without an announcement or in compact mode (the picker owns the column).
fn stacked_announcement_rows(input: &WelcomeLayoutInput<'_>) -> u16 {
    let Some(ann) = input.announcement else {
        return 0;
    };
    if input.compact {
        return 0;
    }
    let content_area = input.content_area;
    let avail = content_area
        .width
        .saturating_sub(prompt::prompt_inset(input.prompt_compact) * 2);
    let width = stacked_info_width(avail, content_area.height, MENU_MIN_WIDTH, input.logo);
    let column = StackedColumn::new(input, PROMPT_HEIGHT);
    // +1 info-slot gap, +1 min flex gap above the tip
    let budget = content_area.height.saturating_sub(
        column.fixed_above(LogoTier::Hidden) + input.menu_height + 1 + column.fixed_below + 1,
    );
    hero_box::announcement_desired_rows(ann, width, input.expanded, input.has_upgrade_cta)
        .min(budget)
}

/// The stacked column's rows around the logo, for one prompt height.
struct StackedColumn<'a> {
    content_height: u16,
    logo: WelcomeLogo<'a>,
    gap_after_logo: u16,
    error_height: u16,
    menu_height: u16,
    fixed_below: u16,
}

impl<'a> StackedColumn<'a> {
    fn new(input: &WelcomeLayoutInput<'a>, prompt_height: u16) -> Self {
        Self {
            content_height: input.content_area.height,
            logo: input.logo,
            gap_after_logo: if input.error_height > 0 { 1 } else { 0 },
            error_height: input.error_height,
            menu_height: input.menu_height,
            fixed_below: WelcomeLayout::fixed_below(input.tip_height, prompt_height),
        }
    }

    /// Logo rows, the gap after them, and the error block.
    fn fixed_above(&self, tier: LogoTier) -> u16 {
        tier.rows(self.logo) + 1 + self.gap_after_logo + self.error_height
    }

    /// Whether the column fits with this tier beside `reserved` info rows (slot + gap) and a one-row flex gap.
    fn fits(&self, tier: LogoTier, reserved: u16) -> bool {
        self.fixed_above(tier) + self.menu_height + reserved + 1 + self.fixed_below
            <= self.content_height
    }

    /// The tier the terminal height allows, stepped down only while the column would overflow beside the draft and the reserved rows.
    fn logo_tier(&self, reserved: u16) -> LogoTier {
        let mut tier = LogoTier::for_height(self.content_height);
        while !self.fits(tier, reserved) {
            match tier.step_down() {
                Some(smaller) => tier = smaller,
                None => break,
            }
        }
        tier
    }
}
/// Gap between prompt and version line.
const VERSION_GAP: u16 = 1;

/// Computed areas for the welcome screen vertical layout.
pub(super) struct WelcomeLayout {
    pub(super) logo: Rect,
    pub(super) error: Rect,
    pub(super) menu: Rect,
    /// Stacked info slot below the menu (narrow layout only): it shows either the announcement or the changelog (the announcement takes priority).
    /// Zero in the hero box layout, which uses `hero_info` instead.
    pub(super) changelog: Rect,
    pub(super) tip: Rect,
    pub(super) prompt: Rect,
    pub(super) version: Rect,
    // Hero box sub-rects (all zero when hero box is inactive).
    pub(super) hero_box: Rect,
    pub(super) hero_logo: Rect,
    pub(super) hero_version: Rect,
    pub(super) hero_subtitle: Rect,
    /// In-box info slot: it shows either the announcement or the changelog (the announcement takes priority).
    pub(super) hero_info: Rect,
    pub(super) hero_menu: Rect,
    /// The art the stacked `logo` rows were reserved for; paint it with [`render_logo_tier`].
    pub(super) logo_tier: LogoTier,
}

/// Inputs to [`WelcomeLayout::compute`] / [`WelcomeLayout::compute_stacked`].
///
/// Bundled (and `Default`-able) so call sites name each field; in particular the two distinct compaction flags can't be silently transposed.
#[derive(Default)]
struct WelcomeLayoutInput<'a> {
    content_area: Rect,
    logo: WelcomeLogo<'a>,
    /// Error/warning row height; 0 when there's nothing to show.
    error_height: u16,
    menu_height: u16,
    tip_height: u16,
    /// Desired changelog height (collapsed to 0 if the terminal is too short).
    changelog_height: u16,
    /// Vertical compaction (session picker visible): skip the logo and the info slot.
    compact: bool,
    /// Horizontal-inset compaction (appearance setting) for the stacked slot.
    prompt_compact: bool,
    announcement: Option<&'a WelcomeAnnouncement>,
    /// Whether a long announcement is expanded inline (vs. collapsed to 2 lines).
    expanded: bool,
    /// Whether the info slot reserves a promo upgrade CTA (spacer and button).
    has_upgrade_cta: bool,
    /// Rows reserved for the prompt box.
    /// `None` keeps the default; the blocking screens that paint no prompt pass 0 to give the rows back to their message.
    prompt_height: Option<u16>,
}

impl WelcomeLayout {
    /// Whether the hero box (side-by-side logo and menu inside a border) is active.
    pub(super) fn has_hero_box(&self) -> bool {
        self.hero_box.width > 0 && self.hero_box.height > 0
    }

    pub(super) fn fixed_below(tip_height: u16, prompt_height: u16) -> u16 {
        let tip_gap = if tip_height > 0 { 1u16 } else { 0 };
        tip_height + tip_gap + prompt_height + VERSION_GAP + 1
    }

    pub(super) fn effective_changelog(
        content_height: u16,
        fixed_above: u16,
        content_slot: u16,
        fixed_below: u16,
        requested: u16,
    ) -> (u16, u16) {
        let gap = if requested > 0 { 1u16 } else { 0 };
        let min_without = fixed_above + content_slot + 1 + fixed_below;
        if requested > 0 && content_height >= min_without + gap + requested {
            (requested, 1)
        } else {
            (0, 0)
        }
    }

    fn compute(input: WelcomeLayoutInput<'_>) -> Self {
        if !input.compact
            && input.content_area.width >= HERO_BOX_MIN_WIDTH
            && let Some(layout) = hero_box::compute_hero_box(&input)
        {
            return layout;
        }

        let WelcomeLayoutInput {
            content_area,
            error_height,
            menu_height,
            tip_height,
            changelog_height,
            compact,
            announcement,
            prompt_height,
            ..
        } = input;
        let zero = Rect::default();
        let prompt_height = prompt_height.unwrap_or(PROMPT_HEIGHT);
        // Centering and the info budget see the one-line box, so a growing draft does not shift the column or reflow the slot
        // The consent screen passes 0 rows and must not be charged for a box it never paints
        let one_line_prompt = prompt_height.min(PROMPT_HEIGHT);

        let gap_after_logo = if error_height > 0 { 1 } else { 0 };
        let tip_gap = if tip_height > 0 { 1u16 } else { 0 };
        let fixed_below = Self::fixed_below(tip_height, prompt_height);
        let column = StackedColumn::new(&input, prompt_height);

        // Stacked info slot: the announcement at its draft-independent rows, else the changelog
        // The announcement outranks the logo, so the tier steps down to keep it on screen; the changelog yields to the logo as before
        let announcement_rows = stacked_announcement_rows(&input);
        let info_height = if announcement.is_some() {
            announcement_rows
        } else {
            changelog_height
        };
        let reserved = if announcement_rows > 0 {
            announcement_rows + 1
        } else {
            0
        };
        // Stacked layout: skip the logo in compact mode (the session picker needs the space); otherwise the height picks the tier and only an overflowing column steps it down
        let logo_tier = if compact {
            LogoTier::Hidden
        } else {
            column.logo_tier(reserved)
        };
        let fixed_above = column.fixed_above(logo_tier);

        // The stacked info slot below the menu holds whichever block is shown (announcement or changelog), matching the hero box's single-slot rule
        let (eff_changelog_height, _) = if !compact {
            Self::effective_changelog(
                content_area.height,
                fixed_above,
                menu_height,
                fixed_below,
                info_height,
            )
        } else {
            (0, 0)
        };
        let eff_changelog_gap = if eff_changelog_height > 0 { 1u16 } else { 0 };
        let logo_gap = 1u16;
        let flex_gap = 1u16;
        // Compute top_pad using the *default* menu height (4 items, 7 rows) and the one-line prompt so the logo position stays constant regardless of picker/focus state or draft length
        let top_pad = if compact {
            0
        } else {
            let default_menu_height = 4u16;
            let remaining = content_area.height.saturating_sub(fixed_above);
            let centered = remaining
                .saturating_sub(default_menu_height)
                .saturating_sub(eff_changelog_gap + eff_changelog_height)
                .saturating_sub(Self::fixed_below(tip_height, one_line_prompt))
                / 3;
            // A growing draft consumes the flex gap before the padding
            centered.min(content_area.height.saturating_sub(
                fixed_above
                    + menu_height
                    + eff_changelog_gap
                    + eff_changelog_height
                    + flex_gap
                    + fixed_below,
            ))
        };
        let [
            _,
            logo,
            _,
            _,
            error,
            menu,
            _,
            changelog,
            _,
            tip,
            _,
            prompt,
            _,
            version,
        ] = Layout::vertical([
            Constraint::Length(top_pad),
            Constraint::Length(logo_tier.rows(input.logo)),
            Constraint::Length(logo_gap), // gap after logo
            Constraint::Length(gap_after_logo),
            Constraint::Length(error_height),
            Constraint::Length(menu_height),
            Constraint::Length(eff_changelog_gap),
            Constraint::Length(eff_changelog_height),
            Constraint::Min(flex_gap),
            Constraint::Length(tip_height),
            Constraint::Length(tip_gap),
            Constraint::Length(prompt_height),
            Constraint::Length(VERSION_GAP),
            Constraint::Length(1), // version
        ])
        .areas(content_area);
        Self {
            logo,
            error,
            menu,
            changelog,
            tip,
            prompt,
            version,
            hero_box: zero,
            hero_logo: zero,
            hero_version: zero,
            hero_subtitle: zero,
            hero_info: zero,
            hero_menu: zero,
            logo_tier,
        }
    }
}

fn render_version_badge(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    config: &WelcomeConfig<'_>,
    hero: bool,
) {
    let title = Span::styled(
        config.title,
        Style::default()
            .fg(theme.gray_bright)
            .add_modifier(Modifier::BOLD),
    );
    let footer = config
        .footer_hint
        .map(|hint| Line::styled(hint, theme.faint()));
    let line = if !hero && let Some(footer) = footer {
        footer
    } else {
        Line::from(vec![
            title,
            Span::styled(
                if config.version.is_empty() { "" } else { " " },
                theme.muted(),
            ),
            Span::styled(config.version, theme.muted()),
        ])
    };
    let line = truncate_line(
        crate::engine::render::line_utils::line_to_static(&line),
        area.width as usize,
    );
    let alignment = if hero {
        Alignment::Left
    } else {
        Alignment::Right
    };
    Paragraph::new(line.alignment(alignment)).render(area, buf);
}

pub fn render_welcome(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    p: &WelcomeRenderParams<'_>,
    prompt: &mut PromptWidget,
) -> WelcomeRenderResult {
    let area = area.intersection(buf.area);
    if area.is_empty() {
        return WelcomeRenderResult::default();
    }
    buf.set_style(area, Style::default().bg(theme.bg_base));
    let cfg = p.config;
    let margin = if p.compact {
        H_MARGIN_COMPACT
    } else {
        H_MARGIN
    };
    let [top_bar, content_area] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)])
        .areas(inset_horizontal(area, margin));
    top_bar::render_top_bar(top_bar, buf, theme, p.cwd, None);
    let mut input = WelcomeLayoutInput {
        content_area,
        logo: cfg.logo,
        error_height: u16::from(p.error.is_some()),
        menu_height: cfg.menu.len().min(content_area.height as usize) as u16,
        tip_height: u16::from(p.tip.is_some()),
        changelog_height: if cfg.notes.is_empty() {
            0
        } else {
            cfg.notes.len().min(usize::from(content_area.height)) as u16 + 2
        },
        prompt_compact: p.compact,
        announcement: p.notice,
        expanded: p.notice_expanded,
        has_upgrade_cta: p.notice_action.is_some(),
        ..WelcomeLayoutInput::default()
    };
    let height = prompt::desired_prompt_height(
        prompt,
        content_area.width,
        p.compact,
        prompt_max_height(&input),
    );
    input.prompt_height = Some(height);
    let layout = WelcomeLayout::compute(input);
    let items: Vec<_> = cfg
        .menu
        .iter()
        .map(|item| (item.shortcut, item.label))
        .collect();
    let mut result = WelcomeRenderResult::default();

    if layout.has_hero_box() {
        let rects = hero_box::render_hero_box(
            &layout,
            buf,
            theme,
            &items,
            p.selected,
            p.mouse_pos,
            p.notice,
            p.notice_expanded,
            cfg,
            p.notice_action,
        );
        result.menu_rects = rects.menu_rects;
        result.notes_rect = rects.changelog_cta_rect;
        result.notice_rect = rects.announcement_rect;
        result.notice_truncated = rects.announcement_truncated;
        result.notice_action_rect = rects.upgrade_cta_rect;
    } else {
        render_logo_tier(layout.logo, buf, theme, layout.logo_tier, cfg.logo);
        let inset = prompt::prompt_inset(p.compact);
        result.menu_rects = menu::render_menu(
            inset_horizontal(layout.menu, inset),
            buf,
            theme,
            &items,
            p.selected,
            p.mouse_pos,
            MENU_MIN_WIDTH,
            cfg.logo,
        );
        if layout.changelog.height > 0 {
            let info = inset_horizontal(layout.changelog, inset);
            if let Some(notice) = p.notice {
                (
                    result.notice_rect,
                    result.notice_truncated,
                    result.notice_action_rect,
                ) = render_announcement_section(
                    info,
                    buf,
                    theme,
                    notice,
                    MENU_MIN_WIDTH,
                    content_area.height,
                    cfg.logo,
                    p.notice_expanded,
                    p.mouse_pos,
                    p.notice_action,
                );
            } else {
                result.notes_rect = render_changelog_section(
                    info,
                    buf,
                    theme,
                    cfg.notes_title,
                    cfg.notes,
                    MENU_MIN_WIDTH,
                    content_area.height,
                    cfg.logo,
                    cfg.notes_clickable,
                    p.mouse_pos,
                );
            }
        }
    }

    if let Some(error) = p.error.filter(|_| layout.error.height > 0) {
        buf.set_line_safe(
            layout.error.x,
            layout.error.y,
            &Line::styled(error, Style::default().fg(theme.accent_error)),
            layout.error.width,
        );
    }
    if let Some(tip) = p.tip.filter(|_| layout.tip.height > 0) {
        let tip = Line::styled(tip, theme.muted());
        buf.set_line_safe(layout.tip.x, layout.tip.y, &tip, layout.tip.width);
    }
    (result.cursor_pos, result.post_flush_escapes) = prompt::render_prompt(
        layout.prompt,
        buf,
        p.prompt_focus,
        prompt,
        &p.prompt_info,
        p.compact,
        cfg.placeholder,
    );
    result.prompt_rect = (!layout.prompt.is_empty()).then_some(layout.prompt);
    if !layout.has_hero_box() || cfg.footer_hint.is_some() {
        render_version_badge(layout.version, buf, theme, cfg, false);
    }
    result
}

fn inset_horizontal(rect: Rect, inset: u16) -> Rect {
    Rect {
        x: rect.x + inset,
        width: rect.width.saturating_sub(inset * 2),
        ..rect
    }
}

/// Render the changelog section (header and bullets), centered to the menu width.
/// When `clickable` (full notes exist) the whole block opens the notes on click and brightens while hovered; returns that clickable rect.
#[allow(clippy::too_many_arguments)]
fn render_changelog_section(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    title: &str,
    bullets: &[String],
    min_width_hint: u16,
    content_height: u16,
    logo: WelcomeLogo<'_>,
    clickable: bool,
    mouse_pos: Option<(u16, u16)>,
) -> Option<Rect> {
    let menu_width = logo::logo_visual_width(content_height, logo)
        .max(30)
        .max(min_width_hint);
    let [_, centered, _] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(menu_width),
        Constraint::Min(0),
    ])
    .flex(Flex::Center)
    .areas(area);

    if centered.width < 20 || centered.height == 0 {
        return None;
    }

    let hovered =
        clickable && mouse_pos.is_some_and(|(mx, my)| centered.contains(Position::new(mx, my)));

    let header_style = hover_style(
        theme,
        hovered,
        Style::default()
            .fg(theme.gray_bright)
            .add_modifier(Modifier::DIM),
    );
    buf.set_span(
        centered.x,
        centered.y,
        &Span::styled(title, header_style),
        centered.width,
    );

    let bullet_style = hover_style(theme, hovered, Style::default().fg(theme.gray_bright));
    let max_text_width = centered.width.saturating_sub(2) as usize; // The "• " prefix is 2 cols
    for (i, bullet) in bullets.iter().enumerate() {
        let row = centered.y + 2 + i as u16;
        if row >= centered.y + centered.height {
            break;
        }
        let truncated = crate::engine::render::line_utils::truncate_str(bullet, max_text_width);
        let text = format!("\u{2022} {truncated}");
        buf.set_span(
            centered.x,
            row,
            &Span::styled(text, bullet_style),
            centered.width,
        );
    }

    clickable.then_some(centered)
}

/// Wrap width of the stacked info slot, centered at the menu width inside the inset.
/// Both `compute`'s height measurement and `render_announcement_section` go through here, so the widths cannot drift.
/// `logo_height` selects the min menu width.
fn stacked_info_width(
    avail_width: u16,
    logo_height: u16,
    min_width_hint: u16,
    logo: WelcomeLogo<'_>,
) -> u16 {
    logo::logo_visual_width(logo_height, logo)
        .max(30)
        .max(min_width_hint)
        .min(avail_width)
}

/// Render the announcement in the stacked info slot, centered to the menu width.
/// Returns `(block_rect, truncated)`: the clickable block and the overflow flag.
#[allow(clippy::too_many_arguments)]
fn render_announcement_section(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    announcement: &WelcomeAnnouncement,
    min_width_hint: u16,
    content_height: u16,
    logo: WelcomeLogo<'_>,
    expanded: bool,
    mouse_pos: Option<(u16, u16)>,
    upgrade_cta: Option<&str>,
) -> (Option<Rect>, bool, Option<Rect>) {
    // Same width the height pre-pass reserved for (see `stacked_info_width`).
    let menu_width = stacked_info_width(area.width, content_height, min_width_hint, logo);
    let [_, centered, _] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(menu_width),
        Constraint::Min(0),
    ])
    .flex(Flex::Center)
    .areas(area);

    if centered.width < 20 || centered.height == 0 {
        return (None, false, None);
    }

    // Mirror the hero: reserve the CTA rows at the bottom, draw the text into what's left, then place the `[label]` button right after the drawn text
    let (text_area, truncated, cta_rect) = hero_box::render_announcement_with_upgrade_cta(
        buf,
        theme,
        centered,
        announcement,
        expanded,
        mouse_pos,
        upgrade_cta,
    );
    (Some(text_area), truncated, cta_rect)
}
