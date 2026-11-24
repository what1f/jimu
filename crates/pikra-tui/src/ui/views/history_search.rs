// Adapted from the Grok Build pager (Apache-2.0). Copyright 2023-2026 SpaceXAI.

//! Prompt history search with background-thread matching.
//!
//! Architecture mirrors the file search `FuzzyFileMatcherDaemon`:
//! - A background `std::thread` owns the history index and matcher.
//! - The UI thread sends queries via a channel (`set_query`) and never blocks.
//! - The background thread scores items, computes indices, and writes results to `Arc<Mutex<…>>`.
//! - The UI thread polls results on each tick via `poll()`.
//! - The daemon spawns lazily on first activation and is kept for reuse.
//!   Every `PromptWidget` (one per agent view, including subagent child views) owns a `HistorySearchState`.
//!   An eager spawn would therefore leak one parked thread per subagent for the process lifetime.

use std::sync::{
    Arc, Mutex,
    mpsc::{SyncSender, sync_channel},
};
use std::thread::{self, JoinHandle};

use crate::engine::search::matcher::{QueryKind, TextMatcher};

/// A single entry in the prompt history.
#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub text: String,
}

/// A matched result with highlight positions (produced by the daemon).
#[derive(Debug, Clone)]
pub struct HistoryMatchResult {
    pub text: String,
    pub indices: Vec<u32>,
}

#[derive(Clone, Default)]
struct Snapshot {
    items: Arc<[HistoryMatchResult]>,
    generation: usize,
}

enum Msg {
    SetItems(Vec<String>),
    SetItemsAndQuery(Vec<String>, String),
    SetQuery(String),
    Stop,
}

struct Daemon {
    shared: Arc<Mutex<Snapshot>>,
    tx: SyncSender<Msg>,
    _handle: JoinHandle<()>,
}

const MAX_RESULTS: usize = 100;

impl Daemon {
    /// Spawn the matcher thread.
    /// `None` when the spawn fails: no half-constructed daemon whose only effect on drop is a `Stop` into a channel nobody reads.
    fn spawn() -> Option<Self> {
        let shared = Arc::new(Mutex::new(Snapshot::default()));
        let (tx, rx) = sync_channel::<Msg>(256);

        let out = shared.clone();
        let worker = move || {
            let mut items = Vec::new();
            let mut generation = 0usize;
            while let Ok(msg) = rx.recv() {
                let query = match drain_to_latest(msg, &rx) {
                    Msg::SetItems(new) => {
                        items = new;
                        String::new()
                    }
                    Msg::SetItemsAndQuery(new, query) => {
                        items = new;
                        query
                    }
                    Msg::SetQuery(query) => query,
                    Msg::Stop => break,
                };
                generation += 1;
                publish_matches(&items, query.trim(), &out, generation);
            }
        };
        match thread::Builder::new()
            .name("history-search".into())
            .spawn(worker)
        {
            Ok(handle) => Some(Self {
                shared,
                tx,
                _handle: handle,
            }),
            Err(_) => None,
        }
    }
}

fn publish_matches(items: &[String], query: &str, out: &Mutex<Snapshot>, generation: usize) {
    let pattern = query
        .chars()
        .map(|ch| format!("({})", regex::escape(&ch.to_string())))
        .collect::<Vec<_>>()
        .join(".*?");
    let fuzzy =
        (!query.is_empty()).then(|| TextMatcher::new(format!("(?s:{pattern})"), QueryKind::Regex));
    let mut hits: Vec<_> = items
        .iter()
        .filter(|text| !text.is_empty())
        .enumerate()
        .filter_map(|(order, text)| {
            let (indices, gaps) = if let Some(matcher) = &fuzzy {
                let captures = matcher.compiled_regex().captures(text)?;
                let positions: Vec<_> = captures
                    .iter()
                    .skip(1)
                    .flatten()
                    .map(|m| m.start())
                    .collect();
                let matched = captures.get(0)?;
                let gaps = matched
                    .as_str()
                    .chars()
                    .count()
                    .saturating_sub(positions.len());
                let indices = text
                    .char_indices()
                    .enumerate()
                    .filter(|(_, (byte, _))| positions.binary_search(byte).is_ok())
                    .map(|(index, _)| index as u32)
                    .collect();
                (indices, gaps)
            } else {
                (Vec::new(), 0)
            };
            Some((gaps, order, text, indices))
        })
        .collect();
    hits.sort_unstable_by_key(|&(gaps, order, _, _)| (gaps, order));
    hits.truncate(MAX_RESULTS);
    let matches = hits
        .into_iter()
        .rev()
        .map(|(_, _, text, indices)| HistoryMatchResult {
            text: text.clone(),
            indices,
        })
        .collect::<Vec<_>>();
    *out.lock().unwrap() = Snapshot {
        items: matches.into(),
        generation,
    };
}

