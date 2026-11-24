// Adapted from the Grok Build pager (Apache-2.0). Copyright 2023-2026 SpaceXAI.

//! Queue pane: renders queued prompts in a `ListPane`.
//!
//! Similar to [`super::todo_pane::TodoPane`] but for queued prompts.
//! Shows `#1`, `#2`, … prefixes (positional, 1-based) with the first line of each prompt's text.
//! Multiline prompts show a `(+N lines)` indicator.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueEntryKind {
    Prompt,
    Command,
    BashCommand,
}

#[derive(Debug, Clone)]
pub struct QueuedPrompt {
    pub id: u64,
    pub text: String,
    pub kind: QueueEntryKind,
}

#[derive(Debug, Clone)]
pub struct QueueEntryWire {
    pub id: String,
    pub text: String,
    pub kind: String,
    pub version: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct QueueLayout {
    pub block_pad_left: u16,
}

impl Default for QueueLayout {
    fn default() -> Self {
        Self { block_pad_left: 2 }
    }
}

use crate::engine::render::line_utils::truncate_str;
use crate::engine::theme::{Theme, ThemeKind};
use crate::ui::views::queue_mutation::{QueueMutation, ServerRowCapabilities};

use super::list_pane::ListItem;

/// Server row is visible in the held pane / "N queued" count: not running, not the pending send-now echo, and not painted-pending.
/// (The painted block represents the message even after the send-now expectation drops; showing the row beside it would duplicate the message.)
#[inline]
pub fn visible_held_server_row(
    id: &str,
    running_id: Option<&str>,
    send_now_id: Option<&str>,
    painted_pending: &std::collections::HashMap<String, (u64, bool)>,
) -> bool {
    let key = id;
    let id = Some(id);
    id != running_id && id != send_now_id && !painted_pending.contains_key(key)
}

/// Where a rendered queue row originates, which determines how an edit (delete / reorder) is routed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueRowOrigin {
    /// A client-local `pending_prompts` entry (skill/image/bash/cron/etc., or any idle-drained prompt). Edits mutate the local queue directly.
    Local,
    /// A server-authoritative `shared_prompt_queues` entry (plain prompt queued while running). Edits route to the agent as `x.ai/queue/*`.
    Server,
}

/// A resolved reference to a queue row, used by the edit handlers to route by origin. Returned by [`QueuePane::row_ref`].
#[derive(Debug, Clone)]
pub struct QueueRowRef {
    pub origin: QueueRowOrigin,
    /// The server `prompt_id` for `Server` rows; `None` for `Local` rows.
    pub server_id: Option<String>,
    /// The server queue-entry version (for versioned removes); 0 for local.
    pub version: u64,
}

/// A `QueuedPrompt` wrapped for display in a `ListPane`.
#[derive(Debug, Clone)]
pub struct QueuedPromptEntry {
    /// Stable selection ID. For local rows this is the `QueuedPrompt.id` monotonic counter.
    /// For server rows it is a stable hash of the server `prompt_id` (so `ListPane` selection works uniformly across origins).
    pub id: u64,
    /// 1-based positional index for display (`#1`, `#2`, …).
    pub position: usize,
    /// The full prompt text (for search/copy).
    pub text: String,
    /// First non-empty line (trimmed) for display.
    first_line: String,
    /// Total line count in the prompt.
    line_count: usize,
    /// Entry kind (Prompt or Command).
    kind: QueueEntryKind,
    /// Row origin: Local (client queue) or Server (shared queue).
    origin: QueueRowOrigin,
    /// Server `prompt_id` for `Server` rows; `None` for local rows.
    server_id: Option<String>,
    /// Server queue-entry version (for versioned removes); 0 for local.
    version: u64,
    /// Mutation/send capabilities under the pane's `QueueMutation`.
    capabilities: ServerRowCapabilities,
    /// Cached styled content (rebuilt with width in `rebuild_styled_for_width`).
    styled: Line<'static>,
}

/// Stable 64-bit id derived from a server `prompt_id` string, so server rows have a `u64` selection id like local rows.
/// Collision with local monotonic counters (small integers) is astronomically unlikely.
fn synth_server_id(prompt_id: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    prompt_id.hash(&mut h);
    // Set the top bit so a synthesized id can never equal a small local monotonic counter (which start at 0 and increment by 1)
    h.finish() | (1 << 63)
}

/// Suffix on a row the pane cannot edit, delete, reorder, or send.
const PROTECTED_MARKER: &str = " · protected";

/// Map a shared-queue wire `kind` string to the local display kind only.
/// Server-only capabilities are projected separately by [`ServerRowCapabilities`].
/// Unknown kinds fall back to a plain prompt.
pub fn kind_from_wire(kind: &str) -> QueueEntryKind {
    match kind {
        "bash" => QueueEntryKind::BashCommand,
        "command" => QueueEntryKind::Command,
        _ => QueueEntryKind::Prompt,
    }
}

