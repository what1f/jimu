//! Full-text search over scrollback text, scanned on a background thread.
//!
//! [`ScrollbackSearchIndex`] caches each entry's [`searchable_text`], keyed by the scrollback's `content_generation`.
//! The searchable text is rendered plain text for markdown blocks and the stored source for the rest.
//! It holds one owned `String` per entry (never a per-rendered-line `Vec`).
//! Memory therefore tracks total searchable-text size rather than rendered-line count.
//!
//! The cache is shared as an `Arc<[IndexedEntry]>` so it can be handed to a background [`SearchDaemon`] without re-cloning the strings.
//! The daemon runs the regex scan off the input thread.
//! Query mutations only enqueue the latest corpus and query (O(1) on the UI thread).
//! [`ScrollbackSearchState::poll`] picks up results once the scan completes.
//! This keeps per-keystroke typing responsive on long sessions where a synchronous scan would stall the input thread.
//!
//! [`searchable_text`]: super::block::RenderBlock::searchable_text

use std::ops::Range;
use std::sync::{
    Arc, Mutex,
    mpsc::{Receiver, Sender, channel},
};
use std::thread::{self, JoinHandle};

use crossterm::event::KeyEvent;

use super::entry::EntryId;
use super::state::ScrollbackState;
use crate::engine::input::line_editor::{LineEditOutcome, LineEditor};
use crate::engine::search::{QueryKind, TextMatcher};

/// A located query match within the scrollback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollbackMatch {
    /// Entry the match was found in.
    pub entry_id: EntryId,
    /// Zero-based line index of the match within the entry's searchable text.
    pub line_in_entry: usize,
    /// Byte range within the entry's searchable text; distinguishes matches that share a `line_in_entry`.
    pub byte_range: Range<usize>,
}

/// Per-entry cache of searchable source text plus a query scan over it. `sync` rebuilds the cache only when
/// scrollback content changes. `find` scans the cache without touching the scrollback.
#[derive(Debug, Default)]
pub struct ScrollbackSearchIndex {
    entries: Arc<[IndexedEntry]>,
    /// `content_generation` the cache was built from; `None` before first sync.
    built_generation: Option<u64>,
}

#[derive(Debug)]
struct IndexedEntry {
    id: EntryId,
    text: String,
}

impl ScrollbackSearchIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild the cached text when scrollback content changed since the last sync. A no-op when `content_generation`
    /// is unchanged, so scrolling and viewport changes never trigger work.
    pub fn sync(&mut self, state: &ScrollbackState) -> bool {
        if self.built_generation == Some(state.content_generation()) {
            return false;
        }
        self.entries = state
            .iter_entries()
            .filter_map(|(id, entry)| {
                entry
                    .block
                    .searchable_text()
                    .map(|text| IndexedEntry { id, text })
            })
            .collect::<Vec<_>>()
            .into();
        self.built_generation = Some(state.content_generation());
        true
    }

    /// A cheap pointer clone of the cached corpus, for handing to the daemon.
    fn entries_arc(&self) -> Arc<[IndexedEntry]> {
        self.entries.clone()
    }

    /// All matches for `matcher`, in scrollback order. Call `sync` first so the cache reflects current content. An
    /// empty query yields nothing (an empty pattern would otherwise match at every byte).
    pub fn find(&self, matcher: &TextMatcher) -> Vec<ScrollbackMatch> {
        scan_matches(&self.entries, matcher)
    }
}

/// Scan `entries` for every match of `matcher`, in scrollback order.
/// Shared by the synchronous [`ScrollbackSearchIndex::find`] and the background [`SearchDaemon`].
/// An empty query yields nothing (an empty pattern would otherwise match at every byte); zero-width matches are skipped for the same reason.
fn scan_matches(entries: &[IndexedEntry], matcher: &TextMatcher) -> Vec<ScrollbackMatch> {
    if matcher.query().is_empty() {
        return Vec::new();
    }
    let regex = matcher.compiled_regex();
    let mut matches = Vec::new();
    for entry in entries {
        // Matches arrive in ascending byte order, so walk newlines forward once per entry to label each match's line instead of rescanning
        let mut line = 0usize;
        let mut counted_to = 0usize;
        for m in regex.find_iter(&entry.text) {
            if m.start() == m.end() {
                continue;
            }
            line += entry
                .text
                .get(counted_to..m.start())
                .map_or(0, |chunk| chunk.matches('\n').count());
            counted_to = m.start();
            matches.push(ScrollbackMatch {
                entry_id: entry.id,
                line_in_entry: line,
                byte_range: m.range(),
            });
        }
    }
    matches
}