/// Drain the channel to the most recent message, coalescing queries.
fn drain_to_latest(first: Msg, rx: &std::sync::mpsc::Receiver<Msg>) -> Msg {
    let mut current = first;
    while let Ok(next) = rx.try_recv() {
        current = match (current, next) {
            // Coalesce consecutive SetQuery, keeping the latest
            (Msg::SetQuery(_), next @ Msg::SetQuery(_)) => next,
            // Preserve the item refresh and latest query as one atomic update.
            (Msg::SetItems(items), Msg::SetQuery(query)) => Msg::SetItemsAndQuery(items, query),
            (Msg::SetItemsAndQuery(items, _), Msg::SetQuery(query)) => {
                Msg::SetItemsAndQuery(items, query)
            }
            // Stop always wins.
            (_, stop @ Msg::Stop) => return stop,
            // SetItems after SetQuery: keep the SetItems (a reset)
            (_, next) => next,
        };
    }
    current
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
    }
}

/// Which entry point opened the overlay.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// `/history`: the composer is the filter query.
    Search,
    /// Up on an empty prompt: selection live-populates the composer.
    Browse,
}

/// UI-side state for the history search overlay. The UI thread never runs the matcher; all matching
/// happens on the daemon thread. The UI sends queries via `update_query()` and polls results via
/// `poll()`, exactly like `FuzzyFileMatcherDaemon`.
pub struct HistorySearchState {
    /// Matcher daemon, built lazily on first activation (the module docs say why eager spawning leaks).
    daemon: Option<Daemon>,
    active: bool,
    saved_text: String,
    snapshot: Snapshot,
    last_gen: usize,
    pub selected: usize,
    /// While `true`, selection tracks the bottom-most entry (most recent, or best match) as results stream in.
    /// Set on `activate`, cleared once the user navigates (Up/Down/PageUp/PageDown/click).
    /// This makes the overlay open with the most recent prompt selected at the bottom of the list.
    stick_to_bottom: bool,
    /// The last query sent to the daemon.
    /// Distinguishes a genuine query change (the user typed, so selection re-anchors to the best match) from a re-application of the same query.
    /// A late background `PromptHistoryLoaded` refresh re-sends the current query and must not clobber the user's selection.
    last_query: String,
    /// Mouse-hovered result index (visual highlight only).
    hovered: Option<usize>,
    /// Browse (the Up-arrow entry point): the selection lives in the composer (live-populated on every move).
    /// Typing detaches to edit, and Down at the newest entry closes.
    /// Search (`/history`) keeps the composer as the filter query instead.
    mode: Mode,
}

impl Default for HistorySearchState {
    fn default() -> Self {
        Self::new()
    }
}

impl HistorySearchState {
    pub fn new() -> Self {
        Self {
            daemon: None,
            active: false,
            saved_text: String::new(),
            snapshot: Snapshot::default(),
            last_gen: 0,
            selected: 0,
            stick_to_bottom: true,
            last_query: String::new(),
            hovered: None,
            mode: Mode::Search,
        }
    }

    /// Build the matcher daemon on first activation.
    /// Returns `false` when the thread spawn failed; that attempt is not cached, so a later activation retries (thread pressure is often transient).
    fn ensure_daemon(&mut self) -> bool {
        if self.daemon.is_none() {
            self.daemon = Daemon::spawn();
        }
        self.daemon.is_some()
    }

