// Adapted from the Grok Build pager (Apache-2.0). Copyright 2023-2026 SpaceXAI.

//! Question view state and helpers.
//!
//! When the agent calls `AskUserQuestion`, the pager takes over the prompt
//! area and shows a structured question UI. This module contains:
//!
//! - [`QuestionViewState`]: all state for the question overlay
//! - [`QuestionSelection`]: per-question selection tracking
//! - [`QuestionFocus`]: navigation vs input mode
//!
//! No rendering or input handling here; this is pure data and helpers.

use std::collections::HashSet;
use std::time::Instant;

use crate::engine::markdown::StreamingMarkdownRenderer;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use unicode_width::UnicodeWidthStr;

use super::support::markdown_syntax;
use crate::engine::input::key::RowWalk;
use crate::engine::render::line_utils::{byte_offset_at_width, truncate_line, truncate_str};
use crate::engine::render::wrapping::word_wrap_lines_with_joiners;
use crate::engine::theme::Theme;
use crate::engine::theme::md_style;
use crate::ui::views::prompt_widget::StashedPrompt;

/// Maximum description lines shown in the question chrome before truncation.
const DEFAULT_MAX_CHROME_DESC_LINES: u16 = 5;

/// Maximum preview lines shown in the question chrome before truncation.
const DEFAULT_MAX_CHROME_PREVIEW_LINES: u16 = 6;

/// Minimum number of option rows that must be visible before dynamic cap reduction kicks in.
const MIN_VISIBLE_OPTION_ROWS: u16 = 3;

fn hovered_bg(theme: &Theme) -> ratatui::style::Color {
    theme.bg_hover
}

/// Per-question selection state.
#[derive(Debug, Clone)]
pub enum QuestionSelection {
    /// Single-choice: at most one option selected.
    Single(Option<usize>),
    /// Multi-choice: zero or more options toggled on.
    Multi(HashSet<usize>),
}

/// A cursor move within one question's answer rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorMotion {
    Next,
    Prev,
    HalfPageDown,
    HalfPageUp,
    PageDown,
    PageUp,
    First,
    Last,
}

