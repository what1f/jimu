//! Host-neutral scrollback pointer and search state.

use std::sync::Arc;

use ratatui::buffer::Buffer;

use crate::engine::markdown::TableCopyMeta;

use super::table_geometry::{CellRef, TableGeometry};
use super::text_selection::{
    ActiveBlockDrag, ActiveTextDrag, AutoScrollDirection, PendingBlockDrag, PendingTextDrag,
    PersistentTextSelection, RangeHit, ResolvedSelectionBoundaries, SelectionEndpoint,
    SelectionKind, SelectionOrigin, apply_selection_boundary, block_drag_threshold_exceeded,
    compute_autoscroll, drag_threshold_exceeded, reconstruct_full_selection_text_with_boundaries,
    reconstruct_selection_text_with_boundaries, reconstruct_table_selection_text_with_meta,
    render_active_selection_overlay, render_block_drag_overlay,
    render_persistent_selection_overlay, resolve_table_drag_kind, semantic_selection_at,
};
use super::types::{derive_selection_text, prewrap_index_per_row};
use super::{EntryId, ScrollbackFrame, ScrollbackSearchState, ScrollbackState};

#[derive(Debug, Default)]
pub(super) struct InteractionState {
    frame: Option<Arc<ScrollbackFrame>>,
    boundaries: ResolvedSelectionBoundaries,
    drag: Option<TextDrag>,
    selection: Option<Selection>,
    block_drag: Option<BlockDrag>,
    block_copy: Option<String>,
}

#[derive(Debug)]
struct BlockDrag {
    pending: PendingBlockDrag,
    anchor: Option<EntryId>,
    head: Option<EntryId>,
    pointer: (u16, u16),
}

#[derive(Debug)]
struct TextDrag {
    id: EntryId,
    pending: PendingTextDrag,
    active: Option<ActiveTextDrag>,
    geometry: Option<TableGeometry>,
    pointer: (u16, u16),
}

#[derive(Debug)]
struct Selection {
    id: EntryId,
    text: String,
    highlight: PersistentTextSelection,
    geometry: Option<TableGeometry>,
}

fn endpoint(hit: RangeHit) -> SelectionEndpoint {
    SelectionEndpoint {
        block_line_idx: hit.block_line_idx,
        col_within_range: hit.col_within_range,
    }
}

impl ScrollbackState {
    pub fn set_search(&mut self, query: &str) {
        let mut search = self
            .search
            .take()
            .unwrap_or_else(ScrollbackSearchState::open);
        search.update_query(query, self);
        self.search = Some(search);
    }

    pub fn search(&self) -> Option<&ScrollbackSearchState> {
        self.search.as_ref()
    }

    pub fn poll_search(&mut self) -> bool {
        let changed = self
            .search
            .as_mut()
            .is_some_and(ScrollbackSearchState::poll);
        if changed {
            self.reveal_current_match();
        }
        changed
    }

    pub fn next_match(&mut self) {
        if let Some(search) = &mut self.search {
            search.next();
            self.reveal_current_match();
        }
    }

    pub fn prev_match(&mut self) {
        if let Some(search) = &mut self.search {
            search.prev();
            self.reveal_current_match();
        }
    }

    pub fn clear_search(&mut self) {
        self.search = None;
    }

    fn reveal_current_match(&mut self) {
        if let Some(matched) = self.search().and_then(ScrollbackSearchState::current)
            && let Some(index) = self.index_of_id(matched.entry_id)
        {
            self.reveal_entry_line(index, matched.line_in_entry);
        }
    }

    pub fn scroll_by(&mut self, delta: i16) {
        if delta < 0 {
            self.scroll_up(delta.unsigned_abs());
        } else if delta > 0 {
            self.scroll_down(delta.unsigned_abs());
        }
    }

