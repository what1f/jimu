//! Bottom padding, held for one turn, that makes the page-flip pose (the last user prompt at the top of the viewport) a real scroll bottom.
//! Without the pad, clamping the scroll offset to the content height would jump the view to the tail.

use super::ScrollbackState;

impl ScrollbackState {
    pub(super) fn arm_pin_reserve(&mut self) {
        self.pin_reserve_active = true;
        self.pin_reserve_after_turn = false;
        // Without a target, the release checks do nothing until positioning captures the pose
        self.pin_reserve_target = None;
        self.pin_reserve_prompt_id = None;
    }

    /// The armed turn is over, so height changes at finish and the "Worked for…" marker must not consume follow-preserve after this point.
    pub fn note_pin_reserve_turn_finished(&mut self) {
        if self.pin_reserve_active {
            self.pin_reserve_after_turn = true;
        }
    }

    fn pin_reserve_scroll_target(&self) -> Option<usize> {
        match (
            self.pin_reserve_target,
            self.pin_reserve_prompt_scroll_target(),
        ) {
            (Some(captured), Some(live)) => Some(captured.max(live)),
            (captured, live) => captured.or(live),
        }
    }

    /// Shift the captured pin pose by any height change ABOVE the pinned prompt.
    /// Growth above moves the prompt's virtual_y, so the captured offset (relative to the visible range top) must move with it.
    /// Changes at or below the prompt leave its offset put, so this is a no-op for ordinary streaming, where the response grows below the prompt.
    pub(super) fn shift_pin_reserve_target_for_changes(&mut self, changes: &[(usize, i32)]) {
        if !self.pin_reserve_active {
            return;
        }
        let Some(target) = self.pin_reserve_target else {
            return;
        };
        let Some(prompt_idx) = self.pin_reserve_prompt_index() else {
            return;
        };
        let start = self.visible_entry_range().start;
        let above: i64 = changes
            .iter()
            .filter(|&&(idx, _)| idx >= start && idx < prompt_idx)
            .map(|&(_, d)| d as i64)
            .sum();
        if above != 0 {
            let shifted = (target as i64 + above).max(0) as usize;
            self.pin_reserve_target = Some(shifted);
            if self.follow_mode && self.follow_preserve_scroll {
                self.scroll_offset = (self.scroll_offset as i64 + above).max(0) as usize;
            }
        }
    }

    /// Shift the captured pose by a coordinate change in the retained layout cache.
    pub(super) fn shift_pin_reserve_target_for_layout(
        &mut self,
        before: Option<usize>,
        after: Option<usize>,
    ) {
        if !self.pin_reserve_active {
            return;
        }
        let (Some(before), Some(after), Some(target)) = (before, after, self.pin_reserve_target)
        else {
            return;
        };
        let delta = after as i64 - before as i64;
        self.pin_reserve_target = Some((target as i64 + delta).max(0) as usize);
        if self.follow_mode && self.follow_preserve_scroll {
            self.scroll_offset = (self.scroll_offset as i64 + delta).max(0) as usize;
        }
    }

    /// Replace the captured pose after changing to a different view coordinate space.
    pub(super) fn reset_pin_reserve_target(&mut self) {
        if !self.pin_reserve_active {
            return;
        }
        let Some(prompt_idx) = self.pin_reserve_prompt_index() else {
            return;
        };
        let range = self.visible_entry_range();
        if !range.contains(&prompt_idx) || self.last_width == 0 {
            return;
        }
        self.measure_span_and_rebuild(range.start, prompt_idx, self.last_width);
        if let Some(target) = self.pin_reserve_prompt_scroll_target() {
            self.pin_reserve_target = Some(target);
        }
    }

    /// Synchronize the owned page-flip pose after a structural rebuild.
    pub(super) fn settle_pin_reserve_target(&mut self) {
        if !self.pin_reserve_active {
            return;
        }
        for _ in 0..2 {
            let Some(target) = self.pin_reserve_prompt_scroll_target() else {
                return;
            };
            self.pin_reserve_target = Some(target);
            if self.follow_mode && self.follow_preserve_scroll {
                self.scroll_offset = target;
            }
            self.compute_total_height_from_cache();
        }
        if !self.follow_mode {
            self.scroll_offset = self.scroll_offset.min(self.max_scroll_offset());
        }
    }