/// Focus mode within the question view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionFocus {
    /// Cursor is on an option row or the free-form row (not typing).
    /// j/k navigate, Enter selects or enters input mode.
    Navigation,
    /// User is typing in the TextArea (input mode).
    /// @-dropdown may be open. Esc exits back to Navigation.
    InputMode,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
    pub preview: Option<String>,
    pub id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Question {
    pub question: String,
    pub options: Vec<QuestionOption>,
    pub multi_select: Option<bool>,
    pub id: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AskUserQuestionMode {
    #[default]
    Default,
    Plan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionResponse {
    pub index: usize,
    pub id: Option<String>,
    pub labels: Vec<String>,
    pub preview: Option<String>,
    pub notes: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub struct QuestionLabels {
    pub freeform: &'static str,
    pub expand: &'static str,
}

impl Default for QuestionLabels {
    fn default() -> Self {
        Self {
            freeform: "Type your answer here",
            expand: " to expand",
        }
    }
}

pub struct QuestionViewState {
    pub labels: QuestionLabels,
    last_option_click: Option<(Instant, usize)>,
    scrollbar_dragging: bool,
    /// The tool call ID of the `AskUserQuestion` invocation.
    pub tool_call_id: String,
    /// The questions to present.
    pub questions: Vec<Question>,
    /// Which question is currently shown (0-based index).
    pub active_tab: usize,
    /// Per-question selection state (same length as `questions`).
    pub selections: Vec<QuestionSelection>,
    /// Current focus mode.
    pub focus: QuestionFocus,
    /// Whether fullscreen mode is active (removes height cap).
    pub fullscreen: bool,
    /// Original prompt state, stashed on entry and restored on exit.
    pub stashed_prompt: StashedPrompt,

    /// Cursor position per question (index into options plus the freeform row).
    pub per_question_cursor: Vec<usize>,
    /// Scroll offset (visual lines) per question.
    pub per_question_scroll: Vec<u16>,
    /// Per-question freeform text (additional context).
    /// Each question has its own text so switching tabs doesn't mix content.
    pub per_question_freeform: Vec<String>,
    /// Whether the per-question freeform answer is "selected" (included in submission).
    /// Toggled by Space, auto-set when exiting InputMode with text.
    /// Independent of the text content; text is preserved on untoggle.
    pub per_question_freeform_selected: Vec<bool>,

    // Recomputed on resize / question switch.
    /// Cached cap on description lines in chrome (capped in non-fullscreen).
    pub cached_desc_cap: u16,
    /// Cached cap on preview lines in chrome (capped in non-fullscreen).
    pub cached_preview_cap: u16,

    pub mode: AskUserQuestionMode,
    pub bottom_panel_index: Option<usize>,
    pub prompt_blocked: bool,
    /// When this question view was created. Used to pause the turn timer while the user is answering questions.
    /// The time spent in the question view is subtracted from the turn elapsed display.
    pub opened_at: Instant,
    /// Wall-clock twin of `opened_at` (UTC ms).
    /// `Instant` does not advance across a suspend, so a pause netted against the wall-anchored turn span must itself be measured on the wall clock.
    /// Otherwise a suspend during an open question would read as worked time.
    pub opened_at_wall_ms: i64,
    /// When `true`, the freeform "Other" input row is hidden.
    /// Used by locally-driven questions (e.g. the credit-limit upsell) that only offer fixed options with no free-text fallback.
    pub no_freeform: bool,
}

impl QuestionViewState {
    /// Initializes per-question vectors (selections, cursors, scroll) based on each question's type (single vs multi-select).
    pub fn new(
        tool_call_id: String,
        questions: Vec<Question>,
        stashed_prompt: StashedPrompt,
    ) -> Self {
        let n = questions.len();
        let selections: Vec<QuestionSelection> = questions
            .iter()
            .map(|q| {
                if q.multi_select.unwrap_or(false) {
                    QuestionSelection::Multi(HashSet::new())
                } else {
                    QuestionSelection::Single(None)
                }
            })
            .collect();

        Self {
            labels: QuestionLabels::default(),
            last_option_click: None,
            scrollbar_dragging: false,
            tool_call_id,
            questions,
            active_tab: 0,
            selections,
            focus: QuestionFocus::Navigation,
            fullscreen: false,
            stashed_prompt,
            per_question_cursor: vec![0; n],
            per_question_scroll: vec![0; n],
            per_question_freeform: vec![String::new(); n],
            per_question_freeform_selected: vec![false; n],
            cached_desc_cap: DEFAULT_MAX_CHROME_DESC_LINES,
            cached_preview_cap: DEFAULT_MAX_CHROME_PREVIEW_LINES,
            mode: AskUserQuestionMode::Default,
            bottom_panel_index: None,
            prompt_blocked: false,
            opened_at: Instant::now(),
            opened_at_wall_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX)),
            no_freeform: false,
        }
    }

    /// Builder-style helper to hide the freeform "Other" input row.
    pub fn with_no_freeform(mut self) -> Self {
        self.no_freeform = true;
        self
    }

    /// Number of items for a given question: options plus 1 free-form row (unless `no_freeform` is set).
    pub fn total_items(&self, question_idx: usize) -> usize {
        let freeform = if self.no_freeform { 0 } else { 1 };
        self.questions
            .get(question_idx)
            .map(|q| q.options.len() + freeform)
            .unwrap_or(1)
    }

    /// Whether the cursor is on the free-form row for the active question.
    pub fn is_on_freeform_row(&self) -> bool {
        if self.no_freeform {
            return false;
        }
        let idx = self.active_tab;
        self.cursor()
            == self
                .questions
                .get(idx)
                .map(|q| q.options.len())
                .unwrap_or(0)
    }

    /// Current cursor position for the active question.
    pub fn cursor(&self) -> usize {
        self.per_question_cursor
            .get(self.active_tab)
            .copied()
            .unwrap_or(0)
    }

    /// Move the cursor within the active question, clamped at both ends.
    pub fn move_cursor(&mut self, motion: CursorMotion) {
        let last = self.total_items(self.active_tab).saturating_sub(1);
        let cursor = self.cursor();
        let target = match motion {
            CursorMotion::Next => cursor + 1,
            CursorMotion::Prev => cursor.saturating_sub(1),
            CursorMotion::HalfPageDown => cursor + (last / 2).max(1),
            CursorMotion::HalfPageUp => cursor.saturating_sub((last.max(1) / 2).max(1)),
            CursorMotion::PageDown => cursor + last.max(1),
            CursorMotion::PageUp => cursor.saturating_sub(last.max(1)),
            CursorMotion::First => 0,
            CursorMotion::Last => last,
        };
        self.set_cursor(target.min(last));
    }

    /// Walk one answer row of the active question, wrapping at both ends.
    pub fn walk_cursor(&mut self, walk: RowWalk) {
        let target = walk.step(self.cursor(), self.total_items(self.active_tab));
        self.set_cursor(target);
    }

    pub fn clear_selection(&mut self, q_idx: usize) {
        match self.selections.get_mut(q_idx) {
            Some(QuestionSelection::Multi(selected)) => selected.clear(),
            Some(QuestionSelection::Single(selected)) => *selected = None,
            None => {}
        }
        if let Some(freeform_selected) = self.per_question_freeform_selected.get_mut(q_idx) {
            *freeform_selected = false;
        }
    }

    /// Set cursor position for the active question, clamped to valid range.
    pub fn set_cursor(&mut self, pos: usize) {
        let max = self.total_items(self.active_tab).saturating_sub(1);
        let clamped = pos.min(max);
        if let Some(c) = self.per_question_cursor.get_mut(self.active_tab) {
            *c = clamped;
        }
    }

    /// Adjust scroll so the cursor row is visible within `visible_h` lines. Call this after every
    /// cursor change. `content_w` is needed to compute per-option visual heights for stacked layout.
    pub fn ensure_cursor_visible(&mut self, visible_h: u16, content_w: usize) {
        let q_idx = self.active_tab;
        let Some(question) = self.questions.get(q_idx) else {
            return;
        };

        let cursor = self.cursor();
        let heights = option_heights(question, content_w, cursor);
        let scroll = self.per_question_scroll.get(q_idx).copied().unwrap_or(0);

        // Compute the visual Y range of the cursor item.
        let cursor_top: u16 = heights.get(..cursor).unwrap_or(&[]).iter().sum();
        let cursor_bottom = cursor_top + heights.get(cursor).copied().unwrap_or(1);

        let mut new_scroll = scroll;

        // If cursor is above the visible window, scroll up.
        if cursor_top < new_scroll {
            new_scroll = cursor_top;
        }

        // If cursor is below the visible window, scroll down.
        if cursor_bottom > new_scroll + visible_h {
            new_scroll = cursor_bottom.saturating_sub(visible_h);
        }

        if let Some(s) = self.per_question_scroll.get_mut(q_idx) {
            *s = new_scroll;
        }
    }

    /// Clamp the active question's scroll offset to the current viewport.
    pub fn clamp_scroll(&mut self, visible_h: u16, content_w: usize) {
        let q_idx = self.active_tab;
        let Some(question) = self.questions.get(q_idx) else {
            return;
        };

        let max_scroll = total_options_height(question, content_w, self.cursor())
            .saturating_sub(self.phantom_freeform_h())
            .saturating_sub(visible_h);
        if let Some(s) = self.per_question_scroll.get_mut(q_idx) {
            *s = (*s).min(max_scroll);
        }
    }

    /// Height of the freeform line that [`option_heights`] and [`total_options_height`] always include.
    /// The line is never rendered when `no_freeform` is set; subtract this from those totals wherever they feed layout or scroll limits.
    pub fn phantom_freeform_h(&self) -> u16 {
        if self.no_freeform { 1 } else { 0 }
    }
}

/// Visual heights for each item in a question: all options, then freeform.
pub fn option_heights(question: &Question, content_w: usize, cursor: usize) -> Vec<u16> {
    let prefix_w = option_prefix_w(question);
    let max_lw = compute_max_label_w(&question.options, content_w);

    question
        .options
        .iter()
        .enumerate()
        .map(|(i, o)| option_visual_height(o, content_w, prefix_w, max_lw, i == cursor))
        .chain(std::iter::once(1u16))
        .collect()
}

/// Total visual height of all option rows plus the freeform row.
pub fn total_options_height(question: &Question, content_w: usize, cursor: usize) -> u16 {
    option_heights(question, content_w, cursor)
        .into_iter()
        .sum()
}

/// Map a visual line offset within the scrolled options list to an item index.
pub fn item_index_at_visual_line(
    question: &Question,
    content_w: usize,
    visual_line: u16,
    cursor: usize,
) -> usize {
    let heights = option_heights(question, content_w, cursor);
    let mut top = 0u16;
    for (idx, height) in heights.iter().copied().enumerate() {
        if visual_line < top + height {
            return idx;
        }
        top += height;
    }
    heights.len().saturating_sub(1)
}

pub fn item_top_offset(
    question: &Question,
    content_w: usize,
    item_index: usize,
    cursor: usize,
) -> u16 {
    let heights = option_heights(question, content_w, cursor);
    let clamped = item_index.min(heights.len());
    heights.get(..clamped).unwrap_or(&[]).iter().sum()
}

pub fn scroll_offset_for_item_delta(
    question: &Question,
    content_w: usize,
    current_scroll: u16,
    delta: i32,
    viewport_height: u16,
    cursor: usize,
    phantom_freeform_h: u16,
) -> u16 {
    // Line-based scrolling: add delta directly to the scroll offset, clamped to [0, max_scroll]. Each
    // visual line (including wrapped description lines) is independent, so we scroll by individual
    // lines instead of jumping whole items.
    let max_scroll = total_options_height(question, content_w, cursor)
        .saturating_sub(phantom_freeform_h)
        .saturating_sub(viewport_height);
    ((current_scroll as i32) + delta).clamp(0, max_scroll as i32) as u16
}

/// Visible option rows height within a rendered question area.
pub fn visible_options_height(
    question: &Question,
    area_height: u16,
    content_w: usize,
    preview: Option<&str>,
    fullscreen: bool,
    desc_cap: u16,
    preview_cap: u16,
) -> u16 {
    area_height.saturating_sub(chrome_height(
        question,
        content_w,
        preview,
        fullscreen,
        desc_cap,
        preview_cap,
    ))
}

/// Maximum scroll offset for the option rows in a rendered question area.
#[allow(clippy::too_many_arguments)]
pub fn max_scroll_offset(
    question: &Question,
    area_height: u16,
    content_w: usize,
    preview: Option<&str>,
    fullscreen: bool,
    desc_cap: u16,
    preview_cap: u16,
    cursor: usize,
) -> u16 {
    total_options_height(question, content_w, cursor).saturating_sub(visible_options_height(
        question,
        area_height,
        content_w,
        preview,
        fullscreen,
        desc_cap,
        preview_cap,
    ))
}

/// Resolve the item index for a screen row within the rendered question area.
#[allow(clippy::too_many_arguments)]
pub fn item_index_at_screen_row(
    question: &Question,
    area: Rect,
    content_w: usize,
    scroll: u16,
    row: u16,
    preview: Option<&str>,
    fullscreen: bool,
    desc_cap: u16,
    preview_cap: u16,
    cursor: usize,
) -> Option<usize> {
    let options_start_y = area.y
        + chrome_height(
            question,
            content_w,
            preview,
            fullscreen,
            desc_cap,
            preview_cap,
        );
    let options_end_y = area.y + area.height;
    if row < options_start_y || row >= options_end_y {
        return None;
    }

    let visual_line = (row - options_start_y) + scroll;
    Some(item_index_at_visual_line(
        question,
        content_w,
        visual_line,
        cursor,
    ))
}

/// Compute the aligned label column width.
pub fn compute_max_label_w(options: &[QuestionOption], content_w: usize) -> usize {
    let cap = content_w * 3 / 5;
    options
        .iter()
        .map(|o| normalize_label(&o.label).width())
        .max()
        .unwrap_or(0)
        .min(cap)
}

/// Visual height of a single option row. Unfocused: always 1 line (collapsed `label description…`).
pub fn option_visual_height(
    option: &QuestionOption,
    content_w: usize,
    prefix_w: usize,
    max_label_w: usize,
    focused: bool,
) -> u16 {
    if !focused {
        return 1;
    }
    let gap = 2;
    let norm_label = normalize_label(&option.label);
    if norm_label.width() > max_label_w {
        let wide_w = content_w.saturating_sub(prefix_w).max(1);
        let label_lines = wrap_label_chunks(&norm_label, wide_w).len() as u16;
        let desc_lines = rendered_option_description_lines(option, wide_w).len() as u16;
        (label_lines + desc_lines).max(1)
    } else {
        let indent = prefix_w + max_label_w + gap;
        let desc_w = content_w.saturating_sub(indent).max(1);
        let desc_lines = rendered_option_description_lines(option, desc_w).len() as u16;
        desc_lines.max(1)
    }
}

/// Inner chrome-height computation with explicit description/preview caps. Same logic as
/// [`chrome_height`] but accepts caps as parameters instead of branching on `fullscreen`. Used by
/// the dynamic-cap fallback in [`question_view_height`].
fn chrome_height_with_dynamic_caps(
    question: &Question,
    content_w: usize,
    preview: Option<&str>,
    desc_cap: u16,
    preview_cap: u16,
) -> u16 {
    let wrap_w = content_w.max(1);
    let (label, desc) = split_question_label_desc(&question.question);
    let raw_line = Line::from(vec![Span::raw(label.to_string())]);
    let label_lines = crate::engine::render::wrapping::word_wrap_line(&raw_line, wrap_w)
        .len()
        .max(1) as u16;
    let desc_lines = if desc.is_empty() {
        0u16
    } else {
        rendered_option_description_lines(
            &QuestionOption {
                label: String::new(),
                description: desc.to_string(),
                preview: None,
                id: None,
            },
            wrap_w,
        )
        .len() as u16
    };
    let preview_lines: u16 = match preview {
        Some(p) if !p.is_empty() => p
            .lines()
            .map(|l| {
                let raw = Line::from(vec![Span::raw(l.to_string())]);
                crate::engine::render::wrapping::word_wrap_line(&raw, wrap_w)
                    .len()
                    .max(1) as u16
            })
            .sum(),
        _ => 0,
    };

    let desc_lines = desc_lines.min(desc_cap);
    let preview_lines = preview_lines.min(preview_cap);

    let preview_gap = if preview_lines > 0 { 1 } else { 0 };
    let label_gap = if label_gap_suppressed(question) { 0 } else { 1 };

    // vpad(1) + label + label_gap(1) + description (if any)
    //   + [preview_gap(1) + preview_lines if preview exists] + gap(1)
    1 + label_lines + label_gap + desc_lines + preview_gap + preview_lines + 1
}

/// A card with no description and no options has nothing under its label, so the blank line meant to separate them would leave it unevenly padded.
/// [`chrome_height_with_dynamic_caps`] and [`render_question_chrome`] must agree on it.
fn label_gap_suppressed(question: &Question) -> bool {
    let (_, desc) = split_question_label_desc(&question.question);
    desc.is_empty() && question.options.is_empty()
}

/// Chrome height for a question: vpad + label lines + gap + [description lines] + gap. Must match
/// `render_question_chrome`.
pub fn chrome_height(
    question: &Question,
    content_w: usize,
    preview: Option<&str>,
    fullscreen: bool,
    desc_cap: u16,
    preview_cap: u16,
) -> u16 {
    if fullscreen {
        chrome_height_with_dynamic_caps(question, content_w, preview, u16::MAX, u16::MAX)
    } else {
        chrome_height_with_dynamic_caps(question, content_w, preview, desc_cap, preview_cap)
    }
}

/// Split question text into a label (first paragraph) and description (rest). A paragraph break is
/// `\n\n`. If no break exists, the full text is the label and the description is empty.
fn split_question_label_desc(text: &str) -> (&str, &str) {
    if let Some(pos) = text.find("\n\n") {
        (
            text.get(..pos).unwrap_or("").trim(),
            text.get(pos + 2..).unwrap_or("").trim(),
        )
    } else {
        (text.trim(), "")
    }
}

impl QuestionViewState {
    /// Toggle an option for a question. Multi: toggle in/out of the HashSet. Single: set to
    /// `Some(option_idx)`, or `None` if already selected (deselect).
    pub fn toggle_option(&mut self, question_idx: usize, option_idx: usize) {
        let Some(sel) = self.selections.get_mut(question_idx) else {
            return;
        };
        match sel {
            QuestionSelection::Multi(set) => {
                if !set.remove(&option_idx) {
                    set.insert(option_idx);
                }
            }
            QuestionSelection::Single(current) => {
                if *current == Some(option_idx) {
                    *current = None;
                } else {
                    *current = Some(option_idx);
                }
            }
        }
    }

    /// Select an option (no toggle; always selects). Single: set to `Some(option_idx)`. Multi: add to
    /// set.
    pub fn select_option(&mut self, question_idx: usize, option_idx: usize) {
        let Some(sel) = self.selections.get_mut(question_idx) else {
            return;
        };
        match sel {
            QuestionSelection::Multi(set) => {
                set.insert(option_idx);
            }
            QuestionSelection::Single(current) => {
                *current = Some(option_idx);
            }
        }
    }

    /// Activate freeform input for the active question. Such questions (e.g. the SuperGrok upsell) have
    /// no freeform row, so `InputMode` must be unreachable.
    pub fn activate_freeform_input(&mut self) -> String {
        if self.no_freeform {
            return String::new();
        }
        let idx = self.active_tab;
        if let Some(sel) = self.per_question_freeform_selected.get_mut(idx) {
            *sel = true;
        }
        if let Some(QuestionSelection::Single(sel)) = self.selections.get_mut(idx) {
            *sel = None;
        }
        self.focus = QuestionFocus::InputMode;
        self.per_question_freeform
            .get(idx)
            .cloned()
            .unwrap_or_default()
    }

    /// Preview text for the currently focused option, if any. Returns `Some(preview)` when the cursor
    /// is on an option (not freeform) and that option has a `preview` field set.
    pub fn focused_preview(&self) -> Option<&str> {
        let q = self.questions.get(self.active_tab)?;
        let cursor = self.cursor();
        let option = q.options.get(cursor)?;
        option.preview.as_deref()
    }

    /// The hard-modal blocked-prompt card: dismissal is refused, so the footer must not advertise it.
    pub fn is_prompt_blocked(&self) -> bool {
        self.prompt_blocked
    }

    /// Labels of the selected options for a given question.
    pub fn selected_labels(&self, question_idx: usize) -> Vec<String> {
        let Some(sel) = self.selections.get(question_idx) else {
            return Vec::new();
        };
        let Some(q) = self.questions.get(question_idx) else {
            return Vec::new();
        };
        match sel {
            QuestionSelection::Multi(set) => {
                let mut indices: Vec<_> = set.iter().copied().collect();
                indices.sort_unstable();
                indices
                    .into_iter()
                    .filter_map(|i| q.options.get(i).map(|o| o.label.clone()))
                    .collect()
            }
            QuestionSelection::Single(Some(idx)) => q
                .options
                .get(*idx)
                .map(|o| vec![o.label.clone()])
                .unwrap_or_default(),
            QuestionSelection::Single(None) => Vec::new(),
        }
    }

    /// True when the active tab has any option selected, or its free-form answer marked selected.
    pub fn active_tab_has_selection(&self) -> bool {
        let q_idx = self.active_tab;
        let option_selected = !self.selected_labels(q_idx).is_empty();
        let freeform_selected = self
            .per_question_freeform_selected
            .get(q_idx)
            .copied()
            .unwrap_or(false);
        option_selected || freeform_selected
    }
}

impl QuestionViewState {
    pub fn build_accepted_response(&self) -> Vec<QuestionResponse> {
        self.questions
            .iter()
            .enumerate()
            .filter_map(|(index, q)| {
                let labels = self.selected_labels(index);
                let notes = self
                    .per_question_freeform_selected
                    .get(index)
                    .copied()
                    .unwrap_or(false)
                    .then(|| self.per_question_freeform.get(index))
                    .flatten()
                    .filter(|note| !note.trim().is_empty())
                    .cloned();
                if labels.is_empty() && notes.is_none() {
                    return None;
                }
                let preview = match self.selections.get(index) {
                    Some(QuestionSelection::Single(Some(selected))) => q
                        .options
                        .get(*selected)
                        .and_then(|option| option.preview.clone()),
                    _ => None,
                };
                Some(QuestionResponse {
                    index,
                    id: q.id.clone(),
                    labels,
                    preview,
                    notes,
                })
            })
            .collect()
    }

    /// Advance to the next question (clamped, no wrap).
    pub fn change_tab(&mut self, index: usize) -> bool {
        if index >= self.questions.len() || index == self.active_tab {
            return false;
        }
        self.active_tab = index;
        self.last_option_click = None;
        self.scrollbar_dragging = false;
        self.focus = QuestionFocus::Navigation;
        true
    }

    pub fn save_freeform(&mut self, text: String) {
        if let Some(answer) = self.per_question_freeform.get_mut(self.active_tab) {
            let selected = !text.trim().is_empty();
            *answer = text;
            self.per_question_freeform_selected[self.active_tab] = selected;
            if selected
                && let Some(QuestionSelection::Single(option)) =
                    self.selections.get_mut(self.active_tab)
            {
                *option = None;
            }
        }
        self.focus = QuestionFocus::Navigation;
    }

    pub fn next_question(&mut self) {
        self.change_tab(self.active_tab.saturating_add(1));
    }

    /// Go to the previous question (clamped, no wrap).
    pub fn prev_question(&mut self) {
        self.change_tab(self.active_tab.saturating_sub(1));
    }
}

/// Desired height for the question view overlay. The description and preview caps shrink
/// dynamically so at least [`MIN_VISIBLE_OPTION_ROWS`] option rows stay visible under the chrome.
pub fn question_view_height(state: &mut QuestionViewState, screen_h: u16, content_w: usize) -> u16 {
    let q_idx = state.active_tab;
    let Some(question) = state.questions.get(q_idx) else {
        return 0;
    };

    // `total_options_height` unconditionally counts a 1-line freeform row
    // When `no_freeform` is set that row is never rendered, so subtract it from the totals below
    // Otherwise the panel keeps a clickable dead row under the last option
    let phantom_freeform = state.phantom_freeform_h();
    let freeform_h: u16 = 1 - phantom_freeform;
    let min_options_space = MIN_VISIBLE_OPTION_ROWS + freeform_h;

    if state.fullscreen {
        // desc_cap/preview_cap are ignored when fullscreen=true (chrome_height routes to u16::MAX internally), but pass MAX for clarity
        let chrome_h = chrome_height(
            question,
            content_w,
            state.focused_preview(),
            true,
            u16::MAX,
            u16::MAX,
        );
        let total = chrome_h
            + total_options_height(question, content_w, state.cursor())
                .saturating_sub(phantom_freeform);
        state.cached_desc_cap = u16::MAX;
        state.cached_preview_cap = u16::MAX;
        return total.min(screen_h);
    }

    let cap = (screen_h as u32 * 33 / 100)
        .max(8)
        .min(screen_h as u32 * 80 / 100) as u16;

    let mut effective_desc_cap = DEFAULT_MAX_CHROME_DESC_LINES;
    let mut effective_preview_cap = DEFAULT_MAX_CHROME_PREVIEW_LINES;

    let mut chrome_h = chrome_height_with_dynamic_caps(
        question,
        content_w,
        state.focused_preview(),
        effective_desc_cap,
        effective_preview_cap,
    );

    if chrome_h + min_options_space > cap {
        // Compute fixed overhead (vpad + label + gaps).
        let (label, desc) = split_question_label_desc(&question.question);
        let raw_line = Line::from(vec![Span::raw(label.to_string())]);
        let label_lines =
            crate::engine::render::wrapping::word_wrap_line(&raw_line, content_w.max(1))
                .len()
                .max(1) as u16;
        let fixed_overhead = 1 + label_lines + 1 + 1; // vpad + label + blank + bottom gap

        // Compute actual description line count so unused desc budget can be reallocated to preview instead of being wasted
        let actual_desc_lines = if desc.is_empty() {
            0u16
        } else {
            rendered_option_description_lines(
                &QuestionOption {
                    label: String::new(),
                    description: desc.to_string(),
                    preview: None,
                    id: None,
                },
                content_w.max(1),
            )
            .len() as u16
        };

        let content_budget = cap
            .saturating_sub(fixed_overhead)
            .saturating_sub(min_options_space);
        effective_desc_cap = content_budget
            .min(DEFAULT_MAX_CHROME_DESC_LINES)
            .min(actual_desc_lines);
        let remaining = content_budget.saturating_sub(effective_desc_cap);
        // Reserve 1 row for the preview gap (blank separator) when preview text exists and there is any remaining budget for preview lines
        let preview_gap_allowance = if state.focused_preview().is_some() && remaining > 0 {
            1u16
        } else {
            0
        };
        effective_preview_cap = remaining
            .saturating_sub(preview_gap_allowance)
            .min(DEFAULT_MAX_CHROME_PREVIEW_LINES);

        // Recompute chrome with reduced caps.
        chrome_h = chrome_height_with_dynamic_caps(
            question,
            content_w,
            state.focused_preview(),
            effective_desc_cap,
            effective_preview_cap,
        );
    }

    state.cached_desc_cap = effective_desc_cap;
    state.cached_preview_cap = effective_preview_cap;

    let total = chrome_h
        + total_options_height(question, content_w, state.cursor())
            .saturating_sub(phantom_freeform);
    total.min(cap)
}

/// Shortcut label for an option index: 1-9 then a-z. Returns `'1'`..`'9'` for indices 0..8,
/// `'a'`..`'z'` for 9..34. Returns `None` for indices 35 and above.
pub fn option_shortcut_label(idx: usize) -> Option<char> {
    match idx {
        0..=8 => Some((b'1' + idx as u8) as char),
        9..=34 => Some((b'a' + (idx - 9) as u8) as char),
        _ => None,
    }
}

/// Map a pressed key character to an option index. Maps `'1'`..`'9'` to 0..8 and `'a'`..`'f'` to
/// 9..14. Only a-f are mapped as shortcuts to avoid conflicts with navigation keys. (g=top,
/// h=prev-question, j=down, k=up, l=next-question, n=next, s=skip).
pub fn option_index_for_key(c: char) -> Option<usize> {
    match c {
        '1'..='9' => Some((c as usize) - ('1' as usize)),
        'a'..='f' => Some(9 + (c as usize) - ('a' as usize)),
        _ => None,
    }
}

/// Horizontal padding: 3 left (accent + pad) + 2 right (scrollbar gutter).
pub const QUESTION_VIEW_HPAD: u16 = 5;

/// Prefix width for option rows. The shortcut column is always 1 character wide (1-9, a-z),
/// followed by a space, then the marker/radio/checkbox, then a space: Multi: `X [✓] ` = 1 + 1 + 3 +
/// 1 = 6. Single: `X (●) ` = 1 + 1 + 3 + 1 = 6.
pub fn option_prefix_w(_question: &Question) -> usize {
    6 // both multi and single use 3-char markers
}

/// Width available for inline prompt text given the full area width.
pub fn inline_text_width(area_width: u16) -> u16 {
    const LEFT_PAD: u16 = 3; // accent column + 2 padding
    const OPTION_PREFIX_W: u16 = 6; // shortcut + marker ("z [x] ")
    const PROMPT_INDICATOR_W: u16 = 2; // "❯ "
    area_width.saturating_sub(LEFT_PAD + OPTION_PREFIX_W + PROMPT_INDICATOR_W)
}

/// Normalize a label for single-line display: replace newlines with spaces.
pub(crate) fn normalize_label(label: &str) -> String {
    label.replace('\n', " ").replace("  ", " ")
}

fn rendered_option_label(option: &QuestionOption, max_label_w: usize) -> String {
    if max_label_w == 0 {
        String::new()
    } else {
        truncate_str(&normalize_label(&option.label), max_label_w)
    }
}

fn rendered_option_description_lines(option: &QuestionOption, width: usize) -> Vec<Line<'static>> {
    if option.description.trim().is_empty() {
        return Vec::new();
    }

    let mut renderer = StreamingMarkdownRenderer::new(md_style::style(), true);
    renderer.push(&option.description);
    // finish() (not render()) so the LaTeX-delimiter normalizer flushes any
    // trailing held-back bytes for this complete, one-shot description.
    renderer.finish(markdown_syntax());
    let view = renderer.view();
    let lines_owned: Vec<Line<'static>> = view
        .lines
        .iter()
        .map(crate::engine::render::line_utils::line_to_static)
        .collect();
    let (wrapped, _) = word_wrap_lines_with_joiners(lines_owned, width.max(1));

    let mut compact = Vec::new();
    let mut prev_blank = true;
    for line in wrapped {
        let is_blank = line.spans.iter().all(|s| s.content.trim().is_empty());
        if is_blank {
            if prev_blank {
                continue;
            }
            prev_blank = true;
            continue;
        }
        prev_blank = false;
        compact.push(line);
    }
    compact
}

