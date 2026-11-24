//! ScrollbackState: unified state for the v3 scrollback pane.
//!
//! This combines entries, scroll position, selection, and turn-based navigation into a single state object.

pub mod groups;
mod layout;
mod nav;
mod pin_reserve;
mod selection;
mod timeline;
mod types;
pub mod verb_group;

pub use layout::ScrollAnchor;
pub use layout::compute_paint_window;
pub use timeline::TimelineEntry;
pub use types::*;

use layout::{LayoutCache, StructuralScrollAnchor};

use std::collections::{HashSet, VecDeque};
use std::ops::Range;
use std::time::Instant;

use indexmap::IndexMap;
use ratatui::layout::Rect;

use super::block::{BlockContent, RenderBlock};
use super::blocks::tool::{EditToolCallBlock, ToolCallBlock};
use super::entry::{EntryId, ScrollbackEntry};
use super::layout::HorizontalLayout;
use super::selection::SelectionBox;
use super::sticky::{PromptDescriptor, StickyHeaderLayout, compute_sticky_layout};
use super::types::DisplayMode;
use super::wrappers::EntryRenderer;
use crate::engine::render::Renderable;
use crate::engine::theme::Theme;
use crate::ui::scrollback::appearance::AppearanceConfig;

/// Lifecycle of a scroll-up warm-up that a resize postponed until the width settles.
/// Settling is measured in frames, not `prepare_layout` calls.
/// One frame prepares layout several times, so a call-based rule would run the warm-up during the very resize that deferred it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum DeferredWarmAbove {
    #[default]
    Idle,
    Deferred,
    Armed,
}

/// Unified scrollback state for the v3 pager.
#[derive(Debug)]
pub struct ScrollbackState {
    pub(super) interaction: super::interaction::InteractionState,
    pub(super) search: Option<super::ScrollbackSearchState>,
    // Content
    /// All entries in the scrollback, keyed by EntryId for O(1) lookup.
    /// IndexMap preserves insertion order for rendering.
    entries: IndexMap<EntryId, ScrollbackEntry>,

    /// Next entry ID to assign.
    next_id: u64,

    /// Set of currently running entry IDs.
    /// Used for O(running) iteration in tick_running().
    running: HashSet<EntryId>,

    /// Entry IDs whose finish-flash (accent stays bright for [`FINISH_FLASH_DURATION_MS`] after completion) may still be active.
    /// Lets `tick()` check O(flashing) recently-finished entries instead of scanning every entry's `finished_at` on every animation tick.
    /// Pushed by `finish_running_with_time`, drained by `tick()` on expiry.
    flashing: Vec<EntryId>,

    /// Set of entry IDs with potentially stale cached heights.
    /// Used for incremental layout updates: only these entries need height recomputation.
    dirty_heights: HashSet<EntryId>,

    /// Minimal mode only: entry IDs already emitted into the terminal's native scrollback. Keyed by `EntryId` (not a
    /// per-entry flag) so it survives `shift_remove` / `remove_from` reordering for free. A positional index would be
    /// stranded by a below-cursor removal. Empty in the alt-screen / inline modes, which never commit.
    committed: HashSet<EntryId>,

    /// Edits this session opened for a permission prompt. Only these refold when the prompt ends.
    permission_opened: HashSet<EntryId>,

    /// Minimal mode only: lowest entry index that *might* be uncommitted (not yet printed into native scrollback). A
    /// lower-bound perf hint so the per-frame commit pass is O(new) rather than O(history). Contract for every mutation
    /// that shifts entry positions: the cursor must never end up *above* an uncommitted entry's index.
    commit_scan_cursor: usize,

    /// Minimal mode only: a bounded ring of entry IDs committed to native scrollback while folded (collapsed reasoning,
    /// truncated tool output). (Committed terminal text can't be mutated, so expansion is a re-print.). Bounded so a
    /// long session never grows it without limit.
    commit_expand_ring: VecDeque<EntryId>,
    // Scroll. `usize` (not `u16`): a long session can render well past 65 535 rows, so the cumulative scroll position
    // must match `virtual_y` (`Vec<usize>`).
    scroll_offset: usize,

    /// Total content height (cached, updated on render).
    ///
    /// `usize` for the same reason as `scroll_offset`: the summed height of a long session can exceed `u16::MAX`.
    total_height: usize,

    /// Viewport height (set on render).
    /// Stays `u16`; a terminal is never 65 535 rows tall.
    viewport_height: u16,

    /// Whether auto-scroll is enabled (follow new content).
    follow_mode: bool,

    /// When true, handle_follow_mode skips the scroll-to-bottom on the first call, preserving a scroll position set by scroll_to_entry_top.
    /// Cleared after one use.
    /// This lets dispatch_send_prompt position the prompt at the viewport top while still enabling follow for new content.
    follow_preserve_scroll: bool,

    /// Content generation captured when follow-preserve is armed, so synthetic turns distinguish appended output from geometry-only rebuilds.
    follow_preserve_content_generation: u64,

    /// Extra rows under the latest user prompt so a page-flip can keep that prompt at the top.
    /// Independent of follow mode and viewport position so scrolling through history does not
    /// make the page-flip position unreachable. Dropped on an explicit bottom gesture or reset.
    pin_reserve_active: bool,

    /// Rows currently added to `total_height` by [`Self::pin_reserve_active`].
    /// Kept beside the flag so follow-preserve can still detect real overflow against the unpadded content height.
    pin_reserve_pad: usize,

    /// Scroll offset of the page-flip pin, captured when the reserve is armed.
    /// Re-deriving it from the last user prompt after a finish-time rebuild can disagree with the pose we scrolled to and drop the pad.
    /// (Finish-time rebuilds: thinking collapse, the "Worked for…" marker, a group fold.)
    pin_reserve_target: Option<usize>,

    /// Stable id of the prompt the pin targets, captured when armed.
    /// The pin tracks this specific prompt, not "the last user prompt".
    /// A mid-turn interjection therefore cannot move the above/below boundary the pad shift uses.
    pin_reserve_prompt_id: Option<EntryId>,

    /// The turn that armed the pin has finished.
    /// Midstream overflow still chases the tail; after this, a remeasure or terminal marker must not.
    pin_reserve_after_turn: bool,

    // Selection
    /// Currently selected entry index.
    selected: Option<usize>,

    /// Selection box to be rendered by the frame (computed during render).
    /// This is stored here so the frame can render it after the scrollback pane.
    selection_box: Option<SelectionBox>,

    // Turns
    /// Detected turns in the conversation.
    turns: Vec<Turn>,

    /// Index of the currently viewed turn.
    current_turn: Option<usize>,

    /// View mode: all turns or single turn.
    view_mode: ViewMode,

    // Cache. Width change invalidates the entire layout cache and triggers a full recompute of entry heights. Resize
    // events are debounced at the event-loop level so only the final width triggers a rebuild.
    last_width: u16,

    /// Layout cache for navigation (entry heights, prompt descriptors).
    layout_cache: Option<LayoutCache>,

    /// One-shot viewport-top anchor armed by a structural entry mutation (removal/insertion) just before it invalidates the layout cache.
    /// Consumed by the next `prepare_layout`; see [`StructuralScrollAnchor`].
    structural_scroll_anchor: Option<StructuralScrollAnchor>,

    // Sticky modes. Display mode applied to thinking blocks when they finish running. Defaults to `Collapsed`
    // (auto-collapse on finish). Toggled by `expand_all_thinking()` (Ctrl+E) between `Expanded` and `Collapsed`.
    thinking_display_mode: DisplayMode,

    // Animation
    /// Frame tick counter for animations (increments each render tick).
    tick: u64,

    // Appearance
    /// Current appearance configuration (hot-reloadable).
    appearance: AppearanceConfig,

    // Batching
    /// When > 0, `push()` skips `rebuild_turns()` and `invalidate_layout_cache()`.
    /// Call `begin_batch()` before bulk insertions and `end_batch()` after.
    batch_depth: u32,

    /// True when gap_after values may need recomputation (display_mode changed, entries added/removed).
    /// Streaming content mutations (`push_chunk_to_*`) leave this false, enabling an O(1) incremental virtual_y patch instead of an O(n) full rebuild.
    gaps_may_be_dirty: bool,

    /// A synchronous settings rebuild populated the cache, but the next frame must still run the full reserve lifecycle.
    full_settlement_pending: bool,

