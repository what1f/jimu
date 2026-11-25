//! Host-facing frame geometry backed by the scrollback renderer.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use tui_scrollbar::{CellFill, ScrollLengths, ScrollMetrics};

use crate::engine::render::color::blend_color;
use crate::engine::render::osc8::LinkTarget;
use crate::engine::render::scrollbar::{
    ScrollbarClickResult, render_scrollbar_styled, scrollbar_click_to_offset, scrollbars_hidden,
};
use crate::engine::theme::Theme;

use super::{EntryId, RenderOutput, ScrollbackState, VisibleLinkMap};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockHit {
    pub entry_index: usize,
    pub entry_id: EntryId,
    pub rect: Rect,
    pub header: Option<Rect>,
    pub foldable: bool,
    pub group_header: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollbarHit {
    Track,
    Thumb,
}

#[derive(Debug, Default)]
pub struct ScrollbackFrame {
    pub output: RenderOutput,
    pub link_map: VisibleLinkMap,
    pub scrollbar_rect: Option<Rect>,
    pub block_hit_rects: Vec<BlockHit>,
    pub sticky_header_rect: Option<Rect>,
    pub(crate) entry_base: usize,
    pub(crate) scrollback_rect: Rect,
    pub(crate) scaled_scrollbar: Option<ScaledScrollbar>,
}

impl ScrollbackFrame {
    pub fn block_at(&self, col: u16, row: u16) -> Option<BlockHit> {
        let pos = ratatui::layout::Position::new(col, row);
        self.block_hit_rects
            .iter()
            .copied()
            .find(|hit| hit.rect.contains(pos))
    }

    pub fn link_target_at(&self, col: u16, row: u16) -> Option<&LinkTarget> {
        Some(&self.link_map.link_at(col, row)?.target)
    }

    pub fn link_at(&self, col: u16, row: u16) -> Option<String> {
        match self.link_target_at(col, row)? {
            LinkTarget::Url(url) => Some(url.to_string()),
            LinkTarget::File(path) => Some(url::Url::from_file_path(path).ok()?.into()),
        }
    }

    pub fn scrollbar_at(&self, col: u16, row: u16) -> Option<ScrollbarHit> {
        let rect = self.scrollbar_rect?;
        if !rect.contains(ratatui::layout::Position::new(col, row)) {
            return None;
        }
        let metrics = self.scaled_scrollbar?.metrics(rect.height);
        Some(match metrics.cell_fill(usize::from(row - rect.y)) {
            CellFill::Empty => ScrollbarHit::Track,
            _ => ScrollbarHit::Thumb,
        })
    }

    /// Bold or folded block headers retain the same group navigation as the keyboard.
    pub fn toggle_block(&self, state: &mut ScrollbackState, col: u16, row: u16) -> bool {
        let Some(hit) = self.block_at(col, row) else {
            return false;
        };
        let Some(index) = state.index_of_id(hit.entry_id) else {
            return false;
        };
        let header = hit
            .header
            .is_some_and(|rect| rect.contains(ratatui::layout::Position::new(col, row)));
        state.set_selected(Some(index));
        if header && !state.toggle_group_expansion() && hit.foldable {
            state.toggle_fold_selected();
        }
        true
    }

    /// Apply the same offset mapping for a track press or a drag motion.
    pub fn drag_scrollbar(&self, state: &mut ScrollbackState, row: u16) -> bool {
        let Some((rect, scaled)) = self.scrollbar_rect.zip(self.scaled_scrollbar) else {
            return false;
        };
        match scrollbar_click_to_offset(
            row.saturating_sub(rect.y),
            rect.height,
            scaled.total,
            scaled.viewport,
        ) {
            ScrollbarClickResult::Top => state.goto_top(),
            ScrollbarClickResult::Bottom => state.goto_bottom(),
            ScrollbarClickResult::Offset(offset) => {
                state.set_scroll_offset(offset.saturating_mul(scaled.scale));
            }
        }
        true
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ScaledScrollbar {
    total: u16,
    offset: u16,
    viewport: u16,
    scale: usize,
}

impl ScaledScrollbar {
    fn metrics(self, height: u16) -> ScrollMetrics {
        ScrollMetrics::new(
            ScrollLengths {
                content_len: usize::from(self.total),
                viewport_len: usize::from(self.viewport),
            },
            usize::from(self.offset),
            height,
        )
    }
}

pub(crate) fn paint_scrollbar(
    buf: &mut Buffer,
    rect: Rect,
    state: &ScrollbackState,
) -> Option<ScaledScrollbar> {
    let (offset, viewport, total) = state.scroll_info();
    if rect.is_empty() || scrollbars_hidden() || total <= usize::from(viewport) {
        return None;
    }

    // Use the same scale as the upstream host and its inverse mouse mapping.
    let scale = if total > usize::from(u16::MAX) {
        total / usize::from(u16::MAX) + 1
    } else {
        1
    };
    let scaled = ScaledScrollbar {
        total: (total / scale) as u16,
        offset: (offset / scale) as u16,
        viewport,
        scale,
    };
    let cfg = &state.appearance().scrollback.scrollbar;
    let theme = Theme::current();
    let bg = cfg.scrollbar_bg.unwrap_or(theme.scrollbar_bg);
    let fg = cfg.scrollbar_fg.unwrap_or(theme.scrollbar_fg);
    let thumb = if state.is_follow_mode() {
        blend_color(bg, fg, 0.4).unwrap_or(fg)
    } else {
        fg
    };
    render_scrollbar_styled(
        buf,
        Some(rect),
        scaled.total,
        scaled.viewport,
        scaled.offset,
        Style::default().bg(bg),
        Style::default().fg(thumb).bg(bg),
    );
    Some(scaled)
}
