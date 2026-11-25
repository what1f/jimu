//! Sticky header computation for AllTurns view.
//!
//! This module handles the "iOS-style" sticky section headers where prompts stick to the top of the viewport when scrolled past.
//! They get pushed off when the next prompt approaches.
//!
//! The algorithm is purely computational (1D coordinate math) and can be tested independently of any rendering logic.
//!
//! # Layout Model
//!
//! The layout works entirely with **total heights**: it doesn't know or care about internal block structure (vpads, content lines, ellipsis, etc.).
//!
//! Each prompt has:
//! - `full_height`: Total rows when rendered inline in the timeline
//! - `min_height`: Minimum rows when fully collapsed as sticky header
//!
//! The layout computes `render_height` (between min and full) and `clip_top`.
//! The block renderer receives this height budget and decides internally how to allocate it (vpads, content, truncation indicators, etc.).

/// Describes a prompt entry for sticky header computation.
///
/// Prompts are "section headers" in the conversation: they mark the start of each turn and can be pinned to the top when scrolled past.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptDescriptor {
    /// Index of the prompt entry in the entries list.
    pub entry_idx: usize,

    /// Y position in virtual scroll space.
    pub y_virtual: usize,

    /// Total height when rendered inline in the timeline.
    /// This is the FULL height including all padding, borders, content, etc.
    pub full_height: u16,

    /// Minimum height when fully collapsed as a sticky header.
    /// The block should still be recognizable at this size.
    /// Typically 3-4 rows (enough for padding + 1-2 content lines + ellipsis).
    pub min_height: u16,

    /// Whether this prompt should stick when scrolled past.
    /// Non-sticky prompts still participate in push calculations but never become pinned themselves.
    /// (They push the previous sticky prompt off screen.)
    pub sticky: bool,
}

pub const MIN_PINNED_HEIGHT: u16 = 4;

/// A prompt to be rendered in the sticky header area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderedPrompt {
    /// Entry index of the prompt.
    pub entry_idx: usize,

    /// Total height budget for rendering this prompt. This is the FULL height the block should render to, including any
    /// internal padding, content, ellipsis, etc. The block decides how to allocate this space internally. Range:
    /// `min_height <= render_height <= full_height`.
    pub render_height: u16,

    /// Rows clipped from the TOP (for push effect ONLY).
    pub clip_top: u16,
}

impl RenderedPrompt {
    /// Number of rows actually visible on screen after clipping.
    #[inline]
    pub fn visible_height(&self) -> u16 {
        self.render_height.saturating_sub(self.clip_top)
    }

    /// Whether this prompt needs a scratch buffer for rendering.
    /// True when clip_top > 0 (we need to render full then copy partial).
    #[inline]
    pub fn needs_scratch_buffer(&self) -> bool {
        self.clip_top > 0
    }
}

/// Result of computing sticky header layout for AllTurns view. This describes what should be rendered in the sticky
/// header area at the top of the viewport. The content area starts after `header_screen_rows()` rows. All 1D layout
/// math is encapsulated here; the renderer just asks for screen positions and scroll offsets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StickyHeaderLayout {
    /// The prompt being pushed off screen (clipped at top).
    /// Only present during push transition when next prompt is approaching.
    pub pushed: Option<RenderedPrompt>,

    /// The main pinned prompt (rendered fully unless viewport is tiny).
    /// None only at the very start of timeline (no prompts scrolled past).
    pub pinned: Option<RenderedPrompt>,
}

/// Gap row between header and content for visual separation.
const HEADER_CONTENT_GAP: u16 = 1;

impl StickyHeaderLayout {
    /// Height of just the header content (pushed + pinned + gap between them).
    /// Does NOT include the gap after the header.
    fn header_content_height(&self) -> u16 {
        let pushed_visible = self.pushed.map_or(0, |p| p.visible_height());
        let pinned_visible = self.pinned.map_or(0, |p| p.visible_height());
        let gap_between = if pushed_visible > 0 && pinned_visible > 0 {
            1
        } else {
            0
        };
        pushed_visible + gap_between + pinned_visible
    }

    /// Total rows the header occupies on screen, including gap after. During push transition (only pushed, no pinned),
    /// there's NO gap after the header.
    pub fn header_screen_rows(&self) -> u16 {
        if !self.has_header() {
            return 0;
        }

        // During push (only pushed, no pinned), no gap after header.
        // This keeps scroll_for_content constant, ensuring smooth scrolling.
        if self.pushed.is_some() && self.pinned.is_none() {
            return self.header_content_height(); // No gap
        }

        // Pinned header: add gap after for visual separation
        self.header_content_height() + HEADER_CONTENT_GAP
    }

