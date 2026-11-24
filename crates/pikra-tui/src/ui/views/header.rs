// Adapted from the Grok Build pager (Apache-2.0). Copyright 2023-2026 SpaceXAI.

//! Conversation header and its screen-coordinate hit targets.

use super::location::{location_parts, worktree_badge};
use crate::engine::{
    render::{
        SafeBuf,
        line_utils::{truncate_line, truncate_to_width},
    },
    theme::Theme,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::{Line, Span},
};
use std::{collections::HashMap, path::Path};
use unicode_width::UnicodeWidthStr;

struct StatusEntry {
    /// Identifier for hit-test lookup (e.g., "context", "queue").
    id: &'static str,
    /// Pre-built styled content.
    line: Line<'static>,
    /// Display width in columns.
    width: u16,
}

/// Builder for the agent status bar.
///
/// Collect items with [`push`], then call [`render`] to lay them out right-aligned with separators and get back hit-test areas.
pub struct AgentStatusBar<'a> {
    items: Vec<StatusEntry>,
    theme: &'a Theme,
    /// Padding from the right edge of the status bar area.
    right_pad: u16,
}

impl<'a> AgentStatusBar<'a> {
    pub fn new(theme: &'a Theme) -> Self {
        Self {
            items: Vec::new(),
            theme,
            right_pad: 0,
        }
    }

    /// Items are rendered left-to-right in push order, but the entire group is right-aligned within the status bar area.
    pub fn push(&mut self, id: &'static str, line: Line<'static>) {
        let width = line.width() as u16;
        self.items.push(StatusEntry { id, line, width });
    }

    /// Prepend an item sized with [`Self::room_for_front`], so it can never push the items already pushed off the row.
    pub fn push_front(&mut self, id: &'static str, line: Line<'static>) {
        let width = line.width() as u16;
        self.items.insert(0, StatusEntry { id, line, width });
    }

    /// Columns a prepended item may take in a row `area_width` wide: what the current group and its joining separator leave.
    pub fn room_for_front(&self, area_width: u16) -> u16 {
        let joining_sep = if self.items.is_empty() {
            0
        } else {
            SEPARATOR_WIDTH
        };
        area_width.saturating_sub(self.right_pad + self.width() + joining_sep)
    }

    /// Columns the group occupies: items plus the separators between them.
    fn width(&self) -> u16 {
        let items: u16 = self.items.iter().map(|e| e.width).sum();
        let seps = (self.items.len() as u16).saturating_sub(1);
        items + seps * SEPARATOR_WIDTH
    }

    /// Render all items right-aligned into the given area. Layout: `··· item0 │ item1 │ item2`;
    /// separators appear only between items, never before the first or after the last. Returns a map
    /// from item ID to screen `Rect` for hit-testing.
    pub fn render(self, buf: &mut Buffer, area: Rect) -> HashMap<&'static str, Rect> {
        if area.height == 0 || area.width == 0 || self.items.is_empty() {
            return HashMap::new();
        }

        buf.set_style(area, Style::default().bg(self.theme.bg_base));

        let sep = separator(self.theme);
        let sep_w = SEPARATOR_WIDTH;

        // Right-align: compute starting x
        let start_x = area
            .x
            .saturating_add(area.width.saturating_sub(self.right_pad + self.width()));

        let mut x = start_x;
        let mut areas = HashMap::new();

        for (i, entry) in self.items.iter().enumerate() {
            // Separator before every item except the first.
            if i > 0 {
                buf.set_span(x, area.y, &sep, sep_w);
                x += sep_w;
            }

            // Render item
            buf.set_line(x, area.y, &entry.line, entry.width);
            areas.insert(
                entry.id,
                Rect {
                    x,
                    y: area.y,
                    width: entry.width,
                    height: 1,
                },
            );
            x += entry.width;
        }

        areas
    }
}

/// ` │ ` a step fainter than the chips it divides — the divider between status-bar items and between the header's title and
/// location.
pub(crate) fn separator(theme: &Theme) -> Span<'static> {
    Span::styled(" │ ", theme.faint().bg(theme.bg_base))
}

/// Display width of [`separator`].
const SEPARATOR_WIDTH: u16 = 3;

pub struct HeaderButton<'a> {
    pub id: &'static str,
    pub label: &'a str,
}

