use super::*;

/// Find the response anchor for a turn: the first AgentMessage of the trailing agent-message run within `range`.
/// Everything else (session events, system messages, empty streaming placeholders) is skipped without breaking the
/// run.
fn response_anchor_in_range(
    entries: &IndexMap<EntryId, ScrollbackEntry>,
    range: Range<usize>,
) -> Option<usize> {
    let mut anchor = None;
    for idx in range.rev() {
        // Turn ranges are always in bounds; keep any anchor already found
        let Some((_, entry)) = entries.get_index(idx) else {
            break;
        };
        match &entry.block {
            RenderBlock::AgentMessage(msg) => {
                if !msg.is_empty() {
                    anchor = Some(idx);
                }
            }
            RenderBlock::ToolCall(_)
            | RenderBlock::Thinking(_)
            | RenderBlock::Subagent(_)
            | RenderBlock::BgTask(_) => break,
            _ => {}
        }
    }
    anchor
}

impl ScrollbackState {
    // Turn Navigation

    pub(super) fn rebuild_turns(&mut self) {
        self.turns.clear();
        let mut current_start: Option<usize> = None;

        for (i, entry) in self.entries.values().enumerate() {
            if matches!(entry.block, RenderBlock::UserPrompt(_)) {
                // Close previous turn
                if let Some(start) = current_start {
                    self.turns.push(Turn {
                        prompt_index: start,
                        end_index: i,
                        status: TurnStatus::Completed,
                    });
                }
                current_start = Some(i);
            }
        }

        // Close the last turn
        if let Some(start) = current_start {
            self.turns.push(Turn {
                prompt_index: start,
                end_index: self.entries.len(),
                status: TurnStatus::Running, // Last turn may still be active
            });
        }

        // Update current_turn
        self.current_turn = if self.turns.is_empty() {
            None
        } else if let Some(sel) = self.selected {
            self.turn_containing(sel)
        } else {
            Some(self.turns.len() - 1)
        };
    }

    pub fn turn_containing(&self, entry_index: usize) -> Option<usize> {
        self.turns
            .iter()
            .position(|t| entry_index >= t.prompt_index && entry_index < t.end_index)
    }

    pub fn current_turn(&self) -> Option<usize> {
        self.current_turn
    }

    pub fn turn_count(&self) -> usize {
        self.turns.len()
    }

    pub fn turn(&self, index: usize) -> Option<&Turn> {
        self.turns.get(index)
    }

    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

    /// Iterate over all entries in scrollback order, paired with their `EntryId`.
    pub fn iter_entries(&self) -> impl Iterator<Item = (EntryId, &ScrollbackEntry)> {
        self.entries.iter().map(|(id, entry)| (*id, entry))
    }

    /// Common logic for jumping to an entry within a turn.
    ///
    /// Sets `current_turn`, resets scroll in SingleTurn mode, selects the entry, and scrolls so it sits at the top of the viewport.
    fn activate_entry(&mut self, turn_idx: usize, entry_idx: usize) {
        self.current_turn = Some(turn_idx);
        if self.view_mode == ViewMode::SingleTurn {
            self.scroll_offset = 0;
            self.follow_mode = false;
        }
        self.selected = Some(entry_idx);
        if self.view_mode == ViewMode::AllTurns {
            self.scroll_to_entry_top(entry_idx);
        } else {
            self.ensure_selected_visible(NavDirection::default());
        }
        self.bump_generation();
    }

    /// Common logic for switching to a specific turn (jumps to its prompt).
    fn activate_turn(&mut self, turn_idx: usize) {
        if let Some(prompt_index) = self.turns.get(turn_idx).map(|t| t.prompt_index) {
            self.activate_entry(turn_idx, prompt_index);
        }
    }

    /// Jump directly to a turn by index: select its prompt and scroll it to the viewport top (same behavior as h/l turn navigation, random access).
    /// Returns `false` for an out-of-range index.
    pub fn jump_to_turn(&mut self, turn_idx: usize) -> bool {
        if turn_idx >= self.turns.len() {
            return false;
        }
        self.activate_turn(turn_idx);
        true
    }

    /// Jump to a turn by its prompt's stable [`EntryId`]: resolve to the current index and activate that turn.
    /// Returns `false` if the id no longer exists or isn't a turn's prompt (e.g. removed since capture).
    /// Stable identity means a shifted index can't land on the wrong block.
    pub fn jump_to_entry(&mut self, prompt_id: EntryId) -> bool {
        let Some(entry_idx) = self.index_of_id(prompt_id) else {
            return false;
        };
        let Some(turn_idx) = self.turns.iter().position(|t| t.prompt_index == entry_idx) else {
            return false;
        };
        self.activate_turn(turn_idx);
        true
    }

    /// Navigate to the next turn (l key).
    /// If we're before the first turn (e.g., at system messages), jumps to the first turn.
    /// Otherwise jumps to the next turn.
    pub fn next_turn(&mut self) -> bool {
        // If no turns exist, nothing to do
        if self.turns.is_empty() {
            return false;
        }

        let next = match self.current_turn {
            Some(current) => {
                if current + 1 >= self.turns.len() {
                    // Already at last turn; re-activate it to scroll its prompt to the top (same as h then l would do)
                    self.activate_turn(current);
                    return true;
                }
                current + 1
            }
            None => {
                // Not at any turn (e.g., before first prompt), go to first turn
                0
            }
        };

        self.activate_turn(next);
        true
    }