    /// Row where content rendering should start (0-indexed from viewport top).
    /// Alias for `header_screen_rows()`.
    #[inline]
    pub fn content_start_row(&self) -> u16 {
        self.header_screen_rows()
    }

    /// Height of the content area given viewport height.
    #[inline]
    pub fn content_height(&self, viewport_height: u16) -> u16 {
        viewport_height.saturating_sub(self.header_screen_rows())
    }

    /// Each c-j/c-k must move the bottom line by exactly one row: `bottom_line = scroll_offset + viewport_height - 1`.
    /// With a sticky header of height H, content scroll is `scroll_offset + H`, so a 1-row header shrink cancels a 1-row scroll and the bottom line still advances by one.
    #[inline]
    pub fn scroll_for_content(&self, scroll_offset: usize) -> usize {
        scroll_offset + self.header_screen_rows() as usize
    }

    /// Whether any sticky header is present.
    #[inline]
    pub fn has_header(&self) -> bool {
        self.pushed.is_some() || self.pinned.is_some()
    }

    /// Entry index of the pinned prompt (if any).
    /// Used to avoid drawing duplicate selection on pinned entry.
    #[inline]
    pub fn pinned_entry_idx(&self) -> Option<usize> {
        self.pinned.as_ref().map(|p| p.entry_idx)
    }

    /// Screen row where the pinned header starts (for selection drawing).
    /// Returns None if no pinned header.
    pub fn pinned_screen_row(&self) -> Option<u16> {
        self.pinned?;
        let pushed_visible = self.pushed.map_or(0, |p| p.visible_height());
        let gap_after_pushed = if pushed_visible > 0 { 1 } else { 0 };
        Some(pushed_visible + gap_after_pushed)
    }

    /// Screen row of the gap after a pinned header (selection corners, the ▲ response-top indicator).
    /// `None` without a pinned header.
    /// A push-only header renders no gap after it (see [`Self::header_screen_rows`]), so this row would point at content.
    pub fn gap_row(&self) -> Option<u16> {
        self.pinned?;
        Some(self.header_content_height())
    }

    /// Screen row where pushed header starts (always 0 if present).
    pub fn pushed_screen_row(&self) -> Option<u16> {
        self.pushed.as_ref().map(|_| 0)
    }

    /// Screen row where the gap between pushed and pinned is (if both present).
    pub fn gap_between_row(&self) -> Option<u16> {
        if let (Some(pushed), Some(_)) = (&self.pushed, &self.pinned) {
            Some(pushed.visible_height())
        } else {
            None
        }
    }

    /// Map a header-relative screen row to the entry index of the prompt rendered there.
    /// Returns `None` if the row falls on a gap or outside the header area.
    /// `row` is relative to the top of the scrollback area (0-indexed).
    pub fn entry_at_header_row(&self, row: u16) -> Option<usize> {
        if !self.has_header() || row >= self.header_screen_rows() {
            return None;
        }

        // Check pushed prompt area (always starts at row 0)
        if let Some(ref pushed) = self.pushed
            && row < pushed.visible_height()
        {
            return Some(pushed.entry_idx);
        }

        // Check pinned prompt area
        if let Some(ref pinned) = self.pinned {
            let pinned_start = self.pinned_screen_row().unwrap_or(0);
            let pinned_end = pinned_start + pinned.visible_height();
            if row >= pinned_start && row < pinned_end {
                return Some(pinned.entry_idx);
            }
        }

        // In a gap (between pushed and pinned, or after header content)
        None
    }

    /// Get the screen area of a header prompt (pushed or pinned) if it matches `entry_idx`.
    /// Returns `(start_row, visible_height, is_pushed)` relative to scrollback area top.
    /// `is_pushed` is true for the disappearing prompt (fading away).
    pub fn header_entry_area(&self, entry_idx: usize) -> Option<(u16, u16, bool)> {
        if let Some(ref pushed) = self.pushed
            && pushed.entry_idx == entry_idx
        {
            let visible = pushed.visible_height();
            if visible > 0 {
                return Some((0, visible, true));
            }
        }
        if let Some(ref pinned) = self.pinned
            && pinned.entry_idx == entry_idx
        {
            let visible = pinned.visible_height();
            if visible > 0 {
                let start = self.pinned_screen_row().unwrap_or(0);
                return Some((start, visible, false));
            }
        }
        None
    }
}