    warm_above: DeferredWarmAbove,

    /// Last observed [`ffmpeg_available`](crate::ui::scrollback::media::ffmpeg_available).
    /// A false-to-true flip (user installs ffmpeg mid-session) must rebuild the layout so reserved heights match the now-full-size posters.
    /// Otherwise a poster paints over the text below its (still banner-sized) reservation.
    ffmpeg_available_snapshot: bool,

    /// Set of group-start EntryIds whose group has been manually expanded by the user (pressing l/Enter on the group header).
    /// When a group's first entry ID is in this set, the fold pass (`groups::apply`) marks its span expanded instead of hiding entries.
    expanded_groups: HashSet<EntryId>,

    // Link map
    /// Monotonically increasing counter, bumped when visible link positions or policy inputs change.
    /// Used by `VisibleLinkMap::is_stale()` to skip rebuilds.
    generation: u64,

    /// Bumped only when entries are added or removed or an entry's content changes.
    /// Never bumped on display toggles (fold/raw/group), appearance, scroll, or viewport.
    /// A stable invalidation key for content-derived caches that must survive view changes, unlike `generation`.
    content_generation: u64,

    /// Session/worktree cwd (`AgentSession.cwd`) for Expanded tool paths.
    cwd: Option<std::path::PathBuf>,
}

impl Default for ScrollbackState {
    fn default() -> Self {
        Self::new()
    }
}

impl ScrollbackState {
    /// Create a new empty state.
    pub fn new() -> Self {
        Self {
            interaction: super::interaction::InteractionState::default(),
            search: None,
            entries: IndexMap::new(),
            next_id: 1, // Start at 1 so 0 can be a sentinel
            running: HashSet::new(),
            flashing: Vec::new(),
            dirty_heights: HashSet::new(),
            committed: HashSet::new(),
            permission_opened: HashSet::new(),
            commit_scan_cursor: 0,
            commit_expand_ring: VecDeque::new(),
            scroll_offset: 0,
            total_height: 0,
            viewport_height: 0,
            follow_mode: true,
            follow_preserve_scroll: false,
            follow_preserve_content_generation: 0,
            pin_reserve_active: false,
            pin_reserve_pad: 0,
            pin_reserve_target: None,
            pin_reserve_prompt_id: None,
            pin_reserve_after_turn: false,
            selected: None,
            selection_box: None,
            turns: Vec::new(),
            current_turn: None,
            view_mode: ViewMode::AllTurns,
            last_width: 0,
            layout_cache: None,
            structural_scroll_anchor: None,
            thinking_display_mode: DisplayMode::Collapsed,
            tick: 0,
            appearance: AppearanceConfig::default(),
            batch_depth: 0,
            gaps_may_be_dirty: false,
            full_settlement_pending: false,
            warm_above: DeferredWarmAbove::Idle,
            ffmpeg_available_snapshot: false,
            expanded_groups: HashSet::new(),
            generation: 0,
            content_generation: 0,
            cwd: None,
        }
    }

    pub fn cwd(&self) -> Option<&std::path::Path> {
        self.cwd.as_deref()
    }

    /// Update session cwd; invalidates cwd-dependent paint, layout, and link maps.
    pub fn set_cwd(&mut self, cwd: Option<std::path::PathBuf>) {
        if self.cwd == cwd {
            return;
        }
        self.cwd = cwd;
        for entry in self.entries.values_mut() {
            entry.invalidate_cache();
        }
        self.dirty_heights = self.entries.keys().copied().collect();
        self.layout_cache = None;
        self.gaps_may_be_dirty = true;
        self.bump_generation();
    }

    /// Create empty continuation state with shared appearance, preferences, and `EntryId` space.
    /// Reconnect replay uses it while pre-outage content is stashed; cross-swap ids may dangle but cannot alias.
    /// Generations advance so equality-cached consumers observe the content swap.
    pub fn fresh_continuation(&self) -> Self {
        let mut fresh = Self::new();
        fresh.next_id = self.next_id;
        fresh.appearance = self.appearance.clone();
        fresh.thinking_display_mode = self.thinking_display_mode;
        fresh.view_mode = self.view_mode;
        fresh.follow_mode = self.follow_mode;
        fresh.cwd = self.cwd.clone();
        fresh.generation = self.generation.wrapping_add(1);
        fresh.content_generation = self.content_generation.wrapping_add(1);
        fresh
    }

    /// Lowest `EntryId` value a future [`push`](Self::push) may assign.
    pub fn id_floor(&self) -> u64 {
        self.next_id
    }

    /// Ensure future `EntryId`s are allocated at or above `floor`.
    /// Called when a stashed state is swapped back in after a [`fresh_continuation`](Self::fresh_continuation) sibling allocated ids.
    /// Ids handed out by the discarded sibling are then never reused.
    pub fn raise_id_floor(&mut self, floor: u64) {
        self.next_id = self.next_id.max(floor);
    }

    /// Advance the invalidation generations strictly past a discarded [`fresh_continuation`](Self::fresh_continuation) sibling's.
    /// Caches keyed on counter equality (link map, search index) that last saw the sibling then cannot mistake this state for it after a restore swap.
    pub fn raise_invalidation_floor(&mut self, sibling: (u64, u64)) {
        self.generation = self.generation.max(sibling.0);
        self.content_generation = self.content_generation.max(sibling.1);
        self.bump_content_generation();
    }

    /// The invalidation-generation pair, for [`Self::raise_invalidation_floor`].
    pub fn invalidation_generations(&self) -> (u64, u64) {
        (self.generation, self.content_generation)
    }

    /// Whether a [`begin_batch`](Self::begin_batch) is currently open.
    pub fn in_batch(&self) -> bool {
        self.batch_depth > 0
    }

    /// Append all entries from `tail` (a `fresh_continuation` sibling of this state) after the existing content. Used
    /// by the cursor-found reconnect reload: nothing was replayed, so the pre-outage transcript is kept. Only the
    /// post-cursor live tail that accumulated in the staging state is attached below it.
    pub fn append_entries_from(&mut self, tail: ScrollbackState) {
        debug_assert!(
            tail.next_id >= self.next_id,
            "append_entries_from requires a fresh_continuation sibling (shared id space)"
        );
        self.entries.extend(tail.entries);
        self.running.extend(tail.running);
        self.dirty_heights.extend(tail.dirty_heights);
        // Carry the tail's committed frontier: with a per-entry flag this traveled with the entry
        // As an id-set it must be merged explicitly so already-committed tail blocks are not re-emitted after the reload
        self.committed.extend(tail.committed);
        self.permission_opened.extend(tail.permission_opened);
        self.expanded_groups.extend(tail.expanded_groups);
        self.next_id = self.next_id.max(tail.next_id);
        // The tail (live during the window) is what equality-cached consumers last saw; the merged state must read as newer than both halves
        self.generation = self.generation.max(tail.generation);
        self.content_generation = self.content_generation.max(tail.content_generation);
        self.rebuild_turns();
        self.gaps_may_be_dirty = true;
        self.invalidate_layout_cache();
        self.bump_content_generation();
    }

    /// Update the appearance configuration.
    pub fn set_appearance(&mut self, appearance: AppearanceConfig) {
        crate::engine::render::bidi::set_enabled(appearance.scrollback.display.rtl_bidi);
        if self.appearance.collapsed_edit_blocks != appearance.collapsed_edit_blocks {
            self.apply_collapsed_edit_blocks_flip(
                self.appearance.collapsed_edit_blocks,
                appearance.collapsed_edit_blocks,
            );
        }
        super::appearance::set_tab_width(appearance.scrollback.display.tab_width);
        self.appearance = appearance;
        // Invalidate caches since appearance affects rendering
        self.layout_cache = None;
        self.gaps_may_be_dirty = true;
        // Mark all entries as having dirty heights
        self.dirty_heights = self.entries.keys().copied().collect();
        for entry in self.entries.values_mut() {
            entry.invalidate_cache();
        }
        self.bump_generation();
    }

    /// Remeasure every entry when a process-wide visibility flag flips (e.g. show thinking blocks) without changing `AppearanceConfig`.
    /// Refreshes the cache synchronously for settings callers and leaves full-dirty state for the next frame's reserve lifecycle settlement.
    pub fn invalidate_heights(&mut self) {
        for entry in self.entries.values_mut() {
            entry.invalidate_cache();
        }
        self.rebuild_layout();
        self.dirty_heights = self.entries.keys().copied().collect();
        self.gaps_may_be_dirty = true;
        self.full_settlement_pending = true;
        self.bump_generation();
    }