    /// Navigate to the previous turn (h key). If currently in a turn's response (not on the prompt), jumps to the
    /// current turn's prompt. If already on a prompt, jumps to the previous turn's prompt. If at first turn's prompt
    /// and pre-turn exists with selectable entries, jumps there.
    pub fn prev_turn(&mut self) -> bool {
        // If no turns exist, nothing to do
        if self.turns.is_empty() {
            return false;
        }

        // Check if we're currently in pre-turn (current_turn is None)
        if self.current_turn.is_none() {
            return false; // Already at pre-turn, can't go further back
        }

        let current = self.current_turn.unwrap();
        let current_turn = match self.turns.get(current) {
            Some(t) => t,
            None => return false,
        };

        // Check if we're on the current turn's prompt or somewhere in its response
        // If selected is None, treat as being "on prompt" (go to previous turn)
        let on_prompt = self.selected.is_none() || self.selected == Some(current_turn.prompt_index);

        if !on_prompt {
            // In the turn's response: go to current turn's prompt first
            self.activate_turn(current);
            return true;
        }

        // Already on a prompt: go to previous turn
        if current == 0 {
            // At first turn's prompt: check if pre-turn exists and has selectable entries
            if current_turn.prompt_index > 0 {
                let range = 0..current_turn.prompt_index;
                let first_selectable = self.find_first_selectable_in_range(range);
                if first_selectable.is_none() {
                    return false; // No selectable entries in pre-turn
                }
                self.current_turn = None;
                if self.view_mode == ViewMode::SingleTurn {
                    self.scroll_offset = 0;
                    self.follow_mode = false;
                }
                self.selected = first_selectable;
                if self.view_mode == ViewMode::AllTurns {
                    self.scroll_offset = 0;
                }
                return true;
            }
            return false; // No pre-turn, already at first turn's prompt
        }

        // Go to previous turn's prompt
        self.activate_turn(current - 1);
        true
    }

    /// Estimated (header-free) top scroll offset of an entry, for cheap candidate pruning before the exact sticky-converged computation.
    fn entry_top_estimate(&self, idx: usize) -> Option<usize> {
        let cache = self.layout_cache.as_ref()?;
        let range = self.visible_entry_range();
        let base = *cache.virtual_y.get(range.start)?;
        let y = *cache.virtual_y.get(idx)?;
        Some(y.saturating_sub(base))
    }

    /// Apply a response snap: select the anchor and put its top at the viewport's content top (`target` from `entry_top_scroll_offset`).
    fn snap_to_response(&mut self, turn_idx: usize, entry_idx: usize, target: usize) {
        self.current_turn = Some(turn_idx);
        self.selected = Some(entry_idx);
        self.scroll_offset = target;
        self.follow_mode = false;
        self.bump_generation();
    }

    /// Snap the viewport up to the nearest response anchor above. Anchors are computed on demand; only plausible
    /// candidates are measured exactly. Returns `false` without a layout or when no anchor lies above.
    pub fn prev_response(&mut self) -> bool {
        if self.viewport_height == 0 || self.last_width == 0 {
            return false;
        }
        self.ensure_layout_cache(self.last_width);
        for t in (0..self.turns.len()).rev() {
            let Some(turn) = self.turns.get(t) else {
                continue;
            };
            let Some(idx) = response_anchor_in_range(&self.entries, turn.range()) else {
                continue;
            };
            if !self.visible_entry_range().contains(&idx) {
                continue;
            }
            // Headers shrink the exact target by at most the truncated-header cap, so anchors estimated further below can be skipped unmeasured
            // (Viewport-height margins invert on tiny terminal splits.)
            let Some(estimate) = self.entry_top_estimate(idx) else {
                continue;
            };
            if estimate
                >= self
                    .scroll_offset
                    .saturating_add(MAX_TRUNCATED_HEADER_HEIGHT as usize + 1)
            {
                continue;
            }
            if let Some(target) = self.entry_top_scroll_offset(idx) {
                debug_assert!(target <= estimate, "exact target exceeded estimate");
                if target < self.scroll_offset {
                    self.snap_to_response(t, idx, target);
                    return true;
                }
            }
        }
        false
    }

    /// Snap the viewport down to the nearest response anchor below. Returns `false` without a layout or when no
    /// anchor lies below.
    pub fn next_response(&mut self) -> bool {
        if self.viewport_height == 0 || self.last_width == 0 {
            return false;
        }
        self.ensure_layout_cache(self.last_width);
        for t in 0..self.turns.len() {
            let Some(turn) = self.turns.get(t) else {
                continue;
            };
            let Some(idx) = response_anchor_in_range(&self.entries, turn.range()) else {
                continue;
            };
            if !self.visible_entry_range().contains(&idx) {
                continue;
            }
            // The exact target never exceeds the estimate, so anchors estimated at or above the current offset can't land below it
            let Some(estimate) = self.entry_top_estimate(idx) else {
                continue;
            };
            if estimate <= self.scroll_offset {
                continue;
            }
            if let Some(target) = self.entry_top_scroll_offset(idx) {
                debug_assert!(target <= estimate, "exact target exceeded estimate");
                if target > self.scroll_offset {
                    self.snap_to_response(t, idx, target);
                    return true;
                }
            }
        }
        false
    }