fn styled_description_lines(
    option: &QuestionOption,
    width: usize,
    row_bg: ratatui::style::Color,
    desc_fg: ratatui::style::Color,
) -> Vec<Line<'static>> {
    rendered_option_description_lines(option, width)
        .into_iter()
        .map(|mut line| {
            line.style = line.style.patch(Style::default().fg(desc_fg).bg(row_bg));
            for span in &mut line.spans {
                span.style = span.style.patch(Style::default().fg(desc_fg).bg(row_bg));
            }
            line
        })
        .collect()
}

/// Build a flat list of styled lines for all option rows and the freeform row.
#[allow(clippy::too_many_arguments)]
pub fn build_flat_option_lines(
    question: &Question,
    content_w: usize,
    cursor: usize,
    hovered: Option<usize>,
    selections: &QuestionSelection,
    theme: &Theme,
    show_freeform: bool,
    freeform_text: &str,
    freeform_selected: bool,
    panel_focused: bool,
) -> Vec<Line<'static>> {
    let prefix_w = option_prefix_w(question);
    let max_lw = compute_max_label_w(&question.options, content_w);
    let is_multi = question.multi_select.unwrap_or(false);

    let mut all_lines = Vec::new();

    let hover_bg = hovered_bg(theme);

    for (i, option) in question.options.iter().enumerate() {
        let is_cursor_item = i == cursor;
        let is_hovered_item = hovered == Some(i);
        let is_selected = match selections {
            QuestionSelection::Multi(set) => set.contains(&i),
            QuestionSelection::Single(sel) => *sel == Some(i),
        };
        let embed = crate::ui::views::modal_window::embedded_row_style(
            theme,
            is_cursor_item && panel_focused,
        );
        // Full TUI: the focused row (keyboard cursor) gets a distinct selection bg, but only when the panel itself owns focus
        // When unfocused, drop the cursor-row bg so it reads as "no active selection"
        // A hovered row (mouse) gets a subtle blend; a normal row gets the dark bg
        let row_bg = match embed {
            Some(e) => e.bg,
            None if is_cursor_item && panel_focused => theme.bg_visual,
            None if is_hovered_item => hover_bg,
            None => theme.bg_light,
        };

        let start = all_lines.len();
        build_single_option_lines(
            &mut all_lines,
            i,
            option,
            is_multi,
            is_selected,
            max_lw,
            prefix_w,
            content_w,
            row_bg,
            embed,
            theme,
            is_cursor_item,
        );
        // Terminal theme (Reset bands): the overlay adds reverse video to
        // the row's line style, which the painter applies to the whole row
        // rect. RGB themes already bake the band bg; the patch is a no-op.
        if embed.is_none() {
            let overlay = if is_cursor_item && panel_focused {
                Some(theme.selection_overlay())
            } else if is_hovered_item {
                Some(theme.hover_overlay())
            } else {
                None
            };
            if let Some(ov) = overlay {
                for line in all_lines.get_mut(start..).unwrap_or(&mut []) {
                    line.style = line.style.patch(ov);
                }
            }
        }
    }

    // The freeform row is hidden in InputMode (the prompt widget below replaces it)
    if show_freeform {
        let freeform_idx = question.options.len();
        all_lines.push(build_freeform_line(
            QuestionLabels::default(),
            freeform_idx == cursor,
            hovered == Some(freeform_idx),
            freeform_text,
            freeform_selected,
            is_multi,
            theme,
            panel_focused,
        ));
    }

    all_lines
}