    /// Drop reserve ownership when its captured prompt is outside the current visible slice.
    pub(super) fn release_pin_reserve_outside_view(&mut self) {
        if !self.pin_reserve_active {
            return;
        }
        let outside = self
            .pin_reserve_prompt_index()
            .is_none_or(|idx| !self.visible_entry_range().contains(&idx));
        if outside {
            self.follow_preserve_scroll = false;
            self.release_pin_reserve();
            self.scroll_offset = self.scroll_offset.min(self.max_scroll_offset());
        }
    }

    /// Clear the reserve's flags, target, and prompt id without changing scroll totals.
    pub(super) fn clear_pin_reserve(&mut self) {
        self.pin_reserve_active = false;
        self.pin_reserve_target = None;
        self.pin_reserve_prompt_id = None;
        self.pin_reserve_after_turn = false;
    }

    /// Index of the prompt the pin targets.
    /// Resolves the stable id captured at arm time so a mid-turn interjection cannot move it.
    /// Falls back to the last user prompt only when no id is stored (e.g. when a resize re-derives the target).
    pub(super) fn pin_reserve_prompt_index(&self) -> Option<usize> {
        match self.pin_reserve_prompt_id {
            Some(id) => self.entries.get_index_of(&id),
            None => self.last_user_prompt_index(),
        }
    }

    /// Release the reserve before an explicit bottom gesture resolves the real tail.
    pub(super) fn release_pin_reserve(&mut self) {
        if !self.pin_reserve_active && self.pin_reserve_pad == 0 {
            return;
        }
        self.clear_pin_reserve();
        if self.layout_cache.is_some() {
            self.compute_total_height_from_cache();
        } else {
            self.total_height = self.total_height.saturating_sub(self.pin_reserve_pad);
            self.pin_reserve_pad = 0;
        }
    }

    pub(super) fn pin_reserve_pad_rows(&self, content_height: usize) -> usize {
        if !self.pin_reserve_active || self.viewport_height == 0 {
            return 0;
        }
        let Some(target) = self.pin_reserve_scroll_target() else {
            return 0;
        };
        // max_offset = content + pad - viewport must be at least `target` so the last user prompt can sit at the top
        target
            .saturating_add(self.viewport_height as usize)
            .saturating_sub(content_height)
    }

    /// Scroll target of the prompt the pin is armed for, adjusted for sticky headers.
    /// Resolving by the captured id keeps a mid-turn interjection or a resize re-derive from retargeting the pad to a later prompt.
    /// Falls back to the last user prompt only when no id is stored.
    pub(super) fn pin_reserve_prompt_scroll_target(&self) -> Option<usize> {
        let idx = self.pin_reserve_prompt_index()?;
        let cache = self.layout_cache.as_ref()?;
        let range = self.visible_entry_range();
        if !range.contains(&idx) {
            return None;
        }
        let base = *cache.virtual_y.get(range.start)?;
        let y = *cache.virtual_y.get(idx)?;
        let entry_y = y.saturating_sub(base);
        // Same sticky-header fixed point as `scroll_to_entry_top`
        // Raw `entry_y` would over-state the pad whenever an earlier prompt is still sticky
        // That would make max_offset > scroll_offset and consume follow-preserve on the next frame
        Some(self.sticky_adjusted_entry_top(cache, &range, entry_y))
    }

    fn last_user_prompt_index(&self) -> Option<usize> {
        if let Some(idx) = self.turns.last().map(|turn| turn.prompt_index)
            && self
                .entries
                .get_index(idx)
                .is_some_and(|(_, entry)| entry.block.is_user_prompt())
        {
            return Some(idx);
        }
        self.entries
            .iter()
            .enumerate()
            .rev()
            .find_map(|(idx, (_, entry))| entry.block.is_user_prompt().then_some(idx))
    }
}