    /// From inside an answer that anchor is the nearest one above, so the indicator only shows when the click has that
    /// answer's top to land on. Cache-only estimate (`&self`, headers ignored) so render can poll it every frame. The
    /// estimate never undershoots the exact target, so a visible indicator always has a real jump behind it.
    pub fn has_response_top_above(&self) -> bool {
        let Some(turn) = self
            .active_turn_for_viewport()
            .and_then(|t| self.turns.get(t))
        else {
            return false;
        };
        let Some(idx) = response_anchor_in_range(&self.entries, turn.range()) else {
            return false;
        };
        self.visible_entry_range().contains(&idx)
            && self
                .entry_top_estimate(idx)
                .is_some_and(|estimate| estimate < self.scroll_offset)
    }

    pub fn set_last_turn_status(&mut self, status: TurnStatus) {
        if let Some(turn) = self.turns.last_mut() {
            turn.status = status;
        }
    }

    // Scrolling

    pub fn scroll_up(&mut self, rows: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(rows as usize);
        self.follow_mode = false;
        self.bump_generation();
    }

    /// Scroll down by n rows. A scroll that lands at the bottom moved real rows and never engages, so a fast
    /// scroll-down ending there can't re-enter follow by accident.
    pub fn scroll_down(&mut self, rows: u16) {
        let max_offset = self
            .total_height
            .saturating_sub(self.viewport_height as usize);
        let before = self.scroll_offset;
        self.scroll_offset = (self.scroll_offset + rows as usize).min(max_offset);

        // rows > 0 keeps degenerate page/half-page calls (0-row viewports) from engaging follow without a real scroll gesture
        if rows > 0
            && self.scroll_offset == before
            && self.scroll_offset >= max_offset
            && self.pin_reserve_pad == 0
            && self.appearance.scrollback.scroll.follow_by_overscroll
        {
            self.follow_mode = true;
            if self.follow_preserve_scroll && self.pin_reserve_after_turn {
                self.follow_preserve_scroll = false;
                self.release_pin_reserve();
                self.scroll_offset = self.max_scroll_offset();
            }
        }
        self.bump_generation();
    }

    /// Rows to advance for a full-page scroll. Without it, a page moves `viewport_height - 2` rows but only
    /// `viewport_height - header` rows are actually on screen. Always moves at least 1 row so paging never stalls on a
    /// tiny viewport.
    fn page_scroll_rows(&self) -> u16 {
        let header = self.current_header_screen_rows();
        self.viewport_height
            .saturating_sub(header)
            .saturating_sub(2)
            .max(1)
    }

    /// Current sticky header height in screen rows (0 when no header/cache).
    fn current_header_screen_rows(&self) -> u16 {
        // Mirror render_with_sticky_headers: no sticky header is drawn when disabled or in compact prompt mode
        // The whole viewport is then content, so paging must not subtract a header height
        if !self.appearance.scrollback.display.sticky_headers || self.appearance.prompt.compact {
            return 0;
        }
        let Some(cache) = self.layout_cache.as_ref() else {
            return 0;
        };
        let range = self.visible_entry_range();
        if range.is_empty() {
            return 0;
        }
        self.current_sticky_layout(cache, &range)
            .header_screen_rows()
    }

    /// Page up: scroll viewport, then select the topmost selectable on-screen entry.
    pub fn page_up(&mut self) {
        self.scroll_up(self.page_scroll_rows());
        self.select_viewport_edge(/* prefer_top */ true);
    }

    /// Page down: scroll viewport, then select the bottommost selectable on-screen entry.
    pub fn page_down(&mut self) {
        self.scroll_down(self.page_scroll_rows());
        self.select_viewport_edge(/* prefer_top */ false);
    }

    /// Select selectable entry covering the visual top (`prefer_top`) or bottom row; walk inward if needed.
    fn select_viewport_edge(&mut self, prefer_top: bool) {
        if self.viewport_height == 0 || self.last_width == 0 {
            return;
        }
        self.ensure_layout_cache(self.last_width);
        let range = self.visible_entry_range();
        if range.is_empty() {
            return;
        }
        let Some((vp_top, vp_bottom)) = self.viewport_virtual_bounds() else {
            return;
        };
        // Empty viewport (bottom == top) has no edge row to anchor on.
        if vp_bottom <= vp_top {
            return;
        }
        let edge_row = if prefer_top { vp_top } else { vp_bottom - 1 };
        let Some(raw_edge) = self.entry_at_virtual_row(edge_row) else {
            return;
        };
        // Clamp into the visible range so `..=` / `..` never panic on inverted bounds.
        let edge_idx = raw_edge.clamp(range.start, range.end - 1);

        let pick = if prefer_top {
            (edge_idx..range.end).find(|&idx| self.is_selectable_in_viewport(idx))
        } else {
            (range.start..=edge_idx)
                .rev()
                .find(|&idx| self.is_selectable_in_viewport(idx))
        };

        if let Some(idx) = pick {
            self.selected = Some(idx);
            self.sync_current_turn();
        }
    }