/// Build an indented description continuation line.
fn build_indented_desc_line(
    indent: usize,
    desc_line: &Line<'static>,
    row_bg: ratatui::style::Color,
) -> Line<'static> {
    Line::from(
        std::iter::once(Span::styled(
            " ".repeat(indent),
            Style::default().bg(row_bg),
        ))
        .chain(desc_line.spans.iter().cloned())
        .collect::<Vec<_>>(),
    )
    .style(Style::default().bg(row_bg))
}

/// A collapsed description: one visual line that still shows a trailing `…` affordance whenever content is hidden.
fn collapsed_description_spans(
    option: &QuestionOption,
    width: usize,
    row_bg: ratatui::style::Color,
    desc_fg: ratatui::style::Color,
) -> Vec<Span<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let lines = styled_description_lines(option, width, row_bg, desc_fg);
    let Some(first) = lines.first().cloned() else {
        return Vec::new();
    };
    let has_more = lines.len() > 1;
    let first_w: usize = first.spans.iter().map(|s| s.content.width()).sum();
    if first_w <= width && !has_more {
        return first.spans;
    }
    let budget = if first_w > width {
        width
    } else {
        width.saturating_sub(1)
    };
    let mut spans = truncate_line(first, budget).spans;
    let ends_ellipsis = spans
        .last()
        .is_some_and(|s| s.content.ends_with('\u{2026}'));
    if !ends_ellipsis {
        spans.push(Span::styled(
            "\u{2026}",
            Style::default().fg(desc_fg).bg(row_bg),
        ));
    }
    spans
}