    /// Mark entry `id` structurally dirty: height re-measure plus gap/fold recompute on the next `prepare_layout`.
    /// For in-place block swaps that can change fold membership (e.g. a tool refinement changing its verb-group kind).
    /// A plain cache invalidation is not enough there.
    pub fn mark_structurally_dirty(&mut self, id: EntryId) {
        self.dirty_heights.insert(id);
        self.gaps_may_be_dirty = true;
    }

    /// Get current appearance config.
    pub fn appearance(&self) -> &AppearanceConfig {
        &self.appearance
    }

    /// Current animation tick value (for spinner frame selection, etc.).
    pub fn animation_tick(&self) -> u64 {
        self.tick
    }

    /// A redraw is requested only when an animated entry (running wave accent or unexpired finish-flash) is actually
    /// inside the viewport window. Redrawing an otherwise static screen at ~30fps is pure waste: the frame diff would
    /// be empty.
    pub fn tick(&mut self) -> bool {
        self.tick = self.tick.wrapping_add(1);

        let mut needs_redraw = !self.running.is_empty() && self.any_running_in_viewport();

        // Finish-flash: O(flashing) over recently-finished entries, not O(entries) over the whole scrollback. Emit one
        // final redraw when a flash expires so the accent repaints in its static state. Otherwise the last-painted bright
        // frame would linger until the next event.
        if !self.flashing.is_empty() {
            let flash_dur = FINISH_FLASH_DURATION_MS as u128;
            let mut still_flashing = std::mem::take(&mut self.flashing);
            still_flashing.retain(|id| {
                let Some(idx) = self.entries.get_index_of(id) else {
                    // The entry was removed (rewind/clear); nothing to repaint
                    return false;
                };
                let active = self
                    .entries
                    .get_index(idx)
                    .and_then(|(_, e)| e.finished_at)
                    .is_some_and(|t| t.elapsed().as_millis() < flash_dur);
                // Redraw while the flash animates and once when it expires, but only if the entry can be seen
                if self.entry_index_in_viewport(idx) {
                    needs_redraw = true;
                }
                active
            });
            self.flashing = still_flashing;
        }

        needs_redraw
    }

    /// Whether any entry is still marked running (visible or not).
    pub fn has_running_entries(&self) -> bool {
        !self.running.is_empty()
    }

    /// Whether any running entry is inside the current viewport window.
    /// Conservative: with no layout yet (before the first draw), every entry counts as visible.
    fn any_running_in_viewport(&self) -> bool {
        self.running.iter().any(|id| {
            self.entries
                .get_index_of(id)
                .is_some_and(|idx| self.entry_index_in_viewport(idx))
        })
    }

    /// Get the current tick counter (for animation synchronization).
    pub fn tick_count(&self) -> u64 {
        self.tick
    }

    /// Check if animation ticks are needed. Off-screen running entries don't need ticks. Finish-flashes deliberately do
    /// not demand ticks: they animate opportunistically while ticks flow for other reasons.
    pub fn needs_animation(&self) -> bool {
        !self.running.is_empty() && self.any_running_in_viewport()
    }

    /// Get the current animation tick value.
    pub fn current_tick(&self) -> u64 {
        self.tick
    }

    // Link map generation

    /// Current link-map generation.
    /// Incremented when positions or link-policy inputs change and invalidate the visible link map.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Bump the generation counter (marks the current `VisibleLinkMap` stale).
    fn bump_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    /// Generation that moves only when the entry set or an entry's content changes.
    /// It does not move on fold/raw/group display toggles, appearance, scroll, or viewport.
    /// Stable invalidation key for content-derived caches (e.g. a search index) that must not rebuild on view changes, unlike `generation`.
    pub fn content_generation(&self) -> u64 {
        self.content_generation
    }

    /// Bump `content_generation` (and `generation` with it, since content changes also move visible links).
    /// Display-only changes (fold/raw/group), appearance, scroll, and viewport call `bump_generation` directly so `content_generation` stays put.
    fn bump_content_generation(&mut self) {
        self.content_generation = self.content_generation.wrapping_add(1);
        self.bump_generation();
    }
    // Batching

    /// Begin a batch of insertions.
    /// While batching, `push()` skips `rebuild_turns()` and `invalidate_layout_cache()`.
    /// Call `end_batch()` when done to run them once.
    pub fn begin_batch(&mut self) {
        self.batch_depth += 1;
    }

    /// End a batch.
    /// Runs the deferred `rebuild_turns()` and `invalidate_layout_cache()` once for all insertions.
    pub fn end_batch(&mut self) {
        self.batch_depth = self.batch_depth.saturating_sub(1);
        if self.batch_depth == 0 {
            self.rebuild_turns();
            self.invalidate_layout_cache();
        }
    }

    // Content Management

    /// Add an entry, assigning it a unique ID.
    ///
    /// Returns the assigned EntryId which can be used to access this entry later.
    pub fn push(&mut self, entry: ScrollbackEntry) -> EntryId {
        let id = EntryId::new(self.next_id);
        self.next_id += 1;

        let mut entry = entry;
        entry.id = id;

        self.apply_edit_default_display_mode(&mut entry);

        // Track if this entry is running
        if entry.is_running {
            self.running.insert(id);
        }

        self.entries.insert(id, entry);
        if self.batch_depth == 0 {
            self.rebuild_turns();
            // Try to extend the cache incrementally. If extension isn't possible (no cache yet, or cache out of sync), fall
            // back to the full invalidation.
            let new_idx = self.entries.len() - 1;
            if !self.extend_layout_cache_with_new_entry(new_idx) {
                self.gaps_may_be_dirty = true;
                self.invalidate_layout_cache();
            } else if self.appearance.scrollback.display.group_max_visible > 0
                || self.appearance.group_tool_verbs
            {
                // A groupable, collapsed new entry may extend a group past the truncation threshold, or start or grow a foldable verb-group run
                // The verb fold is gated on `group_tool_verbs`, independent of the truncation threshold
                // Mark both dirty so prepare_layout Case 2 fires (Case 3 doesn't check gaps_may_be_dirty)
                if let Some((_, new_e)) = self.entries.get_index(new_idx)
                    && new_e.block.is_groupable()
                    && new_e.display_mode == DisplayMode::Collapsed
                {
                    self.gaps_may_be_dirty = true;
                    self.dirty_heights.insert(id);
                }
            }
            // Successful extend: gaps were updated inline, so gaps_may_be_dirty deliberately stays unset
            // Setting it would force the next streaming chunk's Case 2 path to do a full virtual_y rebuild
        } else {
            // In batch mode: defer to end_batch's full rebuild for safety.
            // (The cache is bulk-rebuilt once when the batch ends.)
            self.gaps_may_be_dirty = true;
            self.layout_cache = None;
        }
        self.bump_content_generation();
        id
    }

    /// Add a block (convenience wrapper).
    ///
    /// Returns the assigned EntryId.
    pub fn push_block(&mut self, block: RenderBlock) -> EntryId {
        self.push(ScrollbackEntry::new(block))
    }

    /// Add a finalized block positioned immediately before the entry `anchor`, instead of at the end. `anchor` must not
    /// already be committed. A terminal's native scrollback is append-only. Inserting above a block already printed
    /// there would emit the new block below content that logically follows it.
    pub fn insert_block_before(&mut self, anchor: EntryId, block: RenderBlock) -> EntryId {
        let Some(index) = self.entries.get_index_of(&anchor) else {
            return self.push_block(block);
        };
        debug_assert!(
            !self.committed.contains(&anchor),
            "insert_block_before: anchor {anchor:?} is already committed — the inserted \
             block would print out of order in native scrollback"
        );

        // Anchor the viewport top before the insertion shifts indices.
        self.arm_structural_scroll_anchor();
        let id = EntryId::new(self.next_id);
        self.next_id += 1;
        let mut entry = ScrollbackEntry::new(block);
        entry.id = id;
        self.apply_edit_default_display_mode(&mut entry);
        if entry.is_running {
            self.running.insert(id);
        }
        self.entries.shift_insert(index, id, entry);

        if let Some(selected) = self.selected.as_mut()
            && *selected >= index
        {
            *selected += 1;
        }
        self.commit_scan_cursor = self.commit_scan_cursor.min(index);

        if self.batch_depth == 0 {
            self.rebuild_turns();
        }
        self.gaps_may_be_dirty = true;
        self.invalidate_layout_cache();
        self.bump_content_generation();
        id
    }