    fn is_selectable_in_viewport(&self, idx: usize) -> bool {
        if self.is_entry_hidden(idx) || !self.entry_overlaps_viewport(idx) {
            return false;
        }
        self.entries
            .get_index(idx)
            .is_some_and(|(_, e)| e.block.is_selectable())
    }

    /// Half page up (Ctrl-U style).
    pub fn half_page_up(&mut self) {
        self.scroll_up(self.viewport_height / 2);
    }

    /// Half page down (Ctrl-D style).
    pub fn half_page_down(&mut self) {
        self.scroll_down(self.viewport_height / 2);
    }

    pub fn goto_top(&mut self) {
        self.scroll_offset = 0;
        self.follow_mode = false;
        let range = self.visible_entry_range();
        if !range.is_empty() {
            self.selected = self.find_first_selectable_in_range(range);
            self.sync_current_turn();
        }
        self.bump_generation();
    }

    pub fn goto_bottom(&mut self) {
        self.release_pin_reserve();
        let max_offset = self
            .total_height
            .saturating_sub(self.viewport_height as usize);
        self.scroll_offset = max_offset;
        self.follow_mode = true;
        // Also drop follow-preserve, or its branch in `follow_scroll_to_bottom` keeps holding the offset computed above
        // That offset may come from estimated heights, leaving End or PageDown stuck above or wedged past the measured bottom
        self.follow_preserve_scroll = false;

        let range = self.visible_entry_range();
        if !range.is_empty() {
            self.selected = self.find_last_selectable_in_range(range);
            self.sync_current_turn();
        }
        self.bump_generation();
    }

    pub fn toggle_follow(&mut self) {
        self.follow_mode = !self.follow_mode;
        if self.follow_mode {
            self.goto_bottom();
        }
    }

    /// Enable follow mode and scroll to bottom.
    pub fn enable_follow_mode(&mut self) {
        self.follow_mode = true;
        self.follow_preserve_scroll = false;
        self.goto_bottom();
    }

    /// Enable follow mode without changing scroll position.
    ///
    /// Use this when you've already positioned the viewport (e.g., after `scroll_to_entry_top`) but want new content to auto-scroll.
    pub fn enable_follow(&mut self) {
        self.follow_mode = true;
    }

    /// Enable follow mode, preserving the current scroll position for one frame.
    /// Like `enable_follow`, but also sets `follow_preserve_scroll` so the first `handle_follow_mode` call doesn't override the scroll position.
    /// Use after `scroll_to_entry_top` to keep the entry at the viewport top until new content arrives.
    pub fn enable_follow_with_preserve(&mut self) {
        self.follow_mode = true;
        self.follow_preserve_scroll = true;
        self.follow_preserve_content_generation = self.content_generation;
    }

    /// Pin an entry at the viewport top with real trailing scroll extent.
    pub(crate) fn page_flip_to_entry(&mut self, idx: usize) {
        self.arm_pin_reserve();
        self.scroll_to_entry_top(idx);
        self.pin_reserve_target = Some(self.scroll_offset);
        self.pin_reserve_prompt_id = self.entries.get_index(idx).map(|(id, _)| *id);
        self.compute_total_height_from_cache();
        self.enable_follow_with_preserve();
    }

    /// `page_flip` without a prompt (bash/synthetic): enable follow-with-preserve only. No `page_flip`, with a prompt:
    /// leave scroll and follow unchanged. No `page_flip`, no prompt: still enable follow-with-preserve (there is no
    /// prompt to snap. pre-setting bash/adoption always engaged follow). Always selects `prompt_idx` when present.
    pub fn follow_new_turn(&mut self, prompt_idx: Option<usize>, page_flip: bool) {
        if let Some(idx) = prompt_idx {
            self.set_selected(Some(idx));
        }
        if page_flip {
            if let Some(idx) = prompt_idx {
                self.page_flip_to_entry(idx);
            } else {
                self.release_pin_reserve();
                self.enable_follow_with_preserve();
            }
        } else if prompt_idx.is_none() {
            self.release_pin_reserve();
            self.enable_follow_with_preserve();
        }
    }

    pub fn is_follow_mode(&self) -> bool {
        self.follow_mode
    }

    pub fn is_follow_preserve_scroll(&self) -> bool {
        self.follow_preserve_scroll
    }

    pub fn has_content_below(&self) -> bool {
        let max_offset = self
            .total_height
            .saturating_sub(self.viewport_height as usize);
        self.total_height > self.viewport_height as usize && self.scroll_offset < max_offset
    }

