//! Frame drawing with cursor blink preservation.
//!
//! # Problem
//!
//! Ratatui's [`Terminal::draw()`] (internally `try_draw()`) unconditionally sends cursor escape sequences on every frame:
//!
//! - If `frame.set_cursor_position()` was called: `Show` and `MoveTo` every frame
//! - If not called: `Hide` every frame
//!
//! Both reset the terminal's cursor blink timer (`Show` restarts the blink cycle, `MoveTo` resets the blink phase).
//! At 30fps, the 500ms blink interval never completes, so the cursor appears solid.
//!
//! # Solution
//!
//! We bypass `try_draw()` and use ratatui's lower-level API directly:
//!
//! ```text
//! terminal.autoresize()     — handle terminal size changes
//! terminal.get_frame()      — get a fresh buffer to render into
//! terminal.flush()          — diff old/new buffers, write only changed cells
//! terminal.swap_buffers()   — prepare for next frame
//! ```
//!
//! Cursor is managed entirely by [`CursorState`] with de-duplication:
//!
//! - **No cell changes, same position**: zero cursor commands, so blink is preserved
//! - **Cells changed, same position**: `MoveTo` to fix the cursor after cell writes
//! - **Position changed**: `MoveTo` (blink resets; expected, the user just typed)
//! - **Visibility transition**: `Show`/`Hide` (only on actual transition)
//! - **Idle (no draw calls)**: nothing sent, so blink runs undisturbed
//!
//! The "no cell changes" case is detectable because [`crate::engine::inline::Terminal`]'s `flush()` returns whether any cells were written.
//! When animated entries are off-screen, the buffer diff is empty and we skip all cursor commands.
//!
//! # Synchronized output
//!
//! Each frame is wrapped in `BeginSynchronizedUpdate` / `EndSynchronizedUpdate` so the terminal presents the cell diff, images, and cursor moves atomically.
//! The wrapper is omitted when tmux is the immediate terminal (see [`crate::engine::input::terminal::should_emit_synchronized_output`]): tmux repaints the whole pane when a block closes and already synchronizes its own output toward the outer terminal.
use crate::engine::inline::LinkSpan;
use crate::engine::input::terminal::{TerminalContext, should_emit_synchronized_output};
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use crossterm::{QueueableCommand, cursor};
use ratatui::Frame;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Position;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::thread::ThreadId;
use std::time::{Duration, Instant};
/// Defined here (beside [`TermWriter`]) so the `render` module does not depend on `app`.
/// Re-exported from `app` as `crate::app::PagerTerminal` for existing call sites.
pub type PagerTerminal = crate::engine::inline::Terminal<CrosstermBackend<TermWriter>>;
#[derive(Debug)]
pub enum WriterEvent {
    Written(u64),
    Failed(std::io::Error),
}
/// Outcome of a bounded writer drain attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriterDrain {
    Drained,
    TimedOut,
}
/// Sequence is reserved before send, so an accepted frame blocks the child-handoff drain before the writer sees it. No queued frame lands after the child takes the tty.
#[derive(Clone, Debug)]
pub struct WriterSync {
    queued: Arc<AtomicU64>,
    written: Arc<AtomicU64>,
    failed: Arc<AtomicBool>,
    writer_active: Arc<AtomicBool>,
    /// The one thread allowed to produce payloads, latched on first send (see [`send_payload`]).
    producer: Arc<OnceLock<ThreadId>>,
    event_tx: Option<futures_channel::mpsc::UnboundedSender<WriterEvent>>,
}
impl Default for WriterSync {
    fn default() -> Self {
        Self::new()
    }
}
impl WriterSync {
    pub fn new() -> Self {
        Self {
            queued: Arc::new(AtomicU64::new(0)),
            written: Arc::new(AtomicU64::new(0)),
            failed: Arc::new(AtomicBool::new(false)),
            writer_active: Arc::new(AtomicBool::new(false)),
            producer: Arc::new(OnceLock::new()),
            event_tx: None,
        }
    }
    fn with_event_sender(event_tx: futures_channel::mpsc::UnboundedSender<WriterEvent>) -> Self {
        Self {
            event_tx: Some(event_tx),
            ..Self::new()
        }
    }
    fn reserve_sequence(&self) -> u64 {
        self.queued.fetch_add(1, Ordering::Release) + 1
    }
    fn mark_written(&self, sequence: u64) {
        self.written.fetch_max(sequence, Ordering::AcqRel);
        if let Some(event_tx) = &self.event_tx {
            let _ = event_tx.unbounded_send(WriterEvent::Written(sequence));
        }
    }
    fn mark_failed(&self, error: std::io::Error) {
        if self
            .failed
            .compare_exchange(false, true, Ordering::Release, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        if let Some(event_tx) = &self.event_tx {
            let _ = event_tx.unbounded_send(WriterEvent::Failed(error));
        }
    }
    pub fn queued(&self) -> u64 {
        self.queued.load(Ordering::Acquire)
    }
    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Acquire)
    }
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }
    fn is_drained(&self) -> bool {
        !self.failed() && self.written() >= self.queued()
    }
    /// Block until the writer flushes every accepted payload, output fails, or the deadline passes.
    pub fn wait_drained(&self, timeout: Duration) -> std::io::Result<WriterDrain> {
        let deadline = Instant::now() + timeout;
        while !self.is_drained() {
            if self.failed() {
                return Err(std::io::Error::other("terminal output failed"));
            }
            if Instant::now() >= deadline {
                return Ok(WriterDrain::TimedOut);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(WriterDrain::Drained)
    }
}
/// Buffers a frame's escape sequences and hands them to a writer thread for the blocking write to stderr / the pty fd.
/// If the terminal emulator is slow to read, only the writer thread stalls; the event loop keeps processing timers, events, and ACP messages.
pub struct WriterPayload {
    pub(crate) sequence: u64,
    pub(crate) data: Vec<u8>,
}
impl WriterPayload {
    /// The raw bytes this payload writes to the tty.
    pub fn data(&self) -> &[u8] {
        &self.data
    }
}
pub type WriterSender = mpsc::Sender<WriterPayload>;
fn writer_exited_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        "terminal writer thread exited",
    )
}
/// Send failure marks shared sync failed; the event loop surfaces it as fatal.
/// One producer only: interleaved reserve+send would put byte-order-sensitive escapes on the tty out of order.
fn send_payload(tx: &WriterSender, sync: &WriterSync, data: Vec<u8>) -> std::io::Result<()> {
    let current = std::thread::current().id();
    let producer = *sync.producer.get_or_init(|| current);
    debug_assert_eq!(
        producer, current,
        "writer payloads must all come from the event-loop thread; a second producer thread \
         can reorder terminal output (see EscapeWriter)"
    );
    let sequence = sync.reserve_sequence();
    if tx.send(WriterPayload { sequence, data }).is_err() {
        sync.mark_failed(writer_exited_error());
        return Err(writer_exited_error());
    }
    Ok(())
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriterAlreadyActive;
impl std::fmt::Display for WriterAlreadyActive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WriterSync already owns a live TermWriter")
    }
}
impl std::error::Error for WriterAlreadyActive {}
pub struct TermWriter {
    buf: Vec<u8>,
    tx: WriterSender,
    sync: WriterSync,
}
impl TermWriter {
    pub fn new(tx: WriterSender, sync: WriterSync) -> Result<Self, WriterAlreadyActive> {
        sync.writer_active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| WriterAlreadyActive)?;
        Ok(Self {
            buf: Vec::with_capacity(32 * 1024),
            tx,
            sync,
        })
    }
    /// Drop the current frame's buffered bytes without sending them.
    pub fn discard(&mut self) {
        self.buf.clear();
    }
    /// Shared writer progress used by the suspend path to [`WriterSync::wait_drained`] before a child takes the tty.
    pub fn writer_sync(&self) -> &WriterSync {
        &self.sync
    }
    /// A cloneable, non-blocking handle onto this writer's queue for
    /// out-of-band escapes (see [`EscapeWriter`]).
    pub fn escape_writer(&self) -> EscapeWriter {
        EscapeWriter::new(self.tx.clone(), self.sync.clone())
    }
}
impl Write for TermWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        send_payload(&self.tx, &self.sync, std::mem::take(&mut self.buf))
    }
}
impl Drop for TermWriter {
    fn drop(&mut self) {
        let _ = self.flush();
        self.sync.writer_active.store(false, Ordering::Release);
    }
}
/// Enqueue: an inline stderr lock from the event loop deadlocks when the terminal stops reading the pty.
/// Event-loop thread only. Drop every clone before [`WriterThread::join_within`] or the writer never sees the channel close.
#[derive(Clone)]
pub struct EscapeWriter {
    tx: WriterSender,
    sync: WriterSync,
}
impl EscapeWriter {
    pub fn new(tx: WriterSender, sync: WriterSync) -> Self {
        Self { tx, sync }
    }
    /// A writer with no writer thread behind it: sends fail against a private,
    /// receiver-less channel and are dropped. For the headless leader (no tty) and tests;
    /// any TUI view must get the live handle from [`TermWriter::escape_writer`] instead.
    pub fn disconnected() -> Self {
        let (tx, _rx) = mpsc::channel::<WriterPayload>();
        Self {
            tx,
            sync: WriterSync::new(),
        }
    }
    /// Never blocks. Covered by `wait_drained`. Send failure is recorded on the shared sync, not returned.
    pub fn emit(&self, bytes: impl Into<Vec<u8>>) {
        let data: Vec<u8> = bytes.into();
        if data.is_empty() {
            return;
        }
        let _ = send_payload(&self.tx, &self.sync, data);
    }
    /// Winapi-only commands run synchronously: a console API call, not a tty write, so they neither block on the pty nor take the stderr lock.
    pub fn emit_command(&self, command: impl crossterm::Command) {
        #[cfg(windows)]
        if !command.is_ansi_code_supported() {
            let _ = command.execute_winapi();
            return;
        }
        let mut ansi = String::new();
        if command.write_ansi(&mut ansi).is_ok() {
            self.emit(ansi);
        }
    }
}
/// Outcome of [`WriterThread::join_within`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriterJoin {
    /// The thread drained its queue and exited.
    Joined,
    /// The thread was still running at the deadline (tty blocked, or a sender still alive)
    /// and has been detached.
    TimedOut,
}
/// Joining ensures all queued frames have been written to the terminal before teardown (e.g. `LeaveAlternateScreen`).
pub struct WriterThread {
    handle: Option<std::thread::JoinHandle<std::io::Result<()>>>,
    sync: WriterSync,
}
impl WriterThread {
    /// Every sender, including [`EscapeWriter`] clones, must be dropped first or this can only time out.
    /// A timeout also covers a terminal that stopped reading; the thread is detached and teardown proceeds.
    pub fn join_within(mut self, grace: Duration) -> std::io::Result<WriterJoin> {
        let Some(handle) = self.handle.take() else {
            return Ok(WriterJoin::Joined);
        };
        let deadline = Instant::now() + grace;
        while !handle.is_finished() {
            if Instant::now() >= deadline {
                drop(handle);
                return Ok(WriterJoin::TimedOut);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        match handle.join() {
            Ok(result) => result.map(|()| WriterJoin::Joined),
            Err(_) => Err(std::io::Error::other("terminal writer thread panicked")),
        }
    }
    pub fn writer_sync(&self) -> &WriterSync {
        &self.sync
    }
}
impl Drop for WriterThread {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
fn write_payload(
    writer: &mut impl Write,
    payload: &WriterPayload,
    sync: &WriterSync,
) -> std::io::Result<()> {
    match writer
        .write_all(&payload.data)
        .and_then(|()| writer.flush())
    {
        Ok(()) => {
            sync.mark_written(payload.sequence);
            Ok(())
        }
        Err(error) => {
            sync.mark_failed(std::io::Error::new(error.kind(), error.to_string()));
            Err(error)
        }
    }
}
/// Everything [`spawn_writer_thread`] hands back to the event loop.
pub type WriterThreadParts = (
    WriterSender,
    WriterSync,
    futures_channel::mpsc::UnboundedReceiver<WriterEvent>,
    WriterThread,
);
/// Errors when the OS refuses to spawn. The handle must be joined during terminal teardown.
pub fn spawn_writer_thread() -> std::io::Result<WriterThreadParts> {
    let (tx, rx) = mpsc::channel::<WriterPayload>();
    let (event_tx, event_rx) = futures_channel::mpsc::unbounded();
    let sync = WriterSync::with_event_sender(event_tx);
    let thread_sync = sync.clone();
    let writer_thread_sync = sync.clone();
    let builder = std::thread::Builder::new().name("term-writer".into());
    let output = crate::engine::tty::dup_tui_stderr()?;
    let handle = builder.spawn(move || -> std::io::Result<()> {
        let mut writer = std::io::BufWriter::with_capacity(64 * 1024, output);
        while let Ok(payload) = rx.recv() {
            let _guard = std::io::stderr().lock();
            write_payload(&mut writer, &payload, &thread_sync)?;
        }
        if thread_sync.failed() {
            Err(std::io::Error::other("terminal output failed"))
        } else {
            Ok(())
        }
    })?;
    Ok((
        tx,
        sync,
        event_rx,
        WriterThread {
            handle: Some(handle),
            sync: writer_thread_sync,
        },
    ))
}
/// Tracks the last cursor position written to the terminal, so each frame emits only the cursor escapes it needs.
/// Redundant `Show`/`Hide`/`MoveTo` would reset the terminal's blink timer.
#[derive(Debug, Default)]
pub struct CursorState {
    /// `None` means the cursor is hidden; `Some((x, y))` means it is visible at (x, y).
    last_pos: Option<(u16, u16)>,
}
/// What cursor commands to emit after a frame render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorAction {
    /// No cursor commands needed, so the blink timer is preserved.
    None,
    /// Cursor is visible and cells changed: reposition after cell writes disturbed the terminal cursor.
    /// Resets blink (unavoidable when cells change on screen).
    Reposition(u16, u16),
    /// Cursor becoming visible at (x, y); needs `MoveTo` and `Show`.
    Show(u16, u16),
    /// Cursor becoming hidden; needs `Hide`.
    Hide,
}
impl CursorState {
    pub fn new() -> Self {
        Self { last_pos: None }
    }
    /// Determine what cursor action to take for this frame.
    ///
    /// No side effects; call [`apply`] to execute the returned action.
    pub fn action(&self, cursor_pos: Option<(u16, u16)>, has_changes: bool) -> CursorAction {
        if cursor_pos == self.last_pos {
            if has_changes && let Some((x, y)) = cursor_pos {
                return CursorAction::Reposition(x, y);
            }
            CursorAction::None
        } else {
            match (cursor_pos, self.last_pos) {
                (Some((x, y)), Some(_)) => CursorAction::Reposition(x, y),
                (Some((x, y)), None) => CursorAction::Show(x, y),
                (None, Some(_)) => CursorAction::Hide,
                (None, None) => CursorAction::None,
            }
        }
    }
    /// `queue!` not `execute!`: cursor commands must batch with the frame and flush atomically on the writer thread.
    pub fn apply<W: Write>(&mut self, action: CursorAction, w: &mut W) {
        match action {
            CursorAction::None => {}
            CursorAction::Reposition(x, y) => {
                let _ = w.queue(cursor::MoveTo(x, y));
                self.last_pos = Some((x, y));
            }
            CursorAction::Show(x, y) => {
                let _ = w.queue(cursor::MoveTo(x, y));
                let _ = w.queue(cursor::Show);
                self.last_pos = Some((x, y));
            }
            CursorAction::Hide => {
                let _ = w.queue(cursor::Hide);
                self.last_pos = None;
            }
        }
    }
}
/// Bypasses ratatui `try_draw()` so cursor management stays conditional. OSC 8 spans go out before the diff, in lockstep with cells.
/// `PostFlush` stays inside the synchronized update so images appear atomically with the cell diff.
/// `ctx` decides whether the frame is wrapped in DEC 2026 at all; both markers follow that one decision.
/// Adopting the terminal size inside the update makes a fullscreen resize clear atomic with the repaint.
pub fn draw_frame(
    terminal: &mut PagerTerminal,
    cursor: &mut CursorState,
    ctx: &TerminalContext,
    render_fn: impl FnOnce(&mut Frame, &mut Vec<LinkSpan>) -> (Option<(u16, u16)>, Option<PostFlush>),
) {
    let synchronized = should_emit_synchronized_output(ctx);
    if synchronized {
        let _ = terminal.backend_mut().queue(BeginSynchronizedUpdate);
    }
    let _ = terminal.autoresize();
    render_and_present(terminal, cursor, synchronized, render_fn);
}
/// [`draw_frame`] without the resize, for a caller that adopted the terminal size before opening the update (minimal mode).
/// A terminal holds the inline resize's cursor-position reply until an open update ends.
pub fn draw_frame_at_adopted_size(
    terminal: &mut PagerTerminal,
    cursor: &mut CursorState,
    ctx: &TerminalContext,
    render_fn: impl FnOnce(&mut Frame, &mut Vec<LinkSpan>) -> (Option<(u16, u16)>, Option<PostFlush>),
) {
    let synchronized = should_emit_synchronized_output(ctx);
    if synchronized {
        let _ = terminal.backend_mut().queue(BeginSynchronizedUpdate);
    }
    render_and_present(terminal, cursor, synchronized, render_fn);
}
fn render_and_present(
    terminal: &mut PagerTerminal,
    cursor: &mut CursorState,
    synchronized: bool,
    render_fn: impl FnOnce(&mut Frame, &mut Vec<LinkSpan>) -> (Option<(u16, u16)>, Option<PostFlush>),
) {
    let mut link_spans: Vec<LinkSpan> = Vec::new();
    let (cursor_pos, post_flush_escapes) = {
        let mut frame = terminal.get_frame();
        render_fn(&mut frame, &mut link_spans)
    };
    terminal.set_frame_links(&link_spans);
    let has_changes = terminal.flush_with_links().unwrap_or(false);
    terminal.swap_buffers();
    let post_flush_wrote_cursor = post_flush_escapes.is_some();
    let action = cursor.action(cursor_pos, has_changes || post_flush_wrote_cursor);
    if !has_changes && !post_flush_wrote_cursor && action == CursorAction::None {
        terminal.backend_mut().writer_mut().discard();
        return;
    }
    if let Some(post_flush) = post_flush_escapes {
        let _ = post_flush.write_to(terminal.backend_mut());
    }
    cursor.apply(action, terminal.backend_mut());
    if let CursorAction::Reposition(x, y) | CursorAction::Show(x, y) = action {
        terminal.record_cursor_position(Position::new(x, y));
    }
    if synchronized {
        let _ = terminal.backend_mut().queue(EndSynchronizedUpdate);
    }
    let _ = terminal.backend_mut().flush();
}

#[cfg(feature = "inline-image")]
pub use super::image_overlay::overlay::PostFlush;
#[cfg(not(feature = "inline-image"))]
#[derive(Debug, Default)]
pub struct PostFlush {
    bytes: String,
}
#[cfg(not(feature = "inline-image"))]
impl PostFlush {
    pub fn plain(bytes: String) -> Self {
        Self { bytes }
    }
    pub fn append(&mut self, other: Self) {
        self.append_plain(other.as_str());
    }
    pub fn append_plain(&mut self, bytes: &str) {
        self.bytes.push_str(bytes);
    }
    pub fn as_str(&self) -> &str {
        &self.bytes
    }
    pub fn write_to(self, writer: &mut impl Write) -> std::io::Result<()> {
        writer.write_all(self.bytes.as_bytes())
    }
}