    /// Fresh Edit entries at the block's Collapsed default adopt the state-owned materialize policy.
    /// An explicit non-Collapsed mode survives.
    /// An explicit Collapsed is indistinguishable from the default and may be upgraded by the effective expanded default.
    fn apply_edit_default_display_mode(&self, entry: &mut ScrollbackEntry) {
        if let RenderBlock::ToolCall(ToolCallBlock::Edit(edit)) = &entry.block
            && entry.display_mode == DisplayMode::Collapsed
        {
            entry.display_mode = edit_default_display_mode(
                self.appearance
                    .scrollback
                    .blocks
                    .edit
                    .effective_expanded(self.appearance.collapsed_edit_blocks),
                edit,
            );
        }
    }

    /// Remove an entry by EntryId. No-op if the id is not present. Used by the cancel-with-restore flow to undo the
    /// user prompt block that was pushed at turn start. Returns `true` if an entry was removed.
    pub fn remove_entry(&mut self, id: EntryId) -> bool {
        // Capture the index before the removal shifts everything after it down.
        let Some(removed_index) = self.entries.get_index_of(&id) else {
            return false;
        };
        // Anchor the viewport top before the removal shifts indices, then re-point it at the survivor if this removal deleted its entry
        self.arm_structural_scroll_anchor();
        self.entries.shift_remove(&id);
        self.migrate_structural_anchor_past_removal(id, removed_index);
        self.running.remove(&id);
        self.dirty_heights.remove(&id);
        self.committed.remove(&id);
        self.permission_opened.remove(&id);
        self.expanded_groups.remove(&id);
        if let Some(sel) = self.selected
            && sel >= self.entries.len()
        {
            self.selected = self.entries.len().checked_sub(1);
        }
        // Clamping alone is not enough here; see the cursor's contract
        if removed_index < self.commit_scan_cursor {
            self.commit_scan_cursor -= 1;
        }
        self.commit_scan_cursor = self.commit_scan_cursor.min(self.entries.len());
        self.rebuild_turns();
        self.gaps_may_be_dirty = true;
        self.invalidate_layout_cache();
        self.bump_content_generation();
        true
    }

    pub fn remove_from(&mut self, index: usize) -> Vec<ScrollbackEntry> {
        // Anchor a viewport parked above the cut; pruned below if the anchored entry itself is in the removed tail
        self.arm_structural_scroll_anchor();
        let mut removed = Vec::new();
        while self.entries.len() > index {
            if let Some((id, entry)) = self.entries.pop() {
                self.running.remove(&id);
                self.dirty_heights.remove(&id);
                self.committed.remove(&id);
                self.permission_opened.remove(&id);
                self.expanded_groups.remove(&id);
                removed.push(entry);
            }
        }
        removed.reverse();
        self.prune_dead_structural_anchor();
        if let Some(sel) = self.selected
            && sel >= self.entries.len()
        {
            self.selected = self.entries.len().checked_sub(1);
        }
        // Clamp the minimal-mode commit cursor: `remove_from` (rewind / trailing auth-error strip) pops the tail
        // That could otherwise leave the cursor past the end and silently skip future commits
        self.commit_scan_cursor = self.commit_scan_cursor.min(self.entries.len());
        self.rebuild_turns();
        self.gaps_may_be_dirty = true;
        self.invalidate_layout_cache();
        self.bump_content_generation();
        removed
    }

    /// Preferred streaming append: writes the chunk, invalidates the render cache, and marks height dirty.
    /// Returns false if the entry is missing or is not an agent message.
    pub fn push_chunk_to_agent(&mut self, id: EntryId, chunk: &str) -> bool {
        if let Some(entry) = self.entries.get_mut(&id)
            && let RenderBlock::AgentMessage(ref mut msg) = entry.block
        {
            msg.push_chunk(chunk);
            entry.invalidate_cache();
            self.dirty_heights.insert(id);
            self.bump_content_generation();
            return true;
        }
        false
    }

    /// Push a chunk to an agent message entry without rendering markdown yet.
    pub fn push_chunk_to_agent_deferred(&mut self, id: EntryId, chunk: &str) -> bool {
        if let Some(entry) = self.entries.get_mut(&id)
            && let RenderBlock::AgentMessage(ref mut msg) = entry.block
        {
            msg.push_chunk_deferred(chunk);
            entry.invalidate_cache();
            self.dirty_heights.insert(id);
            self.bump_content_generation();
            return true;
        }
        false
    }

    /// Push streaming output to an execute block entry. Returns true if successful, false if the entry doesn't exist or
    /// isn't an execute block.
    pub fn set_execute_output(&mut self, id: EntryId, output: &str) -> bool {
        if let Some(entry) = self.entries.get_mut(&id)
            && let RenderBlock::ToolCall(ToolCallBlock::Execute(ref mut exec)) = entry.block
        {
            // The input is a snapshot of the full accumulated buffer.
            // The shell now sends clean output (no ANSI codes) when the client sets x.ai/bashOutputNoColor: true, so no stripping is needed
            exec.output = Some(output.to_string());
            entry.invalidate_cache();
            self.dirty_heights.insert(id);
            self.bump_content_generation();
            return true;
        }
        false
    }

    /// Push a text chunk to a thinking block entry.
    /// Similar to `push_chunk_to_agent()`, this handles all necessary cache invalidation for streaming thinking content.
    /// Returns true if successful, false if the entry doesn't exist or isn't a thinking block.
    pub fn push_chunk_to_thinking(&mut self, id: EntryId, chunk: &str) -> bool {
        if let Some(entry) = self.entries.get_mut(&id)
            && let RenderBlock::Thinking(ref mut block) = entry.block
        {
            block.push_chunk(chunk);
            entry.invalidate_cache();
            self.dirty_heights.insert(id);
            self.bump_content_generation();
            return true;
        }
        false
    }

    /// Push a chunk to a thinking entry without rendering markdown yet.
    pub fn push_chunk_to_thinking_deferred(&mut self, id: EntryId, chunk: &str) -> bool {
        if let Some(entry) = self.entries.get_mut(&id)
            && let RenderBlock::Thinking(ref mut block) = entry.block
        {
            block.push_chunk_deferred(chunk);
            entry.invalidate_cache();
            self.dirty_heights.insert(id);
            self.bump_content_generation();
            return true;
        }
        false
    }

    /// Append incremental output delta to an execute tool call entry. Used when the shell sends incremental
    /// `output_delta` instead of full buffers. Delegates to `push_chunk_to_execute` which handles cache invalidation.
    /// Returns true if successful, false if the entry doesn't exist or isn't an execute block.
    pub fn append_execute_output(&mut self, id: EntryId, delta: &str) -> bool {
        self.push_chunk_to_execute(id, delta)
    }

    /// Push an output chunk to an execute tool call entry.
    /// Similar to `push_chunk_to_agent()`, this handles all necessary cache invalidation for streaming command output.
    /// Returns true if successful, false if the entry doesn't exist or isn't an execute block.
    pub fn push_chunk_to_execute(&mut self, id: EntryId, chunk: &str) -> bool {
        if let Some(entry) = self.entries.get_mut(&id)
            && let RenderBlock::ToolCall(ToolCallBlock::Execute(ref mut block)) = entry.block
        {
            block.push_output(chunk);
            entry.invalidate_cache();
            // Always mark dirty: word wrap may change line count even for same-line appends
            // HashSet dedup makes this cheap when called repeatedly.
            self.dirty_heights.insert(id);
            self.bump_content_generation();
            return true;
        }
        false
    }

    /// Mark an entry's height as dirty, requiring recomputation on next prepare_layout().
    ///
    /// Use this when you modify an entry's content directly (e.g., via get_by_id_mut()) and the change might affect its rendered height.
    pub fn mark_height_dirty(&mut self, id: EntryId) {
        self.dirty_heights.insert(id);
        self.gaps_may_be_dirty = true;
        self.bump_content_generation();
    }