    /// Ensure the selected entry is visible in the viewport, using minimal scrolling. large entries that exceed it
    /// always show their top. Only scrolls if the entry is not fully visible. `min_page_fraction`: if the scroll is
    /// smaller than this fraction, use it instead (disabled for large entries and at edges).
    pub(super) fn ensure_selected_visible(&mut self, _direction: NavDirection) {
        let Some(selected_idx) = self.selected else {
            return;
        };

        if self.entries.is_empty() || self.viewport_height == 0 || self.last_width == 0 {
            return;
        }

        // Get scroll config (cumulative scroll math is usize)
        let base_margin = self.appearance.scrollback.scroll.margin as usize;
        let min_scroll = self
            .appearance
            .scrollback
            .scroll
            .min_scroll_lines(self.viewport_height) as usize;

        // Get visible range; positions are relative to this range
        let visible_range = self.visible_entry_range();
        if !visible_range.contains(&selected_idx) {
            return; // Selected entry not in visible range, nothing to do
        }

        // Stay O(viewport): only an off-viewport selection (the path that scrolls) gets a bounded measure around it
        // The scroll branch below re-derives scroll
        // Measuring around an on-viewport selection would jump it (see measure_around_entry)
        self.ensure_layout_cache(self.last_width);
        if !self.entry_overlaps_viewport(selected_idx) {
            self.measure_around_entry(selected_idx, self.last_width);
        }

        // Margin only applies when there's content to show in that direction.
        // At scroll edges, margin would just show empty space.
        let max_scroll = self
            .total_height
            .saturating_sub(self.viewport_height as usize);
        let at_top = self.scroll_offset == 0;
        let at_bottom = self.scroll_offset >= max_scroll;
        let top_margin = if at_top { 0 } else { base_margin };
        let bottom_margin = if at_bottom { 0 } else { base_margin };

        let Some(ref cache) = self.layout_cache else {
            return;
        };

        // Compute RELATIVE positions for the visible range.
        // scroll_offset is relative to the start of visible_range, not entry 0.
        let Some(&base_y) = cache.virtual_y.get(visible_range.start) else {
            return;
        };
        let Some(&entry_y_abs) = cache.virtual_y.get(selected_idx) else {
            return;
        };
        let Some(entry_info) = cache.entries.get(selected_idx) else {
            return;
        };
        let entry_y = entry_y_abs - base_y;
        let entry_height = entry_info.height;
        let entry_bottom = entry_y + entry_height as usize;

        // Build prompt descriptors relative to visible range (for sticky layout)
        let relative_prompts = self.build_relative_prompt_descriptors(cache, &visible_range);

        // Compute sticky header at current scroll position
        let sticky =
            compute_sticky_layout(self.scroll_offset, self.viewport_height, &relative_prompts);
        let header_height = sticky.header_screen_rows();

        // Content area bounds
        let content_top = self.scroll_offset + header_height as usize;
        let viewport_bottom = self.scroll_offset + self.viewport_height as usize;

        // Effective bounds with margin
        let effective_top = content_top + top_margin;
        let effective_bottom = viewport_bottom.saturating_sub(bottom_margin);

        // Check if entry is fully visible (with margin)
        let is_fully_visible = entry_y >= effective_top && entry_bottom <= effective_bottom;

        if is_fully_visible {
            return; // No scroll needed
        }

        // Check if entry fits in viewport content area
        let content_height = self.viewport_height.saturating_sub(header_height);
        let entry_fits = entry_height <= content_height;

        let old_scroll = self.scroll_offset;

        if entry_y < effective_top {
            // Entry top is clipped (or within margin): scroll up to show top with margin
            let target_scroll = self.find_scroll_for_entry_at_content_top(
                entry_y.saturating_sub(top_margin),
                &relative_prompts,
            );
            self.scroll_offset = target_scroll;
            self.follow_mode = false;
        } else if entry_bottom > effective_bottom {
            // Entry bottom is clipped (or within margin)
            if entry_fits {
                // Entry fits at current header height: try scrolling to show bottom with margin
                let target_scroll =
                    (entry_bottom + bottom_margin).saturating_sub(self.viewport_height as usize);

                // But check: at the new scroll position, will a sticky header appear that clips the entry's top?
                let sticky_at_target =
                    compute_sticky_layout(target_scroll, self.viewport_height, &relative_prompts);
                let header_at_target = sticky_at_target.header_screen_rows();
                let content_top_at_target = target_scroll + header_at_target as usize;

                if entry_y >= content_top_at_target {
                    // Entry top will still be visible after scroll: use this target
                    self.scroll_offset = target_scroll;
                } else {
                    // Sticky header would clip the top: show top instead
                    let target_scroll =
                        self.find_scroll_for_entry_at_content_top(entry_y, &relative_prompts);
                    self.scroll_offset = target_scroll;
                }
            } else {
                // Entry doesn't fit: show top at content area top
                let target_scroll =
                    self.find_scroll_for_entry_at_content_top(entry_y, &relative_prompts);
                self.scroll_offset = target_scroll;
            }
            self.follow_mode = false;
        }

        // Apply minimum scroll: if we scrolled but less than min_scroll, use min_scroll. Only applies when. Entry fits in
        // viewport (large entries must show their top exactly). Won't end at scroll edge (would create empty space).
        let started_at_edge = at_top || at_bottom;
        let new_at_top = self.scroll_offset == 0;
        let new_at_bottom = self.scroll_offset >= max_scroll;
        let ends_at_edge = new_at_top || new_at_bottom;
        if min_scroll > 0
            && entry_fits
            && !started_at_edge
            && !ends_at_edge
            && self.scroll_offset != old_scroll
        {
            let actual_scroll = self.scroll_offset.abs_diff(old_scroll);

            if actual_scroll > 0 && actual_scroll < min_scroll {
                // Scroll more to meet minimum
                let max_scroll = self
                    .total_height
                    .saturating_sub(self.viewport_height as usize);
                if self.scroll_offset > old_scroll {
                    // Scrolling down: add more
                    self.scroll_offset = (old_scroll + min_scroll).min(max_scroll);
                } else {
                    // Scrolling up: subtract more
                    self.scroll_offset = old_scroll.saturating_sub(min_scroll);
                }
            }
        }
    }