/// Word-wrap an overflowing label into chunks of at most `width` columns.
fn wrap_label_chunks(label: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut remaining = label;
    while !remaining.is_empty() {
        if remaining.width() <= width {
            out.push(remaining.to_string());
            break;
        }
        let byte_end = byte_offset_at_width(remaining, width);
        let break_at = remaining
            .get(..byte_end)
            .and_then(|s| s.rfind(' '))
            .map(|i| i + 1)
            .unwrap_or(byte_end);
        let break_at = if break_at == 0 {
            remaining
                .char_indices()
                .nth(1)
                .map(|(i, _)| i)
                .unwrap_or(remaining.len())
        } else {
            break_at
        };
        let Some((chunk, rest)) = remaining.split_at_checked(break_at) else {
            break;
        };
        out.push(chunk.to_string());
        remaining = rest.trim_start();
    }
    out
}

/// Build the visual lines for a single option and append them to `out`.
#[allow(clippy::too_many_arguments)]
fn build_single_option_lines(
    out: &mut Vec<Line<'static>>,
    idx: usize,
    option: &QuestionOption,
    is_multi: bool,
    is_selected: bool,
    max_label_w: usize,
    prefix_w: usize,
    content_w: usize,
    row_bg: ratatui::style::Color,
    embed: Option<crate::ui::views::modal_window::EmbeddedRowStyle>,
    theme: &Theme,
    focused: bool,
) {
    let fg = |normal| embed.map_or(normal, |e| e.fg(normal));
    let shortcut_ch = option_shortcut_label(idx).unwrap_or(' ');
    let num_str = format!("{shortcut_ch}");
    let num_style = Style::default().fg(fg(theme.accent_user)).bg(row_bg);
    let label_style = Style::default()
        .fg(fg(theme.text_primary))
        .bg(row_bg)
        .add_modifier(if focused {
            Modifier::BOLD
        } else {
            Modifier::empty()
        });

    // Build prefix spans (number and marker/checkbox)
    let prefix_spans: Vec<Span<'static>> = if is_multi {
        let (checkbox, cb_style) = if is_selected {
            (
                "[x]".to_string(),
                Style::default()
                    .fg(fg(theme.text_primary))
                    .bg(row_bg)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            (
                "[ ]".to_string(),
                Style::default().fg(fg(theme.gray)).bg(row_bg),
            )
        };
        vec![
            Span::styled(format!("{num_str} "), num_style),
            Span::styled(format!("{checkbox} "), cb_style),
        ]
    } else {
        // Single-select: radio buttons (●) / (○)
        let (radio, radio_style) = if is_selected {
            (
                format!("({})", crate::engine::render::glyphs::filled_dot()), // (●) falls back to (•) on legacy ConHost
                Style::default()
                    .fg(fg(theme.text_primary))
                    .bg(row_bg)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            (
                "(\u{25cb})".to_string(), // (○)
                Style::default().fg(fg(theme.gray)).bg(row_bg),
            )
        };
        vec![
            Span::styled(format!("{num_str} "), num_style),
            Span::styled(format!("{radio} "), radio_style),
        ]
    };

    let gap = 2usize;
    let indent = prefix_w + max_label_w + gap;
    let desc_w = content_w.saturating_sub(indent).max(1);

    if !focused {
        let mut spans = prefix_spans;
        let label = rendered_option_label(option, max_label_w);
        let padded_label = format!("{label:<width$}", width = max_label_w);
        spans.push(Span::styled(padded_label, label_style));
        let desc_spans = collapsed_description_spans(option, desc_w, row_bg, fg(theme.gray));
        if !desc_spans.is_empty() {
            spans.push(Span::styled(" ".repeat(gap), Style::default().bg(row_bg)));
            spans.extend(desc_spans);
        }
        out.push(Line::from(spans).style(Style::default().bg(row_bg)));
        return;
    }

    let norm_label = normalize_label(&option.label);
    if norm_label.width() > max_label_w {
        let wide_w = content_w.saturating_sub(prefix_w).max(1);
        let chunks = wrap_label_chunks(&norm_label, wide_w);
        for (li, chunk) in chunks.into_iter().enumerate() {
            if li == 0 {
                let mut spans = prefix_spans.clone();
                spans.push(Span::styled(chunk, label_style));
                out.push(Line::from(spans).style(Style::default().bg(row_bg)));
            } else {
                let spans = vec![
                    Span::styled(" ".repeat(prefix_w), Style::default().bg(row_bg)),
                    Span::styled(chunk, label_style),
                ];
                out.push(Line::from(spans).style(Style::default().bg(row_bg)));
            }
        }
        for line in styled_description_lines(option, wide_w, row_bg, fg(theme.gray)) {
            out.push(build_indented_desc_line(prefix_w, &line, row_bg));
        }
    } else {
        let label = rendered_option_label(option, max_label_w);
        let padded_label = format!("{label:<width$}", width = max_label_w);
        let mut label_spans = prefix_spans;
        label_spans.push(Span::styled(padded_label, label_style));
        let desc_lines = styled_description_lines(option, desc_w, row_bg, fg(theme.gray));
        if let Some(first_desc) = desc_lines.first()
            && !first_desc.spans.is_empty()
        {
            label_spans.push(Span::styled(" ".repeat(gap), Style::default().bg(row_bg)));
            label_spans.extend(first_desc.spans.iter().cloned());
        }
        out.push(Line::from(label_spans).style(Style::default().bg(row_bg)));
        for line in desc_lines.iter().skip(1) {
            out.push(build_indented_desc_line(indent, line, row_bg));
        }
    }
}

/// Build the freeform row line. `prefix_w` is the total prefix width used by option rows (number
/// and marker), so the freeform row aligns with the option labels. `freeform_text` is the
/// per-question freeform text; when non-empty the row shows as ticked with a preview of the answer.
#[allow(clippy::too_many_arguments)]
fn build_freeform_line(
    labels: QuestionLabels,
    is_cursor: bool,
    is_hovered: bool,
    freeform_text: &str,
    is_selected: bool,
    is_multi: bool,
    theme: &Theme,
    panel_focused: bool,
) -> Line<'static> {
    // Whitespace-only freeform is treated as empty and never shown as selected
    let is_selected = is_selected && !freeform_text.trim().is_empty();

    let embed =
        crate::ui::views::modal_window::embedded_row_style(theme, is_cursor && panel_focused);
    let fg = |normal| embed.map_or(normal, |e| e.fg(normal));
    let row_bg = match embed {
        Some(e) => e.bg,
        None if is_cursor && panel_focused => theme.bg_visual,
        None if is_hovered => hovered_bg(theme),
        None => theme.bg_light,
    };
    // Terminal theme (Reset bands): reverse video via the line style, which
    // the painter applies to the whole row rect. No-op on RGB themes.
    let overlay = if embed.is_none() && is_cursor && panel_focused {
        Some(theme.selection_overlay())
    } else if embed.is_none() && is_hovered {
        Some(theme.hover_overlay())
    } else {
        None
    };

    // Multi-select: [x]/[ ] checkboxes.  Single-select: (●)/(○) radio buttons.
    // Both are 3 display cells, same as option rows
    let marker: String = if is_multi {
        (if is_selected { "[x]" } else { "[ ]" }).to_string()
    } else if is_selected {
        format!("({})", crate::engine::render::glyphs::filled_dot())
    } else {
        "(\u{25cb})".to_string()
    };
    let _marker_display_w: usize = 3;
    let marker_style = if is_selected {
        Style::default()
            .fg(fg(theme.text_primary))
            .bg(row_bg)
            .add_modifier(Modifier::BOLD)
    } else if is_cursor {
        Style::default().fg(fg(theme.accent_user)).bg(row_bg)
    } else {
        Style::default().fg(fg(theme.gray)).bg(row_bg)
    };
    // Stable shortcut "z": always 1 character, matching option labels
    let num_str = "z".to_string();
    let num_style = Style::default().fg(fg(theme.accent_user)).bg(row_bg);
    let marker_with_space = format!("{marker} ");

    let has_text = !freeform_text.trim().is_empty();
    let prompt_indicator = Style::default().fg(fg(theme.accent_user)).bg(row_bg);
    let (label, label_style) = if is_selected && has_text {
        // Show a truncated preview of the typed answer.
        let first_line = freeform_text.lines().next().unwrap_or("");
        let preview = truncate_str(first_line, 50);
        (
            preview,
            Style::default().fg(fg(theme.text_primary)).bg(row_bg),
        )
    } else if has_text {
        // Has text but not selected: show dimmed preview
        let first_line = freeform_text.lines().next().unwrap_or("");
        let preview = truncate_str(first_line, 50);
        (preview, Style::default().fg(fg(theme.gray)).bg(row_bg))
    } else {
        // Empty: show placeholder
        (
            labels.freeform.to_owned(),
            Style::default().fg(fg(theme.gray)).bg(row_bg),
        )
    };

    let mut spans = vec![
        Span::styled(format!("{num_str} "), num_style),
        Span::styled(marker_with_space, marker_style),
    ];
    // Show ❯ prompt indicator only when there's text (not on placeholder).
    if has_text {
        spans.push(Span::styled(
            crate::engine::render::glyphs::prompt_arrow(),
            prompt_indicator,
        ));
    }
    spans.push(Span::styled(label, label_style));

    let mut style = Style::default().bg(row_bg);
    if let Some(ov) = overlay {
        style = style.patch(ov);
    }
    Line::from(spans).style(style)
}

