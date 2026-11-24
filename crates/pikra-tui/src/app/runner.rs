use super::{Outcome, TuiApp, TuiInput, screen::Screen};
use crate::{
    engine::{
        inline::Terminal,
        terminal::{
            SetDefaultCursor, negotiated_kitty_flags,
            osc7501::{ProgramStatus, ProgramStatusReporter},
            set_pushed_kitty_flags, terminal_context,
        },
    },
    harness::{NoticeLevel, TuiEvent},
};
use crossterm::{
    cursor::{SetCursorStyle, Show},
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        EventStream, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};
use futures_channel::mpsc;
use futures_core::Stream;
use futures_util::{FutureExt, StreamExt, select_biased};
use ratatui::backend::{Backend, CrosstermBackend};
use std::{
    io::{self, Write},
    sync::{
        Once,
        atomic::{AtomicU8, Ordering},
    },
    thread,
    time::Duration,
};

const ACTIVE: u8 = 1;
const RAW: u8 = 2;
const PASTE: u8 = 4;
const ALT: u8 = 8;
const KEYS: u8 = 16;
const REPORTED: u8 = 32;
static TERMINAL: AtomicU8 = AtomicU8::new(0);
static PANIC_HOOK: Once = Once::new();

/// Restore the terminal on ordinary exit, partial setup, and aborting panics.
pub(super) struct TerminalGuard {
    flags: KeyboardEnhancementFlags,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        PANIC_HOOK.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                if cfg!(panic = "abort") {
                    restore_terminal(TERMINAL.swap(0, Ordering::AcqRel));
                }
                previous(info);
            }));
        });
        TERMINAL
            .compare_exchange(0, ACTIVE, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| io::Error::other("A terminal session is already open"))?;
        let mut guard = Self {
            flags: KeyboardEnhancementFlags::empty(),
        };
        if !crossterm::terminal::is_raw_mode_enabled()? {
            TERMINAL.fetch_or(RAW, Ordering::Release);
            enable_raw_mode()?;
        }
        TERMINAL.fetch_or(PASTE, Ordering::Release);
        execute!(io::stdout(), EnableBracketedPaste)?;
        let context = terminal_context();
        if context.kitty_skip_reason().is_none()
            && crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false)
        {
            guard.flags = negotiated_kitty_flags(None, None);
        }
        Ok(guard)
    }

    pub(super) fn alternate(&self) -> bool {
        TERMINAL.load(Ordering::Acquire) & ALT != 0
    }

    pub(super) fn set_alternate(&mut self, mut out: impl Write, alternate: bool) -> io::Result<()> {
        let old = self.alternate();
        if old != alternate && TERMINAL.load(Ordering::Acquire) & KEYS != 0 {
            execute!(out, PopKeyboardEnhancementFlags)?;
            TERMINAL.fetch_and(!KEYS, Ordering::Release);
            set_pushed_kitty_flags(KeyboardEnhancementFlags::empty());
        }
        if alternate && !old {
            TERMINAL.fetch_or(ALT, Ordering::Release);
            execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
        } else if old && !alternate {
            execute!(
                out,
                SetDefaultCursor,
                DisableMouseCapture,
                LeaveAlternateScreen
            )?;
            TERMINAL.fetch_and(!ALT, Ordering::Release);
        }
        if !self.flags.is_empty() && TERMINAL.load(Ordering::Acquire) & KEYS == 0 {
            TERMINAL.fetch_or(KEYS, Ordering::Release);
            execute!(out, PushKeyboardEnhancementFlags(self.flags))?;
            set_pushed_kitty_flags(self.flags);
        }
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal(TERMINAL.load(Ordering::Acquire));
        TERMINAL.store(0, Ordering::Release);
    }
}

fn restore_terminal(state: u8) {
    if state == 0 {
        return;
    }
    let mut out = io::stdout();
    let _ = execute!(out, EndSynchronizedUpdate);
    if state & REPORTED != 0 {
        let wire = ProgramStatus::clear().encode_for_terminal(std::env::var_os("TMUX").is_some());
        let _ = out.write_all(wire.as_bytes());
    }
    if state & KEYS != 0 {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
        set_pushed_kitty_flags(KeyboardEnhancementFlags::empty());
    }
    if state & ALT != 0 {
        let _ = execute!(out, DisableMouseCapture, LeaveAlternateScreen);
    }
    if state & PASTE != 0 {
        let _ = execute!(out, DisableBracketedPaste);
    }
    let _ = execute!(
        out,
        SetDefaultCursor,
        SetCursorStyle::DefaultUserShape,
        Show
    );
    if state & RAW != 0 {
        let _ = disable_raw_mode();
    }
}