    /// Build prompt descriptors relative to a visible range.
    /// Filters to only prompts in the range, adjusts y_virtual to be relative to the range start.
    /// This is needed because scroll_offset is relative to the visible range, not the full entry list.
    pub(super) fn build_relative_prompt_descriptors(
        &self,
        cache: &LayoutCache,
        visible_range: &Range<usize>,
    ) -> Vec<PromptDescriptor> {
        if visible_range.is_empty() {
            return Vec::new();
        }

        let Some(&base_y) = cache.virtual_y.get(visible_range.start) else {
            return Vec::new();
        };

        cache
            .prompt_descriptors
            .iter()
            .filter(|p| visible_range.contains(&p.entry_idx))
            .map(|p| PromptDescriptor {
                entry_idx: p.entry_idx, // Keep absolute index for lookup
                y_virtual: p.y_virtual.saturating_sub(base_y), // Relative Y
                full_height: p.full_height,
                min_height: p.min_height,
                sticky: p.sticky,
            })
            .collect()
    }

    /// Compute the current sticky header layout.
    /// Uses the current scroll_offset and viewport_height.
    /// Requires a valid layout cache; the caller passes it in, which avoids `&mut self`.
    pub(super) fn current_sticky_layout(
        &self,
        cache: &LayoutCache,
        visible_range: &Range<usize>,
    ) -> StickyHeaderLayout {
        let relative_prompts = self.build_relative_prompt_descriptors(cache, visible_range);
        compute_sticky_layout(self.scroll_offset, self.viewport_height, &relative_prompts)
    }

    /// Scroll offset that puts `entry_y` (relative to the visible range's top) at the content-area top, below any
    /// sticky header. Shared by `entry_top_scroll_offset` and the pin reserve's `pin_reserve_prompt_scroll_target`, so
    /// the convergence math lives in one place.
    pub(super) fn sticky_adjusted_entry_top(
        &self,
        cache: &LayoutCache,
        visible_range: &Range<usize>,
        entry_y: usize,
    ) -> usize {
        let relative_prompts = self.build_relative_prompt_descriptors(cache, visible_range);
        let mut scroll = entry_y;
        for _ in 0..3 {
            let sticky = compute_sticky_layout(scroll, self.viewport_height, &relative_prompts);
            scroll = entry_y.saturating_sub(sticky.header_screen_rows() as usize);
        }
        scroll
    }

    /// Find scroll position that puts entry_y at top of content area.
    ///
    /// Uses relative prompt descriptors and iterates to find a stable position because header height depends on scroll position.
    fn find_scroll_for_entry_at_content_top(
        &self,
        entry_y: usize,
        relative_prompts: &[PromptDescriptor],
    ) -> usize {
        let mut target_scroll = entry_y;

        // Iterate to find fixed point (max 3 iterations as safety bound).
        // Converges quickly because header shrinks monotonically as we scroll up.
        for _ in 0..3 {
            let sticky =
                compute_sticky_layout(target_scroll, self.viewport_height, relative_prompts);
            let header_at_target = sticky.header_screen_rows();
            let content_top_at_target = target_scroll + header_at_target as usize;

            if entry_y >= content_top_at_target {
                break; // Entry would be visible at this scroll position
            }

            // Need to scroll up more to account for header
            target_scroll = entry_y.saturating_sub(header_at_target as usize);
        }

        target_scroll
    }

    /// Scroll to put a specific entry at the top of the viewport.
    /// Unlike ensure_selected_visible (which only scrolls if entry is outside viewport), this always scrolls to position the entry at the top.
    /// Used for 'l' (next turn) navigation where we want the prompt at the very top.
    pub fn scroll_to_entry_top(&mut self, entry_idx: usize) {
        let Some(scroll) = self.entry_top_scroll_offset(entry_idx) else {
            return;
        };
        self.scroll_offset = scroll;
        self.follow_mode = false;
        self.bump_generation();
    }