    pub fn scroll_to_ratio(&mut self, ratio: f32) {
        if ratio.is_nan() {
            return;
        }
        let (_, viewport, total) = self.scroll_info();
        let max = total.saturating_sub(usize::from(viewport));
        self.set_scroll_offset((max as f64 * f64::from(ratio.clamp(0.0, 1.0))).round() as usize);
    }

    pub fn scroll_to_top(&mut self) {
        self.goto_top();
    }

    pub fn scroll_to_bottom(&mut self) {
        self.goto_bottom();
    }

    pub fn last_frame(&self) -> Option<&ScrollbackFrame> {
        self.interaction.frame.as_deref()
    }

    pub(super) fn present_frame(
        &mut self,
        frame: Arc<ScrollbackFrame>,
        boundaries: ResolvedSelectionBoundaries,
        buf: &mut Buffer,
    ) {
        self.interaction.frame = Some(frame);
        self.interaction.boundaries = boundaries;

        if let Some(drag) = &self.interaction.drag
            && drag.active.is_some()
        {
            let (col, row) = drag.pointer;
            self.update_selection(col, row);
        }
        if let Some(drag) = &self.interaction.block_drag
            && drag.head.is_some()
        {
            let (col, row) = drag.pointer;
            self.update_selection(col, row);
        }
        let Some(frame) = self.last_frame() else {
            return;
        };
        let model = &frame.output.selection_model;
        if let Some(drag) = &self.interaction.drag
            && let Some(mut active) = drag.active
            && let Some(index) = self
                .index_of_id(drag.id)
                .and_then(|i| i.checked_sub(frame.entry_base))
        {
            active.anchor.entry_idx = index;
            active.head.entry_idx = index;
            render_active_selection_overlay(model, &active, drag.geometry.as_ref(), buf);
        } else if let Some(drag) = &self.interaction.block_drag
            && let Some((anchor, head)) = drag.anchor.zip(drag.head)
            && let Some((anchor, head)) = self.index_of_id(anchor).zip(self.index_of_id(head))
            && anchor.max(head) >= frame.entry_base
        {
            render_block_drag_overlay(
                model,
                &ActiveBlockDrag {
                    anchor_entry_idx: anchor.saturating_sub(frame.entry_base),
                    head_entry_idx: head.saturating_sub(frame.entry_base),
                },
                buf,
            );
        } else if let Some(selection) = &self.interaction.selection
            && let Some(index) = self
                .index_of_id(selection.id)
                .and_then(|i| i.checked_sub(frame.entry_base))
        {
            let highlight = PersistentTextSelection {
                entry_idx: index,
                ..selection.highlight
            };
            render_persistent_selection_overlay(
                model,
                &highlight,
                selection.geometry.as_ref(),
                buf,
            );
        }
    }

    /// Begin a character drag at the first selectable range under the pointer.
    pub fn start_selection(&mut self, col: u16, row: u16) -> bool {
        self.clear_text_selection();
        let Some(frame) = self.last_frame() else {
            return false;
        };
        let model = &frame.output.selection_model;
        let Some(anchor) = model.hit_test_selectable_range(col, row) else {
            if !frame
                .scrollback_rect
                .contains(ratatui::layout::Position::new(col, row))
            {
                return false;
            }
            let block = model
                .hit_test_visible_block(col, row)
                .filter(|b| b.drag_startable);
            let index = block.map_or(0, |block| block.entry_idx);
            let anchor = block
                .and_then(|_| {
                    frame
                        .block_hit_rects
                        .iter()
                        .find(|hit| hit.entry_index == frame.entry_base + index)
                })
                .map(|hit| hit.entry_id);
            self.interaction.block_drag = Some(BlockDrag {
                pending: PendingBlockDrag {
                    anchor_entry_idx: index,
                    start_col: col,
                    start_row: row,
                },
                anchor,
                head: None,
                pointer: (col, row),
            });
            return true;
        };
        self.arm_text_drag(anchor, col, row)
    }