impl QueuedPromptEntry {
    /// Create a new entry from a `QueuedPrompt` and its current position.
    pub fn new(prompt: &QueuedPrompt, position: usize, mutation: QueueMutation) -> Self {
        let first_line = prompt
            .text
            .lines()
            .map(|l| l.trim())
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .to_string();

        let line_count = prompt.text.lines().count();
        let capabilities = ServerRowCapabilities::for_local(mutation);

        // Build initial styled line (will be rebuilt with proper width later).
        let styled = Self::build_styled(&first_line, line_count, prompt.kind, None, capabilities);

        Self {
            id: prompt.id,
            position,
            text: prompt.text.clone(),
            first_line,
            line_count,
            kind: prompt.kind,
            origin: QueueRowOrigin::Local,
            server_id: None,
            version: 0,
            capabilities,
            styled,
        }
    }

    /// Create an entry from a server-authoritative shared-queue wire entry.
    /// The wire `kind` selects the display style: plain prompts, bash (`! ` prefix), and the agent-driven cron kind.
    pub fn from_server(wire: &QueueEntryWire, position: usize, mutation: QueueMutation) -> Self {
        let first_line = wire
            .text
            .lines()
            .map(|l| l.trim())
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .to_string();
        let line_count = wire.text.lines().count();
        let kind = kind_from_wire(&wire.kind);
        let capabilities = ServerRowCapabilities::for_pane(&wire.kind, mutation);
        let styled = Self::build_styled(&first_line, line_count, kind, None, capabilities);
        Self {
            id: synth_server_id(&wire.id),
            position,
            text: wire.text.clone(),
            first_line,
            line_count,
            kind,
            origin: QueueRowOrigin::Server,
            server_id: Some(wire.id.clone()),
            version: wire.version,
            capabilities,
            styled,
        }
    }

    /// Rebuild the styled content with proper truncation for the given width.
    ///
    /// The first line is truncated so the `(+N lines)` suffix stays visible.
    pub fn rebuild_styled_for_width(&mut self, available_width: u16) {
        self.styled = Self::build_styled(
            &self.first_line,
            self.line_count,
            self.kind,
            Some(available_width as usize),
            self.capabilities,
        );
    }

    /// Build the styled `Line` for display.
    ///
    /// If `max_width` is provided, the first line is truncated so the gray suffix fits: `(+N lines)` for a
    /// multiline entry, then the protected marker, which is dropped when the text, after the `(+N lines)`
    /// suffix is reserved, would keep less than the marker's own width.
    fn build_styled(
        first_line: &str,
        line_count: usize,
        kind: QueueEntryKind,
        max_width: Option<usize>,
        capabilities: ServerRowCapabilities,
    ) -> Line<'static> {
        let theme = Theme::current();
        let extra_lines = line_count.saturating_sub(1);

        // Build the suffix for multiline prompts: " (+N lines)" or " (+1 line)"
        let mut suffix = if extra_lines > 0 {
            if extra_lines == 1 {
                " (+1 line)".to_string()
            } else {
                format!(" (+{extra_lines} lines)")
            }
        } else {
            String::new()
        };
        let marker_width = PROTECTED_MARKER.width();
        if capabilities.is_protected()
            && max_width
                .is_none_or(|w| w.saturating_sub(suffix.width() + marker_width) >= marker_width)
        {
            suffix.push_str(PROTECTED_MARKER);
        }
        let suffix_width = suffix.width();

        let content_max_width = max_width.map(|w| w.saturating_sub(suffix_width));