struct StatusOutput<W: Write> {
    reporter: ProgramStatusReporter,
    writer: W,
    native: bool,
    clear_attempted: bool,
}

impl<W: Write> StatusOutput<W> {
    fn new(app: &TuiApp, writer: W, native: bool) -> Self {
        let chrome = &app.config.chrome;
        Self {
            reporter: ProgramStatusReporter::new(chrome.osc7501_enabled, chrome.app_name.clone()),
            writer,
            native,
            clear_attempted: false,
        }
    }

    fn emit(&mut self, status: ProgramStatus) -> io::Result<()> {
        if self.native && self.reporter.is_enabled() {
            TERMINAL.fetch_or(REPORTED, Ordering::Release);
        }
        self.reporter.emit(&mut self.writer, status)
    }

    fn clear(&mut self) -> io::Result<()> {
        if std::mem::replace(&mut self.clear_attempted, true) {
            return Ok(());
        }
        self.reporter.clear(&mut self.writer)?;
        if self.native {
            TERMINAL.fetch_and(!REPORTED, Ordering::Release);
        }
        Ok(())
    }
}

impl<W: Write> Drop for StatusOutput<W> {
    fn drop(&mut self) {
        let _ = self.clear();
    }
}

impl TuiApp {
    pub async fn run(mut self) -> io::Result<()> {
        let mut guard = TerminalGuard::enter()?;
        self.start(true);
        let mut backend = CrosstermBackend::new(io::stdout());
        let origin = backend.get_cursor_position()?;
        let size = backend.size()?;
        let area = ratatui::layout::Rect::new(
            0,
            origin.y.min(size.height.saturating_sub(1)),
            size.width,
            1,
        );
        let mut term = Terminal::with_options(
            backend,
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(area),
            },
        )?;
        let mut screen = Screen::default();
        let (mut tx, ticks) = mpsc::channel(1);
        // No async runtime dependency. Dropping the receiver wakes the producer on its next tick.
        thread::Builder::new()
            .name("tui-pulse".into())
            .spawn(move || {
                loop {
                    thread::park_timeout(Duration::from_millis(16));
                    if tx.try_send(()).is_err_and(|e| e.is_disconnected()) {
                        break;
                    }
                }
            })?;
        let input = EventStream::new().map(|e| e.map(TuiInput::Terminal));
        let mut status = StatusOutput::new(&self, io::stdout(), true);
        let result = self
            .drive(input, ticks, &mut status, |app| {
                screen.native(&mut term, app, &mut guard)
            })
            .await;
        let clear = status.clear();
        let screen_clear = if guard.alternate() {
            Ok(())
        } else {
            term.clear()
        };
        result.and(clear).and(screen_clear)
    }

    /// Drive a supplied backend without touching the process terminal. The returned
    /// terminal retains its final frame and can be inspected by the embedding host.
    pub async fn run_on<B: Backend, S: Stream<Item = TuiInput>>(
        self,
        backend: B,
        input: S,
    ) -> io::Result<Terminal<B>> {
        self.run_on_with_status(backend, input, io::sink()).await
    }

    /// Drive an injected backend and send OSC 7501 reports to a host-provided writer.
    /// The writer is flushed at each transition and receives a final clear on exit.
    pub async fn run_on_with_status<B: Backend, S: Stream<Item = TuiInput>, W: Write>(
        mut self,
        backend: B,
        input: S,
        writer: W,
    ) -> io::Result<Terminal<B>> {
        self.start(false);
        let mut term = Terminal::new(backend)?;
        let mut screen = Screen::default();
        let mut status = StatusOutput::new(&self, writer, false);
        let result = self
            .drive(
                input.map(Ok),
                futures_util::stream::pending(),
                &mut status,
                |app| screen.backend(&mut term, app),
            )
            .await;
        result.and(status.clear())?;
        Ok(term)
    }

    async fn drive(
        &mut self,
        input: impl Stream<Item = io::Result<TuiInput>>,
        ticks: impl Stream<Item = ()>,
        status: &mut StatusOutput<impl Write>,
        mut draw: impl FnMut(&mut Self) -> io::Result<()>,
    ) -> io::Result<()> {
        let mut events = self.harness.subscribe().fuse();
        let mut input = Box::pin(input.fuse());
        let mut ticks = Box::pin(ticks.fuse());
        let mut dirty = true;
        let mut burst = false;
        status.emit(ProgramStatus::idle())?;
        let result = 'drive: loop {
            self.modals.retain(|m| !m.cancelled());
            if let Err(e) = status.emit(self.program_status()) {
                break Err(e);
            }
            if dirty && let Err(e) = draw(self) {
                break Err(e);
            }
            if self.quit {
                break Ok(());
            }
            dirty = true;
            // Give a waiting key a turn even when the provider continuously produces deltas.
            if std::mem::take(&mut burst)
                && let Some(Some(item)) = input.next().now_or_never()
            {
                match item {
                    Ok(i) => self.deliver_input(i),
                    Err(e) => break Err(e),
                }
                continue;
            }
            let jobs_pending = !self.jobs.is_empty();
            // A drained FuturesUnordered is immediately ready with None. Mask it
            // until something is scheduled, leaving idle loops asleep.
            let job = async {
                if jobs_pending {
                    self.jobs.next().await
                } else {
                    std::future::pending().await
                }
            }
            .fuse();
            let abort = async {
                match self.aborting.as_mut() {
                    Some(abort) => abort.await,
                    None => std::future::pending().await,
                }
            }
            .fuse();
            futures_util::pin_mut!(job, abort);
            select_biased! {
                ()=abort=>{
                    self.aborting=None;
                    if self.running {
                        self.event(TuiEvent::TurnEnded);
                    }
                },
                control=self.controls.next().fuse()=>if let Some(cmd)=control {
                    self.control(cmd);
                },
                outcome=job=>if let Some(outcome)=outcome {
                    self.finish_job(outcome);
                },
                event=events.select_next_some()=>{
                    self.event(event);
                    for n in 1..32 {
                        if let Err(e) = status.emit(self.program_status()) {
                            break 'drive Err(e);
                        }
                        // Render a newly opened request before any more events or mouse input.
                        if self.modals.front().is_some_and(super::Modal::is_interaction) {
                            break;
                        }
                        let Some(Some(next))=events.next().now_or_never() else {break;};
                        self.event(next);
                        burst=n==31;
                    }
                },
                item=input.next()=>match item {
                    Some(Ok(i))=>self.deliver_input(i),
                    Some(Err(e))=>break Err(e),
                    None=>break Ok(())
                },
                ()=ticks.select_next_some()=>{
                    dirty = self.appearance_changed();
                    dirty |= self.history.state.tick();
                    dirty |= self.history.state.poll_search();
                    dirty |= self.pointer_tick();
                    #[cfg(feature = "history-search")]
                    {
                        dirty |= self.composer.poll_history();
                    }
                }
            }
        };
        self.modals.clear();
        self.controls.close();
        while self.controls.try_recv().is_ok() {}
        if let Some(abort) = self.aborting.take() {
            abort.await;
        } else if self.running {
            self.harness.abort().await;
        }
        result
    }
    fn deliver_input(&mut self, input: TuiInput) {
        // A host future may enqueue a modal and return Pending during the same
        // select poll in which the next key becomes ready.
        while let Ok(cmd) = self.controls.try_recv() {
            self.control(cmd);
        }
        if !self.quit {
            self.input(input);
        }
    }
    fn finish_job(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Submitted(Err(error)) => {
                self.starting_turn = false;
                self.running = false;
                self.history.finish_pending();
                self.status = ProgramStatus::error(&error);
                self.history.notice(NoticeLevel::Error, error);
            }
            Outcome::Result(Err(error))
            | Outcome::Session(Err(error))
            | Outcome::Rewind(Err(error)) => self.history.notice(NoticeLevel::Error, error),
            Outcome::Session(Ok(())) => self.restore(),
            Outcome::Rewind(Ok(draft)) => {
                self.restore();
                if let Some(text) = draft {
                    self.focus_prompt();
                    self.composer.set_text(&text);
                    self.refresh_completion();
                }
            }
            Outcome::Done | Outcome::Result(Ok(())) | Outcome::Submitted(Ok(())) => {
                self.refresh_meters_and_queue()
            }
        }
    }
}