/// Latest scan results published by the daemon for the UI thread to pick up.
///
/// `request_generation` is assigned synchronously by the UI and identifies the exact query/corpus request that produced this snapshot.
#[derive(Clone, Default, Debug)]
struct SearchSnapshot {
    matches: Arc<[ScrollbackMatch]>,
    request_generation: u64,
    /// The query these matches were computed for.
    /// `poll` compares it against the live editor and drops results for a query the user typed past while the scan was in flight.
    /// The match count and cursor therefore never desync from the visible query.
    query: String,
}

/// Work sent from the UI thread to the daemon. Each keystroke is one atomic `Update` carrying the latest query
/// plus, only when content changed, the new corpus. Bundling them means the daemon can never wake having seen a new
/// corpus but not yet the matching query. A split like that would publish one stale result before correcting.
enum SearchMsg {
    Update {
        /// New corpus to scan, or `None` to keep the corpus the daemon holds.
        corpus: Option<Arc<[IndexedEntry]>>,
        /// Query to scan for.
        query: String,
        /// UI-owned request identity.
        request_generation: u64,
    },
    /// Shut the daemon thread down.
    Stop,
}

/// The newest corpus and query coalesced from a burst of `Update`s.
#[derive(Default)]
struct DrainedUpdate {
    corpus: Option<Arc<[IndexedEntry]>>,
    query: Option<String>,
    request_generation: Option<u64>,
    stop: bool,
}

/// Coalesce all currently-pending messages, keeping the newest corpus and the newest query. A later `None` corpus
/// means "unchanged" and must not clobber a corpus carried by an earlier message in the burst. `Stop` always wins
/// and ends draining immediately.
fn drain_to_latest(first: SearchMsg, rx: &Receiver<SearchMsg>) -> DrainedUpdate {
    let mut out = DrainedUpdate::default();
    let mut msg = first;
    loop {
        match msg {
            SearchMsg::Update {
                corpus,
                query,
                request_generation,
            } => {
                if corpus.is_some() {
                    out.corpus = corpus;
                }
                out.query = Some(query);
                out.request_generation = Some(request_generation);
            }
            SearchMsg::Stop => {
                out.stop = true;
                return out;
            }
        }
        match rx.try_recv() {
            Ok(next) => msg = next,
            Err(_) => return out,
        }
    }
}

/// Owns the background scan thread and the shared result snapshot.
#[derive(Debug)]
struct SearchDaemon {
    shared: Arc<Mutex<SearchSnapshot>>,
    tx: Sender<SearchMsg>,
    /// Deliberately detached, never joined: closing a search must not block the UI on an in-flight scan.
    /// `Stop` and channel-disconnect both end the thread, and the worker owns its `Arc` clones, so dropping this is safe.
    _handle: JoinHandle<()>,
}

impl SearchDaemon {
    fn new() -> Self {
        let shared = Arc::new(Mutex::new(SearchSnapshot::default()));
        // Unbounded so `update_query`'s send never blocks the input thread.
        // A bounded channel could stall the 257th keystroke on a full buffer, and dropping on full (try_send) could lose the final query
        // Messages are tiny (an Arc pointer and the query) and the daemon drains the whole queue on each wakeup, so it stays short in practice
        let (tx, rx) = channel::<SearchMsg>();

        let out = shared.clone();
        let handle = thread::spawn(move || {
            let mut corpus: Arc<[IndexedEntry]> = Arc::from([]);
            let mut query = String::new();
            while let Ok(msg) = rx.recv() {
                let update = drain_to_latest(msg, &rx);
                if update.stop {
                    break;
                }
                if let Some(new_corpus) = update.corpus {
                    corpus = new_corpus;
                }
                if let Some(new_query) = update.query {
                    query = new_query;
                }
                let Some(request_generation) = update.request_generation else {
                    continue;
                };

                // Every non-`Stop` burst carries a query, so rescan once here.
                // Compile and scan off the lock; the mutex only guards the quick snapshot swap below, never the scan itself
                let matcher = TextMatcher::new(query.as_str(), QueryKind::Regex);
                let matches: Arc<[ScrollbackMatch]> = if query.is_empty() || matcher.is_error() {
                    Arc::from([])
                } else {
                    scan_matches(&corpus, &matcher).into()
                };
                *out.lock().unwrap() = SearchSnapshot {
                    matches,
                    request_generation,
                    query: query.clone(),
                };
            }
        });

        Self {
            shared,
            tx,
            _handle: handle,
        }
    }
}