    fn arm_text_drag(&mut self, anchor: RangeHit, col: u16, row: u16) -> bool {
        let Some(frame) = self.last_frame() else {
            return false;
        };
        let model = &frame.output.selection_model;
        let Some(hit) = frame
            .block_hit_rects
            .iter()
            .find(|hit| hit.entry_index == frame.entry_base + anchor.entry_idx)
        else {
            return false;
        };
        let pending = PendingTextDrag {
            anchor,
            start_col: col,
            start_row: row,
            anchor_content_width: model.visible_block_content_width(anchor.entry_idx),
        };
        self.interaction.drag = Some(TextDrag {
            id: hit.entry_id,
            pending,
            active: None,
            geometry: None,
            pointer: (col, row),
        });
        true
    }

    pub fn update_selection(&mut self, col: u16, row: u16) -> bool {
        let Some(mut drag) = self.interaction.drag.take() else {
            return self.update_block_drag(col, row);
        };
        let changed = self.update_drag(&mut drag, col, row);
        self.interaction.drag = Some(drag);
        changed
    }

    fn update_block_drag(&mut self, col: u16, row: u16) -> bool {
        let Some(mut drag) = self.interaction.block_drag.take() else {
            return false;
        };
        let Some(frame) = self.last_frame() else {
            return false;
        };
        let model = &frame.output.selection_model;
        let head_id = frame.block_at(col, row).map(|hit| hit.entry_id);

        // A press on chrome can still enter a text range. Once converted, keep the gesture in that range.
        if let Some(hit) = model.hit_test_selectable_range(col, row)
            && (col, row) != (drag.pending.start_col, drag.pending.start_row)
            && self.arm_text_drag(hit, col, row)
        {
            if let Some(mut text) = self.interaction.drag.take() {
                text.geometry =
                    self.table_geometry(&hit, text.id, text.pending.anchor_content_width);
                text.active = Some(ActiveTextDrag {
                    anchor: hit,
                    head: hit,
                    kind: resolve_table_drag_kind(
                        text.geometry.as_ref(),
                        &hit,
                        &hit,
                        SelectionKind::Linear,
                    ),
                    anchor_content_width: text.pending.anchor_content_width,
                });
                self.interaction.drag = Some(text);
            }
            return true;
        }

        drag.pointer = (col, row);
        let old_head = drag.head;
        if drag.anchor.is_some() && block_drag_threshold_exceeded(&drag.pending, col, row) {
            drag.head = head_id.or(old_head).or(drag.anchor);
        }
        let changed = drag.head != old_head;
        self.interaction.block_drag = Some(drag);
        changed
    }

    fn update_drag(&self, drag: &mut TextDrag, col: u16, row: u16) -> bool {
        drag.pointer = (col, row);
        if drag.active.is_none() && !drag_threshold_exceeded(&drag.pending, col, row) {
            return false;
        }
        let Some(frame) = self.last_frame() else {
            return false;
        };
        let Some(index) = self
            .index_of_id(drag.id)
            .and_then(|i| i.checked_sub(frame.entry_base))
        else {
            return false;
        };
        let model = &frame.output.selection_model;
        let anchor = RangeHit {
            entry_idx: index,
            ..drag.pending.anchor
        };
        if drag.active.is_none() {
            drag.geometry =
                self.table_geometry(&anchor, drag.id, drag.pending.anchor_content_width);
        }
        let old_head = drag.active.map_or(anchor, |active| RangeHit {
            entry_idx: index,
            ..active.head
        });
        let head = model
            .hit_test_nearest_in_range(anchor, col, row)
            .unwrap_or(old_head);
        let previous = drag
            .active
            .map_or(SelectionKind::Linear, |active| active.kind);
        let updated = ActiveTextDrag {
            anchor,
            head,
            kind: resolve_table_drag_kind(drag.geometry.as_ref(), &anchor, &head, previous),
            anchor_content_width: drag.pending.anchor_content_width,
        };
        let changed = drag.active != Some(updated);
        drag.active = Some(updated);
        changed
    }