    /// Exact scroll offset that puts `entry_idx` at the top of the viewport's content area, below any sticky header.
    /// Returns `None` without a usable layout. Measures the target entry exactly; does not move the viewport.
    fn entry_top_scroll_offset(&mut self, entry_idx: usize) -> Option<usize> {
        if self.entries.is_empty() || self.viewport_height == 0 || self.last_width == 0 {
            return None;
        }
        if entry_idx >= self.entries.len() {
            return None;
        }

        // Ensure layout cache is valid
        self.ensure_layout_cache(self.last_width);
        // Measure the target exactly first (settle only re-pins top/bottom, never an arbitrary target)
        // Measuring above is safe: callers consume the post-measure offset
        self.measure_scroll_target(entry_idx, self.last_width);

        let cache = self.layout_cache.as_ref()?;

        // scroll_offset is relative to the start of visible_entry_range, not entry 0.
        let visible_range = self.visible_entry_range();
        let base_y = *cache.virtual_y.get(visible_range.start)?;
        let entry_y_abs = *cache.virtual_y.get(entry_idx)?;
        let entry_y = entry_y_abs - base_y;

        // Account for the sticky header: the content area starts at scroll_offset + header_height, so scroll_offset = entry_y - header
        // The header depends on scroll_offset (sticky headers collapse as you scroll), so the shared helper iterates to a fixed point
        Some(self.sticky_adjusted_entry_top(cache, &visible_range, entry_y))
    }

    pub fn scroll_to_entry_center(&mut self, entry_idx: usize) {
        if self.entries.is_empty() || self.viewport_height == 0 || self.last_width == 0 {
            return;
        }
        if entry_idx >= self.entries.len() {
            return;
        }
        self.ensure_layout_cache(self.last_width);
        // Measure the target exactly first so the center uses exact offsets; otherwise an estimated off-screen target drifts off-center
        self.measure_scroll_target(entry_idx, self.last_width);
        let Some(ref cache) = self.layout_cache else {
            return;
        };
        let visible_range = self.visible_entry_range();
        let Some(&base_y) = cache.virtual_y.get(visible_range.start) else {
            return;
        };
        let Some(&entry_y_abs) = cache.virtual_y.get(entry_idx) else {
            return;
        };
        let entry_y = entry_y_abs - base_y;
        let half_vp = self.viewport_height as usize / 2;
        let target = entry_y.saturating_sub(half_vp);
        let relative_prompts = self.build_relative_prompt_descriptors(cache, &visible_range);
        let mut scroll = target;
        for _ in 0..3 {
            let sticky = compute_sticky_layout(scroll, self.viewport_height, &relative_prompts);
            let header = sticky.header_screen_rows();
            scroll = entry_y.saturating_sub(half_vp + header as usize);
        }
        self.scroll_offset = scroll;
        self.follow_mode = false;
        self.bump_generation();
    }

    /// Select entry `entry_idx` and reveal it: expand a fold and un-truncate its group so it is no longer hidden.
    /// Wrapped or tall entries thus land the match on screen instead of scrolling it past the viewport.
    pub fn reveal_entry_line(&mut self, entry_idx: usize, line_in_entry: usize) {
        if entry_idx >= self.entries.len() {
            return;
        }
        self.set_selected(Some(entry_idx));

        // is_entry_hidden and group-truncation visibility are only meaningful once the layout cache exists. On a cache
        // miss build it first so the unhide below isn't silently skipped is_entry_hidden conservatively reports "visible"
        // when the cache is absent. That would leave a truncated target hidden after the rebuild gate.
        if self.layout_cache.is_none() && self.last_width > 0 {
            self.rebuild_layout();
            self.dirty_heights.clear();
        }

        // Track whether the reveal actually changed what is shown; only then (or when the cache is missing/stale) is the O(history) rebuild needed
        let mut layout_changed = false;

        // Un-truncate the containing group if truncation currently hides it.
        // Captured before the fold below, which would split the collapsed run.
        if self.is_entry_hidden(entry_idx) {
            let group = self.group_range_of(entry_idx, true);
            if let Some((&start_id, _)) = self.entries.get_index(group.start) {
                layout_changed |= self.expanded_groups.insert(start_id);
            }
        }

        // Expand the entry so a collapsed fold doesn't hide the matched line.
        let respect_manual_folds = self.appearance.scrollback.scroll.respect_manual_folds;
        if let Some((_, entry)) = self.entries.get_index_mut(entry_idx)
            && entry.is_foldable()
            && entry.display_mode != DisplayMode::Expanded
        {
            entry.set_display_mode(DisplayMode::Expanded);
            if respect_manual_folds {
                entry.display_mode_pinned = true;
            }
            // Revealing an expanded run's head re-anchors the run; migrate the expansion key with it (same discipline as the fold path)
            self.rekey_verb_group_expansion(entry_idx);
            layout_changed = true;
        }

        // Rebuild only when the reveal changed display state, the cache is gone, or heights are stale. Holding n/N across
        // already-visible matches only moves the selection. After a real rebuild drop dirty marks so the next frame's
        // incremental path can't snap off the target.
        if layout_changed || self.layout_cache.is_none() || !self.dirty_heights.is_empty() {
            self.rebuild_layout();
            self.dirty_heights.clear();
        } else {
            // rebuild_layout would have refreshed total_height; do the cheap O(visible-range) sum here. Skips only
            // rebuild_layout's per-entry re-estimation.
            self.compute_total_height_from_cache();
        }

        // Center the entry, then nudge toward the matched line within it. The logical line maps through the entry's
        // wrapped output to its rendered-row offset, so a match below a wrapped line isn't left off screen. That offset is
        // clamped to the entry height and the result to max_offset, so the nudge can't park the view past the last entry.
        self.scroll_to_entry_center(entry_idx);
        let max_row_offset = self
            .get_cached_entry_height(entry_idx)
            .unwrap_or(0)
            .saturating_sub(1);
        let header_rows = self
            .layout_cache
            .as_ref()
            .and_then(|cache| cache.entries.get(entry_idx))
            .map_or(0, |info| u16::from(info.is_expanded_verb_header()));
        let row_offset = self
            .rendered_row_offset_within_entry(entry_idx, line_in_entry)
            .saturating_add(header_rows)
            .min(max_row_offset);
        let max_offset = self
            .total_height
            .saturating_sub(self.viewport_height as usize);
        self.scroll_offset = self
            .scroll_offset
            .saturating_add(row_offset as usize)
            .min(max_offset);

        self.bump_generation();
    }