impl Drop for SearchDaemon {
    fn drop(&mut self) {
        // Best-effort: a closed channel just means the thread already exited.
        let _ = self.tx.send(SearchMsg::Stop);
    }
}

/// An interactive search session over the scrollback. `update_query` only enqueues the corpus and query for the
/// daemon. it never scans.
#[derive(Debug)]
pub struct ScrollbackSearchState {
    /// Canonical editable query and cursor.
    editor: LineEditor,
    /// Cached searchable text; re-synced lazily on content change, never per frame.
    /// Lives on the UI thread and is handed to the daemon as an `Arc`.
    index: ScrollbackSearchIndex,
    /// Compiled query derived from `editor` for highlighting and error state.
    matcher: TextMatcher,
    /// Latest matches from the daemon, in scrollback order.
    matches: Arc<[ScrollbackMatch]>,
    /// Cursor into `matches`; `None` when there are no matches.
    current: Option<usize>,
    /// `true` while the query is still being edited (search bar), `false` once accepted (browsing with `n` / `N`).
    composing: bool,
    /// Background scan thread; dropped (and stopped) when the session closes.
    daemon: SearchDaemon,
    /// Snapshot request generation last observed by `poll`.
    last_seen_generation: u64,
    /// Monotonic UI-owned request generation, incremented only when enqueuing query/corpus work.
    request_generation: u64,
}

impl ScrollbackSearchState {
    /// Open an empty session in the composing phase.
    /// No matches exist until the first [`update_query`](Self::update_query) result is [`poll`](Self::poll)ed in.
    pub fn open() -> Self {
        Self {
            editor: LineEditor::default(),
            index: ScrollbackSearchIndex::new(),
            matcher: TextMatcher::new("", QueryKind::Regex),
            matches: Arc::from([]),
            current: None,
            composing: true,
            daemon: SearchDaemon::new(),
            last_seen_generation: 0,
            request_generation: 0,
        }
    }

    /// Replace the canonical query, recompile its derived matcher, and enqueue the latest corpus/query snapshot for the background scan.
    /// The corpus is only re-synced and re-sent when scrollback content changed since the last send.
    /// Steady-state keystrokes therefore just push a query string.
    pub fn update_query(&mut self, query: &str, state: &ScrollbackState) {
        self.editor.set_text(query);
        self.update_derived_query(state);
    }

    fn update_derived_query(&mut self, state: &ScrollbackState) {
        let query = self.editor.text().to_owned();
        // Compile UI-side: the render layer highlights from this matcher, and it backs `query` / `has_error`, all of which must update immediately
        self.matcher = TextMatcher::new(query.as_str(), QueryKind::Regex);
        // Keep the last settled result navigable while the next scan is in flight
        // Empty and malformed queries are known synchronously to have no matches, so those can clear immediately
        if query.is_empty() || self.matcher.is_error() {
            self.matches = Arc::from([]);
            self.current = None;
        }

        // Re-send the corpus only when `sync` actually rebuilt it (content changed)
        // Otherwise `None` tells the daemon to keep the corpus it already holds
        // The index itself guards the rebuild on `content_generation`, so no separate generation tracking is needed
        let corpus = self.index.sync(state).then(|| self.index.entries_arc());
        let Some(request_generation) = self.request_generation.checked_add(1) else {
            return;
        };
        self.request_generation = request_generation;
        let _ = self.daemon.tx.send(SearchMsg::Update {
            corpus,
            query,
            request_generation,
        });
    }