    /// Get number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Clear all entries.
    pub fn clear(&mut self) {
        self.interaction = super::interaction::InteractionState::default();
        self.clear_search();
        self.entries.clear();
        self.running.clear();
        self.flashing.clear();
        self.dirty_heights.clear();
        self.committed.clear();
        self.permission_opened.clear();
        self.expanded_groups.clear();
        // Note: we don't reset next_id to avoid ID reuse
        self.selected = None;
        self.turns.clear();
        self.current_turn = None;
        self.scroll_offset = 0;
        self.pin_reserve_active = false;
        self.pin_reserve_pad = 0;
        self.pin_reserve_target = None;
        self.pin_reserve_prompt_id = None;
        self.pin_reserve_after_turn = false;
        self.commit_scan_cursor = 0;
        self.commit_expand_ring.clear();
        self.invalidate_layout_cache();
        self.bump_content_generation();
    }

    // These support `crate::minimal`'s commit pipeline (print finalized blocks into native scrollback)
    // The authoritative state is the `committed` id-set; `commit_scan_cursor` is only a lower-bound hint to keep the per-frame scan O(new)
    // Reached from the minimal crate via `minimal_api::{is_committed, mark_committed, commit_scan_cursor, …}`

    /// Lowest entry index that may still be uncommitted (minimal-mode hint).
    pub(crate) fn commit_scan_cursor(&self) -> usize {
        self.commit_scan_cursor
    }

    /// Advance the commit scan cursor, clamped to the current entry count.
    pub(crate) fn set_commit_scan_cursor(&mut self, cursor: usize) {
        self.commit_scan_cursor = cursor.min(self.entries.len());
    }

    /// Whether the entry `id` was already emitted into native scrollback.
    pub(crate) fn is_committed(&self, id: EntryId) -> bool {
        self.committed.contains(&id)
    }

    /// Mark the entry at `index` as committed to native scrollback.
    /// No-op if the index is out of range.
    pub(crate) fn mark_committed(&mut self, index: usize) {
        if let Some((&id, _)) = self.entries.get_index(index) {
            self.committed.insert(id);
        }
    }

    /// Maximum number of folded-commit IDs retained for `Ctrl+E` / `/expand`.
    const EXPAND_RING_CAP: usize = 256;

    /// Record that the entry `id` was committed to native scrollback in a folded display mode (collapsed reasoning / truncated tool output).
    /// `Ctrl+E` / `/expand` can then re-print it in full.
    /// Bounded: the oldest entry is dropped once the ring is full.
    pub(crate) fn record_committed_for_expand(&mut self, id: EntryId) {
        self.commit_expand_ring.push_back(id);
        while self.commit_expand_ring.len() > Self::EXPAND_RING_CAP {
            self.commit_expand_ring.pop_front();
        }
    }

    /// Pop the most-recently committed folded entry whose entry still exists, for `Ctrl+E` / `/expand` to re-print fully.
    /// Returns `None` when nothing folded remains to expand.
    /// Stale IDs (entries removed by rewind / clear) are skipped.
    pub fn take_expandable_committed(&mut self) -> Option<EntryId> {
        while let Some(id) = self.commit_expand_ring.pop_back() {
            if self.entries.contains_key(&id) {
                return Some(id);
            }
        }
        None
    }

    /// Get entry by index.
    pub fn get(&self, index: usize) -> Option<&ScrollbackEntry> {
        self.entries.get_index(index).map(|(_, v)| v)
    }

    /// Get entry by index mutably.
    pub fn get_mut(&mut self, index: usize) -> Option<&mut ScrollbackEntry> {
        self.entries.get_index_mut(index).map(|(_, v)| v)
    }

    /// Get the last entry.
    pub fn last(&self) -> Option<&ScrollbackEntry> {
        self.entries.last().map(|(_, v)| v)
    }

    /// Get the last entry mutably.
    pub fn last_mut(&mut self) -> Option<&mut ScrollbackEntry> {
        self.entries.last_mut().map(|(_, v)| v)
    }

    /// Mark the last entry as running.
    /// When entering running state (`running = true`), also starts timing on tool call blocks via `ToolCallBlock::start_timing()`.
    /// This is the single point where block timing begins; constructors default to `started_at = None`.
    pub fn set_last_running(&mut self, running: bool) {
        if let Some(id) = self.entries.last().map(|(_, entry)| entry.id) {
            self.set_entry_running(id, running);
        }
    }

    pub fn set_entry_running(&mut self, id: EntryId, running: bool) {
        let Some(entry) = self.entries.get_mut(&id) else {
            return;
        };
        let was_running = entry.is_running;
        entry.is_running = running;
        entry.invalidate_cache();
        if running && !was_running {
            if let RenderBlock::ToolCall(ref mut tc) = entry.block {
                tc.start_timing();
            }
            self.running.insert(entry.id);
        } else if !running && was_running {
            self.running.remove(&entry.id);
        }
    }

    /// Get entry by ID.
    /// O(1) average via IndexMap.
    /// Returns None if the entry doesn't exist (was removed or ID is invalid).
    pub fn get_by_id(&self, id: EntryId) -> Option<&ScrollbackEntry> {
        self.entries.get(&id)
    }

    /// Get entry by ID mutably.
    /// O(1) average via IndexMap.
    /// Returns None if the entry doesn't exist (was removed or ID is invalid).
    pub fn get_by_id_mut(&mut self, id: EntryId) -> Option<&mut ScrollbackEntry> {
        self.entries.get_mut(&id)
    }

    /// The escalation fires only on that rising edge, so a user's collapse of an already-untrusted block sticks. A row
    /// already awaiting a permission then opens once if the new Edit has hunks, unless the fold is pinned. Stamps
    /// `started_at` on the new block and invalidates the entry's render cache.
    pub(crate) fn replace_tool_block(
        &mut self,
        entry_id: EntryId,
        mut block: RenderBlock,
        started_at: Option<Instant>,
    ) -> bool {
        let respect_manual_folds = self.appearance.scrollback.scroll.respect_manual_folds;
        let expanded_by_default = self
            .appearance
            .scrollback
            .blocks
            .edit
            .effective_expanded(self.appearance.collapsed_edit_blocks);
        let Some(entry) = self.entries.get_mut(&entry_id) else {
            return false;
        };
        if let RenderBlock::ToolCall(new_tc) = &mut block
            && let Some(t) = started_at
        {
            new_tc.set_started_at(t);
        }
        let kind_changed = verb_group::verb_group_kind_changed(&entry.block, &block);
        let (is_same_kind, untrusted_rising) = match (&entry.block, &block) {
            (
                RenderBlock::ToolCall(ToolCallBlock::Edit(old)),
                RenderBlock::ToolCall(ToolCallBlock::Edit(new)),
            ) => (
                true,
                new.is_success() && new.summary_untrusted && !old.summary_untrusted,
            ),
            (RenderBlock::ToolCall(old), RenderBlock::ToolCall(new)) => (
                std::mem::discriminant(old) == std::mem::discriminant(new),
                false,
            ),
            _ => (false, false),
        };
        entry.block = block;
        if is_same_kind {
            if untrusted_rising && !(respect_manual_folds && entry.display_mode_pinned) {
                entry.display_mode = DisplayMode::Expanded;
            }
        } else if !(respect_manual_folds && entry.display_mode_pinned) {
            entry.display_mode = match &entry.block {
                RenderBlock::ToolCall(ToolCallBlock::Edit(edit)) => {
                    edit_default_display_mode(expanded_by_default, edit)
                }
                block => block.default_display_mode(),
            };
        }
        let pending = entry.is_pending_user_input;
        entry.invalidate_cache();
        if kind_changed {
            self.mark_structurally_dirty(entry_id);
        }
        // The first pending mark often lands on the eager Other placeholder, before
        // hunks exist. Open once when that Edit arrives, unless the user pinned a fold.
        if pending {
            self.open_permission_edit(entry_id);
        }
        true
    }