        match kind {
            QueueEntryKind::Prompt => {
                let display_text = if let Some(max_w) = content_max_width {
                    truncate_str(first_line, max_w)
                } else {
                    first_line.to_string()
                };

                let mut spans = vec![Span::styled(
                    display_text,
                    Style::default().fg(theme.accent_user),
                )];

                if !suffix.is_empty() {
                    spans.push(Span::styled(suffix, Style::default().fg(theme.gray)));
                }

                Line::from(spans)
            }
            QueueEntryKind::Command => {
                // Slash commands: `/command` in magenta, args (if any) in bright gray.
                let trimmed = first_line.trim();

                // Truncate the whole line first, then split into command and args
                let truncated = if let Some(max_w) = content_max_width {
                    truncate_str(trimmed, max_w)
                } else {
                    trimmed.to_string()
                };

                let mut spans = if let Some(space_idx) = truncated.find(' ') {
                    let (cmd, args) = truncated.split_at(space_idx);
                    vec![
                        Span::styled(cmd.to_string(), Style::default().fg(theme.accent_assistant)),
                        Span::styled(args.to_string(), Style::default().fg(theme.gray_bright)),
                    ]
                } else {
                    vec![Span::styled(
                        truncated,
                        Style::default().fg(theme.accent_assistant),
                    )]
                };

                if !suffix.is_empty() {
                    spans.push(Span::styled(suffix, Style::default().fg(theme.gray)));
                }

                Line::from(spans)
            }
            QueueEntryKind::BashCommand => {
                // Bash commands: `! ` prefix in yellow (theme.command), command text in yellow.
                let display_text = if let Some(max_w) = content_max_width {
                    // Reserve 2 chars for "! " prefix.
                    truncate_str(first_line, max_w.saturating_sub(2))
                } else {
                    first_line.to_string()
                };

                let mut spans = vec![
                    Span::styled("! ", Style::default().fg(theme.command)),
                    Span::styled(display_text, Style::default().fg(theme.command)),
                ];

                if !suffix.is_empty() {
                    spans.push(Span::styled(suffix, Style::default().fg(theme.gray)));
                }

                Line::from(spans)
            }
        }
    }
}

impl ListItem for QueuedPromptEntry {
    fn content(&self) -> &Line<'_> {
        &self.styled
    }

    fn prefix(&self) -> Option<Line<'_>> {
        let theme = Theme::current();
        Some(Line::from(vec![
            Span::styled(
                format!("#{}", self.position),
                Style::default().fg(theme.gray),
            ),
            Span::raw(" "),
        ]))
    }

    fn stable_id(&self) -> u64 {
        self.id
    }

    fn search_text(&self) -> &str {
        &self.text
    }

    fn copy_text(&self) -> String {
        self.text.clone()
    }
}

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::StatefulWidget;

use super::support::preview_overlay::{PreviewConfig, PreviewStyle, render_preview_overlay};

use super::list_pane::{ListPane, ListPaneConfig, ListPaneState, ListPaneStyle, WrapMode};
use super::overlay::OverlayState;

/// Event returned by [`QueuePane::handle_key`] signaling intent to the caller. The queue pane never
/// mutates business state (the prompt queue) directly. It signals intent via these events, and
/// `AgentView::handle_queue_key` performs the actual mutation on `AgentSession::pending_prompts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueEvent {
    /// Delete the selected prompt from the queue.
    DeleteSelected { id: u64 },
    /// Edit the selected prompt (load into prompt widget).
    EditSelected { id: u64 },
    /// Swap the selected prompt with its neighbor above.
    SwapUp { id: u64 },
    /// Swap the selected prompt with its neighbor below.
    SwapDown { id: u64 },
    /// Force-send the selected prompt as a mid-turn interjection.
    ForceInterject { id: u64 },
}

/// Maximum height (in lines) the queue pane will request.
const MAX_QUEUE_HEIGHT: u16 = 3;

/// Hit-test and hover state for one per-row action button (`[edit]`, `[Send now]`, `[cancel]`).
/// Rect and row binding are rebound on every `QueuePane::render`; hover persists across frames so a stationary pointer keeps its highlight.
#[derive(Default)]
struct RowActionButton {
    /// Screen rect from the last render; `None` when the button didn't render.
    rect: Option<Rect>,
    /// Entry id the rendered button acts on.
    entry_id: Option<u64>,
    /// Entry id whose button is under the mouse, if any. Drives the button's hover fg color.
    hovered_id: Option<u64>,
}

impl RowActionButton {
    /// Drop the previous frame's rect/row binding (start of each render).
    fn reset(&mut self) {
        self.rect = None;
        self.entry_id = None;
    }

    /// Bind the just-rendered button to its screen rect and row.
    fn bind(&mut self, rect: Rect, id: u64) {
        self.rect = Some(rect);
        self.entry_id = Some(id);
    }

    /// Row id the button acts on when `(col, row)` hits its rect. `fallback` is the pane's selected row id.
    fn hit(&self, col: u16, row: u16, fallback: Option<u64>) -> Option<u64> {
        let rect = self.rect?;
        if rect.contains((col, row).into()) {
            self.entry_id.or(fallback)
        } else {
            None
        }
    }

    /// Update the hover from the mouse position. Returns `true` if it changed (caller redraws).
    fn update_hover(&mut self, col: u16, row: u16, fallback: Option<u64>) -> bool {
        let over = self.rect.is_some_and(|r| r.contains((col, row).into()));
        let new_id = if over {
            self.entry_id.or(fallback)
        } else {
            None
        };
        if new_id != self.hovered_id {
            self.hovered_id = new_id;
            true
        } else {
            false
        }
    }