/// Render the complete question view into the given area. `area` is the region above the textarea
/// allocated for the question chrome and option rows. The accent `┃` line and background are
/// rendered here. Return value from [`render_question_view`] with layout info for mouse handling.
pub struct QuestionViewRenderResult {
    /// Y coordinate where the scrollable options area starts (after chrome header).
    pub options_start_y: u16,
    /// Y coordinate where the scrollable options area ends (before freeform/inline prompt).
    pub options_end_y: u16,
    pub option_rects: Vec<(usize, Rect)>,
    pub tab_rects: Vec<Option<Rect>>,
    pub scrollbar: Option<Rect>,
}

pub fn render_question_view(
    buf: &mut Buffer,
    area: Rect,
    state: &QuestionViewState,
    hovered_item: Option<usize>,
    theme: &Theme,
    focused: bool,
) -> QuestionViewRenderResult {
    let area = area.intersection(buf.area);
    if area.height == 0 || area.width <= QUESTION_VIEW_HPAD {
        return QuestionViewRenderResult {
            options_start_y: area.y,
            options_end_y: area.y,
            option_rects: Vec::new(),
            tab_rects: Vec::new(),
            scrollbar: None,
        };
    }

    let q_idx = state.active_tab;
    let Some(question) = state.questions.get(q_idx) else {
        return QuestionViewRenderResult {
            options_start_y: area.y,
            options_end_y: area.y,
            option_rects: Vec::new(),
            tab_rects: Vec::new(),
            scrollbar: None,
        };
    };

    let content_w = area.width.saturating_sub(QUESTION_VIEW_HPAD) as usize;

    // Fill background, same as the focused prompt (bg_light)
    let bg = Style::default().bg(theme.bg_light);
    buf.set_style(area, bg);

    // Accent line ┃ on the left column, blue to match the shortcut key color
    let accent_style = Style::default().fg(theme.accent_user);
    for row in area.y..area.y + area.height {
        if let Some(cell) = buf.cell_mut((area.x, row)) {
            cell.set_symbol(crate::engine::render::glyphs::accent_bar()); // ┃ falls back to │ on legacy ConHost
            cell.set_style(accent_style);
        }
    }

    // Content area (left: accent + 2-char pad, right: 2-char pad for scrollbar)
    let content_x = area.x + 3;
    let content_width = area.width.saturating_sub(QUESTION_VIEW_HPAD);
    let mut y = area.y;
    let mut tab_rects = Vec::new();
    if state.questions.len() > 1 && area.height >= 5 {
        let tab_labels: Vec<_> = (1..=state.questions.len())
            .map(|number| number.to_string())
            .collect();
        let tabs: Vec<_> = tab_labels.iter().map(String::as_str).collect();
        let mut chrome = super::modal_window::ModalWindowState::with_tabs(tabs.len());
        chrome.active_tab = state.active_tab;
        let tab_area = Rect::new(area.x + 1, y, area.width - 1, (area.height / 3).max(1));
        let rows = super::modal_window::render_tab_bar(buf, tab_area, &mut chrome, &tabs, theme);
        tab_rects = chrome.tab_rects;
        y += rows.min(tab_area.height);
    }

    // Vertical padding at the top.
    y += 1;

    // Clip to the panel bottom: the accounted height and the rendered height can disagree (wrap-width drift, stale caps)
    // The chrome must degrade to truncation instead of writing past the area; set_line past the buffer bottom aborts the TUI
    y = render_question_chrome(
        buf,
        content_x,
        y,
        content_width,
        area.y + area.height,
        state,
        question,
        theme,
        state.fullscreen,
        state.cached_desc_cap,
        state.cached_preview_cap,
    );

    y += 1;

    let options_start_y = y;

    let visible_bottom = area.y + area.height;
    let scroll = state.per_question_scroll.get(q_idx).copied().unwrap_or(0) as usize;
    let cursor = state.cursor();
    let is_input_mode = state.focus == QuestionFocus::InputMode;

    let freeform_text = state
        .per_question_freeform
        .get(q_idx)
        .map(|s| s.as_str())
        .unwrap_or("");
    let freeform_selected = state
        .per_question_freeform_selected
        .get(q_idx)
        .copied()
        .unwrap_or(false);

    // The freeform row is always rendered sticky at the bottom (not in the scrollable list), unless in InputMode where the inline prompt replaces it
    // When `no_freeform` is set the row is hidden entirely.
    let sticky_freeform = !is_input_mode && !state.no_freeform;
    let freeform_h: u16 = if sticky_freeform { 1 } else { 0 };

    // Build option lines WITHOUT the freeform row (it's sticky or inline).
    let default_selection = QuestionSelection::Single(None);
    let all_lines = build_flat_option_lines(
        question,
        content_w,
        cursor,
        hovered_item,
        state.selections.get(q_idx).unwrap_or(&default_selection),
        theme,
        false, // never in scroll list
        freeform_text,
        freeform_selected,
        focused,
    );

    let visible_h = visible_bottom.saturating_sub(y).saturating_sub(freeform_h) as usize;
    let heights = option_heights(question, content_w, cursor);
    let row_indices = heights
        .into_iter()
        .enumerate()
        .flat_map(|(index, height)| std::iter::repeat_n(index, usize::from(height)));
    let mut option_rects = Vec::new();
    for (line, option) in all_lines
        .iter()
        .zip(row_indices)
        .skip(scroll)
        .take(visible_h)
    {
        if y >= visible_bottom.saturating_sub(freeform_h) {
            break;
        }
        let row_rect = Rect {
            x: content_x,
            y,
            width: content_width,
            height: 1,
        };
        option_rects.push((option, row_rect));
        buf.set_style(row_rect, line.style);
        buf.set_line(content_x, y, line, content_width);
        y += 1;
    }

    if sticky_freeform {
        let freeform_y = visible_bottom.saturating_sub(1);
        if freeform_y >= y {
            let freeform_idx = question.options.len();
            let is_multi = question.multi_select.unwrap_or(false);
            let _prefix_w = option_prefix_w(question);
            let freeform_line = build_freeform_line(
                state.labels,
                freeform_idx == cursor,
                hovered_item == Some(freeform_idx),
                freeform_text,
                freeform_selected,
                is_multi,
                theme,
                focused,
            );
            let row_rect = Rect {
                x: content_x,
                y: freeform_y,
                width: content_width,
                height: 1,
            };
            option_rects.push((freeform_idx, row_rect));
            buf.set_style(row_rect, freeform_line.style);
            buf.set_line(content_x, freeform_y, &freeform_line, content_width);
        }
    }

    // Unfocus dim: when the user has navigated to the scrollback (or any other pane), blend foregrounds toward `bg_light` so the panel recedes
    // Mirrors the unfocused prompt widget pattern (`prompt_widget.rs:1948`)
    if !focused {
        crate::engine::render::color::recede_area(buf, area, theme.bg_light, 0.66);
    }

    let options_end_y = visible_bottom.saturating_sub(freeform_h);
    let scrollbar = render_question_scrollbar(
        buf,
        area.right().saturating_sub(1),
        state,
        theme,
        (options_start_y, options_end_y),
        content_w,
    );
    QuestionViewRenderResult {
        options_start_y,
        options_end_y,
        option_rects,
        tab_rects,
        scrollbar,
    }
}