    /// Rendered-row offset of `line_in_entry` within entry `entry_idx` at the current layout width.
    /// Maps the logical line the search index reports through the entry's word-wrapped output to the row actually painted.
    /// Reveal thus scrolls to where the match is rather than to an unwrapped estimate.
    fn rendered_row_offset_within_entry(&self, entry_idx: usize, line_in_entry: usize) -> u16 {
        if self.last_width == 0 {
            return u16::try_from(line_in_entry).unwrap_or(u16::MAX);
        }
        let Some((_, entry)) = self.entries.get_index(entry_idx) else {
            return 0;
        };
        let theme = Theme::current();
        let entry_area_width = self.entry_area_width(self.last_width);
        EntryRenderer::new(entry, &theme)
            .with_appearance_ref(&self.appearance)
            .with_cwd(self.cwd())
            .rendered_row_of_logical_line(entry_area_width, line_in_entry)
    }

    /// Handle follow mode auto-scroll (call during rendering). Only follow if current turn is "running" (no end-of-turn
    /// marker). In SingleTurn mode, only follow if viewing the running turn. In AllTurns mode, only follow if at the
    /// bottom viewing running content.
    pub fn handle_follow_mode(&mut self) {
        if !self.follow_mode {
            return;
        }

        self.follow_scroll_to_bottom();

        // Auto-select the last selectable entry when following.
        // With follow_auto_select: only move selection when it's already at the tail (tracking new content) or when nothing is selected
        // This prevents overriding the user's selection when they fold or unfold a block in the middle while follow mode is on
        let range = self.visible_entry_range();
        if !range.is_empty() {
            let last_selectable = self.find_last_selectable_in_range(range);
            let should_select = self.selected.is_none()
                || (self.appearance.scrollback.scroll.follow_auto_select
                    && self.selected == last_selectable);
            if should_select {
                // Re-query in case entries were added since we last checked
                let range = self.visible_entry_range();
                self.selected = self.find_last_selectable_in_range(range);
                self.sync_current_turn();
            }
        }
    }

    /// Re-pin the viewport to the bottom for follow mode without touching the selection. Auto-selecting the last entry
    /// there would overwrite the selection while folding, etc. Pins unconditionally; callers must only invoke it when
    /// `follow_mode` is set (both current callers gate on it).
    pub(super) fn follow_scroll_to_bottom(&mut self) {
        debug_assert!(
            self.follow_mode,
            "follow_scroll_to_bottom called outside follow mode"
        );
        // When follow_preserve_scroll is set, keep the current scroll position. No explicit invalidation is needed: any
        // user interaction (scroll, fold, turn nav) sets follow_mode=false, making this unreachable.
        if self.follow_preserve_scroll {
            // Overflow is measured against the unpadded transcript
            // The pad makes max_offset equal the pin pose, so comparing to the padded max would look like overflow on every frame and eat the pin
            let unpadded_total = self.total_height.saturating_sub(self.pin_reserve_pad);
            let unpadded_max = unpadded_total.saturating_sub(self.viewport_height as usize);
            let live_pin = if self.pin_reserve_active {
                self.pin_reserve_prompt_scroll_target()
                    .unwrap_or(self.scroll_offset)
            } else {
                self.scroll_offset
            };
            let content_changed = self.pin_reserve_active
                || self.content_generation != self.follow_preserve_content_generation;
            if content_changed && unpadded_max > live_pin && !self.pin_reserve_after_turn {
                self.follow_preserve_scroll = false;
                self.release_pin_reserve();
                self.scroll_offset = self.max_scroll_offset();
            }
            // Otherwise: all new content still fits below the prompt. Stay put.
        } else {
            // Normal follow: scroll to bottom, unconditionally
            // max_scroll_offset() is 0 when the content fits, which also heals an offset left wedged past the end by a shrink
            self.scroll_offset = self.max_scroll_offset();
        }
    }
}