    /// Entries still on their old policy default re-materialize under the new one.
    /// Flag off to on collapses rows open only because `expanded_by_default` was `Some(true)`.
    /// `Some(false)` is collapsed on both sides, so that walk no-ops.
    pub fn apply_collapsed_edit_blocks_flip(&mut self, old_flag: bool, new_flag: bool) {
        let edit_cfg = &self.appearance.scrollback.blocks.edit;
        let old_expanded = edit_cfg.effective_expanded(old_flag);
        let new_expanded = edit_cfg.effective_expanded(new_flag);
        let respect_manual_folds = self.appearance.scrollback.scroll.respect_manual_folds;
        if old_expanded != new_expanded {
            for entry in self.entries.values_mut() {
                let RenderBlock::ToolCall(ToolCallBlock::Edit(edit)) = &entry.block else {
                    continue;
                };
                if respect_manual_folds && entry.display_mode_pinned {
                    continue;
                }
                if entry.display_mode == edit_default_display_mode(old_expanded, edit) {
                    entry.display_mode = edit_default_display_mode(new_expanded, edit);
                }
            }
        }
        self.clear_group_expansion();
        self.invalidate_heights();
    }

    /// Get the index of an entry by its ID.
    /// O(1) average via IndexMap.
    pub fn index_of_id(&self, id: EntryId) -> Option<usize> {
        self.entries.get_index_of(&id)
    }

    /// Capture a width-stable bookmark of the viewport-top content, to re-pin it after a resize/re-wrap (the `/jump` capture-and-restore).
    /// `None` when there's no layout to anchor to.
    pub fn capture_scroll_bookmark(&self) -> Option<ScrollAnchor> {
        self.capture_scroll_anchor()
    }

    /// Re-pin the viewport to a bookmark from [`Self::capture_scroll_bookmark`].
    pub fn restore_scroll_bookmark(&mut self, bookmark: ScrollAnchor) {
        self.restore_scroll_anchor(bookmark);
    }

    /// Mark an entry as finished (no longer running).
    ///
    /// If the entry has been running for less than `MIN_RUNNING_DURATION_MS`, the finish is deferred so the animated "running" state is visible.
    pub fn finish_running(&mut self, id: EntryId) {
        self.finish_running_with_time(id, None);
    }

    /// Mark every running entry finished. `finish_turn` alone would leave them animating forever.
    pub fn finish_all_running(&mut self) {
        let ids: Vec<EntryId> = self.running.iter().copied().collect();
        for id in ids {
            self.finish_running(id);
        }
    }

    /// Mark an entry as no longer running, with optional thinking time.
    ///
    /// For thinking blocks, the thinking_time_ms will be displayed in collapsed mode.
    pub fn finish_running_with_time(&mut self, id: EntryId, thinking_time_ms: Option<i64>) {
        self.running.remove(&id);
        // Track the finish-flash window so `tick()` checks O(flashing) entries instead of scanning the whole scrollback per tick
        if self.entries.contains_key(&id) && !self.flashing.contains(&id) {
            self.flashing.push(id);
        }
        let thinking_mode = self.thinking_display_mode;
        let respect_manual_folds = self.appearance.scrollback.scroll.respect_manual_folds;
        if let Some(entry) = self.get_by_id_mut(id) {
            entry.is_running = false;
            entry.finished_at = Some(Instant::now());
            // Finish streaming renderers (final safety re-render)
            match &mut entry.block {
                RenderBlock::AgentMessage(msg) => msg.finish(),
                RenderBlock::Thinking(thinking) => {
                    // finish() freezes the local started_at timer into elapsed_time_ms
                    // Only use server time as a fallback when no local timer exists (e.g., during replay)
                    thinking.finish();
                    if thinking.elapsed_time_ms().is_none()
                        && let Some(time_ms) = thinking_time_ms
                    {
                        thinking.set_elapsed_time_ms(Some(time_ms));
                    }
                }
                RenderBlock::ToolCall(ToolCallBlock::Execute(b)) => b.finish(),
                RenderBlock::ToolCall(ToolCallBlock::Read(b)) => b.finish(),
                RenderBlock::ToolCall(ToolCallBlock::Edit(b)) => b.finish(),
                RenderBlock::ToolCall(ToolCallBlock::Search(b)) => b.finish(),
                RenderBlock::ToolCall(ToolCallBlock::ListDir(b)) => b.finish(),
                RenderBlock::ToolCall(ToolCallBlock::WebFetch(b)) => b.finish(),
                RenderBlock::ToolCall(ToolCallBlock::WebSearch(b)) => b.finish(),
                RenderBlock::ToolCall(ToolCallBlock::MemorySearch(b)) => b.finish(),
                RenderBlock::ToolCall(ToolCallBlock::SentMessage(b)) => b.finish(),
                RenderBlock::ToolCall(ToolCallBlock::Other(b)) => b.finish(),
                _ => {}
            }
            // Let the block decide what display mode to adopt on finish. For thinking blocks, use the sticky
            // `thinking_display_mode` so Ctrl+E is respected across the session. Exception: an already-Expanded thinking block
            // keeps its mode. Entries the user manually folded (pinned) keep their mode.
            if !(respect_manual_folds && entry.display_mode_pinned) {
                if matches!(entry.block, RenderBlock::Thinking(_)) {
                    if entry.display_mode != DisplayMode::Expanded {
                        entry.display_mode = thinking_mode;
                    }
                } else if let Some(mode) = entry.block.finished_display_mode()
                    && entry.display_mode != DisplayMode::Collapsed
                {
                    entry.display_mode = mode;
                }
            }
            entry.invalidate_cache();
        }
        // Mark height dirty since collapsed mode has different height. display_mode may have changed, so gaps need recomputation.
        self.dirty_heights.insert(id);
        self.gaps_may_be_dirty = true;

        self.bump_content_generation();
    }

    /// Mark an entry as awaiting (or no longer awaiting) user input. Returns `false` if the entry doesn't exist or
    /// already had the requested value.
    pub fn set_pending_user_input(&mut self, id: EntryId, pending: bool) -> bool {
        let Some(entry) = self.get_by_id_mut(id) else {
            return false;
        };
        if entry.is_pending_user_input == pending {
            return false;
        }
        entry.is_pending_user_input = pending;
        // No `invalidate_cache()`: the cached output is identical, only the post-pass bullet styling differs frame to frame
        // The flip is structural though: `run_step` keeps pending rows out of verb-group runs
        // An already-folded run must re-run its folds to surface the prompt row (and refold once it resolves)
        self.mark_structurally_dirty(id);
        true
    }

    /// Open a permission edit once. A pinned fold is a user choice and must stick
    /// across the per-frame pending clear/re-mark.
    pub(crate) fn open_permission_edit(&mut self, id: EntryId) {
        if self.permission_opened.contains(&id) {
            return;
        }
        let Some(entry) = self.get_by_id_mut(id) else {
            return;
        };
        if entry.display_mode_pinned {
            return;
        }
        let RenderBlock::ToolCall(ToolCallBlock::Edit(edit)) = &entry.block else {
            return;
        };
        if edit.hunks.is_empty() || entry.display_mode == DisplayMode::Expanded {
            return;
        }
        entry.display_mode = DisplayMode::Expanded;
        entry.invalidate_cache();
        self.permission_opened.insert(id);
        self.mark_structurally_dirty(id);
    }

    /// Put a permission-opened edit back to its default fold once the prompt is gone.
    /// A pinned fold is a user choice and is left alone.
    pub fn close_permission_edit(&mut self, id: EntryId) {
        if !self.permission_opened.remove(&id) {
            return;
        }
        let expanded_by_default = self
            .appearance
            .scrollback
            .blocks
            .edit
            .effective_expanded(self.appearance.collapsed_edit_blocks);
        let Some(entry) = self.get_by_id_mut(id) else {
            return;
        };
        if entry.display_mode_pinned {
            return;
        }
        let RenderBlock::ToolCall(ToolCallBlock::Edit(edit)) = &entry.block else {
            return;
        };
        let mode = edit_default_display_mode(expanded_by_default, edit);
        if entry.display_mode != mode {
            entry.display_mode = mode;
            entry.invalidate_cache();
            self.mark_structurally_dirty(id);
        }
    }

    /// Clear the pending-user-input flag from every entry.
    ///
    /// Called by `AgentView` before re-syncing flags from the current permission/question queues so stale marks don't linger.
    pub fn clear_all_pending_user_input(&mut self) {
        // Real transitions are structural (mirrors `set_pending_user_input`): un-flagging returns a row to verb-group
        // membership. This clear is the only path back to false when a resolved permission simply stops being re-marked by
        // the next sync.
        let flagged: Vec<EntryId> = self
            .entries
            .iter()
            .filter_map(|(id, e)| e.is_pending_user_input.then_some(*id))
            .collect();
        for id in flagged {
            if let Some(entry) = self.entries.get_mut(&id) {
                entry.is_pending_user_input = false;
            }
            self.mark_structurally_dirty(id);
        }
    }

