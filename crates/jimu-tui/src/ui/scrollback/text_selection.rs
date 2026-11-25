use std::cmp::{max, min};
use std::ops::Range;
use std::sync::{Arc, LazyLock};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use regex::Regex;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::engine::markdown::{CellJoin, TableCopyMeta};
use crate::engine::theme::Theme;
use crate::ui::scrollback::table_geometry::{CellRef, TableGeometry, WrappedCellJoiner};
use crate::ui::scrollback::types::SelectionBoundary;

/// Direction for drag auto-scroll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoScrollDirection {
    Up,
    Down,
}

/// State for timer-driven drag auto-scroll. While active, `tick_drag_autoscroll` scrolls by `speed` rows per tick
/// in the given direction. The direction and speed are recomputed from the mouse position each time the pointer
/// moves. The state is cleared when the pointer returns inside the content area or the drag ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DragAutoScrollState {
    pub direction: AutoScrollDirection,
    /// Rows to scroll per tick.
    pub speed: u16,
}

/// Rows of near-edge interior zone that still trigger autoscroll.
const EDGE_THRESHOLD: u16 = 2;

/// Compute autoscroll direction and speed from mouse position relative to the scrollback content area.
/// Returns `Some(state)` when the pointer is above, below, or within [`EDGE_THRESHOLD`] rows of the content boundary.
/// Returns `None` when the pointer is comfortably inside the viewport.
pub fn compute_autoscroll(mouse_row: u16, content_area: Rect) -> Option<DragAutoScrollState> {
    let top = content_area.y;
    let bottom = content_area.y.saturating_add(content_area.height);

    if content_area.height == 0 {
        return None;
    }

    if mouse_row < top.saturating_add(EDGE_THRESHOLD) {
        // Above or near top edge.
        let distance = top.saturating_add(EDGE_THRESHOLD).saturating_sub(mouse_row);
        Some(DragAutoScrollState {
            direction: AutoScrollDirection::Up,
            speed: speed_for_distance(distance),
        })
    } else if mouse_row >= bottom.saturating_sub(EDGE_THRESHOLD) {
        // Below or near bottom edge.
        let distance = mouse_row
            .saturating_sub(bottom.saturating_sub(EDGE_THRESHOLD))
            .saturating_add(1);
        Some(DragAutoScrollState {
            direction: AutoScrollDirection::Down,
            speed: speed_for_distance(distance),
        })
    } else {
        None
    }
}

