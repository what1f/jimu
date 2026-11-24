//! Conversation timeline: one entry per turn, for jump navigation UIs (`/jump` picker; the timeline sidebar builds on the same data).

use super::*;

/// Max preview length stored per timeline entry.
/// Render paths truncate further to the available width; this only bounds the snapshot.
const PREVIEW_MAX_CHARS: usize = 120;

/// One turn in the conversation timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineEntry {
    /// Turn's display ordinal (snapshot-only; not used to act on the transcript).
    pub turn_idx: usize,
    /// Stable id of the turn's `UserPrompt` entry: the jump/preview target.
    /// Resolved to an index only at the [`ScrollbackState`] boundary, so a removal (`shift_remove`) can't make a stale index target another block.
    pub prompt_entry_id: EntryId,
    /// First non-empty line of the prompt text, capped at [`PREVIEW_MAX_CHARS`] chars.
    pub preview: String,
}

/// First non-empty line, capped in chars with a `…` marker.
/// Bounded single pass: the length probe stops one char past the cap, so a huge one-line prompt costs O(cap), not O(line length).
/// The cap counts chars (not display width) on purpose: it bounds the stored snapshot, and render paths re-truncate to their width.
fn prompt_preview(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let mut out: String = line.chars().take(PREVIEW_MAX_CHARS).collect();
    if out.chars().count() == PREVIEW_MAX_CHARS && line.chars().nth(PREVIEW_MAX_CHARS).is_some() {
        out.pop();
        out.push('\u{2026}');
    }
    out
}

impl ScrollbackState {
    /// Timeline entries, one per turn in conversation order (oldest first).
    /// Each entry carries the prompt's stable [`EntryId`].
    /// Dispatch resolves it to an index at the boundary, so the snapshot stays correct across both appends and removals.
    pub fn timeline_entries(&self) -> Vec<TimelineEntry> {
        self.turns
            .iter()
            .enumerate()
            .filter_map(|(turn_idx, turn)| {
                let (id, entry) = self.entries.get_index(turn.prompt_index)?;
                let preview = match &entry.block {
                    RenderBlock::UserPrompt(block) => prompt_preview(&block.text),
                    _ => String::new(),
                };
                Some(TimelineEntry {
                    turn_idx,
                    prompt_entry_id: *id,
                    preview,
                })
            })
            .collect()
    }

    /// Preview for one turn (avoids building the whole entry list when a single hover needs it, e.g. the sidebar tick popup).
    pub fn turn_preview(&self, turn_idx: usize) -> Option<String> {
        let turn = self.turns.get(turn_idx)?;
        self.entries
            .get_index(turn.prompt_index)
            .and_then(|(_, entry)| match &entry.block {
                RenderBlock::UserPrompt(block) => Some(prompt_preview(&block.text)),
                _ => None,
            })
    }

    /// The focused turn: the last turn whose prompt is at/above the viewport top, or the first turn while pre-turn content owns the top.
    /// `None` only when there are no turns or no layout.
    /// Trailing turns short enough to never own the top row never become active; they're fully on screen when it matters.
    pub fn active_turn_for_viewport(&self) -> Option<usize> {
        if self.view_mode == ViewMode::SingleTurn {
            return self.current_turn;
        }
        if self.turns.is_empty() {
            return None;
        }
        Some(self.prompts_above_top(false)?.saturating_sub(1))
    }

    /// The nearest turn an upward scroll can land on: the last turn whose prompt is STRICTLY above the viewport top, `None` when nothing is above.
    /// The ▲ chevron steps here rather than `active - 1`: from mid-turn it first aligns the current turn's own prompt, like the h key.
    /// It can never target a trailing turn that no scroll reaches (the stuck-▲ bug).
    pub fn turn_above_viewport_top(&self) -> Option<usize> {
        if self.view_mode == ViewMode::SingleTurn {
            return self.current_turn?.checked_sub(1);
        }
        self.prompts_above_top(true)?.checked_sub(1)
    }

    /// The nearest turn below the viewport top.
    /// Before the first prompt, this is the first turn; on a prompt row, it is the following turn.
    pub fn turn_below_viewport_top(&self) -> Option<usize> {
        if self.view_mode == ViewMode::SingleTurn {
            let next = self.current_turn?.checked_add(1)?;
            return (next < self.turns.len()).then_some(next);
        }
        let next = self.prompts_above_top(false)?;
        (next < self.turns.len()).then_some(next)
    }

    /// Count of turns whose prompt row is above the viewport top (`strict`: strictly above; else at-or-above).
    /// Prompt rows are monotone in turn order, so this is a partition point over cached `virtual_y`.
    fn prompts_above_top(&self, strict: bool) -> Option<usize> {
        let cache = self.layout_cache.as_ref()?;
        let range = self.visible_entry_range();
        let base = *cache.virtual_y.get(range.start)?;
        let top = base + self.scroll_offset;
        Some(self.turns.partition_point(|turn| {
            cache
                .virtual_y
                .get(turn.prompt_index)
                .is_some_and(|&prompt_y| {
                    if strict {
                        prompt_y < top
                    } else {
                        prompt_y <= top
                    }
                })
        }))
    }
}