    pub fn pending_user_input_ids(&self) -> std::collections::HashSet<EntryId> {
        self.entries
            .iter()
            .filter_map(|(id, entry)| entry.is_pending_user_input.then_some(*id))
            .collect()
    }

    /// Whether any entry is currently flagged as awaiting user input.
    ///
    /// Used by the animation driver: a flagged entry needs ticks even when nothing is `running` (the tool is paused on the user, not the model).
    pub fn has_pending_user_input(&self) -> bool {
        self.entries.values().any(|e| e.is_pending_user_input)
    }

    /// Invalidate all running entries for re-render.
    /// Call this periodically (e.g., every second) to update dynamic content like "[Running for Xs]" timers.
    /// This is O(running_count), not O(total).
    pub fn tick_running(&mut self) {
        let running_ids: Vec<EntryId> = self.running.iter().copied().collect();
        for id in running_ids {
            if let Some(entry) = self.get_by_id_mut(id) {
                entry.invalidate_cache();
            }
        }
    }

    /// Start a new streaming agent message.
    /// Creates an empty AgentMessageBlock in streaming mode and returns its EntryId.
    /// Use `get_by_id_mut()` to access the entry and push chunks.
    pub fn start_streaming_agent(&mut self) -> EntryId {
        let block = RenderBlock::agent_message_streaming();
        let entry = ScrollbackEntry::running(block);
        self.push(entry)
    }

    /// Get an entry by index (immutable).
    pub fn entry(&self, index: usize) -> Option<&ScrollbackEntry> {
        self.entries.get_index(index).map(|(_, v)| v)
    }