    /// Send to the daemon.
    /// A disconnected channel means the matcher thread is gone (panicked).
    /// Drop the daemon so the next activation respawns it instead of serving an overlay that never updates.
    fn send(&mut self, msg: Msg) {
        let Some(daemon) = &self.daemon else {
            return;
        };
        if daemon.tx.send(msg).is_err() {
            self.daemon = None;
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn saved_text(&self) -> &str {
        &self.saved_text
    }

    pub fn result_count(&self) -> usize {
        self.snapshot.items.len()
    }

    /// Push the latest history into a running daemon.
    /// No-op before first activation (`activate_inner` re-sends the items it is given, so nothing is lost).
    pub fn refresh_items(&mut self, history: &[HistoryEntry]) {
        if self.daemon.is_none() {
            return;
        }
        let items: Vec<String> = history.iter().map(|e| e.text.clone()).collect();
        if self.last_query.is_empty() {
            self.send(Msg::SetItems(items));
        } else {
            self.send(Msg::SetItemsAndQuery(items, self.last_query.clone()));
        }
    }

    /// Activate in Search mode (`/history`): send items to the daemon and show the overlay.
    /// The composer is the filter query; navigation highlights only, Enter/Tab accepts.
    /// Returns `false` when the matcher thread could not start; the overlay stays closed and callers must leave the composer alone.
    #[must_use]
    pub fn activate(&mut self, history: &[HistoryEntry], current_text: &str) -> bool {
        self.activate_inner(history, current_text, Mode::Search)
    }

    /// Activate in Browse mode (Up on an empty prompt): the same panel, but the caller fills the newest entry straight into the composer.
    /// Every selection move live-populates the composer; typing detaches to edit, and Down at the newest entry closes the panel.
    /// Returns `false` when the matcher thread could not start.
    #[must_use]
    pub fn activate_browse(&mut self, history: &[HistoryEntry], current_text: &str) -> bool {
        self.activate_inner(history, current_text, Mode::Browse)
    }

    fn activate_inner(&mut self, history: &[HistoryEntry], current_text: &str, mode: Mode) -> bool {
        if !self.ensure_daemon() {
            return false;
        }
        self.active = true;
        self.mode = mode;
        self.saved_text = current_text.to_string();
        // Open with the most-recent prompt (rendered at the bottom) selected; `poll` keeps it pinned to the bottom until the user navigates
        self.stick_to_bottom = true;
        self.last_query.clear();
        if let Some(daemon) = &self.daemon {
            self.last_gen = daemon
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .generation;
        }
        self.snapshot = Snapshot::default();
        self.selected = 0;
        self.hovered = None;
        self.refresh_items(history);
        true
    }

    /// True while the overlay is in browse mode (see [`Self::activate_browse`]).
    pub fn is_browse(&self) -> bool {
        self.active && self.mode == Mode::Browse
    }

    /// Deactivate: clear the overlay.
    /// The daemon thread stays alive for reuse, but its copy of the history is released (`activate` re-sends items).
    /// Retained memory is therefore bounded by the time the overlay is open.
    pub fn deactivate(&mut self) {
        self.active = false;
        self.mode = Mode::Search;
        self.snapshot = Snapshot::default();
        self.selected = 0;
        self.send(Msg::SetItems(Vec::new()));
    }

    /// Send a query update to the daemon (non-blocking, never stalls UI).
    pub fn update_query(&mut self, query: &str) {
        if self.daemon.is_none() {
            return;
        }
        // A genuinely new query (the user typed) re-anchors selection to the best match at the bottom
        // Re-applying the *same* query (e.g. a late `PromptHistoryLoaded` refresh) must not move a selection the user has already navigated to.
        if query != self.last_query {
            self.last_query = query.to_string();
            self.stick_to_bottom = true;
        }
        self.send(Msg::SetQuery(query.to_string()));
    }

    /// Poll for new results from the daemon. Returns `true` if changed.
    /// Call on every tick while the overlay is active.
    pub fn poll(&mut self) -> bool {
        if !self.active {
            return false;
        }
        let Some(daemon) = &self.daemon else {
            return false;
        };
        let snap = daemon
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if snap.generation == self.last_gen {
            return false;
        }
        self.last_gen = snap.generation;
        self.snapshot = snap;
        let len = self.snapshot.items.len();
        if len == 0 {
            self.selected = 0;
        } else if self.stick_to_bottom {
            // Keep the bottom entry (most recent, or best match) selected as results stream in or the query narrows
            self.selected = len - 1;
        } else {
            self.selected = self.selected.min(len - 1);
        }
        true
    }

    /// Currently hovered index (mouse-driven), if any.
    pub fn hovered(&self) -> Option<usize> {
        self.hovered
    }

    /// Set hovered index. Returns `true` if changed.
    pub fn set_hovered(&mut self, index: Option<usize>) -> bool {
        let clamped = index.filter(|&i| i < self.snapshot.items.len());
        let changed = clamped != self.hovered;
        self.hovered = clamped;
        changed
    }

    /// Select the hovered item (for click-to-accept). Returns `true` if valid.
    pub fn select_hovered(&mut self) -> bool {
        if let Some(idx) = self.hovered
            && idx < self.snapshot.items.len()
        {
            self.stick_to_bottom = false;
            self.selected = idx;
            true
        } else {
            false
        }
    }

    /// Move the selection one row up (older). No wrap: at the top (oldest) the selection stays put.
    /// Returns `true` when it moved.
    pub fn move_up(&mut self) -> bool {
        let len = self.snapshot.items.len();
        if len == 0 || self.selected == 0 {
            return false;
        }
        self.stick_to_bottom = false;
        self.selected -= 1;
        true
    }

    /// Move the selection one row down (newer). No wrap: returns `false` at the bottom (newest).
    /// The caller closes the overlay there, so a Down right after opening (newest is selected) backs out of history.
    pub fn move_down(&mut self) -> bool {
        let len = self.snapshot.items.len();
        if len == 0 || self.selected >= len - 1 {
            return false;
        }
        self.stick_to_bottom = false;
        self.selected += 1;
        true
    }

    /// Move selection by a page (half of visible height).
    pub fn page_move(&mut self, delta: isize, visible_rows: usize) {
        let half = (visible_rows / 2).max(1) as isize;
        let len = self.snapshot.items.len();
        if len == 0 {
            return;
        }
        self.stick_to_bottom = false;
        let max_idx = len - 1;
        let current = self.selected.min(max_idx);
        self.selected = (current as isize + delta * half).clamp(0, max_idx as isize) as usize;
    }

    /// Selected entry (returns `None`; use `selected_text()` instead).
    pub fn selected(&self) -> Option<&HistoryEntry> {
        // We can't return &HistoryEntry from Arc<[HistoryMatchResult]>; callers should use selected_text()
        // None satisfies the type signature used by the accept logic; the accept path uses selected_text() via a separate check
        None
    }

    /// Text of the currently selected entry.
    pub fn selected_text(&self) -> Option<&str> {
        self.snapshot
            .items
            .get(self.selected)
            .map(|r| r.text.as_str())
    }

    /// Get a result at a given index (for rendering).
    pub fn result_at(&self, idx: usize) -> Option<&HistoryMatchResult> {
        self.snapshot.items.get(idx)
    }
}

#[derive(Clone, Copy)]
pub struct HistoryLabels<'a> {
    pub title: &'a str,
    pub loading: &'a str,
    pub empty: &'a str,
}

impl Default for HistoryLabels<'_> {
    fn default() -> Self {
        Self {
            title: " history ",
            loading: "  Loading…",
            empty: "  No matching history",
        }
    }
}