    pub(crate) fn apply_query_key(
        &mut self,
        key: &KeyEvent,
        state: &ScrollbackState,
    ) -> LineEditOutcome {
        let outcome = self.editor.handle_key(key);
        if outcome == LineEditOutcome::TextChanged {
            self.update_derived_query(state);
        }
        outcome
    }

    pub fn apply_query_paste(&mut self, text: &str, state: &ScrollbackState) -> LineEditOutcome {
        let outcome = self.editor.insert_paste(text);
        if outcome == LineEditOutcome::TextChanged {
            self.update_derived_query(state);
        }
        outcome
    }

    /// Apply one canonical query-edit key. Returns whether the key was consumed.
    pub fn handle_query_key(&mut self, key: &KeyEvent, state: &ScrollbackState) -> bool {
        self.apply_query_key(key, state) != LineEditOutcome::Unhandled
    }

    /// Pick up the latest scan results from the daemon.
    /// Returns `true` when the results changed (so the caller can redraw / reveal the new match).
    /// On a change the cursor parks on the first match, preserving the old "jump to the first match when the query changes" behavior.
    pub fn poll(&mut self) -> bool {
        // Hold the lock only for the cheap compares (and, on a real change, an Arc-pointer clone)
        // Never clone the whole snapshot: that would heap-allocate its `query: String` on every no-change tick at ~30 Hz
        let guard = self.daemon.shared.lock().unwrap();
        if guard.request_generation == self.last_seen_generation {
            return false;
        }
        self.last_seen_generation = guard.request_generation;
        // Drop a scan that finished for a superseded query
        // While it was in flight the user kept typing, so the editor and derived matcher have already moved on
        // Applying it would desync the match count and cursor from the visible query until the current query's scan lands
        if guard.request_generation != self.request_generation || guard.query != self.query() {
            return false;
        }
        let matches = guard.matches.clone();
        drop(guard);
        self.matches = matches;
        self.current = (!self.matches.is_empty()).then_some(0);
        true
    }

    /// Move the cursor to the next match, wrapping past the end.
    /// No-op when there are no matches.
    pub fn next(&mut self) {
        self.step(1);
    }

    /// Move the cursor to the previous match, wrapping past the front.
    /// No-op when there are no matches.
    pub fn prev(&mut self) {
        self.step(-1);
    }

    fn step(&mut self, delta: isize) {
        let len = self.matches.len();
        if len == 0 {
            self.current = None;
            return;
        }
        let from = self.current.unwrap_or(0) as isize;
        // `rem_euclid` keeps the index within `0..len` for either direction.
        self.current = Some((from + delta).rem_euclid(len as isize) as usize);
    }

    /// Freeze the query and switch from composing to browsing.
    /// The matcher and match list are retained so `n` / `N` can step through them.
    pub fn accept(&mut self) {
        self.composing = false;
    }

    /// The match under the cursor, if any.
    pub fn current(&self) -> Option<&ScrollbackMatch> {
        self.current.and_then(|i| self.matches.get(i))
    }

    /// Cursor position within the match list (0-based), if there are matches.
    pub fn current_index(&self) -> Option<usize> {
        self.current
    }

    /// Total number of matches for the current query.
    pub fn match_count(&self) -> usize {
        self.matches.len()
    }

    /// The raw query string being searched.
    pub fn query(&self) -> &str {
        self.editor.text()
    }

    pub fn query_viewport(&self, width: usize) -> crate::engine::textarea::SingleLineViewport {
        self.editor.viewport(width)
    }

    /// The compiled query regex for the highlight pass, or `None` when the query is empty or fails to compile (nothing to highlight).
    /// Decoupled from the index; the render layer re-runs it per visible row.
    pub fn highlight_regex(&self) -> Option<regex::Regex> {
        (!self.query().is_empty() && !self.matcher.is_error())
            .then(|| self.matcher.compiled_regex().clone())
    }

    /// Whether the current query is a malformed regex (so it matches nothing).
    pub fn has_error(&self) -> bool {
        self.matcher.is_error()
    }

    /// Whether the query is still being edited (`true`) versus accepted and being browsed (`false`).
    pub fn is_composing(&self) -> bool {
        self.composing
    }
}