    /// Iterate over all entries mutably.
    pub fn entries_mut(&mut self) -> indexmap::map::ValuesMut<'_, EntryId, ScrollbackEntry> {
        self.entries.values_mut()
    }

    // Widget Helpers (used by ScrollbackPane widget)

    /// The one pre-render mutation point: refresh viewport size, rebuild the layout cache if needed, then apply follow-mode scroll.
    /// Callers must not mutate layout elsewhere or heights and scroll position drift from what is painted.
    pub fn prepare_layout(&mut self, width: u16, height: u16) -> bool {
        // Mid-session ffmpeg install: invalidate cached banner-sized reservations.
        if !self.ffmpeg_available_snapshot && crate::ui::scrollback::media::ffmpeg_available() {
            self.ffmpeg_available_snapshot = true;
            self.gaps_may_be_dirty = true;
            self.invalidate_layout_cache();
        }

        // Bump generation when viewport dimensions change: screen coordinates of visible links shift, so the VisibleLinkMap must be rebuilt
        if height != self.viewport_height || width != self.last_width {
            self.bump_generation();
        }

        // Update viewport height
        self.viewport_height = height;
        self.release_pin_reserve_outside_view();

        // Take an armed StructuralScrollAnchor unconditionally so it never outlives the first layout pass after its mutation
        // Only the same-width full rebuild below applies it
        let structural_anchor = self.structural_scroll_anchor.take();

        // Case 1: Cache missing or width changed, full rebuild
        if self.layout_cache.is_none() || width != self.last_width || self.full_settlement_pending {
            // A width change re-wraps every entry. The absolute wrapped-row scroll_offset would then point at different
            // content after the rebuild (the resize jump). Anchoring is intentionally limited to the not-following path.
            // Follow mode (including the follow_preserve_scroll page-flip) re-pins each frame, so it needs no anchor.
            let scroll_anchor =
                if width != self.last_width && !self.follow_mode && self.scroll_offset > 0 {
                    self.capture_scroll_anchor()
                } else {
                    None
                };

            let width_changed = width != self.last_width;
            let resized = width_changed && self.last_width != 0;
            let pre_rebuild_pin = self.pin_reserve_target;
            let has_targeted_dirty =
                !self.dirty_heights.is_empty() && self.dirty_heights.len() < self.entries.len();
            if width_changed {
                for entry in self.entries.values_mut() {
                    entry.invalidate_width_caches();
                }
                self.last_width = width;
            }
            if self.full_settlement_pending {
                self.layout_cache = None;
                self.full_settlement_pending = false;
            }
            // Full rebuild produces cheap height ESTIMATES for every entry.
            self.ensure_layout_cache(width);
            if resized
                && self.follow_mode
                && self.follow_preserve_scroll
                && self.pin_reserve_active
                && let Some(target) = self.pin_reserve_prompt_scroll_target()
            {
                self.scroll_offset = target;
            }
            self.compute_total_height_from_cache();
            // Re-pin the anchored content to the viewport top now that virtual_y is rebuilt at the new width (before settle clamps / re-pins to it)
            if let Some(anchor) = scroll_anchor {
                self.restore_scroll_anchor(anchor);
            } else if !width_changed {
                // Same-width rebuild forced by a structural mutation: re-pin the pre-mutation viewport-top content by stable EntryId
                // On a width change the anchor is dropped instead; its row offset is meaningless after a re-wrap
                self.apply_structural_scroll_anchor(structural_anchor, width);
            }
            self.fixup_hidden_selection();
            if self.follow_mode && !self.follow_preserve_scroll {
                self.handle_follow_mode();
            }
            // Page-flip preserve defers release until its authoritative target is restored/reset.
            self.settle_visible_measurements(width, layout::SettlementFollowPolicy::Defer);
            if resized {
                self.reset_pin_reserve_target();
                self.compute_total_height_from_cache();
                if self.follow_mode
                    && self.follow_preserve_scroll
                    && self.pin_reserve_active
                    && let Some(target) = self.pin_reserve_target
                {
                    self.scroll_offset = target;
                } else {
                    self.scroll_offset = self.scroll_offset.min(self.max_scroll_offset());
                }
            } else if self.pin_reserve_active {
                self.compute_total_height_from_cache();
                self.settle_pin_reserve_target();
            }
            let pin_entry_gone = self.pin_reserve_active
                && self.pin_reserve_prompt_id.is_some()
                && self.pin_reserve_prompt_index().is_none();
            if pin_entry_gone {
                let unpadded_total = self.total_height.saturating_sub(self.pin_reserve_pad);
                let was_following = self.follow_mode;
                self.follow_preserve_scroll = false;
                self.clear_pin_reserve();
                self.total_height = unpadded_total;
                self.pin_reserve_pad = 0;
                if was_following {
                    self.scroll_offset = self.max_scroll_offset();
                } else {
                    self.scroll_offset = self.scroll_offset.min(self.max_scroll_offset());
                }
            } else if !resized
                && (has_targeted_dirty || self.pin_reserve_after_turn)
                && self.follow_mode
                && self.follow_preserve_scroll
            {
                let unpadded_total = self.total_height.saturating_sub(self.pin_reserve_pad);
                let shrink_target = if has_targeted_dirty {
                    pre_rebuild_pin
                } else {
                    self.pin_reserve_target
                };
                if shrink_target.is_some_and(|target| target >= unpadded_total) {
                    self.follow_preserve_scroll = false;
                    self.clear_pin_reserve();
                    self.total_height = unpadded_total;
                    self.pin_reserve_pad = 0;
                    self.scroll_offset = self.max_scroll_offset();
                }
            }
            self.handle_follow_mode();
            // Pre-measure a few pages above the bottom so the first scroll-up is glitch-free (no-op unless bottom-pinned)
            // Warming three off-screen pages per drag event, only to throw them away at the next width, profiled as the largest cost of a resize
            // Hence the deferral
            if resized {
                self.warm_above = DeferredWarmAbove::Deferred;
            } else {
                self.warm_above = DeferredWarmAbove::Idle;
                self.warm_measure_pages_above(width);
            }
            self.dirty_heights.clear();
            self.gaps_may_be_dirty = false;
            return true;
        }

        // Case 2: Some entries have dirty heights, incremental update
        if !self.dirty_heights.is_empty() {
            // Viewport-top identity before heights change
            // Case 2 retains the cache (no insert/remove), so the plain index stays valid for the duration of this call
            let top_anchor = self.viewport_top_anchor_point();
            let pin_before = self.pin_reserve_prompt_scroll_target();
            let changes = self.update_dirty_entry_heights(width);
            self.dirty_heights.clear();

            if !changes.is_empty() {
                if self.gaps_may_be_dirty {
                    // Structural change (fold/expand/add/remove): full rebuild
                    self.rebuild_virtual_y_from_heights();
                    let pin_after = self.pin_reserve_prompt_scroll_target();
                    self.shift_pin_reserve_target_for_layout(pin_before, pin_after);
                    self.gaps_may_be_dirty = false;
                    self.compute_total_height_from_cache();
                    self.fixup_hidden_selection();
                } else {
                    // Fast path (streaming): only heights changed, gaps are stable.
                    // Patch virtual_y in O(n-k) where k is the earliest dirty index.
                    // For streaming (dirty entry at end), this is O(1).
                    self.shift_pin_reserve_target_for_changes(&changes);
                    let total_delta = self.patch_virtual_y_for_dirty(&changes);
                    // Streamed growth must shrink the reserve rather than inflate max_offset.
                    let content = self.total_height.saturating_sub(self.pin_reserve_pad);
                    let new_content = (content as i64 + total_delta as i64).max(0) as usize;
                    self.pin_reserve_pad = self.pin_reserve_pad_rows(new_content);
                    self.total_height = new_content.saturating_add(self.pin_reserve_pad);
                }
            } else if self.gaps_may_be_dirty {
                // Heights didn't change, but structural state is dirty (e.g., a new entry was pushed that extends a group needing truncation)
                // Must rebuild to apply group truncation even though heights are stable.
                self.rebuild_virtual_y_from_heights();
                let pin_after = self.pin_reserve_prompt_scroll_target();
                self.shift_pin_reserve_target_for_layout(pin_before, pin_after);
                self.gaps_may_be_dirty = false;
                self.compute_total_height_from_cache();
                self.fixup_hidden_selection();
            } else {
                // No heights changed, but visible range may have shifted
                self.compute_total_height_from_cache();
            }

            // Re-pin the pre-change viewport-top row: geometry above it may have shifted virtual_y while the absolute scroll_offset stayed put
            // (This is an exact no-op for changes at/below the top.)
            if let Some((entry_idx, rows_into_span)) = top_anchor {
                self.repin_viewport_top_to_entry(entry_idx, rows_into_span);
            }
            self.handle_follow_mode();
            // A scroll/content change may have brought estimated entries into view (e.g. streaming while scrolled up); measure them exactly.
            self.settle_visible_measurements(width, layout::SettlementFollowPolicy::Evaluate);
            self.run_pending_warm_above(width);
            return !changes.is_empty();
        }

        // Case 3: Nothing structurally changed, but total_height still depends on visible_entry_range()
        // That range can change between renders (view mode switch, turn navigation).
        // Recompute unconditionally; it's just summing a slice
        self.compute_total_height_from_cache();
        // Handle follow mode even when nothing changed structurally.
        // Needed after fold_selected_impl clears dirty_heights: the fold leaves the cache clean
        // Follow/preserve state may still need to react to the new total_height (e.g., consume preserve on overflow)
        if self.follow_mode {
            self.handle_follow_mode();
            if self.follow_preserve_scroll && !self.pin_reserve_active {
                self.scroll_offset = self.scroll_offset.min(self.max_scroll_offset());
            }
        }
        // Scroll-up (no dirty heights) reveals estimated off-screen entries; this is the on-demand measurement path for plain scrolling
        self.settle_visible_measurements(width, layout::SettlementFollowPolicy::Evaluate);
        self.run_pending_warm_above(width);
        false
    }

    /// Mark the start of a frame that will draw this scrollback.
    /// Hosts must call this once per frame; it is the only signal of a frame boundary [`DeferredWarmAbove`] has.
    pub fn begin_frame(&mut self) {
        if self.warm_above == DeferredWarmAbove::Deferred {
            self.warm_above = DeferredWarmAbove::Armed;
        }
    }

    fn run_pending_warm_above(&mut self, width: u16) {
        if self.warm_above == DeferredWarmAbove::Armed {
            self.warm_above = DeferredWarmAbove::Idle;
            self.warm_measure_pages_above(width);
        }
    }

    /// Invalidate caches if width changed.
    pub fn invalidate_if_width_changed(&mut self, width: u16) {
        if width != self.last_width {
            for entry in self.entries.values_mut() {
                entry.invalidate_cache();
            }
            self.last_width = width;
            self.layout_cache = None;
            self.gaps_may_be_dirty = true;
            // Mark all heights as dirty
            self.dirty_heights = self.entries.keys().copied().collect();
        }
    }

    /// Get current scroll offset.
    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    pub fn capture_viewport_snapshot(&self) -> ViewportSnapshot {
        ViewportSnapshot {
            scroll_offset: self.scroll_offset,
            follow_mode: self.follow_mode,
            follow_preserve_scroll: self.follow_preserve_scroll,
            follow_preserve_content_generation: self.follow_preserve_content_generation,
            viewport_height: self.viewport_height,
            last_width: self.last_width,
            selected: self.selected,
            current_turn: self.current_turn,
            view_mode: self.view_mode,
            total_height: self.total_height,
        }
    }

    pub fn restore_viewport_snapshot(&mut self, snap: ViewportSnapshot) {
        self.scroll_offset = snap.scroll_offset;
        self.follow_mode = snap.follow_mode;
        self.follow_preserve_scroll = snap.follow_preserve_scroll;
        self.follow_preserve_content_generation = snap.follow_preserve_content_generation;
        self.viewport_height = snap.viewport_height;
        self.last_width = snap.last_width;
        self.selected = snap.selected;
        self.current_turn = snap.current_turn;
        self.view_mode = snap.view_mode;
        self.invalidate_layout_cache();
    }

    /// Set viewport height.
    pub fn set_viewport_height(&mut self, height: u16) {
        self.viewport_height = height;
    }

    /// Set total content height.
    pub fn set_total_height(&mut self, height: usize) {
        self.total_height = height;
    }

    /// Set the scroll offset directly (e.g., from a scrollbar click).
    ///
    /// Clamps to `[0, max_offset]` and disables follow mode since the user is explicitly positioning the viewport.
    pub fn set_scroll_offset(&mut self, offset: usize) {
        let max_offset = self
            .total_height
            .saturating_sub(self.viewport_height as usize);
        self.scroll_offset = offset.min(max_offset);
        self.follow_mode = false;
        self.bump_generation();
    }

    /// Get mutable reference to an entry.
    pub fn entry_mut(&mut self, index: usize) -> Option<&mut ScrollbackEntry> {
        self.entries.get_index_mut(index).map(|(_, v)| v)
    }

    /// Whether group truncation replaces or hides this entry's content at the current layout (a "N more" header, or a hidden member at height 0).
    /// Either way the entry's own content is not what's on screen.
    pub fn entry_content_hidden_by_group(&self, idx: usize) -> bool {
        let Some(cache) = self.layout_cache.as_ref() else {
            return false;
        };
        let Some(info) = cache.entries.get(idx) else {
            return false;
        };
        (info.is_group_header() && !info.is_expanded_verb_header()) || info.height == 0
    }

    /// Whether entry `idx` overlaps the current viewport (cached offsets + the current scroll).
    /// A visible entry is already exact (the prior settle covers the visible window), so callers can skip re-measuring it.
    fn entry_overlaps_viewport(&self, idx: usize) -> bool {
        if !self.visible_entry_range().contains(&idx) {
            return false;
        }
        let Some((top, bottom)) = self.viewport_virtual_bounds() else {
            return false;
        };
        let Some(cache) = self.layout_cache.as_ref() else {
            return false;
        };
        let Some(&entry_top) = cache.virtual_y.get(idx) else {
            return false;
        };
        let Some(info) = cache.entries.get(idx) else {
            return false;
        };
        let entry_bottom = entry_top + info.height as usize;
        entry_top < bottom && entry_bottom > top
    }
}

/// Display mode a freshly materialized Edit block adopts (fresh `push`, or a kind upgrade in `replace_tool_block`).
/// Failed edits collapse; summaries the one-liner can't truthfully compress expand. Otherwise the effective
/// expanded default (`EditBlockConfig::effective_expanded`) decides.
fn edit_default_display_mode(expanded_by_default: bool, edit: &EditToolCallBlock) -> DisplayMode {
    if edit.is_success() && (edit.summary_untrusted || expanded_by_default) {
        DisplayMode::Expanded
    } else {
        DisplayMode::Collapsed
    }
}