/// Map distance-beyond-edge to scroll speed (rows per tick).
fn speed_for_distance(distance: u16) -> u16 {
    match distance {
        0..=2 => 1,
        3..=5 => 2,
        6..=10 => 3,
        _ => 5,
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedSelectionModel {
    pub ranges: Vec<ResolvedSelectableRange>,
    pub visible_blocks: Vec<VisibleBlockGeometry>,
    pub content_area: Rect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedSelectionBoundary {
    entry_idx: usize,
    range_id: u16,
    block_line_idx: usize,
    boundary: Arc<SelectionBoundary>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ResolvedSelectionBoundaries(Vec<ResolvedSelectionBoundary>);

impl ResolvedSelectionBoundaries {
    pub(crate) fn push(&mut self, line: &ResolvedSelectableLine, boundary: Arc<SelectionBoundary>) {
        self.0.push(ResolvedSelectionBoundary {
            entry_idx: line.entry_idx,
            range_id: line.range_id,
            block_line_idx: line.block_line_idx,
            boundary,
        });
    }

    fn boundary_for_line(&self, line: &ResolvedSelectableLine) -> Option<&SelectionBoundary> {
        self.0
            .iter()
            .find(|boundary| {
                boundary.entry_idx == line.entry_idx
                    && boundary.range_id == line.range_id
                    && boundary.block_line_idx == line.block_line_idx
            })
            .map(|boundary| boundary.boundary.as_ref())
    }

    pub(crate) fn boundary_for_hit(&self, hit: &RangeHit) -> Option<&SelectionBoundary> {
        self.0
            .iter()
            .find(|boundary| {
                boundary.entry_idx == hit.entry_idx
                    && boundary.range_id == hit.range_id
                    && boundary.block_line_idx == hit.block_line_idx
            })
            .map(|boundary| boundary.boundary.as_ref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSelectableRange {
    pub entry_idx: usize,
    pub range_id: u16,
    pub lines: Vec<ResolvedSelectableLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSelectableLine {
    pub entry_idx: usize,
    /// Block `selection_range` ids count up from 0.
    /// `u16::MAX` is reserved for the labeled group header's synthetic row (`render::GROUP_HEADER_RANGE_ID`).
    pub range_id: u16,
    pub block_line_idx: usize,
    pub screen_y: u16,
    pub screen_x: u16,
    pub selectable_cols: Range<u16>,
    /// Logical copy text: trailing-trimmed for `Selectable::All`, a span subset for `Selectable::Spans`, or the stored override for tool headers.
    pub text: String,
    /// Exact text painted in the selectable columns (logical order, untrimmed), or `None` for synthetic rows whose painted cells equal `text`.
    /// Selection snaps and slices against this so drag columns line up with drawn cells even when `text` diverges from what was painted.
    /// See [`crate::ui::scrollback::types::painted_selectable_region`].
    pub painted_region: Option<String>,
    pub joiner_to_previous: Option<String>,
}

impl ResolvedSelectableLine {
    /// `(col distance, clamped col-within-range)` for a pointer at screen `col` on this line. Distance is 0 with the
    /// exact offset inside the selectable span, otherwise the gap to the nearer edge with the offset clamped to that
    /// edge. Every hit test resolves columns through this, so their same-row behavior cannot diverge.
    fn col_metrics(&self, col: u16) -> Option<(u16, u16)> {
        let start = self.screen_x.saturating_add(self.selectable_cols.start);
        let end = self.screen_x.saturating_add(self.selectable_cols.end);
        if end <= start {
            return None;
        }

        let width = end.saturating_sub(start);
        Some(if col < start {
            (start.saturating_sub(col), 0)
        } else if col >= end {
            (
                col.saturating_sub(end.saturating_sub(1)),
                width.saturating_sub(1),
            )
        } else {
            (0, col.saturating_sub(start))
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleBlockGeometry {
    pub entry_idx: usize,
    pub area: Rect,
    pub content_area: Rect,
    pub selection_area: Rect,
    pub content_width: u16,
    pub top_clipped: bool,
    pub bottom_clipped: bool,
    pub drag_startable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeHit {
    pub entry_idx: usize,
    pub range_id: u16,
    /// Stable line identifier: the line's index within the block's full output.
    /// Unlike the position in the visible `range.lines[]` array, this does not change when the viewport scrolls.
    pub block_line_idx: usize,
    pub col_within_range: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingTextDrag {
    pub anchor: RangeHit,
    pub start_col: u16,
    pub start_row: u16,
    /// The anchor block's `VisibleBlockGeometry.content_width` at mouse-down (`None` when the block had no geometry then).
    /// Copy needs the width the drag's `block_line_idx` values were captured against even after the block scrolls fully out of `visible_blocks`.
    pub anchor_content_width: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveTextDrag {
    pub anchor: RangeHit,
    pub head: RangeHit,
    pub kind: SelectionKind,
    /// Carried over from [`PendingTextDrag::anchor_content_width`].
    pub anchor_content_width: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionEndpoint {
    pub block_line_idx: usize,
    pub col_within_range: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionOrigin {
    Drag,
    DoubleClick,
    TripleClick,
}

/// Shape of a text selection: `Linear` sweeps whole lines between the endpoints.
/// Drags anchored inside a detected table cell are table-shaped (see [`crate::ui::scrollback::table_geometry`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectionKind {
    #[default]
    Linear,
    /// Head latched to the anchor cell: text selection clamped to the cell's column band, spanning its wrapped fragment lines.
    TableCell,
    /// A rectangular range of whole cells (`anchor` to `head`), copied as TSV.
    /// Cells are carried, not re-derived: endpoints can sit on columns that resolve to no cell, and paint/copy must match resolution.
    TableGrid { anchor: CellRef, head: CellRef },
}

/// Side-car [`TableGeometry`] keyed to the selection it was resolved for.
/// Consumers check the key; a stale side-car is ignored (table kinds paint nothing without geometry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableSelectionGeometry {
    pub entry_idx: usize,
    pub range_id: u16,
    pub geometry: TableGeometry,
}

impl TableSelectionGeometry {
    /// The geometry, if it was resolved for (`entry_idx`, `range_id`).
    pub fn for_selection(&self, entry_idx: usize, range_id: u16) -> Option<&TableGeometry> {
        (self.entry_idx == entry_idx && self.range_id == range_id).then_some(&self.geometry)
    }
}

/// A text selection that persists after mouse-up for visual feedback.
///
/// Cleared on the next click elsewhere, Escape, or any scrollback navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PersistentTextSelection {
    pub entry_idx: usize,
    pub range_id: u16,
    pub anchor: SelectionEndpoint,
    pub head: SelectionEndpoint,
    pub origin: SelectionOrigin,
    pub kind: SelectionKind,
}

impl ResolvedSelectionModel {
    pub fn push_line(&mut self, line: ResolvedSelectableLine) {
        if let Some(last) = self.ranges.last_mut()
            && last.entry_idx == line.entry_idx
            && last.range_id == line.range_id
        {
            last.lines.push(line);
            return;
        }

        self.ranges.push(ResolvedSelectableRange {
            entry_idx: line.entry_idx,
            range_id: line.range_id,
            lines: vec![line],
        });
    }

    pub fn hit_test_selectable_range(&self, col: u16, row: u16) -> Option<RangeHit> {
        let mut best: Option<(u16, RangeHit)> = None;

        for range in &self.ranges {
            for line in &range.lines {
                if line.screen_y != row {
                    continue;
                }
                let Some((distance, col_within_range)) = line.col_metrics(col) else {
                    continue;
                };

                let hit = RangeHit {
                    entry_idx: range.entry_idx,
                    range_id: range.range_id,
                    block_line_idx: line.block_line_idx,
                    col_within_range,
                };
                if distance == 0 {
                    return Some(hit);
                }
                if best
                    .as_ref()
                    .is_none_or(|(best_distance, _)| distance < *best_distance)
                {
                    best = Some((distance, hit));
                }
            }
        }

        best.map(|(_, hit)| hit)
    }

    /// Unlike [`Self::hit_test_selectable_range`] this never lands on another range and never misses while the anchor's
    /// range has visible lines. That way a drag over a dead row keeps extending the selection instead of retreating.
    /// `None` only when the range has no selectable lines in this model (scrolled fully out).
    pub fn hit_test_nearest_in_range(
        &self,
        anchor: RangeHit,
        col: u16,
        row: u16,
    ) -> Option<RangeHit> {
        let range = self.range(anchor.entry_idx, anchor.range_id)?;
        let mut best: Option<((u16, u16), usize, RangeHit)> = None;

        for line in &range.lines {
            let Some((col_distance, col_within_range)) = line.col_metrics(col) else {
                continue;
            };

            let key = (line.screen_y.abs_diff(row), col_distance);
            let anchor_distance = line.block_line_idx.abs_diff(anchor.block_line_idx);
            let hit = RangeHit {
                entry_idx: range.entry_idx,
                range_id: range.range_id,
                block_line_idx: line.block_line_idx,
                col_within_range,
            };
            if best.as_ref().is_none_or(|(best_key, best_anchor_dist, _)| {
                key < *best_key || (key == *best_key && anchor_distance > *best_anchor_dist)
            }) {
                best = Some((key, anchor_distance, hit));
            }
        }

        best.map(|(_, _, hit)| hit)
    }

    pub fn hit_test_visible_block(&self, col: u16, row: u16) -> Option<&VisibleBlockGeometry> {
        self.visible_blocks
            .iter()
            .find(|block| rect_contains(block.area, col, row))
    }

    /// The content width `entry_idx`'s block was rendered at this frame, or `None` when the block is not in the viewport.
    pub fn visible_block_content_width(&self, entry_idx: usize) -> Option<u16> {
        self.visible_blocks
            .iter()
            .find(|b| b.entry_idx == entry_idx)
            .map(|b| b.content_width)
    }

    pub fn range(&self, entry_idx: usize, range_id: u16) -> Option<&ResolvedSelectableRange> {
        self.ranges
            .iter()
            .find(|range| range.entry_idx == entry_idx && range.range_id == range_id)
    }

    /// Hit-test for text selection: only return a hit when the click is directly on selectable columns (distance == 0).
    ///
    /// Clicks on accent bars, borders, and padding fall through to block-level handling (fold toggle).
    pub fn hit_test_text_exact(&self, col: u16, row: u16) -> Option<RangeHit> {
        for range in &self.ranges {
            for line in &range.lines {
                if line.screen_y != row {
                    continue;
                }
                let Some((distance, col_within_range)) = line.col_metrics(col) else {
                    continue;
                };
                if distance == 0 {
                    return Some(RangeHit {
                        entry_idx: range.entry_idx,
                        range_id: range.range_id,
                        block_line_idx: line.block_line_idx,
                        col_within_range,
                    });
                }
            }
        }
        None
    }

    /// Look up the selectable line matching a [`RangeHit`].
    ///
    /// Shorthand for `model.range(hit)` then `find(block_line_idx)`.
    pub fn line_for_hit(&self, hit: &RangeHit) -> Option<&ResolvedSelectableLine> {
        self.range(hit.entry_idx, hit.range_id)?
            .lines
            .iter()
            .find(|l| l.block_line_idx == hit.block_line_idx)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingBlockDrag {
    pub anchor_entry_idx: usize,
    pub start_col: u16,
    pub start_row: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveBlockDrag {
    pub anchor_entry_idx: usize,
    pub head_entry_idx: usize,
}

pub fn drag_threshold_exceeded(pending: &PendingTextDrag, col: u16, row: u16) -> bool {
    let dx = pending.start_col.abs_diff(col);
    let dy = pending.start_row.abs_diff(row);
    dx >= 1 || dy >= 1
}

pub fn reconstruct_selection_text(
    model: &ResolvedSelectionModel,
    drag: &ActiveTextDrag,
) -> Option<String> {
    reconstruct_selection_text_with_boundaries(model, &ResolvedSelectionBoundaries::default(), drag)
}

pub(crate) fn reconstruct_selection_text_with_boundaries(
    model: &ResolvedSelectionModel,
    boundaries: &ResolvedSelectionBoundaries,
    drag: &ActiveTextDrag,
) -> Option<String> {
    let range = model.range(drag.anchor.entry_idx, drag.anchor.range_id)?;
    let start_bl = min(drag.anchor.block_line_idx, drag.head.block_line_idx);
    let end_bl = max(drag.anchor.block_line_idx, drag.head.block_line_idx);
    let mut out = String::new();
    let mut first = true;

    for line in &range.lines {
        if line.block_line_idx < start_bl || line.block_line_idx > end_bl {
            continue;
        }
        if !first {
            out.push_str(line.joiner_to_previous.as_deref().unwrap_or("\n"));
        }
        first = false;
        let slice =
            selection_slice_for_line_by_block_idx(drag, line, boundaries.boundary_for_line(line))?;
        out.push_str(&slice);
    }

    if out.is_empty() && !first {
        return Some(out);
    }
    if first {
        // No visible lines found in the current model for the drag range.
        // This can happen when both anchor and head have scrolled off-screen.
        return None;
    }

    Some(out)
}

pub fn block_drag_threshold_exceeded(pending: &PendingBlockDrag, col: u16, row: u16) -> bool {
    let dx = pending.start_col.abs_diff(col);
    let dy = pending.start_row.abs_diff(row);
    dx >= 1 || dy >= 1
}

pub fn render_block_drag_overlay(
    model: &ResolvedSelectionModel,
    drag: &ActiveBlockDrag,
    buf: &mut Buffer,
) {
    let theme = Theme::current();
    let start = min(drag.anchor_entry_idx, drag.head_entry_idx);
    let end = max(drag.anchor_entry_idx, drag.head_entry_idx);

    for block in &model.visible_blocks {
        if block.entry_idx >= start && block.entry_idx <= end {
            for y in block.area.y..block.area.y.saturating_add(block.area.height) {
                for x in block.area.x..block.area.x.saturating_add(block.area.width) {
                    if let Some(cell) = buf.cell_mut((x, y)) {
                        apply_selection_highlight(&theme, cell);
                    }
                }
            }
        }
    }
}

pub fn render_active_selection_overlay(
    model: &ResolvedSelectionModel,
    drag: &ActiveTextDrag,
    table: Option<&TableGeometry>,
    buf: &mut Buffer,
) {
    render_selection_overlay_impl(
        model,
        drag.anchor.entry_idx,
        drag.anchor.range_id,
        SelectionEndpoint {
            block_line_idx: drag.anchor.block_line_idx,
            col_within_range: drag.anchor.col_within_range,
        },
        SelectionEndpoint {
            block_line_idx: drag.head.block_line_idx,
            col_within_range: drag.head.col_within_range,
        },
        drag.kind,
        table,
        buf,
    );
}

/// Render a persistent text selection overlay (after mouse-up).
/// Unlike [`render_active_selection_overlay`], which reads from [`ActiveTextDrag`], this reads from [`PersistentTextSelection`].
/// It maps stable `block_line_idx` coordinates back to screen positions using the current frame's [`ResolvedSelectionModel`].
pub fn render_persistent_selection_overlay(
    model: &ResolvedSelectionModel,
    selection: &PersistentTextSelection,
    table: Option<&TableGeometry>,
    buf: &mut Buffer,
) {
    render_selection_overlay_impl(
        model,
        selection.entry_idx,
        selection.range_id,
        selection.anchor,
        selection.head,
        selection.kind,
        table,
        buf,
    );
}

#[allow(clippy::too_many_arguments)]
fn render_selection_overlay_impl(
    model: &ResolvedSelectionModel,
    entry_idx: usize,
    range_id: u16,
    anchor: SelectionEndpoint,
    head: SelectionEndpoint,
    kind: SelectionKind,
    table: Option<&TableGeometry>,
    buf: &mut Buffer,
) {
    let Some(range) = model.range(entry_idx, range_id) else {
        return;
    };
    // Table kinds need their geometry; without it (stale side-car) paint nothing rather than a misleading linear sweep
    let table = match kind {
        SelectionKind::Linear => None,
        SelectionKind::TableCell | SelectionKind::TableGrid { .. } => match table {
            Some(geom) => Some(geom),
            None => return,
        },
    };

    let theme = Theme::current();
    let start_bl = min(anchor.block_line_idx, head.block_line_idx);
    let end_bl = max(anchor.block_line_idx, head.block_line_idx);

    for line in &range.lines {
        let col_ranges: Vec<Range<u16>> = if let Some(geom) = table {
            // Snapped to grapheme boundaries and clipped to content so the highlight matches the copy.
            table_selected_cols_for_line(geom, kind, anchor, head, line.block_line_idx)
                .into_iter()
                .map(|cols| {
                    let display = display_text_for_cols(&line.text);
                    let snapped = endpoint_start_col(display.as_ref(), cols.start)
                        ..endpoint_end_col(display.as_ref(), cols.end.saturating_sub(1));
                    clip_cols_to_content(display.as_ref(), snapped)
                })
                .collect()
        } else {
            if line.block_line_idx < start_bl || line.block_line_idx > end_bl {
                continue;
            }
            match selected_cols_for_endpoints(
                anchor.block_line_idx,
                anchor.col_within_range,
                head.block_line_idx,
                head.col_within_range,
                line,
            ) {
                Some(cols) => vec![cols],
                None => continue,
            }
        };
        for cols in col_ranges {
            for col in cols.start..cols.end {
                let screen_x = line
                    .screen_x
                    .saturating_add(line.selectable_cols.start)
                    .saturating_add(col);
                if let Some(cell) = buf.cell_mut((screen_x, line.screen_y)) {
                    apply_selection_highlight(&theme, cell);
                }
            }
        }
    }
}

/// A table-cell selection's endpoints clamped into the cell's box (fragment lines x column band), normalized so start <= end.
/// The head can sit outside the cell whenever the latch kept the selection there.
fn table_cell_span(
    geom: &TableGeometry,
    cell: crate::ui::scrollback::table_geometry::CellRef,
    anchor: SelectionEndpoint,
    head: SelectionEndpoint,
) -> ((usize, u16), (usize, u16)) {
    let lines = geom.row_lines(cell.row);
    let band = geom.band(cell.col);
    let clamp = |ep: SelectionEndpoint| {
        (
            ep.block_line_idx
                .clamp(lines.start, lines.end.saturating_sub(1).max(lines.start)),
            ep.col_within_range
                .clamp(band.start, band.end.saturating_sub(1).max(band.start)),
        )
    };
    let a = clamp(anchor);
    let h = clamp(head);
    if a <= h { (a, h) } else { (h, a) }
}

/// Clip a painted column range to the non-whitespace content it covers, mirroring the copy (which trims fragments and skips blank ones).
/// Whitespace between content columns stays inside the range.
fn clip_cols_to_content(text: &str, cols: Range<u16>) -> Range<u16> {
    let mut start: Option<u16> = None;
    let mut end = cols.start;
    let mut col = 0u16;
    for grapheme in text.graphemes(true) {
        let width = UnicodeWidthStr::width(grapheme) as u16;
        if width == 0 {
            continue;
        }
        let next = col.saturating_add(width);
        if col >= cols.end {
            break;
        }
        if next > cols.start && !grapheme.chars().all(char::is_whitespace) {
            start.get_or_insert(col.max(cols.start));
            end = next.min(cols.end);
        }
        col = next;
    }
    match start {
        Some(start) => start..end,
        None => cols.start..cols.start,
    }
}

/// Selected column ranges on one line of a table-shaped selection.
/// `TableCell` yields at most one band-clamped range, `TableGrid` one band per selected column.
/// Border glyphs are never included.
fn table_selected_cols_for_line(
    geom: &TableGeometry,
    kind: SelectionKind,
    anchor: SelectionEndpoint,
    head: SelectionEndpoint,
    block_line_idx: usize,
) -> Vec<Range<u16>> {
    // Lines outside the grid can never paint; keep them allocation-free.
    if !geom.line_range().contains(&block_line_idx) {
        return Vec::new();
    }
    match kind {
        SelectionKind::Linear => Vec::new(),
        SelectionKind::TableCell => {
            let Some(cell) = geom.cell_at(anchor.block_line_idx, anchor.col_within_range) else {
                return Vec::new();
            };
            let band = geom.band(cell.col);
            let ((l0, c0), (l1, c1)) = table_cell_span(geom, cell, anchor, head);
            if block_line_idx < l0 || block_line_idx > l1 {
                return Vec::new();
            }
            let start = if block_line_idx == l0 { c0 } else { band.start };
            let end = if block_line_idx == l1 {
                c1.saturating_add(1).min(band.end)
            } else {
                band.end
            };
            if start >= end {
                return Vec::new();
            }
            // Named binding: `vec![start..end]` trips single_range_in_vec_init.
            let band_cols: Range<u16> = start..end;
            vec![band_cols]
        }
        SelectionKind::TableGrid { anchor: a, head: h } => {
            let Some(row) = geom.row_of_line(block_line_idx) else {
                return Vec::new();
            };
            let (r0, r1) = (a.row.min(h.row), a.row.max(h.row));
            let (c0, c1) = (a.col.min(h.col), a.col.max(h.col));
            if row < r0 || row > r1 {
                return Vec::new();
            }
            (c0..=c1)
                .filter(|&c| c < geom.n_cols())
                .map(|c| geom.band(c))
                .collect()
        }
    }
}

/// Resolve a drag's [`SelectionKind`], with hysteresis.
/// The head is latched from the cell the drag already holds, so only another cell's content changes the mode.
/// Grid-line anchors stay `Linear`.
pub fn resolve_table_drag_kind(
    geom: Option<&TableGeometry>,
    anchor: &RangeHit,
    head: &RangeHit,
    prev: SelectionKind,
) -> SelectionKind {
    let Some(geom) = geom else {
        return SelectionKind::Linear;
    };
    let Some(a) = geom.cell_at(anchor.block_line_idx, anchor.col_within_range) else {
        return SelectionKind::Linear;
    };
    let held = match prev {
        SelectionKind::TableGrid { head, .. } => head,
        SelectionKind::Linear | SelectionKind::TableCell => a,
    };
    let h = geom.latched_cell_at(held, head.block_line_idx, head.col_within_range);
    if a == h {
        SelectionKind::TableCell
    } else {
        SelectionKind::TableGrid { anchor: a, head: h }
    }
}

/// Copied text for a table-shaped selection: the band-clamped span for `TableCell`, whole cells as TSV for `TableGrid`.
/// `None` (meaning fall back to linear) for `Linear` drags or an anchor that no longer resolves.
pub fn reconstruct_table_selection_text(
    geom: &TableGeometry,
    drag: &ActiveTextDrag,
    text_at: impl Fn(usize) -> Option<String>,
) -> Option<String> {
    reconstruct_table_selection_text_with_meta(geom, drag, text_at, None)
}

pub fn reconstruct_table_selection_text_with_meta(
    geom: &TableGeometry,
    drag: &ActiveTextDrag,
    text_at: impl Fn(usize) -> Option<String>,
    meta: Option<&TableCopyMeta>,
) -> Option<String> {
    let anchor = SelectionEndpoint {
        block_line_idx: drag.anchor.block_line_idx,
        col_within_range: drag.anchor.col_within_range,
    };
    let head = SelectionEndpoint {
        block_line_idx: drag.head.block_line_idx,
        col_within_range: drag.head.col_within_range,
    };
    match drag.kind {
        SelectionKind::Linear => None,
        SelectionKind::TableCell => {
            let cell = geom.cell_at(anchor.block_line_idx, anchor.col_within_range)?;
            let Some(n_cols) = geom.checked_n_cols() else {
                return Some(String::new());
            };
            if cell.row >= geom.n_rows() || cell.col >= n_cols {
                return Some(String::new());
            }
            let band = geom.band(cell.col);
            let ((l0, c0), (l1, c1)) = table_cell_span(geom, cell, anchor, head);
            if l0 > l1 {
                return Some(String::new());
            }
            let row_start = geom.row_lines(cell.row).start;
            let mut joined = String::new();
            let mut prev_vis = None;
            let mut joiner = WrappedCellJoiner::new(band.end.saturating_sub(band.start));
            let copy = geom.cell_copy(cell, meta);
            for line in l0..=l1 {
                let Some(text) = text_at(line) else { continue };
                let start = if line == l0 {
                    endpoint_start_col(&text, c0).max(band.start)
                } else {
                    band.start
                };
                let end = if line == l1 {
                    endpoint_end_col(&text, c1).min(band.end)
                } else {
                    band.end
                };
                let copied = crate::ui::scrollback::types::slice_display_cols(&text, start, end);
                if let Some(copy) = copy {
                    let fragment = copied.trim();
                    if fragment.is_empty() {
                        continue;
                    }
                    let vis = line.saturating_sub(row_start);
                    if let Some(prev) = prev_vis {
                        for i in prev..vis {
                            if let Some(CellJoin::Gap(s)) = copy.joins.get(i) {
                                joined.push_str(s);
                            }
                        }
                    }
                    joined.push_str(fragment);
                    prev_vis = Some(vis);
                } else {
                    let full_band = crate::ui::scrollback::types::slice_display_cols(
                        &text, band.start, band.end,
                    );
                    joiner.push(&full_band, copied.trim());
                }
            }
            if copy.is_some() {
                Some(joined)
            } else {
                Some(joiner.into_string())
            }
        }
        SelectionKind::TableGrid { anchor, head } => {
            Some(geom.grid_tsv_with_meta(anchor, head, text_at, meta))
        }
    }
}

/// Uniform selection band in the classic inverted colors (`bg_base` on `text_primary`).
/// Styled spans (inline code, links, syntax highlighting) join the band instead of inverting to their own colors.
/// Terminal-native / colorless themes fall back to reverse video.
pub(crate) fn apply_selection_highlight(theme: &Theme, cell: &mut ratatui::buffer::Cell) {
    let band = theme.text_primary;
    if band == Color::Reset || theme.bg_base == Color::Reset {
        cell.modifier.insert(Modifier::REVERSED);
        return;
    }
    // A search-match highlight painted earlier in the frame sets REVERSED; left in place it would swap the band right back out
    cell.modifier.remove(Modifier::REVERSED);
    cell.set_fg(theme.bg_base);
    cell.set_bg(band);
}

fn selection_slice_for_line_by_block_idx(
    drag: &ActiveTextDrag,
    line: &ResolvedSelectableLine,
    boundary: Option<&SelectionBoundary>,
) -> Option<String> {
    let cols = selected_cols_for_line_by_block_idx(drag, line)?;
    let visible_width = line
        .selectable_cols
        .end
        .saturating_sub(line.selectable_cols.start);
    // Columns are visual cells of the painted row. Otherwise a trailing-trimmed or overridden `text` could drift from
    // the cells the user dragged.
    let selected = if crate::engine::render::bidi::is_enabled() {
        match line.painted_region.as_deref() {
            // Override rows (tool headers) paint a display that differs from the stored copy text
            // Its columns can't index the painted cells, so copy the whole override on overlap
            Some(region) if line.text.trim_end() != region.trim_end() => line.text.clone(),
            Some(region) => {
                let sliced = slice_text_cols(region, cols.clone());
                // Strip render-only trailing padding once the selection reaches the row end, matching the multi-row path
                // Only a row whose copy `text` is the trimmed form of the painted region (a `Selectable::All` row) qualifies
                // A `Selectable::Spans` row with legitimate trailing spaces keeps them
                if cols.end == visible_width && line.text == region.trim_end() {
                    sliced.trim_end().to_string()
                } else {
                    sliced
                }
            }
            None => slice_text_cols(&line.text, cols.clone()),
        }
    } else {
        slice_text_cols(&line.text, cols.clone())
    };
    Some(apply_selection_boundary(
        selected,
        boundary,
        cols.start == 0,
        cols.end == visible_width,
    ))
}

pub(crate) fn apply_selection_boundary(
    selected: String,
    boundary: Option<&SelectionBoundary>,
    include_prefix: bool,
    include_suffix: bool,
) -> String {
    let Some(boundary) = boundary else {
        return selected;
    };
    boundary.apply(selected, include_prefix, include_suffix)
}

/// Compute the selected column range for a given line based on anchor/head endpoints. Single-line:
/// `floor(min(anchor_col, head_col)).past(max(anchor_col, head_col))`. Multi-line first: `floor(start_col).width`.
/// Multi-line last: `0.past(end_col)`. Multi-line middle: `0.width` (full line).
fn selected_cols_for_endpoints(
    anchor_block_line: usize,
    anchor_col: u16,
    head_block_line: usize,
    head_col: u16,
    line: &ResolvedSelectableLine,
) -> Option<Range<u16>> {
    let width = line
        .selectable_cols
        .end
        .saturating_sub(line.selectable_cols.start);
    let bl = line.block_line_idx;
    let start_bl = min(anchor_block_line, head_block_line);
    let end_bl = max(anchor_block_line, head_block_line);
    let anchor_is_start = anchor_block_line <= head_block_line;

    if bl < start_bl || bl > end_bl {
        return None;
    }

    // Snap on the painted row so ranges line up with the drawn cells
    // Prefer the painted region (untrimmed, exact painted cells) over the trailing-trimmed `text`
    // On an RTL row the trimmed spaces paint at the opposite edge, so a narrower `text` would snap the highlight band off the drawn cells
    let snap_src = if crate::engine::render::bidi::is_enabled() {
        line.painted_region.as_deref().unwrap_or(line.text.as_str())
    } else {
        line.text.as_str()
    };
    let display = display_text_for_cols(snap_src);
    if start_bl == end_bl {
        let start = endpoint_start_col(display.as_ref(), min(anchor_col, head_col));
        return Some(
            start.min(width)
                ..endpoint_end_col(display.as_ref(), max(anchor_col, head_col)).min(width),
        );
    }

    if bl == start_bl {
        let start = if anchor_is_start {
            anchor_col
        } else {
            head_col
        };
        return Some(endpoint_start_col(display.as_ref(), start).min(width)..width);
    }

    if bl == end_bl {
        let end = if anchor_is_start {
            head_col
        } else {
            anchor_col
        };
        return Some(0..endpoint_end_col(display.as_ref(), end).min(width));
    }

    Some(0..width)
}

/// Exclusive end for a selection that includes the cell at `col`: one column past the grapheme there.
/// On a padded or empty line `col` can sit past the text, so the result is never less than `col + 1`.
fn endpoint_end_col(text: &str, col: u16) -> u16 {
    crate::ui::scrollback::types::col_past_grapheme(text, col).max(col.saturating_add(1))
}

/// Start of a selection whose first cell is `col`: the first column of the grapheme there, so a wide character is selected whole.
/// When `col` is past the text, it is returned unchanged.
fn endpoint_start_col(text: &str, col: u16) -> u16 {
    crate::ui::scrollback::types::grapheme_cells_at(text, col).map_or(col, |cells| cells.start)
}

fn selected_cols_for_line_by_block_idx(
    drag: &ActiveTextDrag,
    line: &ResolvedSelectableLine,
) -> Option<Range<u16>> {
    selected_cols_for_endpoints(
        drag.anchor.block_line_idx,
        drag.anchor.col_within_range,
        drag.head.block_line_idx,
        drag.head.col_within_range,
        line,
    )
}

/// Reconstruct the full selected text from the block's complete output lines.
/// Unlike [`reconstruct_selection_text`], which only sees lines currently visible on screen, this reads the block's full output.
/// Copy produces the complete selection even when the anchor or head has scrolled off-screen.
pub fn reconstruct_full_selection_text(
    block_lines: &[crate::ui::scrollback::types::BlockLine],
    drag: &ActiveTextDrag,
) -> Option<String> {
    reconstruct_full_selection_text_with_boundaries(
        block_lines,
        &crate::ui::scrollback::types::SelectionBoundaries::default(),
        drag,
    )
}

pub(crate) fn reconstruct_full_selection_text_with_boundaries(
    block_lines: &[crate::ui::scrollback::types::BlockLine],
    boundaries: &crate::ui::scrollback::types::SelectionBoundaries,
    drag: &ActiveTextDrag,
) -> Option<String> {
    use crate::ui::scrollback::types::{
        Selectable, painted_selectable_region, selectable_cols, slice_display_cols,
    };

    let start_bl = min(drag.anchor.block_line_idx, drag.head.block_line_idx);
    let end_bl = max(drag.anchor.block_line_idx, drag.head.block_line_idx);
    let anchor_is_start = drag.anchor.block_line_idx <= drag.head.block_line_idx;

    let mut out = String::new();
    let mut first = true;

    for (idx, line) in block_lines.iter().enumerate() {
        if idx < start_bl || idx > end_bl {
            continue;
        }
        if line.selection_range != Some(drag.anchor.range_id) {
            continue;
        }

        let Some(cols) = selectable_cols(&line.content, &line.selectable) else {
            continue;
        };
        let boundary = boundaries.get(idx).map(Arc::as_ref);
        let width = boundary
            .map_or(cols.clone(), |boundary| {
                boundary.anchored_cols(cols.clone())
            })
            .end
            .saturating_sub(cols.start);

        // Map against the exact text painted in the selectable columns, in logical order
        // This is the string `set_line_safe_bidi` reorders, so the visual drag columns line up 1:1 with the drawn cells
        // The resolved-model overlay path shares this via `painted_region`
        let region = painted_selectable_region(line);

        // Tool-header rows paint a truncated/reworded display but copy a stored string
        // When that override diverges from what is painted (ignoring render-only trailing padding), its columns can't be reorder-mapped onto it
        // This is the same trim-tolerant predicate the overlay path uses
        let override_text = line
            .selection_text
            .as_deref()
            .filter(|ov| ov.trim_end() != region.trim_end());

        // Snap endpoints on the painted region (matches drawn cells).
        // `col_range.end == width` decides suffix re-attachment below.
        let display = display_text_for_cols(&region);
        let col_range = if start_bl == end_bl {
            let s = min(drag.anchor.col_within_range, drag.head.col_within_range);
            let e = max(drag.anchor.col_within_range, drag.head.col_within_range);
            endpoint_start_col(display.as_ref(), s).min(width)
                ..endpoint_end_col(display.as_ref(), e).min(width)
        } else if idx == start_bl {
            let s = if anchor_is_start {
                drag.anchor.col_within_range
            } else {
                drag.head.col_within_range
            };
            endpoint_start_col(display.as_ref(), s).min(width)..width
        } else if idx == end_bl {
            let e = if anchor_is_start {
                drag.head.col_within_range
            } else {
                drag.anchor.col_within_range
            };
            0..endpoint_end_col(display.as_ref(), e).min(width)
        } else {
            0..width
        };

        if !first {
            out.push_str(line.joiner.as_deref().unwrap_or("\n"));
        }
        first = false;
        let selected = match override_text {
            // Painted display is reordered and differs from the stored copy text, so visual columns don't index it: copy it whole on overlap
            Some(ov) if crate::engine::render::bidi::is_enabled() => ov.to_string(),
            // LTR: column-precise partial copy of the stored string.
            Some(ov) => slice_display_cols(ov, col_range.start, col_range.end),
            None => {
                let sliced = slice_text_cols(&region, col_range.clone());
                // Strip render-only trailing padding once the selection reaches the row end (matches `derive_selection_text` for `All` rows)
                if matches!(line.selectable, Selectable::All) && col_range.end == width {
                    sliced.trim_end().to_string()
                } else {
                    sliced
                }
            }
        };
        out.push_str(&apply_selection_boundary(
            selected,
            boundary,
            col_range.start == 0,
            col_range.end == width,
        ));
    }

    if first {
        return None;
    }
    Some(out)
}

/// Visual display order for column snap when rtl_bidi paints visual cells.
fn display_text_for_cols(text: &str) -> std::borrow::Cow<'_, str> {
    crate::engine::render::bidi::visual_text(text)
}

/// Slice by visual columns; returns logical-order text for the clipboard.
fn slice_text_cols(text: &str, cols: Range<u16>) -> String {
    if crate::engine::render::bidi::is_enabled() && crate::engine::render::bidi::needs_bidi(text) {
        return crate::engine::render::bidi::logical_slice_for_visual_cols(
            text,
            cols.start as usize,
            cols.end as usize,
        );
    }
    crate::ui::scrollback::types::slice_display_cols(text, cols.start, cols.end)
}

fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x
        && col < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

// Word / URL boundary detection (for double-click selection)

/// All printable ASCII punctuation except underscore, matching tmux's `word-separators` default from `options-table.c`.
pub const DEFAULT_WORD_SEPARATORS: &str = "!\"#$%&'()*+,-./:;<=>?@[\\]^`{|}~";

/// Three-class partition for word boundary detection (whitespace / separator / word).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Word,
    Whitespace,
    Separator,
}

fn classify_grapheme(grapheme: &str, separators: &str) -> CharClass {
    match grapheme.chars().next() {
        Some(c) if c.is_whitespace() => CharClass::Whitespace,
        Some(c) if separators.contains(c) => CharClass::Separator,
        _ => CharClass::Word,
    }
}

/// Find word boundaries around a display column using tmux-style three-class grouping.
/// Returns the range in display columns.
pub fn word_boundaries_at_col(text: &str, col: u16, separators: &str) -> Range<u16> {
    let mut segments: Vec<(u16, u16, CharClass)> = Vec::new();
    let mut current_col: u16 = 0;

    for grapheme in text.graphemes(true) {
        let width = UnicodeWidthStr::width(grapheme) as u16;
        if width == 0 {
            continue;
        }
        let next_col = current_col.saturating_add(width);
        segments.push((
            current_col,
            next_col,
            classify_grapheme(grapheme, separators),
        ));
        current_col = next_col;
    }

    if segments.is_empty() {
        return 0..0;
    }

    // If col is past the end, clamp to the last segment.
    let target_idx = segments
        .iter()
        .position(|(start, end, _)| col >= *start && col < *end)
        .unwrap_or(segments.len() - 1);

    let Some(&(_, _, target_class)) = segments.get(target_idx) else {
        return 0..0;
    };

    let mut left = target_idx;
    while left > 0
        && left
            .checked_sub(1)
            .and_then(|j| segments.get(j))
            .is_some_and(|seg| seg.2 == target_class)
    {
        left -= 1;
    }

    let mut right = target_idx;
    while right + 1 < segments.len()
        && segments
            .get(right + 1)
            .is_some_and(|seg| seg.2 == target_class)
    {
        right += 1;
    }

    match (segments.get(left), segments.get(right)) {
        (Some(&(start, _, _)), Some(&(_, end, _))) => start..end,
        _ => 0..0,
    }
}

/// Pre-compiled regex for URL detection, cached for the process lifetime.
static URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:https?|ftp|file)://[^\s\x{00}-\x{1f}]+").expect("URL regex must compile")
});

/// Punctuation that may appear as trailing prose punctuation after a URL.
const TRAILING_URL_PUNCT: &[char] = &['.', ',', ':', ';', '!', '?', ')', ']', '}', '>', '"', '\''];

/// Strip trailing punctuation from a URL match, respecting balanced brackets.
///
/// Closing brackets are only stripped when unbalanced (no matching opener inside the URL body), handling prose like `(see https://example.com)`.
fn strip_trailing_url_punctuation(url: &str) -> &str {
    let mut end = url.len();

    while let Some(prefix) = url.get(..end) {
        let last = match prefix.chars().next_back() {
            Some(c) if TRAILING_URL_PUNCT.contains(&c) => c,
            _ => break,
        };

        if let Some(open) = match last {
            ')' => Some('('),
            ']' => Some('['),
            '}' => Some('{'),
            '>' => Some('<'),
            _ => None,
        } {
            let opens = prefix.chars().filter(|&c| c == open).count();
            let closes = prefix.chars().filter(|&c| c == last).count();
            if opens >= closes {
                break;
            }
        }

        end -= last.len_utf8();
    }

    url.get(..end).unwrap_or("")
}

/// Compute the display-column width of a string via grapheme clusters.
fn display_width(text: &str) -> u16 {
    text.graphemes(true).fold(0u16, |acc, g| {
        acc.saturating_add(UnicodeWidthStr::width(g) as u16)
    })
}

/// Try to find a URL that spans the given display column in `text`. Scans `text` for URLs matching common schemes
/// (`https?://`, `ftp://`, `file://`). Trailing punctuation (`.`, `,`, `)`, etc.) is stripped when unbalanced,
/// handling prose contexts like `"see https://example.com."`.
pub fn url_range_at_col(text: &str, col: u16) -> Option<Range<u16>> {
    for m in URL_RE.find_iter(text) {
        let Some(prefix) = text.get(..m.start()) else {
            continue;
        };
        let col_start = display_width(prefix);
        let url = strip_trailing_url_punctuation(m.as_str());

        // Skip degenerate URLs reduced to just the scheme (e.g. "https://").
        if url
            .find("://")
            .and_then(|i| url.get(i + 3..))
            .is_some_and(str::is_empty)
        {
            continue;
        }

        let col_end = col_start.saturating_add(display_width(url));

        if col >= col_start && col < col_end {
            return Some(col_start..col_end);
        }
    }

    None
}

/// Wrap-aware word or URL selection.
/// `head` is inclusive; `text` is the joined fragment text and is never empty.
/// `None` means nothing selectable.
#[derive(Debug, PartialEq, Eq)]
pub struct SemanticSelection {
    pub anchor: SelectionEndpoint,
    pub head: SelectionEndpoint,
    pub text: String,
}

struct ConcatFragment {
    block_line_idx: usize,
    text_start: u16,
    text_end: u16,
}

/// Snap when an inclusive concat column lands inside a joiner.
/// `Forward` snaps to the next fragment's col 0, `Backward` to the previous fragment's last col.
#[derive(Clone, Copy)]
enum JoinerColSnap {
    Forward,
    Backward,
}

/// Select the wrap-group word or URL at `hit`. `head` is inclusive; `text` is the joined fragment text and is never
/// empty.
#[must_use]
pub fn semantic_selection_at(
    model: &ResolvedSelectionModel,
    hit: &RangeHit,
    separators: &str,
) -> Option<SemanticSelection> {
    let range = model.range(hit.entry_idx, hit.range_id)?;
    let lines = &range.lines;
    let hit_pos = lines
        .iter()
        .position(|line| line.block_line_idx == hit.block_line_idx)?;

    let mut lo = hit_pos;
    while lo > 0
        && lines
            .get(lo)
            .is_some_and(|line| line.joiner_to_previous.is_some())
    {
        lo -= 1;
    }
    let mut hi = hit_pos;
    while hi + 1 < lines.len()
        && lines
            .get(hi + 1)
            .is_some_and(|line| line.joiner_to_previous.is_some())
    {
        hi += 1;
    }

    // Single-row case (and, under reordering, the wrap-group case too; see below)
    // The hit column is a visual cell of the painted row and `word_or_url_slice` maps it against that same row
    if lo == hi
        || (crate::engine::render::bidi::is_enabled() && wrap_group_needs_bidi(lines, lo, hi))
    {
        let line_idx = if lo == hi { lo } else { hit_pos };
        let line = lines.get(line_idx)?;
        // The hit column is a visual cell of the painted row, so resolve the word against the painted region (identity/`text` when reordering off)
        let src = if crate::engine::render::bidi::is_enabled() {
            line.painted_region.as_deref().unwrap_or(line.text.as_str())
        } else {
            line.text.as_str()
        };
        let (sel, text) = word_or_url_slice(src, hit.col_within_range, separators)?;
        return Some(SemanticSelection {
            anchor: SelectionEndpoint {
                block_line_idx: line.block_line_idx,
                col_within_range: sel.start,
            },
            head: SelectionEndpoint {
                block_line_idx: line.block_line_idx,
                col_within_range: sel.end.saturating_sub(1),
            },
            text,
        });
    }

    let mut concat = String::new();
    let mut text_byte_ranges = Vec::with_capacity(hi - lo + 1);
    let group = lines.get(lo..=hi)?;
    for (offset, line) in group.iter().enumerate() {
        if offset > 0
            && let Some(joiner) = line.joiner_to_previous.as_deref()
        {
            concat.push_str(joiner);
        }
        let text_byte_start = concat.len();
        concat.push_str(&line.text);
        text_byte_ranges.push((line.block_line_idx, text_byte_start, concat.len()));
    }

    // Measure fragment columns on the finished concat so a grapheme split across a wrap boundary is not counted twice
    let mut fragments = Vec::with_capacity(text_byte_ranges.len());
    let mut hit_concat_col = None;
    for (block_line_idx, text_byte_start, text_byte_end) in text_byte_ranges {
        let Some(start_prefix) = concat.get(..text_byte_start) else {
            continue;
        };
        let Some(end_prefix) = concat.get(..text_byte_end) else {
            continue;
        };
        let text_start = display_width(start_prefix);
        let text_end = display_width(end_prefix);
        fragments.push(ConcatFragment {
            block_line_idx,
            text_start,
            text_end,
        });
        if block_line_idx == hit.block_line_idx {
            let Some(local_text) = concat.get(text_byte_start..text_byte_end) else {
                continue;
            };
            hit_concat_col = Some(map_local_hit_to_concat_col(
                local_text,
                text_start,
                text_end,
                hit.col_within_range,
            ));
        }
    }

    let (sel, text) = word_or_url_slice(&concat, hit_concat_col?, separators)?;
    let last_col = sel.end.saturating_sub(1);
    let anchor = map_inclusive_concat_col(&fragments, sel.start, JoinerColSnap::Forward)?;
    let head = map_inclusive_concat_col(&fragments, last_col, JoinerColSnap::Backward)?;
    if (anchor.block_line_idx, anchor.col_within_range)
        > (head.block_line_idx, head.col_within_range)
    {
        return None;
    }

    Some(SemanticSelection { anchor, head, text })
}

/// Map a fragment-local click column into concat display columns.
/// When a wrap splits a grapheme, the continuation still has local width but adds little or none in concat.
/// Those absorbed columns snap to the last concat column of the split cluster instead of drifting into later text.
fn map_local_hit_to_concat_col(
    local_text: &str,
    text_start: u16,
    text_end: u16,
    col_within_range: u16,
) -> u16 {
    let local_width = display_width(local_text);
    let concat_width = text_end.saturating_sub(text_start);
    let absorbed = local_width.saturating_sub(concat_width);
    if col_within_range < absorbed {
        text_start.saturating_sub(1)
    } else {
        text_start.saturating_add(col_within_range - absorbed)
    }
}

/// Whether any row in the wrap group `[lo, hi]` needs bidi reordering.
/// Rows are painted per row, so a whole-group concat would reorder differently than the screen.
/// When reordering applies we resolve the word on the hit row alone.
fn wrap_group_needs_bidi(lines: &[ResolvedSelectableLine], lo: usize, hi: usize) -> bool {
    lines
        .get(lo..=hi)
        .into_iter()
        .flatten()
        .any(|l| crate::engine::render::bidi::needs_bidi(&l.text))
}

fn word_or_url_slice(text: &str, col: u16, separators: &str) -> Option<(Range<u16>, String)> {
    // Word boundaries follow visual cells when RTL is present; clipboard is logical.
    let display = display_text_for_cols(text);
    let range = url_range_at_col(display.as_ref(), col)
        .unwrap_or_else(|| word_boundaries_at_col(display.as_ref(), col, separators));
    if range.is_empty() {
        return None;
    }
    let sliced = if crate::engine::render::bidi::needs_bidi(text) {
        slice_text_cols(text, range.clone())
    } else {
        crate::ui::scrollback::types::slice_display_cols(text, range.start, range.end)
    };
    if sliced.is_empty() {
        return None;
    }
    Some((range, sliced))
}

fn fragment_last_col(frag: &ConcatFragment) -> Option<SelectionEndpoint> {
    let width = frag.text_end.saturating_sub(frag.text_start);
    (width > 0).then_some(SelectionEndpoint {
        block_line_idx: frag.block_line_idx,
        col_within_range: width.saturating_sub(1),
    })
}

fn map_inclusive_concat_col(
    fragments: &[ConcatFragment],
    col: u16,
    snap: JoinerColSnap,
) -> Option<SelectionEndpoint> {
    for (i, frag) in fragments.iter().enumerate() {
        if col < frag.text_start {
            return match snap {
                JoinerColSnap::Forward => Some(SelectionEndpoint {
                    block_line_idx: frag.block_line_idx,
                    col_within_range: 0,
                }),
                JoinerColSnap::Backward => fragment_last_col(fragments.get(i.checked_sub(1)?)?),
            };
        }
        if col < frag.text_end {
            return Some(SelectionEndpoint {
                block_line_idx: frag.block_line_idx,
                col_within_range: col.saturating_sub(frag.text_start),
            });
        }
    }

    match snap {
        JoinerColSnap::Forward => None,
        JoinerColSnap::Backward => fragment_last_col(fragments.last()?),
    }
}