    /// Clear the hover (mouse left the queue pane). Returns `true` if it was previously hovered (caller should redraw).
    fn clear_hover(&mut self) -> bool {
        if self.hovered_id.is_some() {
            self.hovered_id = None;
            true
        } else {
            false
        }
    }

    /// Whether the button bound to row `id` is under the mouse.
    fn is_hovered_for(&self, id: u64) -> bool {
        self.hovered_id == Some(id)
    }
}

/// Self-contained queue pane component. Does NOT own the queue data: entries are rebuilt each frame
/// from `AgentSession::pending_prompts`. Owns `ListPaneState` for scroll, selection, and rendering
/// state.
pub struct QueuePane {
    pub labels: QueueLabels,
    /// Styled entries for `ListPane` rendering.
    /// Rebuilt from the queue at the start of each `render()` call.
    entries: Vec<QueuedPromptEntry>,
    /// List pane state (scroll, selection, search, layout cache).
    pub list_state: ListPaneState,
    /// Visual style for the list pane framework.
    list_style: ListPaneStyle,
    /// Theme kind at the last render. Used to detect a theme switch and refresh `list_style`, whose `selection_bg` is captured from the theme.
    /// (Otherwise the focused-row highlight keeps the previous theme's `bg_highlight`, e.g. Dark's dark band leaking into Light.)
    last_theme: ThemeKind,
    /// Shared visibility/focus state.
    pub overlay: OverlayState,
    /// Previous queue length, used for auto-show detection.
    prev_len: usize,
    /// How rows get their capabilities; applied on every `sync_from_merged` rebuild.
    mutation: QueueMutation,
    /// `[Send now]` (force-interject) action button.
    send_now: RowActionButton,
    /// `[cancel]` (row delete) action button.
    delete_button: RowActionButton,
    /// `[edit]` (queued-row edit) action button.
    edit_button: RowActionButton,
    /// Entry id of the row currently under the mouse cursor, if any.
    /// Drives the hover affordance: the hovered row reveals its action buttons just like the selected row does when the pane is focused.
    hovered_row_id: Option<u64>,
    /// Content area from the last `render`, used to hit-test which row the mouse is over (for `hovered_row_id`).
    last_inner: Option<Rect>,
}

impl Default for QueuePane {
    fn default() -> Self {
        Self::new()
    }
}

impl QueuePane {
    /// Create a new empty queue pane.
    pub fn new() -> Self {
        let config = ListPaneConfig {
            follow_enabled: false,
            wrap_toggle_enabled: false,
            search_enabled: false, // Queue is small, no search needed
            copy_enabled: true,
            show_selection_when_unfocused: false,
            visual_select_enabled: false,
            filter_enabled: false,
            goto_line_enabled: false,
        };
        let mut list_state = ListPaneState::new_with_config(WrapMode::NoWrap, false, config);
        list_state.set_clipboard_provider(Box::new(crate::engine::clipboard::SystemClipboard));
        Self {
            labels: QueueLabels::default(),
            entries: Vec::new(),
            list_state,
            list_style: ListPaneStyle::default(),
            last_theme: Theme::current_kind(),
            overlay: OverlayState::hidden(),
            prev_len: 0,
            mutation: QueueMutation::PerRowKind,
            send_now: RowActionButton::default(),
            delete_button: RowActionButton::default(),
            edit_button: RowActionButton::default(),
            hovered_row_id: None,
            last_inner: None,
        }
    }

    /// Takes effect at the next `sync_from_merged`, which every queue change triggers.
    pub fn set_mutation(&mut self, mutation: QueueMutation) {
        self.mutation = mutation;
    }

    pub fn mutation(&self) -> QueueMutation {
        self.mutation
    }

    /// This is only correct because every server-queued prompt is older than every local one.
    /// `immediate_server_send_eligible` enforces that invariant. A prompt may take the immediate-send
    /// (server) path only while the local queue is empty.
    pub fn sync_from_merged(
        &mut self,
        local: &std::collections::VecDeque<QueuedPrompt>,
        server: &[QueueEntryWire],
        running_id: Option<&str>,
        send_now_id: Option<&str>,
        painted_pending: &std::collections::HashMap<String, (u64, bool)>,
    ) {
        // Rebuild entries; positions are recomputed 1-based across the union.
        self.entries.clear();
        let mut pos = 1;
        for wire in server {
            if !visible_held_server_row(&wire.id, running_id, send_now_id, painted_pending) {
                continue;
            }
            self.entries
                .push(QueuedPromptEntry::from_server(wire, pos, self.mutation));
            pos += 1;
        }
        for prompt in local {
            self.entries
                .push(QueuedPromptEntry::new(prompt, pos, self.mutation));
            pos += 1;
        }

        // A server row leaves the list the moment it starts running, is sent now, or gets painted
        // Nothing else re-points the selection, and `selected_id` is returned without checking it still exists
        // A focused pane would then keep acting on a row that is gone
        if let Some(selected) = self.list_state.selected_id()
            && !self.entries.iter().any(|e| e.id == selected)
        {
            match self.entries.last() {
                Some(last) => self.list_state.select_by_id(last.id),
                None => self.list_state.clear_selection(),
            }
        }

        let new_len = self.entries.len();

        // Auto-show when hidden and the queue grew, or went from empty to non-empty (after external hide reset)
        if new_len > 0 && !self.overlay.visible && (self.prev_len == 0 || new_len > self.prev_len) {
            self.overlay.visible = true;
        }

        if new_len == 0 && self.overlay.visible {
            self.overlay.visible = false;
            self.overlay.focused = false;
        }

        // Hidden and empty: pin prev_len so the next enqueue always recovers
        if new_len == 0 && !self.overlay.visible {
            self.prev_len = 0;
        } else {
            self.prev_len = new_len;
        }
    }