/// Render the question view scrollbar. Call this AFTER `render_prompt_chrome`.
pub fn render_question_scrollbar(
    buf: &mut Buffer,
    scrollbar_x: u16,
    state: &QuestionViewState,
    theme: &Theme,
    scroll_region: (u16, u16),
    content_width: usize,
) -> Option<Rect> {
    let q_idx = state.active_tab;
    let question = state.questions.get(q_idx)?;

    let (scroll_top, scroll_bottom) = scroll_region;
    let visible_options_h = scroll_bottom.saturating_sub(scroll_top);
    let total_option_h = total_options_height(question, content_width, state.cursor())
        .saturating_sub(state.phantom_freeform_h());

    let scroll = state.per_question_scroll.get(q_idx).copied().unwrap_or(0);

    if total_option_h > visible_options_h && visible_options_h > 0 {
        let scrollbar_area = Rect {
            x: scrollbar_x,
            y: scroll_top,
            width: 1,
            height: visible_options_h,
        };
        // Use visible colors against bg_light: dim track, bright thumb.
        let track_style = Style::default().fg(theme.gray_dim).bg(theme.bg_light);
        let thumb_style = Style::default().fg(theme.gray).bg(theme.bg_light);
        crate::engine::render::scrollbar::render_scrollbar_styled(
            buf,
            Some(scrollbar_area),
            total_option_h,
            visible_options_h,
            scroll,
            track_style,
            thumb_style,
        );
        return Some(scrollbar_area);
    }
    None
}

/// Render a truncation indicator line: `... Ctrl-F to expand`.
fn render_truncation_indicator(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    theme: &Theme,
    labels: QuestionLabels,
) {
    let style = Style::default().fg(theme.gray).bg(theme.bg_light);
    let indicator = Line::from(vec![
        Span::styled("... ", style),
        Span::styled(
            "Ctrl-F",
            Style::default().fg(theme.accent_user).bg(theme.bg_light),
        ),
        Span::styled(labels.expand, style),
    ]);
    buf.set_line(x, y, &indicator, width);
}

/// Render question chrome: label line and description.
#[allow(clippy::too_many_arguments)]
fn render_question_chrome(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    max_y: u16,
    state: &QuestionViewState,
    question: &Question,
    theme: &Theme,
    fullscreen: bool,
    desc_cap: u16,
    preview_cap: u16,
) -> u16 {
    let mut cur_y = y;
    let w = width as usize;
    // Never write below the panel or the buffer
    // The area itself should already be inside the buffer, but a mis-sized area must degrade to truncation, not an abort
    let max_y = max_y.min(buf.area.bottom());

    // Split into label (first paragraph) and description (rest).
    let (label_text, desc_text) = split_question_label_desc(&question.question);

    let label_style = Style::default()
        .fg(theme.text_primary)
        .add_modifier(Modifier::BOLD);

    let raw_line = Line::from(vec![Span::styled(label_text.to_string(), label_style)]);
    let wrapped = crate::engine::render::wrapping::word_wrap_line(&raw_line, w);
    for line in &wrapped {
        if cur_y >= max_y {
            return cur_y;
        }
        buf.set_line(x, cur_y, line, width);
        cur_y += 1;
    }

    // Blank line separating the label from what follows it.
    if !label_gap_suppressed(question) {
        cur_y += 1;
    }

    if !desc_text.is_empty() {
        let desc_lines = styled_description_lines(
            &QuestionOption {
                label: String::new(),
                description: desc_text.to_string(),
                preview: None,
                id: None,
            },
            w,
            theme.bg_light,
            theme.gray,
        );
        let raw_desc_count = desc_lines.len() as u16;
        let is_truncated = !fullscreen && raw_desc_count > desc_cap;
        for (desc_rendered, line) in desc_lines.into_iter().enumerate() {
            let desc_rendered = desc_rendered as u16;
            if !fullscreen && desc_rendered >= desc_cap {
                break;
            }
            if cur_y >= max_y {
                return cur_y;
            }
            // Always render the real content line first
            // When truncated and there is room for content plus an indicator (cap >= 2), the indicator lands after the second-to-last real line
            // When cap == 1 we show the single content line without an indicator; there is no room for both
            buf.set_line(x, cur_y, &line, width);
            cur_y += 1;
            if is_truncated && desc_cap >= 2 && desc_rendered == desc_cap.saturating_sub(2) {
                if cur_y >= max_y {
                    return cur_y;
                }
                render_truncation_indicator(buf, x, cur_y, width, theme, state.labels);
                cur_y += 1;
                break;
            }
        }
    }

    if let Some(preview_text) = state.focused_preview()
        && !preview_text.is_empty()
    {
        let preview_style = Style::default().fg(theme.gray).bg(theme.bg_light);

        // Count total preview lines first to determine truncation.
        // Uses Span::raw to match chrome_height (style doesn't affect wrapping).
        let mut total_preview_count = 0u16;
        for text_line in preview_text.lines() {
            let raw = Line::from(vec![Span::raw(text_line.to_string())]);
            total_preview_count += crate::engine::render::wrapping::word_wrap_line(&raw, w)
                .len()
                .max(1) as u16;
        }

        // Cap to match chrome_height accounting.
        let capped_count = if fullscreen {
            total_preview_count
        } else {
            total_preview_count.min(preview_cap)
        };

        // Only emit the preview gap if there will be visible preview lines.
        if capped_count > 0 {
            cur_y += 1;
        }

        let is_truncated = !fullscreen && total_preview_count > preview_cap;
        let mut preview_rendered = 0u16;
        'preview_done: for text_line in preview_text.lines() {
            let raw = Line::from(vec![Span::styled(text_line.to_string(), preview_style)]);
            let wrapped = crate::engine::render::wrapping::word_wrap_line(&raw, w);
            for line in &wrapped {
                if !fullscreen && preview_rendered >= preview_cap {
                    break 'preview_done;
                }
                if cur_y >= max_y {
                    return cur_y;
                }
                // Always render the real content line first
                // Append the truncation indicator only when cap >= 2 so at least one real preview line is visible above it
                buf.set_line(x, cur_y, line, width);
                cur_y += 1;
                preview_rendered += 1;
                if is_truncated
                    && preview_cap >= 2
                    && preview_rendered == preview_cap.saturating_sub(1)
                {
                    if cur_y >= max_y {
                        return cur_y;
                    }
                    render_truncation_indicator(buf, x, cur_y, width, theme, state.labels);
                    cur_y += 1;
                    break 'preview_done;
                }
            }
        }
    }

    cur_y
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionInput {
    Changed,
    EditFreeform,
    ChangeTab(usize),
    Submit,
    Cancel,
    Unhandled,
}