pub fn history_scroll_offset(state: &HistorySearchState, rows: u16) -> usize {
    state
        .selected
        .saturating_sub(usize::from(rows.saturating_sub(1)))
}

pub fn render_history(
    buf: &mut ratatui::buffer::Buffer,
    area: ratatui::layout::Rect,
    prompt: ratatui::layout::Rect,
    state: &HistorySearchState,
    theme: &crate::engine::theme::Theme,
    loading: bool,
    labels: HistoryLabels<'_>,
) -> Option<super::completion_dropdown::DropdownChrome> {
    use crate::engine::render::{SafeBuf, scrollbar::render_scrollbar_styled};
    use ratatui::{
        layout::Rect,
        style::{Modifier, Style},
        text::Line,
    };
    if !state.is_active() {
        return None;
    }
    let result_count = state.result_count();
    let chrome = super::completion_dropdown::render_dropdown_chrome(
        buf,
        result_count,
        (result_count as u16).clamp(1, 8),
        None,
        prompt,
        area,
        (0, 0),
        "",
        false,
        theme,
    )?;
    let caption_style = theme.muted_over_chrome();
    let items_x = chrome.items.x;
    let items_width = chrome.items.width;
    let item_rows = chrome.items.height;
    let top_border_y = chrome.panel.y;
    let bottom_border_y = chrome.panel.bottom().saturating_sub(1);
    let label_width = chrome.panel.width.saturating_sub(8);
    buf.set_line_safe(
        chrome.panel.x + 1,
        top_border_y,
        &Line::styled(labels.title, caption_style.bg(theme.bg_base)),
        label_width,
    );
    let hover_bg = theme.bg_hover;
    if result_count == 0 {
        let msg_y = top_border_y + 1;
        if msg_y < bottom_border_y {
            let message = if loading {
                labels.loading
            } else {
                labels.empty
            };
            buf.set_string_safe(items_x, msg_y, message, caption_style.bg(theme.bg_light));
        }
    } else {
        let selected = state.selected;
        let visible_rows = item_rows as usize;
        let scroll_offset = history_scroll_offset(state, item_rows);
        let visible_end = result_count.min(scroll_offset + visible_rows);
        let needs_scrollbar = result_count > visible_rows;
        let text_width = if needs_scrollbar {
            items_width.saturating_sub(2)
        } else {
            items_width
        };
        for (vi, ri) in (scroll_offset..visible_end).enumerate() {
            let row_y = top_border_y + 1 + vi as u16;
            if row_y >= bottom_border_y {
                break;
            }
            let result = match state.result_at(ri) {
                Some(r) => r,
                None => continue,
            };
            let is_selected = ri == selected;
            let is_hovered = state.hovered() == Some(ri) && !is_selected;
            let row_bg = if is_selected {
                theme.bg_visual
            } else if is_hovered {
                hover_bg
            } else {
                theme.bg_light
            };
            let bold = if is_selected {
                Modifier::BOLD
            } else {
                Modifier::empty()
            };
            let fill_width = if needs_scrollbar {
                items_width.saturating_sub(1)
            } else {
                items_width
            };
            for col in items_x..items_x + fill_width {
                if let Some(cell) = buf.cell_mut((col, row_y)) {
                    cell.set_char(' ');
                    cell.set_style(Style::default().bg(row_bg));
                }
            }
            let prefix_w: u16 = crate::engine::render::glyphs::PROMPT_ARROW_WIDTH;
            let prefix = if is_selected {
                crate::engine::render::glyphs::prompt_arrow()
            } else {
                "  "
            };
            let pfx_style = Style::default()
                .fg(theme.text_primary)
                .bg(row_bg)
                .add_modifier(bold);
            for (i, ch) in prefix.chars().enumerate() {
                let px = items_x + i as u16;
                if px < items_x + text_width
                    && let Some(cell) = buf.cell_mut((px, row_y))
                {
                    cell.set_char(ch);
                    cell.set_style(if is_selected {
                        pfx_style
                    } else {
                        Style::default().bg(row_bg)
                    });
                }
            }
            let match_style = Style::default()
                .fg(theme.accent_user)
                .bg(row_bg)
                .add_modifier(bold);
            let normal_style = Style::default()
                .fg(theme.text_primary)
                .bg(row_bg)
                .add_modifier(bold);
            let mut indices = result.indices.as_slice();
            let mut col = items_x + prefix_w;
            let max_col = items_x + prefix_w + text_width.saturating_sub(prefix_w);
            let preview: String = result
                .text
                .chars()
                .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
                .collect();
            for (ci, ch) in preview.chars().enumerate() {
                let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
                if col + cw > max_col {
                    if col > items_x + prefix_w
                        && let Some(cell) = buf.cell_mut((col.saturating_sub(1), row_y))
                    {
                        cell.set_char('\u{2026}');
                    }
                    break;
                }
                let is_match = indices.first() == Some(&(ci as u32));
                if is_match {
                    let Some(rest) = indices.get(1..) else { break };
                    indices = rest;
                }
                let st = if is_match { match_style } else { normal_style };
                if let Some(cell) = buf.cell_mut((col, row_y)) {
                    cell.set_symbol(&ch.to_string());
                    cell.set_style(st);
                }
                col += cw;
            }
            let row_rect = Rect {
                x: items_x,
                y: row_y,
                width: fill_width,
                height: 1,
            };
            if is_selected {
                buf.set_style(row_rect, theme.selection_overlay());
            } else if is_hovered {
                buf.set_style(row_rect, theme.hover_overlay());
            }
        }
        if needs_scrollbar {
            let sb_area = Rect {
                x: items_x + items_width - 1,
                y: top_border_y + 1,
                width: 1,
                height: item_rows,
            };
            render_scrollbar_styled(
                buf,
                Some(sb_area),
                result_count as u16,
                item_rows,
                scroll_offset as u16,
                Style::default().bg(theme.bg_dark),
                Style::default().fg(theme.gray_dim).bg(theme.bg_dark),
            );
        }
    }
    Some(chrome)
}