    /// Call on host animation ticks while a drag is held at the viewport edge.
    pub fn tick_selection_autoscroll(&mut self) -> bool {
        let pointer = self
            .interaction
            .drag
            .as_ref()
            .filter(|drag| drag.active.is_some())
            .map(|drag| drag.pointer)
            .or_else(|| {
                self.interaction
                    .block_drag
                    .as_ref()
                    .filter(|drag| drag.head.is_some())
                    .map(|drag| drag.pointer)
            });
        let Some((_, row)) = pointer else {
            return false;
        };
        let Some(frame) = self.last_frame() else {
            return false;
        };
        let Some(scroll) = compute_autoscroll(row, frame.output.selection_model.content_area)
        else {
            return false;
        };
        let before = self.scroll_offset();
        match scroll.direction {
            AutoScrollDirection::Up => self.scroll_up(scroll.speed),
            AutoScrollDirection::Down => self.scroll_down(scroll.speed),
        }
        before != self.scroll_offset()
    }

    /// Finish a drag. The host can send the selected text to its clipboard.
    pub fn finish_selection(&mut self) -> bool {
        let Some(drag) = self.interaction.drag.take() else {
            return self.finish_block_drag();
        };
        let Some(mut active) = drag.active else {
            return false;
        };
        let Some(text) = self.copy_drag(drag.id, &mut active, drag.geometry.as_ref()) else {
            return false;
        };
        self.save_selection(
            drag.id,
            text,
            PersistentTextSelection {
                entry_idx: active.anchor.entry_idx,
                range_id: active.anchor.range_id,
                anchor: endpoint(active.anchor),
                head: endpoint(active.head),
                kind: active.kind,
                origin: SelectionOrigin::Drag,
            },
            drag.geometry
                .filter(|_| active.kind != SelectionKind::Linear),
        )
    }