    /// Selection ids of the current rows, in display order.
    pub fn entry_ids(&self) -> Vec<u64> {
        self.entries.iter().map(|e| e.id).collect()
    }

    /// Resolve a row's origin metadata by its selection id (for edit routing).
    pub fn row_ref(&self, id: u64) -> Option<QueueRowRef> {
        self.entries
            .iter()
            .find(|e| e.id == id)
            .map(|e| QueueRowRef {
                origin: e.origin,
                server_id: e.server_id.clone(),
                version: e.version,
            })
    }

    /// Internal capabilities for the selected merged row.
    pub fn row_capabilities(&self, id: u64) -> Option<ServerRowCapabilities> {
        self.entries
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| entry.capabilities)
    }

    /// Whether the pane should be visible in the layout.
    pub fn is_visible(&self) -> bool {
        self.overlay.visible && !self.entries.is_empty()
    }

    /// Called after overlay state changes that hide the pane.
    pub fn on_state_change(&mut self) {
        if !self.overlay.visible {
            self.list_state.close_input_bar();
        }
    }

    /// Reset auto-show edge after external hide on an empty merged queue.
    pub fn reset_auto_show_edge(&mut self) {
        self.prev_len = 0;
    }

    /// Desired height in lines for layout computation. Returns 0 when hidden or empty. Otherwise:
    /// `min(queue.len(), MAX_QUEUE_HEIGHT)`.
    pub fn desired_height(&self) -> u16 {
        if !self.is_visible() {
            return 0;
        }
        (self.entries.len() as u16).clamp(1, MAX_QUEUE_HEIGHT)
    }

    /// Handle a key event when the queue pane is focused. Returns `Some(QueueEvent)` for queue-specific
    /// actions (delete, edit, reorder). Returns `None` if the key wasn't a queue action; the caller
    /// should then try `handle_navigation_key` for j/k etc.
    pub fn handle_key(
        &self,
        key: &KeyEvent,
        interject: crate::engine::input::key::KeyShortcut,
    ) -> Option<QueueEvent> {
        let id = self.selected_id()?;
        let capabilities = self.row_capabilities(id)?;
        if interject.matches(key) {
            return capabilities
                .can_send_now()
                .then_some(QueueEvent::ForceInterject { id });
        }
        match key.code {
            KeyCode::Char('x') | KeyCode::Delete | KeyCode::Backspace => capabilities
                .can_delete()
                .then_some(QueueEvent::DeleteSelected { id }),
            KeyCode::Char('e') | KeyCode::Enter => capabilities
                .can_edit()
                .then_some(QueueEvent::EditSelected { id }),
            KeyCode::Char('J')
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    || key.modifiers.contains(KeyModifiers::NONE) =>
            {
                // Shift+J or uppercase J (same thing in practice)
                if key.code == KeyCode::Char('J') {
                    capabilities
                        .can_reorder()
                        .then_some(QueueEvent::SwapDown { id })
                } else {
                    None
                }
            }
            KeyCode::Char('K')
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    || key.modifiers.contains(KeyModifiers::NONE) =>
            {
                if key.code == KeyCode::Char('K') {
                    capabilities
                        .can_reorder()
                        .then_some(QueueEvent::SwapUp { id })
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Handle navigation keys (j/k, scrolling, etc.).
    ///
    /// Delegates to ListPane. Returns `true` if consumed.
    pub fn handle_navigation_key(&mut self, key: &KeyEvent) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        self.list_state.handle_key_event(key, &self.entries)
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        self.list_state.handle_paste(text, &self.entries)
    }

    /// Get the stable ID of the currently selected entry, if any.
    pub fn selected_id(&self) -> Option<u64> {
        self.list_state.selected_id()
    }

    /// Get the full text of the currently selected entry, if any.
    ///
    /// Used to determine if the entry is multiline (for preview overlay).
    pub fn selected_text(&self) -> Option<&str> {
        let id = self.list_state.selected_id()?;
        self.entries
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.text.as_str())
    }

    pub fn yank_copy_target(&self) -> Option<(u64, String)> {
        let entry = self
            .hovered_row_id
            .and_then(|id| self.entries.iter().find(|e| e.id == id))
            .or_else(|| {
                self.selected_id()
                    .and_then(|id| self.entries.iter().find(|e| e.id == id))
            })?;
        Some((entry.id, entry.copy_text()))
    }

    /// Select the deleted row's neighbor (so the cursor doesn't jump to the top).
    /// The neighbor is resolved from the merged `entries` to stay correct across the server/local boundary.
    /// Call before the next `sync_from_merged` rebuild.
    pub fn select_after_delete(&mut self, deleted_id: u64) {
        let Some(pos) = self.entries.iter().position(|e| e.id == deleted_id) else {
            return;
        };
        // `pos` exists, so `entries` is non-empty and `len() - 1` is safe.
        let survivors = self.entries.len() - 1;
        if survivors == 0 {
            return;
        }
        let new_pos = pos.min(survivors - 1);
        if let Some(entry) = self
            .entries
            .iter()
            .filter(|e| e.id != deleted_id)
            .nth(new_pos)
        {
            self.list_state.select_by_id(entry.id);
        }
    }

    /// Handle a mouse scroll event over the queue pane area.
    pub fn handle_scroll(&mut self, lines: i32, col: u16, row: u16) {
        let max = match self.list_state.viewport_height() {
            0..=5 => 1,
            _ => lines.unsigned_abs() as i32,
        };
        let capped = lines.signum() * lines.abs().min(max);
        self.list_state
            .handle_scroll_event(capped, col, row, &self.entries);
        // Rows move under a stationary cursor while wheel-scrolling, but hover is otherwise only refreshed on `Moved` events
        // Re-resolve the hovered row from the pointer's current position against the just-updated scroll offset
        // The hover bg and action buttons then track the row now under the pointer instead of the pre-scroll entry
        self.update_row_hover(col, row);
    }

    /// Handle a mouse click/drag event over the queue pane area.
    ///
    /// Returns `true` if the event was consumed.
    pub fn handle_mouse(&mut self, kind: MouseEventKind, col: u16, row: u16, area: Rect) -> bool {
        self.list_state
            .handle_mouse_event(kind, col, row, area, &self.entries)
    }

    /// Hit-test the action-row `[Interject]` button. Returns the id of the row the button belongs to (the hovered row, else the selected row).
    pub fn send_now_click(&self, col: u16, row: u16) -> Option<u64> {
        self.send_now.hit(col, row, self.selected_id())
    }

    /// Hit-test the action-row `[cancel]` button (removes the row).
    pub fn delete_click(&self, col: u16, row: u16) -> Option<u64> {
        self.delete_button.hit(col, row, self.selected_id())
    }

    /// Hit-test the action-row `[edit]` button (opens the queued-row edit).
    pub fn edit_click(&self, col: u16, row: u16) -> Option<u64> {
        self.edit_button.hit(col, row, self.selected_id())
    }

    /// Update the `[cancel]` hover. Returns `true` if it changed (caller redraws).
    pub fn update_delete_hover(&mut self, col: u16, row: u16) -> bool {
        let selected = self.selected_id();
        self.delete_button.update_hover(col, row, selected)
    }

    /// Clear the `[cancel]` hover. Returns `true` if it was previously hovered.
    pub fn clear_delete_hover(&mut self) -> bool {
        self.delete_button.clear_hover()
    }

    /// Update the `[Interject]` hover (drives the brighten-on-hover fg, same as the `[Dashboard]` button). Returns `true` if it changed.
    pub fn update_send_now_hover(&mut self, col: u16, row: u16) -> bool {
        let selected = self.selected_id();
        self.send_now.update_hover(col, row, selected)
    }

    /// Clear the `[Interject]` hover. Returns `true` if it was previously hovered.
    pub fn clear_send_now_hover(&mut self) -> bool {
        self.send_now.clear_hover()
    }

    /// Update the `[edit]` hover. Returns `true` if it changed (caller redraws).
    pub fn update_edit_hover(&mut self, col: u16, row: u16) -> bool {
        let selected = self.selected_id();
        self.edit_button.update_hover(col, row, selected)
    }

    /// Clear the `[edit]` hover. Returns `true` if it was previously hovered.
    pub fn clear_edit_hover(&mut self) -> bool {
        self.edit_button.clear_hover()
    }

    /// Update which row the mouse is hovering over (the row, not just its delete button).
    /// Drives the action-button reveal on hover. Returns `true` if the hovered row changed (caller should redraw).
    pub fn update_row_hover(&mut self, col: u16, row: u16) -> bool {
        let new_id = self.row_id_at(col, row);
        if new_id != self.hovered_row_id {
            self.hovered_row_id = new_id;
            true
        } else {
            false
        }
    }

    /// Clear the hovered row (mouse left the queue pane). Returns `true` if a row was previously hovered (caller should redraw).
    pub fn clear_row_hover(&mut self) -> bool {
        if self.hovered_row_id.is_some() {
            self.hovered_row_id = None;
            true
        } else {
            false
        }
    }

    /// Resolve the entry id at screen position `(col, row)` using the last rendered content area and the list layout.
    /// `None` when the position is outside the rows.
    fn row_id_at(&self, col: u16, row: u16) -> Option<u64> {
        let inner = self.last_inner?;
        if !inner.contains((col, row).into()) {
            return None;
        }
        let ry = row.saturating_sub(inner.y) as usize;
        let vy = self.list_state.scroll_offset() + ry;
        let idx = self.list_state.layout().item_at_y(vy)?;
        self.entries.get(idx).map(|e| e.id)
    }

    /// That button is likewise right-aligned to the full width (it insets only on the left).
    fn content_area(area: Rect, layout_cfg: &QueueLayout) -> Rect {
        let pad_left = 1 + layout_cfg.block_pad_left.saturating_sub(1);
        Rect {
            x: area.x + pad_left,
            y: area.y,
            width: area.width.saturating_sub(pad_left),
            height: area.height,
        }
    }

    /// Render the queue pane into the given area.
    ///
    /// If `overlay_area` is provided and the selected entry is multiline, renders a preview overlay showing the full content.
    pub fn render(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        focused: bool,
        layout_cfg: &QueueLayout,
        overlay_area: Option<Rect>,
        can_send_now: bool,
    ) {
        // Detect a theme switch and refresh the list style. Without this it would keep the theme active at
        // construction (default Dark) after the user switches.
        let current_theme = Theme::current_kind();
        if current_theme != self.last_theme {
            self.last_theme = current_theme;
            self.list_style = ListPaneStyle::default();
        }

        let inner = Self::content_area(area, layout_cfg);
        self.last_inner = Some(inner);
        if self.entries.is_empty() {
            return;
        }

        // Rebuild styled content with proper width for truncation.
        // Account for prefix width: "#N " where N is the position (1-based).
        // Max position determines prefix width: #1-#9 take 3 chars, #10-#99 take 4, etc
        let max_pos = self.entries.len();
        let prefix_width = 2 + digit_count(max_pos); // "#" + digits + " "
        let content_width = (inner.width as usize).saturating_sub(prefix_width);
        for entry in &mut self.entries {
            entry.rebuild_styled_for_width(content_width as u16);
        }

        // When the queue overflows, the ListPane reserves its scrollbar in the last column of its render area
        // That column is where the right-aligned action buttons ([cancel]) sit
        // Render the list one column wider (borrowing right outer padding) to push the scrollbar just past the buttons instead of underneath them
        let list_area = if self.entries.len() as u16 > inner.height {
            Rect {
                width: inner.width + 1,
                ..inner
            }
        } else {
            inner
        };
        self.list_state
            .prepare_layout(&self.entries, list_area.width, list_area.height);
        ListPane::new(&self.entries)
            .focused(focused)
            .style(self.list_style)
            .render(list_area, buf, &mut self.list_state);

        // Hover affordance: paint the same dim row-hover bg the scrollback and dock use.
        // Selection wins: skip when the hovered row is the focused selection, which already carries the (stronger) selection bg.
        let hovered_idx = self
            .hovered_row_id
            .and_then(|id| self.entries.iter().position(|e| e.id == id));
        let selected_idx = if focused {
            self.list_state.selected_index()
        } else {
            None
        };
        if let Some(idx) = hovered_idx
            && hovered_idx != selected_idx
        {
            // Skip when the hovered row is scrolled outside the viewport (e.g. a stale hover after a wheel-scroll).
            // A plain saturating_sub would collapse an above-viewport row to row 0 and mis-paint it
            let item_y = self.list_state.layout().virtual_y(idx);
            if let Some(rel) = item_y.checked_sub(self.list_state.scroll_offset())
                && rel < inner.height as usize
            {
                let screen_y = inner.y + rel as u16;
                let hover_bg = Theme::current().row_hover_bg();
                let row = Rect::new(inner.x, screen_y, inner.width, 1);
                buf.set_style(row, Style::default().bg(hover_bg));
            }
        }

        // Action buttons (optional [Send now], then [edit], then [cancel], right-aligned)
        // They render for the row under the mouse (hover affordance) or, when the pane is focused, the selected row
        // Hover takes precedence so mouse users can act on any row without focusing/selecting it first
        self.send_now.reset();
        self.delete_button.reset();
        self.edit_button.reset();
        let action_idx = self
            .hovered_row_id
            .and_then(|id| self.entries.iter().position(|e| e.id == id))
            .or_else(|| {
                if focused {
                    self.list_state.selected_index()
                } else {
                    None
                }
            });
        if let Some(idx) = action_idx
            && let Some(entry) = self.entries.get(idx)
        {
            use crate::engine::render::SafeBuf;
            let theme = Theme::current();
            // Skip when the action row is scrolled outside the viewport
            // A plain saturating_sub would otherwise bind the buttons to row 0 and mis-route clicks to an off-screen entry
            let item_y = self.list_state.layout().virtual_y(idx);
            if let Some(rel) = item_y.checked_sub(self.list_state.scroll_offset())
                && rel < inner.height as usize
            {
                let screen_y = inner.y + rel as u16;
                let btn_style = Style::default().fg(theme.gray);
                // Right-to-left walk. A button renders only when its whole label fits at or right of `inner.x`.
                // Saturating toward 0 would paint into the left gutter and overlap already-placed buttons
                // A `checked_sub` underflow means the label doesn't fit, not that x is 0
                let mut right = inner.x + inner.width;
                let fits = |right: u16, w: u16| right.checked_sub(w).filter(|&x| x >= inner.x);

                let cancel_label = self.labels.cancel;
                let cancel_w = cancel_label.width() as u16;
                if entry.capabilities.can_delete()
                    && let Some(cancel_x) = fits(right, cancel_w)
                {
                    right = cancel_x;
                    let cancel_style = if self.delete_button.is_hovered_for(entry.id) {
                        Style::default().fg(theme.accent_error)
                    } else {
                        btn_style
                    };
                    buf.set_string_safe(cancel_x, screen_y, cancel_label, cancel_style);
                    self.delete_button
                        .bind(Rect::new(cancel_x, screen_y, cancel_w, 1), entry.id);
                }

                let interject_label = self.labels.send_now;
                let interject_w = interject_label.width() as u16;
                let show_send_now = can_send_now && entry.capabilities.can_send_now();

                // [edit] always paints; keyboard `e` works either way. Flush to
                // neighbours so the queued message cannot leak through a gap.
                // Drop [edit] if [Send now] fits alone but not with [edit].
                let edit_label = self.labels.edit;
                let edit_w = edit_label.width() as u16;
                let send_now_fits_alone = show_send_now && fits(right, interject_w).is_some();
                if entry.capabilities.can_edit()
                    && (!send_now_fits_alone || fits(right, interject_w + edit_w).is_some())
                    && let Some(edit_x) = fits(right, edit_w)
                {
                    right = edit_x;
                    let edit_style = if self.edit_button.is_hovered_for(entry.id) {
                        Style::default().fg(theme.text_primary)
                    } else {
                        btn_style
                    };
                    buf.set_string_safe(edit_x, screen_y, edit_label, edit_style);
                    self.edit_button
                        .bind(Rect::new(edit_x, screen_y, edit_w, 1), entry.id);
                }

                if show_send_now && let Some(interject_x) = fits(right, interject_w) {
                    // Brighten the fg on hover (same hover color as the [Dashboard] button) so it reads as clickable
                    let interject_style = if self.send_now.is_hovered_for(entry.id) {
                        Style::default().fg(theme.text_primary)
                    } else {
                        btn_style
                    };
                    buf.set_string_safe(interject_x, screen_y, interject_label, interject_style);
                    self.send_now
                        .bind(Rect::new(interject_x, screen_y, interject_w, 1), entry.id);
                }
            }
        }

        // Multiline preview overlay: show when focused and selected entry has 2+ lines.
        if focused
            && let Some(overlay) = overlay_area
            && let Some(text) = self.selected_text()
            && text.lines().count() > 1
        {
            let theme = Theme::current();
            // Use accent_user for text (matches prompt color in queue list).
            let style = PreviewStyle::new(theme.bg_dark, theme.accent_user, theme.gray_dim);
            render_preview_overlay(buf, overlay, text, style, PreviewConfig::default());
        }
    }
}

fn digit_count(n: usize) -> usize {
    if n == 0 {
        1
    } else {
        (n as f64).log10().floor() as usize + 1
    }
}

#[derive(Clone, Copy, Debug)]
pub struct QueueLabels {
    pub cancel: &'static str,
    pub send_now: &'static str,
    pub edit: &'static str,
}

impl Default for QueueLabels {
    fn default() -> Self {
        Self {
            cancel: "[cancel]",
            send_now: "[Send now]",
            edit: "[edit]",
        }
    }
}