impl QuestionViewState {
    pub fn handle_navigation_key(
        &mut self,
        key: &KeyEvent,
        visible_rows: u16,
        content_width: usize,
    ) -> QuestionInput {
        if self.questions.is_empty() || self.focus != QuestionFocus::Navigation {
            return QuestionInput::Unhandled;
        }
        self.last_option_click = None;
        let ctrl = key.modifiers == KeyModifiers::CONTROL;
        let plain = key.modifiers.is_empty();
        if let Some(walk) = RowWalk::from_key(key) {
            self.walk_cursor(walk);
        } else if self.is_on_freeform_row()
            && (plain || key.modifiers == KeyModifiers::SHIFT)
            && matches!(key.code, KeyCode::Char(ch) if ch != ' ')
        {
            return QuestionInput::EditFreeform;
        } else {
            match key.code {
                KeyCode::Esc if plain && !self.is_prompt_blocked() => return QuestionInput::Cancel,
                KeyCode::Char('y') if ctrl && !self.is_prompt_blocked() => {
                    return QuestionInput::Cancel;
                }
                KeyCode::Char('c') if ctrl => return QuestionInput::Submit,
                KeyCode::Char('X') if key.modifiers == KeyModifiers::SHIFT => {
                    return QuestionInput::Submit;
                }
                KeyCode::Char('f') if ctrl => self.fullscreen = !self.fullscreen,
                KeyCode::Char('d') if ctrl => self.move_cursor(CursorMotion::HalfPageDown),
                KeyCode::Char('u') if ctrl => self.move_cursor(CursorMotion::HalfPageUp),
                KeyCode::Down | KeyCode::Char('j') if plain || ctrl => {
                    self.move_cursor(CursorMotion::Next)
                }
                KeyCode::Up | KeyCode::Char('k') if plain || ctrl => {
                    self.move_cursor(CursorMotion::Prev)
                }
                KeyCode::PageDown => self.move_cursor(CursorMotion::PageDown),
                KeyCode::PageUp => self.move_cursor(CursorMotion::PageUp),
                KeyCode::Home | KeyCode::Char('g') if plain => {
                    self.move_cursor(CursorMotion::First)
                }
                KeyCode::End | KeyCode::Char('G')
                    if plain || key.modifiers == KeyModifiers::SHIFT =>
                {
                    self.move_cursor(CursorMotion::Last)
                }
                KeyCode::Left | KeyCode::Char('h' | '[') if plain || ctrl => self.prev_question(),
                KeyCode::Right | KeyCode::Char('l' | ']') if plain || ctrl => self.next_question(),
                KeyCode::Enter | KeyCode::Char(' ') if plain && self.is_on_freeform_row() => {
                    return QuestionInput::EditFreeform;
                }
                KeyCode::Enter if plain => {
                    if self.confirm_option(self.cursor()) {
                        return QuestionInput::Submit;
                    }
                }
                KeyCode::Char(' ') if plain => self.toggle_current_option(),
                KeyCode::Char('z') if plain && !self.no_freeform => {
                    self.move_cursor(CursorMotion::Last);
                    return QuestionInput::EditFreeform;
                }
                KeyCode::Char(ch) if plain => {
                    let Some(index) = option_index_for_key(ch)
                        .filter(|&i| i < self.questions[self.active_tab].options.len())
                    else {
                        return QuestionInput::Unhandled;
                    };
                    if self.confirm_option(index) {
                        return QuestionInput::Submit;
                    }
                }
                _ => return QuestionInput::Unhandled,
            }
        }
        self.ensure_cursor_visible(visible_rows, content_width);
        QuestionInput::Changed
    }
    fn toggle_current_option(&mut self) {
        let active = self.active_tab;
        self.toggle_option(active, self.cursor());
        if matches!(
            self.selections.get(active),
            Some(QuestionSelection::Single(Some(_)))
        ) {
            self.per_question_freeform_selected[active] = false;
        }
    }

    fn confirm_option(&mut self, index: usize) -> bool {
        self.set_cursor(index);
        let active = self.active_tab;
        self.select_option(active, index);
        self.per_question_freeform_selected[active] = false;
        if active + 1 >= self.questions.len() {
            true
        } else {
            self.next_question();
            false
        }
    }
}

impl QuestionViewRenderResult {
    pub fn hit_test(&self, col: u16, row: u16) -> Option<usize> {
        self.option_rects
            .iter()
            .find_map(|&(idx, rect)| rect.contains((col, row).into()).then_some(idx))
    }

    pub fn tab_at(&self, col: u16, row: u16) -> Option<usize> {
        self.tab_rects
            .iter()
            .position(|rect| rect.is_some_and(|rect| rect.contains((col, row).into())))
    }

    pub fn handle_mouse(
        &self,
        state: &mut QuestionViewState,
        area: Rect,
        mouse: &MouseEvent,
    ) -> QuestionInput {
        let left_down = mouse.kind == MouseEventKind::Down(MouseButton::Left);
        if mouse.kind == MouseEventKind::Up(MouseButton::Left) && state.scrollbar_dragging {
            state.scrollbar_dragging = false;
            return QuestionInput::Changed;
        }
        if let Some(scrollbar) = self.scrollbar
            && (left_down && scrollbar.contains((mouse.column, mouse.row).into())
                || state.scrollbar_dragging
                    && mouse.kind == MouseEventKind::Drag(MouseButton::Left))
        {
            let width = usize::from(area.width.saturating_sub(QUESTION_VIEW_HPAD));
            let total =
                total_options_height(&state.questions[state.active_tab], width, state.cursor())
                    .saturating_sub(state.phantom_freeform_h());
            let max_scroll = total.saturating_sub(scrollbar.height);
            let travel = scrollbar.height.saturating_sub(1).max(1) as u32;
            let position = mouse.row.saturating_sub(scrollbar.y).min(travel as u16) as u32;
            state.per_question_scroll[state.active_tab] =
                (position * u32::from(max_scroll) / travel) as u16;
            state.scrollbar_dragging = true;
            return QuestionInput::Changed;
        }
        if !area.contains((mouse.column, mouse.row).into()) {
            return QuestionInput::Unhandled;
        }
        let Some(option_count) = state
            .questions
            .get(state.active_tab)
            .map(|question| question.options.len())
        else {
            return QuestionInput::Unhandled;
        };
        let width = usize::from(area.width.saturating_sub(QUESTION_VIEW_HPAD));
        let visible_rows = self.options_end_y.saturating_sub(self.options_start_y);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(index) = self.tab_at(mouse.column, mouse.row)
                    && index != state.active_tab
                {
                    return QuestionInput::ChangeTab(index);
                }
                if let Some(index) = self.hit_test(mouse.column, mouse.row) {
                    state.set_cursor(index);
                    let active = state.active_tab;
                    if index == option_count && !state.no_freeform {
                        state.last_option_click = None;
                        if state.per_question_freeform_selected[active] {
                            state.per_question_freeform_selected[active] = false;
                            return QuestionInput::Changed;
                        }
                        return QuestionInput::EditFreeform;
                    }
                    let now = Instant::now();
                    if state.last_option_click.is_some_and(|(at, previous)| {
                        previous == index && now.duration_since(at).as_millis() < 300
                    }) {
                        state.last_option_click = None;
                        if state.confirm_option(index) {
                            return QuestionInput::Submit;
                        }
                    } else {
                        state.last_option_click = Some((now, index));
                        state.toggle_current_option();
                    }
                } else {
                    return QuestionInput::Unhandled;
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                if (self.options_start_y..self.options_end_y).contains(&mouse.row) =>
            {
                let scroll = &mut state.per_question_scroll[state.active_tab];
                *scroll = if mouse.kind == MouseEventKind::ScrollUp {
                    scroll.saturating_sub(1)
                } else {
                    scroll.saturating_add(1)
                };
                state.clamp_scroll(visible_rows, width);
            }
            MouseEventKind::Moved => {}
            _ => return QuestionInput::Unhandled,
        }
        QuestionInput::Changed
    }
}