    fn finish_block_drag(&mut self) -> bool {
        let Some(drag) = self.interaction.block_drag.take() else {
            return false;
        };
        let Some((a, b)) = drag
            .anchor
            .zip(drag.head)
            .and_then(|(a, b)| self.index_of_id(a).zip(self.index_of_id(b)))
        else {
            return false;
        };
        let Some(frame) = self.last_frame() else {
            return false;
        };
        let model = &frame.output.selection_model;
        let text = (a.min(b)..=a.max(b))
            .filter_map(|index| {
                let entry = self.entry(index)?;
                if !entry.block.is_drag_block_selectable()
                    || self.entry_content_hidden_by_group(index)
                {
                    return None;
                }
                let width = index
                    .checked_sub(frame.entry_base)
                    .and_then(|i| model.visible_block_content_width(i))
                    .unwrap_or(model.content_area.width);
                let ctx = entry.context(width, self.appearance(), self.cwd());
                entry
                    .block
                    .copy_visible_text_in_state(&ctx)
                    .filter(|text| !text.is_empty())
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        if text.is_empty() {
            return false;
        }
        self.interaction.block_copy = Some(text);
        true
    }

    pub fn select_word(&mut self, col: u16, row: u16) -> bool {
        let Some((hit, id)) = self.exact_hit(col, row) else {
            return false;
        };
        let Some(frame) = self.last_frame() else {
            return false;
        };
        let Some(word) = semantic_selection_at(
            &frame.output.selection_model,
            &hit,
            &self.appearance().word_separators,
        ) else {
            return false;
        };
        self.save_selection(
            id,
            word.text,
            PersistentTextSelection {
                entry_idx: hit.entry_idx,
                range_id: hit.range_id,
                anchor: word.anchor,
                head: word.head,
                kind: SelectionKind::Linear,
                origin: SelectionOrigin::DoubleClick,
            },
            None,
        )
    }

    pub fn select_line(&mut self, col: u16, row: u16) -> bool {
        let Some((hit, id)) = self.exact_hit(col, row) else {
            return false;
        };
        let Some(frame) = self.last_frame() else {
            return false;
        };
        let Some(line) = frame.output.selection_model.line_for_hit(&hit) else {
            return false;
        };
        let width = line
            .selectable_cols
            .end
            .saturating_sub(line.selectable_cols.start);
        if width == 0 {
            return false;
        }
        let boundary = self.interaction.boundaries.boundary_for_hit(&hit);
        let text = apply_selection_boundary(line.text.clone(), boundary, true, true);
        self.save_selection(
            id,
            text,
            PersistentTextSelection {
                entry_idx: hit.entry_idx,
                range_id: hit.range_id,
                anchor: SelectionEndpoint {
                    block_line_idx: hit.block_line_idx,
                    col_within_range: 0,
                },
                head: SelectionEndpoint {
                    block_line_idx: hit.block_line_idx,
                    col_within_range: width - 1,
                },
                kind: SelectionKind::Linear,
                origin: SelectionOrigin::TripleClick,
            },
            None,
        )
    }

    /// Select a table cell. On a grid line, select the table as tab-separated values.
    pub fn select_cell(&mut self, col: u16, row: u16) -> bool {
        let Some((hit, id)) = self.exact_hit(col, row) else {
            return false;
        };
        let width = self.last_frame().and_then(|frame| {
            frame
                .output
                .selection_model
                .visible_block_content_width(hit.entry_idx)
        });
        let Some(geometry) = self.table_geometry(&hit, id, width) else {
            return false;
        };
        let (first, last, kind) = match geometry.cell_at(hit.block_line_idx, hit.col_within_range) {
            Some(cell) => (cell, cell, SelectionKind::TableCell),
            None => {
                let first = CellRef { row: 0, col: 0 };
                let last = CellRef {
                    row: geometry.n_rows() - 1,
                    col: geometry.n_cols() - 1,
                };
                (
                    first,
                    last,
                    SelectionKind::TableGrid {
                        anchor: first,
                        head: last,
                    },
                )
            }
        };
        let Some(text) =
            self.with_selection_source(id, hit.range_id, width, Some(&geometry), |source, meta| {
                match kind {
                    SelectionKind::TableCell => geometry.cell_text_with_meta(first, source, meta),
                    _ => geometry.grid_tsv_with_meta(first, last, source, meta),
                }
            })
        else {
            return false;
        };
        self.save_selection(
            id,
            text,
            PersistentTextSelection {
                entry_idx: hit.entry_idx,
                range_id: hit.range_id,
                anchor: SelectionEndpoint {
                    block_line_idx: geometry.row_lines(first.row).start,
                    col_within_range: geometry.band(first.col).start,
                },
                head: SelectionEndpoint {
                    block_line_idx: geometry.row_lines(last.row).end.saturating_sub(1),
                    col_within_range: geometry.band(last.col).end.saturating_sub(1),
                },
                origin: SelectionOrigin::TripleClick,
                kind,
            },
            Some(geometry),
        )
    }

    pub fn clear_text_selection(&mut self) {
        self.interaction.drag = None;
        self.interaction.selection = None;
        self.interaction.block_drag = None;
        self.interaction.block_copy = None;
    }

    pub fn selected_text(&self) -> Option<String> {
        self.interaction
            .selection
            .as_ref()
            .filter(|selection| self.get_by_id(selection.id).is_some())
            .map(|selection| selection.text.clone())
            .or_else(|| self.interaction.block_copy.clone())
    }

    pub fn has_selection(&self) -> bool {
        self.interaction.block_copy.is_some()
            || self
                .interaction
                .selection
                .as_ref()
                .is_some_and(|s| self.get_by_id(s.id).is_some())
    }

    fn exact_hit(&self, col: u16, row: u16) -> Option<(RangeHit, EntryId)> {
        let frame = self.last_frame()?;
        let hit = frame.output.selection_model.hit_test_text_exact(col, row)?;
        let id = frame
            .block_hit_rects
            .iter()
            .find(|block| block.entry_index == frame.entry_base + hit.entry_idx)?
            .entry_id;
        Some((hit, id))
    }

    fn save_selection(
        &mut self,
        id: EntryId,
        text: String,
        highlight: PersistentTextSelection,
        geometry: Option<TableGeometry>,
    ) -> bool {
        if text.is_empty() {
            return false;
        }
        self.clear_text_selection();
        self.interaction.selection = Some(Selection {
            id,
            text,
            highlight,
            geometry,
        });
        true
    }

    fn table_geometry(
        &self,
        hit: &RangeHit,
        id: EntryId,
        width: Option<u16>,
    ) -> Option<TableGeometry> {
        if let Some(line) = self.last_frame()?.output.selection_model.line_for_hit(hit)
            && !line.text.contains(['│', '┌', '├', '└'])
        {
            return None;
        }
        self.with_selection_source(id, hit.range_id, width, None, |source, _| {
            TableGeometry::detect(source, hit.block_line_idx)
        })
        .flatten()
    }

    fn with_selection_source<R>(
        &self,
        id: EntryId,
        range_id: u16,
        width: Option<u16>,
        geometry: Option<&TableGeometry>,
        f: impl FnOnce(&dyn Fn(usize) -> Option<String>, Option<&TableCopyMeta>) -> R,
    ) -> Option<R> {
        let entry = self.get_by_id(id)?;
        let effective = entry.effective_output(width?, self.appearance(), false, self.cwd());
        let lines = &effective.output().lines;
        let source = |i: usize| {
            let line = lines.get(i)?;
            (line.selection_range == Some(range_id)).then(|| derive_selection_text(line))
        };
        let Some(geom) = geometry else {
            return Some(f(&source, None));
        };
        let offset = entry
            .block
            .markdown_body_line_offset(entry.display_mode(), self.appearance());
        let prewrap = prewrap_index_per_row(lines);
        Some(entry.block.with_table_copy_meta(|tables| {
            let meta = prewrap
                .get(geom.line_range().start)
                .and_then(|i| i.checked_sub(offset))
                .and_then(|i| {
                    tables.iter().find(|t| {
                        t.line_index == i
                            && t.n_cols == geom.n_cols()
                            && t.line_count == geom.line_range().len()
                            && t.cells.len() == geom.n_rows() * t.n_cols
                    })
                });
            f(&source, meta)
        }))
    }

    fn copy_drag(
        &self,
        id: EntryId,
        drag: &mut ActiveTextDrag,
        geometry: Option<&TableGeometry>,
    ) -> Option<String> {
        if drag.kind != SelectionKind::Linear
            && let Some(geom) = geometry
            && let Some(text) = self
                .with_selection_source(
                    id,
                    drag.anchor.range_id,
                    drag.anchor_content_width,
                    Some(geom),
                    |source, meta| {
                        if TableGeometry::detect(source, drag.anchor.block_line_idx).as_ref()
                            != Some(geom)
                        {
                            return None;
                        }
                        reconstruct_table_selection_text_with_meta(geom, drag, source, meta)
                    },
                )
                .flatten()
        {
            return Some(text);
        }
        drag.kind = SelectionKind::Linear;
        if let Some(entry) = self.get_by_id(id)
            && let Some(width) = drag.anchor_content_width
        {
            entry.ensure_cached(width, self.appearance(), false, self.cwd());
            let rendered = entry.cached_rendered_output_ref();
            if let Some(text) = reconstruct_full_selection_text_with_boundaries(
                &rendered.output.lines,
                &rendered.boundaries,
                drag,
            ) {
                return Some(text);
            }
        }
        reconstruct_selection_text_with_boundaries(
            &self.last_frame()?.output.selection_model,
            &self.interaction.boundaries,
            drag,
        )
    }
}