pub struct HeaderConfig<'a> {
    pub title: Option<&'a str>,
    pub cwd: &'a Path,
    pub buttons: &'a [HeaderButton<'a>],
    pub pointer: Option<(u16, u16)>,
    pub suppress_hits: bool,
}

#[derive(Default)]
pub struct HeaderHits {
    pub path: Option<Rect>,
    pub buttons: HashMap<&'static str, Rect>,
}

impl HeaderHits {
    pub fn button_at(&self, column: u16, row: u16) -> Option<&'static str> {
        self.buttons
            .iter()
            .find_map(|(&id, rect)| rect.contains((column, row).into()).then_some(id))
    }
}

pub fn render_header(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    config: &HeaderConfig<'_>,
) -> HeaderHits {
    let area = area.intersection(buf.area);
    if area.is_empty() {
        return HeaderHits::default();
    }
    let bg = Style::default().bg(theme.bg_base);
    let dim = theme.dim().bg(theme.bg_base);
    buf.set_style(area, bg);
    // The status group is right-aligned and gives its exact occupied width back to the location.
    let mut status = AgentStatusBar::new(theme);
    let mut budget = area.width.saturating_sub(1);
    let mut buttons = Vec::new();
    for item in config.buttons.iter().rev() {
        let width = item.label.width().min(usize::from(u16::MAX)) as u16;
        if width == 0 || width > budget {
            continue;
        }
        budget -= width;
        let x = area.x + budget + 1;
        let hovered = !config.suppress_hits
            && config
                .pointer
                .is_some_and(|(column, row)| row == area.y && column >= x && column < x + width);
        let style = if hovered {
            bg.fg(theme.text_primary)
        } else {
            bg.fg(theme.gray)
        };
        buttons.push((item.id, Line::styled(item.label.to_owned(), style)));
        budget = budget.saturating_sub(SEPARATOR_WIDTH);
    }
    for (id, line) in buttons.into_iter().rev() {
        status.push(id, line);
    }
    let areas = status.render(buf, area);
    let parts = location_parts(config.cwd);
    let location_budget = areas
        .values()
        .map(|rect| rect.x)
        .min()
        .map_or(area.width, |x| x.saturating_sub(area.x).saturating_sub(1));
    let mut location = Vec::new();
    if let Some(branch) = parts.branch {
        location.push(Span::styled(branch, dim));
        location.push(Span::styled(" ", bg));
    }
    if parts.is_worktree {
        location.push(worktree_badge(theme).patch_style(bg));
    }
    let prefix_width = location.iter().map(|s| s.width() as u16).sum::<u16>();
    let path_width = parts.cwd_display.width().min(usize::from(u16::MAX)) as u16;
    let title = config.title.and_then(|title| {
        const PATH_MIN: u16 = 12;
        let location_min = prefix_width
            .saturating_add(path_width.min(PATH_MIN))
            .saturating_add(SEPARATOR_WIDTH);
        let title_cap = (location_budget / 2).min(location_budget.saturating_sub(location_min));
        let title = truncate_to_width(title, usize::from(title_cap));
        (!title.is_empty()).then(|| title.into_owned())
    });
    let mut path_offset = prefix_width;
    let mut spans = Vec::new();
    if let Some(title) = title {
        let sep = separator(theme);
        path_offset += (title.width() + sep.width()) as u16;
        spans.push(Span::styled(title, bg.fg(theme.text_secondary)));
        spans.push(sep);
    }
    let path_x = area.x.saturating_add(path_offset);
    let visible_path_width = path_width.min(location_budget.saturating_sub(path_offset));
    let path_rect = Rect::new(path_x, area.y, visible_path_width, 1);
    let hovered = !config.suppress_hits
        && config
            .pointer
            .is_some_and(|(x, y)| path_rect.contains((x, y).into()));
    let style = if hovered {
        bg.fg(theme.text_primary)
    } else if path_offset > prefix_width {
        dim
    } else {
        bg.fg(theme.text_secondary)
    };
    location.push(Span::styled(parts.cwd_display, style));
    spans.extend(location);
    let line = truncate_line(Line::from(spans), usize::from(location_budget));
    buf.set_line_safe(area.x, area.y, &line, location_budget);
    if config.suppress_hits {
        HeaderHits::default()
    } else {
        HeaderHits {
            path: (visible_path_width > 0).then_some(path_rect),
            buttons: areas,
        }
    }
}