/// Prompts must be sorted by `y_virtual` ascending.
/// Sticky header is the last prompt scrolled past; render height shrinks toward `min_height` and the next prompt clips from the top.
pub fn compute_sticky_layout(
    scroll_offset: usize,
    viewport_height: u16,
    prompts: &[PromptDescriptor],
) -> StickyHeaderLayout {
    // No prompts or no scroll means no pinning
    if prompts.is_empty() || scroll_offset == 0 {
        return StickyHeaderLayout::default();
    }

    // Find the last sticky prompt that's been scrolled past (y_virtual < scroll_offset).
    // Non-sticky prompts (e.g. expanded user prompts) are skipped for pinning but still participate in push calculations below.
    let pinned_idx = match prompts
        .iter()
        .rposition(|p| p.sticky && p.y_virtual < scroll_offset)
    {
        Some(idx) => idx,
        None => {
            // No sticky prompts have been scrolled past yet
            return StickyHeaderLayout::default();
        }
    };

    let Some(pinned_prompt) = prompts.get(pinned_idx) else {
        return StickyHeaderLayout::default();
    };

    // As we scroll past, the pinned prompt shrinks (gradual collapse)
    let render_height = calculate_render_height(pinned_prompt, scroll_offset, viewport_height);

    // Check if next prompt is pushing
    let next_prompt_info = if let Some(next) = prompts.get(pinned_idx + 1) {
        let next_naive_row = next.y_virtual.saturating_sub(scroll_offset);

        // Push starts when next prompt would overlap with current header + gap.
        let header_with_gap = render_height + HEADER_CONTENT_GAP;
        if next_naive_row <= header_with_gap as usize {
            Some((next, next_naive_row))
        } else {
            None
        }
    } else {
        None
    };

    match next_prompt_info {
        Some((_next_prompt, next_naive_row)) => {
            // During push transition: the next prompt is approaching from below
            if next_naive_row == 0 {
                // Next prompt is at row 0 (its inline position); no header needed
                // The next prompt takes over the sticky position.
                return StickyHeaderLayout::default();
            }

            // The current (pinned) header is being pushed off as the next prompt approaches. If next_naive_row == 1, that
            // means only the gap row is visible (row 0).
            let pushed_visible = (next_naive_row as u16).saturating_sub(1);

            if pushed_visible == 0 {
                // Only the gap row is visible, none of the pinned prompt's content. No header needed.
                return StickyHeaderLayout::default();
            }

            // For pushed headers, use min(full_height, render_height):
            // - If full_height < render_height: use full_height (don't inflate small prompts with empty padding that we'd then clip into)
            // - If full_height >= render_height: use render_height (keep the collapsed view with proper truncation/ellipsis)
            let pushed_render_height = pinned_prompt.full_height.min(render_height);
            let push_clip = pushed_render_height.saturating_sub(pushed_visible);

            StickyHeaderLayout {
                pushed: Some(RenderedPrompt {
                    entry_idx: pinned_prompt.entry_idx,
                    render_height: pushed_render_height,
                    clip_top: push_clip,
                }),
                pinned: None, // Next prompt stays inline, not pinned
            }
        }
        None => {
            // Normal pinning, no push happening
            StickyHeaderLayout {
                pushed: None,
                pinned: Some(RenderedPrompt {
                    entry_idx: pinned_prompt.entry_idx,
                    render_height,
                    clip_top: 0,
                }),
            }
        }
    }
}

/// Calculate render height for a prompt in sticky header. The shrinking rate matches the scroll rate (1 row per
/// scroll), maintaining bottom line continuity. render_height decreases by 1. header shrinks by 1 row. content_area
/// height increases by 1 row. So bottom_line increases by 1.
fn calculate_render_height(
    prompt: &PromptDescriptor,
    scroll_offset: usize,
    viewport_height: u16,
) -> u16 {
    // How many rows have scrolled past the prompt's top
    // `scroll_offset` is a cumulative usize position (can exceed u16 in long sessions), so clamp before narrowing
    // This only feeds `full_height.saturating_sub(..)` (a u16), so any value past u16::MAX collapses the header to min height anyway
    let scroll_past = scroll_offset
        .saturating_sub(prompt.y_virtual)
        .min(u16::MAX as usize) as u16;

    // Height shrinks 1:1 with scroll_past until we hit minimum
    let height = prompt.full_height.saturating_sub(scroll_past);

    // A collapsed sticky header can never be taller than the prompt rendered inline. A lazily-estimated prompt can
    // keep the `MAX_TRUNCATED_HEADER_HEIGHT` seed, never measured because it sits above the viewport when pinned.
    // Without this clamp it would pad a short pinned prompt with empty rows instead of collapsing to its real height.
    let min_height = prompt.min_height.max(1).min(prompt.full_height.max(1));

    height.max(min_height).min(viewport_height)
}
