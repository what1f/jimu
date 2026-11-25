//! Render finalized entries for the terminal's native scrollback.

use std::path::Path;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::engine::render::Renderable;
use crate::engine::theme::Theme;

use super::appearance::AppearanceConfig;
use super::wrappers::EntryRenderer;
use super::{
    BlockBackground, DisplayMode, EntryId, RenderBlock, ScrollbackEntry, ScrollbackState,
    ToolCallBlock,
};

/// A styled terminal row and whether its source continues onto the next row.
#[derive(Debug, Clone)]
pub struct InlineRow {
    pub line: Line<'static>,
    pub soft_wrap: bool,
}

/// Display mode for a block crossing the native scrollback commit frontier.
pub fn inline_display_mode(block: &RenderBlock, collapse_thinking: bool) -> DisplayMode {
    match block {
        RenderBlock::ToolCall(ToolCallBlock::Edit(_)) => DisplayMode::Expanded,
        RenderBlock::ToolCall(
            tc @ (ToolCallBlock::Search(_)
            | ToolCallBlock::Read(_)
            | ToolCallBlock::ListDir(_)
            | ToolCallBlock::MemorySearch(_)
            | ToolCallBlock::IntegrationSearch(_)),
        ) if tc.is_success() => DisplayMode::Collapsed,
        RenderBlock::ToolCall(_) => DisplayMode::Truncated,
        RenderBlock::Thinking(_) if collapse_thinking => DisplayMode::Collapsed,
        _ => DisplayMode::Expanded,
    }
}

/// Appearance shared by the live inline tail and its finalized terminal rows.
pub fn inline_appearance(base: &AppearanceConfig) -> AppearanceConfig {
    let mut a = base.clone();
    a.show_timestamps = false;
    a.scrollback.layout.block_pad_left = 0;
    a.scrollback.layout.block_pad_right = 0;
    a.scrollback.blocks.thinking.body_dim_italic = true;
    a.scrollback.blocks.thinking.collapsed_expand_hint = true;
    a.scrollback.blocks.thinking.rail_under_bullet = true;
    a.scrollback.blocks.prompt.bg = BlockBackground::None;
    a.render_mermaid = super::appearance::RenderMermaid::Off;
    a
}

/// Render a block already in its inline display mode. The host supplies the localized truncation footer.
/// A zero row limit preserves the full output.
pub fn render_inline(
    entry: &ScrollbackEntry,
    appearance: &AppearanceConfig,
    cwd: Option<&Path>,
    width: u16,
    max_rows: u16,
    truncation_footer: impl FnOnce(u16) -> Line<'static>,
) -> Vec<InlineRow> {
    if width == 0 {
        return Vec::new();
    }
    let theme = Theme::current();
    let renderer = EntryRenderer::new(entry, &theme)
        .with_appearance_ref(appearance)
        .with_cwd(cwd)
        .with_tick(0)
        .with_flat_background(true)
        .with_hide_accent(true)
        .with_dim_accent(true);
    let full_height = renderer.desired_height(width);
    let height = if max_rows == 0 {
        full_height
    } else {
        max_rows.min(full_height)
    };
    if height == 0 {
        return Vec::new();
    }

    let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
    renderer.render(Rect::new(0, 0, width, full_height), &mut buf);
    let mut wraps = renderer.row_soft_wraps(height);
    if height < full_height {
        let footer_row = height - 1;
        for x in 0..width {
            if let Some(cell) = buf.cell_mut((x, footer_row)) {
                cell.reset();
            }
        }
        let hidden = full_height - footer_row;
        let footer = truncation_footer(hidden).style(theme.dim().bg(Color::Reset));
        buf.set_line(0, footer_row, &footer, width);
        for flag in wraps
            .iter_mut()
            .skip(usize::from(footer_row.saturating_sub(1)))
        {
            *flag = false;
        }
    }

    (0..height)
        .map(|y| InlineRow {
            line: line_from_buffer(&buf, y),
            soft_wrap: wraps.get(usize::from(y)).copied().unwrap_or(false),
        })
        .collect()
}

fn line_from_buffer(buf: &Buffer, y: u16) -> Line<'static> {
    let last_visible = (0..buf.area.width).rev().find(|&x| {
        buf.cell((x, y))
            .is_some_and(|c| !c.skip && c.symbol() != "" && c.symbol() != " ")
    });
    let Some(last_visible) = last_visible else {
        return Line::default();
    };
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut skip = 0;
    for x in 0..=last_visible {
        let Some(cell) = buf.cell((x, y)) else {
            continue;
        };
        if skip > 0 {
            skip -= 1;
            continue;
        }
        let text = cell.symbol();
        if text.is_empty() {
            continue;
        }
        skip = text.width().saturating_sub(1);
        let style = Style::default()
            .fg(cell.fg)
            .bg(cell.bg)
            .add_modifier(cell.modifier);
        if let Some(previous) = spans.last_mut().filter(|span| span.style == style) {
            previous.content.to_mut().push_str(text);
        } else {
            spans.push(Span::styled(text.to_owned(), style));
        }
    }
    Line::from(spans)
}

impl ScrollbackState {
    /// Emit newly finalized blocks in order. A failed emit leaves that entry at the frontier for retry.
    pub fn commit_inline<E>(
        &mut self,
        width: u16,
        max_rows: u16,
        turn_running: bool,
        collapse_thinking: bool,
        truncation_footer: impl Fn(u16) -> Line<'static>,
        mut emit: impl FnMut(EntryId, &[InlineRow]) -> Result<(), E>,
    ) -> Result<usize, E> {
        if width == 0 {
            return Ok(0);
        }
        let appearance = inline_appearance(self.appearance());
        let mut index = self.commit_scan_cursor();
        let mut count = 0;
        while let Some(entry) = self.get(index) {
            let id = entry.id;
            if self.is_committed(id) {
                index += 1;
                continue;
            }
            if entry.is_pending_user_input
                || (turn_running
                    && entry.is_running
                    && !self.inline_running_entry_is_final(index, entry))
            {
                break;
            }
            if entry.is_running {
                self.finish_running(id);
            }
            let entry = self
                .get_mut(index)
                .expect("entry remains at the commit frontier");
            let mode = inline_display_mode(&entry.block, collapse_thinking);
            entry.set_display_mode(mode);
            let entry = self
                .get(index)
                .expect("entry remains at the commit frontier");
            let rows = render_inline(
                entry,
                &appearance,
                self.cwd(),
                width,
                max_rows,
                &truncation_footer,
            );
            self.set_commit_scan_cursor(index);
            emit(id, &rows)?;
            self.mark_committed(index);
            if matches!(mode, DisplayMode::Collapsed | DisplayMode::Truncated) {
                self.record_committed_for_expand(id);
            }
            index += 1;
            count += 1;
        }
        self.set_commit_scan_cursor(index);
        Ok(count)
    }

    fn inline_running_entry_is_final(&self, index: usize, entry: &ScrollbackEntry) -> bool {
        matches!(entry.block, RenderBlock::BgTask(_))
            || (matches!(entry.block, RenderBlock::AgentMessage(_))
                && ((index + 1)..self.len()).any(|j| {
                    self.get(j)
                        .is_some_and(|e| !matches!(e.block, RenderBlock::Thinking(_)))
                }))
    }
}
