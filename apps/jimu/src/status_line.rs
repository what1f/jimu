//! Asynchronous external shell command status-line runner and ANSI SGR parser.
//!
//! Runs a user-configured shell command (`settings.json` -> `status_line`) on a background task
//! with fingerprint deduplication, latest-only coalescing, optional periodic refresh, strict
//! timeout/output bounds, and Unix process-group isolation so timed-out or dropped commands never
//! leak orphan processes.

use crate::settings::StatusLineConfig;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use serde_json::{Value, json};
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    sync::Notify,
    task::JoinHandle,
    time,
};
use vte::{Params, Parser, Perform};

/// Default per-execution timeout for the external status-line command.
pub const DEFAULT_STATUS_LINE_TIMEOUT_MS: u64 = 1_500;

/// Maximum number of stdout bytes read from a single status-line invocation.
pub const MAX_STATUS_LINE_STDOUT_BYTES: usize = 64 * 1024;

const DEFAULT_DEBOUNCE_MS: u64 = 80;
const MIN_REFRESH_MS: u64 = 500;

/// Snapshot serialized to JSON and piped to the external `status_line` command's `stdin`.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusLineSnapshot {
    pub version: String,
    pub session_id: String,
    pub transcript_path: String,
    pub model_id: String,
    pub model_display_name: String,
    pub current_dir: String,
    pub project_dir: String,
    pub git_branch: String,
    pub context_window_size: u64,
    pub used_percentage: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub total_cost_usd: f64,
    pub total_duration_ms: u64,
    pub effort: String,
    pub fast_mode: bool,
    pub permission_mode: String,
    pub running: bool,
}

impl StatusLineSnapshot {
    /// Serialize the snapshot into the documented stdin JSON payload.
    pub fn to_json(&self) -> Value {
        json!({
            "version": self.version,
            "session_id": self.session_id,
            "transcript_path": self.transcript_path,
            "model": {
                "id": self.model_id,
                "display_name": self.model_display_name,
            },
            "workspace": {
                "current_dir": self.current_dir,
                "project_dir": self.project_dir,
            },
            "git_branch": self.git_branch,
            "context_window": {
                "context_window_size": self.context_window_size,
                "used_percentage": (self.used_percentage * 10.0).round() / 10.0,
                "current_usage": {
                    "input_tokens": self.input_tokens,
                    "output_tokens": self.output_tokens,
                    "cache_read_input_tokens": self.cache_read_input_tokens,
                    "cache_creation_input_tokens": self.cache_creation_input_tokens,
                },
            },
            "cost": {
                "total_cost_usd": self.total_cost_usd,
                "total_duration_ms": self.total_duration_ms,
            },
            "effort": self.effort,
            "fast_mode": self.fast_mode,
            "permission_mode": self.permission_mode,
            "running": self.running,
        })
    }

    /// Coarse fingerprint that avoids respawning on sub-second duration jitter or float noise.
    pub fn fingerprint(&self) -> u64 {
        let mut h = DefaultHasher::new();
        self.version.hash(&mut h);
        self.session_id.hash(&mut h);
        self.transcript_path.hash(&mut h);
        self.model_id.hash(&mut h);
        self.model_display_name.hash(&mut h);
        self.current_dir.hash(&mut h);
        self.project_dir.hash(&mut h);
        self.git_branch.hash(&mut h);
        self.context_window_size.hash(&mut h);
        ((self.used_percentage * 10.0).round() as i64).hash(&mut h);
        self.input_tokens.hash(&mut h);
        self.output_tokens.hash(&mut h);
        self.cache_read_input_tokens.hash(&mut h);
        self.cache_creation_input_tokens.hash(&mut h);
        ((self.total_cost_usd * 10_000.0).round() as i64).hash(&mut h);
        (self.total_duration_ms / 1_000).hash(&mut h);
        self.effort.hash(&mut h);
        self.fast_mode.hash(&mut h);
        self.permission_mode.hash(&mut h);
        self.running.hash(&mut h);
        h.finish()
    }
}

struct PendingState {
    latest: Option<(u64, StatusLineSnapshot)>,
    last_queued_fp: Option<u64>,
}

/// Non-blocking background runner for an external `status_line` command.
pub struct StatusLineRunner {
    cached: Arc<RwLock<Option<Vec<Line<'static>>>>>,
    pending: Arc<Mutex<PendingState>>,
    notify: Arc<Notify>,
    stopped: Arc<AtomicBool>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl StatusLineRunner {
    /// Start a background status-line runner task. `on_update` is called whenever cached output
    /// changes so the TUI can schedule a redraw.
    pub fn spawn(
        config: StatusLineConfig,
        cwd: PathBuf,
        on_update: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let cached: Arc<RwLock<Option<Vec<Line<'static>>>>> = Arc::new(RwLock::new(None));
        let pending = Arc::new(Mutex::new(PendingState {
            latest: None,
            last_queued_fp: None,
        }));
        let notify = Arc::new(Notify::new());
        let stopped = Arc::new(AtomicBool::new(false));

        let worker_cached = Arc::clone(&cached);
        let worker_pending = Arc::clone(&pending);
        let worker_notify = Arc::clone(&notify);
        let worker_stopped = Arc::clone(&stopped);
        let timeout_ms = config
            .timeout_ms
            .filter(|&ms| ms > 0)
            .unwrap_or(DEFAULT_STATUS_LINE_TIMEOUT_MS);
        let refresh_interval = config
            .refresh_ms
            .filter(|&ms| ms > 0)
            .map(|ms| Duration::from_millis(ms.max(MIN_REFRESH_MS)));

        let handle = tokio::spawn(async move {
            let mut last_snapshot: Option<StatusLineSnapshot> = None;
            let mut last_ran_fp: Option<u64> = None;
            let mut first_run = true;

            loop {
                if worker_stopped.load(Ordering::Acquire) {
                    break;
                }

                // Wait for either a new snapshot notification or an optional periodic refresh tick.
                let next_snap = take_pending(&worker_pending, last_ran_fp);
                let (fp, snap) = match next_snap {
                    Some(pair) => pair,
                    None => {
                        match refresh_interval {
                            Some(interval) if last_snapshot.is_some() => {
                                tokio::select! {
                                    () = worker_notify.notified() => {}
                                    () = time::sleep(interval) => {
                                        if worker_stopped.load(Ordering::Acquire) {
                                            break;
                                        }
                                        if let Some(prev) = last_snapshot.clone() {
                                            let fp = prev.fingerprint();
                                            execute_and_cache(
                                                &config.command,
                                                &cwd,
                                                &prev,
                                                timeout_ms,
                                                &worker_cached,
                                                &on_update,
                                            )
                                            .await;
                                            last_ran_fp = Some(fp);
                                            continue;
                                        }
                                    }
                                }
                            }
                            _ => {
                                worker_notify.notified().await;
                            }
                        }
                        if worker_stopped.load(Ordering::Acquire) {
                            break;
                        }
                        if !first_run {
                            time::sleep(Duration::from_millis(DEFAULT_DEBOUNCE_MS)).await;
                        }
                        match take_pending(&worker_pending, last_ran_fp) {
                            Some(pair) => pair,
                            None => continue,
                        }
                    }
                };

                first_run = false;
                last_snapshot = Some(snap.clone());
                last_ran_fp = Some(fp);

                execute_and_cache(
                    &config.command,
                    &cwd,
                    &snap,
                    timeout_ms,
                    &worker_cached,
                    &on_update,
                )
                .await;
            }
        });

        Self {
            cached,
            pending,
            notify,
            stopped,
            task: Mutex::new(Some(handle)),
        }
    }

    /// Submit the latest snapshot from the synchronous draw path (O(1), non-blocking).
    pub fn update_snapshot(&self, snapshot: StatusLineSnapshot) {
        let fp = snapshot.fingerprint();
        let mut guard = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if guard.last_queued_fp == Some(fp) {
            return;
        }
        guard.last_queued_fp = Some(fp);
        guard.latest = Some((fp, snapshot));
        drop(guard);
        self.notify.notify_one();
    }

    /// O(1) read of the most recently rendered status-line output.
    pub fn current_lines(&self) -> Option<Vec<Line<'static>>> {
        self.cached
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Drop for StatusLineRunner {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        self.notify.notify_one();
        if let Some(handle) = self.task.lock().unwrap_or_else(|e| e.into_inner()).take() {
            handle.abort();
        }
    }
}

fn take_pending(
    pending: &Mutex<PendingState>,
    last_ran_fp: Option<u64>,
) -> Option<(u64, StatusLineSnapshot)> {
    let mut guard = pending.lock().unwrap_or_else(|e| e.into_inner());
    let (fp, snap) = guard.latest.take()?;
    if last_ran_fp == Some(fp) {
        return None;
    }
    Some((fp, snap))
}

async fn execute_and_cache(
    command: &str,
    cwd: &Path,
    snapshot: &StatusLineSnapshot,
    timeout_ms: u64,
    cached: &RwLock<Option<Vec<Line<'static>>>>,
    on_update: &Arc<dyn Fn() + Send + Sync>,
) {
    let payload = snapshot.to_json();
    if let Ok(lines) = run_command_once(command, cwd, &payload, timeout_ms).await {
        let is_blank = lines
            .iter()
            .all(|line| line.spans.iter().all(|s| s.content.trim().is_empty()));
        let next = if is_blank { None } else { Some(lines) };
        let changed = {
            let mut slot = cached.write().unwrap_or_else(|e| e.into_inner());
            if *slot == next {
                false
            } else {
                *slot = next;
                true
            }
        };
        if changed {
            on_update();
        }
    }
}

/// Kill an entire Unix process group (`-pgid`) with `SIGKILL`.
#[cfg(unix)]
fn kill_process_group(pid: u32) {
    if let Ok(pgid) = i32::try_from(pid)
        && pgid > 0
    {
        // SAFETY: `pgid > 0` targets only the child process group created via `process_group(0)`.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {}

/// Drop guard guaranteeing the spawned process group is killed on every exit path (normal exit,
/// non-zero exit, timeout, wait failure, or future cancellation).
struct ProcessGroupGuard {
    pid: Option<u32>,
}

impl ProcessGroupGuard {
    fn new(child: &Child) -> Self {
        Self { pid: child.id() }
    }

    fn kill_now(&mut self) {
        if let Some(pid) = self.pid.take() {
            kill_process_group(pid);
        }
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.kill_now();
    }
}

/// Execute a status-line shell command once with JSON piped to `stdin`, bounded `stdout`, strict
/// timeout, and process-group cleanup on timeout or cancellation.
pub async fn run_command_once(
    command: &str,
    cwd: &Path,
    input_json: &Value,
    timeout_ms: u64,
) -> Result<Vec<Line<'static>>, String> {
    let input_bytes = serde_json::to_vec(input_json).map_err(|e| e.to_string())?;

    let mut cmd = {
        #[cfg(unix)]
        {
            let mut c = Command::new("sh");
            c.arg("-c").arg(command);
            c.process_group(0);
            c
        }
        #[cfg(not(unix))]
        {
            let mut c = Command::new("cmd");
            c.arg("/C").arg(command);
            c
        }
    };

    cmd.current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|err| format!("failed to spawn status_line command: {err}"))?;
    let mut pg_guard = ProcessGroupGuard::new(&child);

    let mut stdin = child.stdin.take();
    let mut stdout = child.stdout.take();

    let timeout = Duration::from_millis(timeout_ms.max(1));
    let io_future = async {
        let write_fut = async {
            if let Some(mut handle) = stdin.take() {
                // Ignore BrokenPipe when fast scripts exit without reading stdin.
                let _ = handle.write_all(&input_bytes).await;
                let _ = handle.shutdown().await;
            }
        };
        let read_fut = async {
            let mut buf = Vec::new();
            if let Some(handle) = stdout.take() {
                let mut limited = handle.take(MAX_STATUS_LINE_STDOUT_BYTES as u64);
                let _ = limited.read_to_end(&mut buf).await;
            }
            buf
        };
        let (_, output) = tokio::join!(write_fut, read_fut);
        let status = child.wait().await;
        (output, status)
    };

    match time::timeout(timeout, io_future).await {
        Ok((output, Ok(status))) => {
            // Reap any lingering background grandchildren in the child's process group even on
            // normal or non-zero exit (harmless ESRCH when no descendants remain).
            pg_guard.kill_now();
            if !status.success() {
                return Err(format!("status_line exited with {status}"));
            }
            let text = String::from_utf8_lossy(&output);
            Ok(parse_ansi_lines(&text))
        }
        Ok((_, Err(err))) => {
            pg_guard.kill_now();
            let _ = child.start_kill();
            let _ = time::timeout(Duration::from_millis(250), child.wait()).await;
            Err(format!("status_line wait error: {err}"))
        }
        Err(_) => {
            // Timed out: kill the entire process group and reap the child handle.
            pg_guard.kill_now();
            let _ = child.start_kill();
            let _ = time::timeout(Duration::from_millis(250), child.wait()).await;
            Err(format!("status_line timed out after {timeout_ms}ms"))
        }
    }
}

/// Parse ANSI-colored stdout text into styled `ratatui` lines using a byte-safe VT100/UTF-8
/// state machine (`vte`).
///
/// Supports standard 16 colors, bright colors, 256-color (`38;5;n` / `48;5;n`), TrueColor
/// (`38;2;r;g;b` / `48;2;r;g;b`, including colon sub-parameters), bold/dim/italic/underline/
/// reversed/crossed-out, and resets. Non-SGR CSI, OSC, DCS/APC/PM/SOS, and incomplete escape
/// sequences are stripped cleanly without ever panicking on UTF-8 boundaries.
pub fn parse_ansi_lines(raw: &str) -> Vec<Line<'static>> {
    let mut parser = Parser::new();
    let mut sink = AnsiLineSink::default();
    parser.advance(&mut sink, raw.as_bytes());
    sink.finish()
}

#[derive(Default)]
struct AnsiLineSink {
    lines: Vec<Line<'static>>,
    current_spans: Vec<Span<'static>>,
    buf: String,
    style: Style,
}

impl AnsiLineSink {
    fn flush_span(&mut self) {
        if !self.buf.is_empty() {
            self.current_spans
                .push(Span::styled(std::mem::take(&mut self.buf), self.style));
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_span();
        if !self.current_spans.is_empty() {
            self.lines.push(Line::from(self.current_spans));
        }
        // Trim trailing empty lines produced by trailing newlines.
        while self
            .lines
            .last()
            .is_some_and(|line| line.spans.iter().all(|s| s.content.is_empty()))
        {
            self.lines.pop();
        }
        self.lines
    }
}

impl Perform for AnsiLineSink {
    fn print(&mut self, c: char) {
        self.buf.push(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => {
                self.flush_span();
                self.lines
                    .push(Line::from(std::mem::take(&mut self.current_spans)));
            }
            b'\t' => {
                self.buf.push_str("  ");
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if action == 'm' && !ignore && intermediates.is_empty() {
            self.flush_span();
            apply_sgr(params, &mut self.style);
        }
    }
}

fn apply_sgr(params: &Params, style: &mut Style) {
    let groups: Vec<&[u16]> = params.iter().collect();
    if groups.is_empty() {
        *style = Style::default();
        return;
    }

    let mut idx = 0usize;
    while idx < groups.len() {
        let group = groups[idx];
        let code = group.first().copied().unwrap_or(0);
        match code {
            0 => *style = Style::default(),
            1 => *style = style.add_modifier(Modifier::BOLD),
            2 => *style = style.add_modifier(Modifier::DIM),
            3 => *style = style.add_modifier(Modifier::ITALIC),
            4 => *style = style.add_modifier(Modifier::UNDERLINED),
            7 => *style = style.add_modifier(Modifier::REVERSED),
            9 => *style = style.add_modifier(Modifier::CROSSED_OUT),
            22 => *style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => *style = style.remove_modifier(Modifier::ITALIC),
            24 => *style = style.remove_modifier(Modifier::UNDERLINED),
            27 => *style = style.remove_modifier(Modifier::REVERSED),
            29 => *style = style.remove_modifier(Modifier::CROSSED_OUT),
            30..=37 => *style = style.fg(ansi_basic_color(code - 30, false)),
            38 => {
                if let Some(color) = parse_extended_color(&groups, &mut idx) {
                    *style = style.fg(color);
                }
            }
            39 => *style = style.fg(Color::Reset),
            40..=47 => *style = style.bg(ansi_basic_color(code - 40, false)),
            48 => {
                if let Some(color) = parse_extended_color(&groups, &mut idx) {
                    *style = style.bg(color);
                }
            }
            49 => *style = style.bg(Color::Reset),
            90..=97 => *style = style.fg(ansi_basic_color(code - 90, true)),
            100..=107 => *style = style.bg(ansi_basic_color(code - 100, true)),
            _ => {}
        }
        idx += 1;
    }
}

fn parse_extended_color(groups: &[&[u16]], idx: &mut usize) -> Option<Color> {
    let group = groups.get(*idx).copied()?;
    // Colon-separated sub-parameters: `38:5:n`, `38:2:r:g:b`, or ITU T.416 `38:2:cs:r:g:b`.
    if group.len() >= 2 {
        return match group[1] {
            5 => u8::try_from(*group.get(2)?).ok().map(Color::Indexed),
            2 => {
                let offset = if group.len() >= 6 { 3 } else { 2 };
                let r = u8::try_from(*group.get(offset)?).ok()?;
                let g = u8::try_from(*group.get(offset + 1)?).ok()?;
                let b = u8::try_from(*group.get(offset + 2)?).ok()?;
                Some(Color::Rgb(r, g, b))
            }
            _ => None,
        };
    }

    // Semicolon-separated parameters: `38;5;n` or `38;2;r;g;b`.
    match groups.get(*idx + 1).and_then(|g| g.first().copied())? {
        5 => {
            let n = u8::try_from(groups.get(*idx + 2).and_then(|g| g.first().copied())?).ok()?;
            *idx += 2;
            Some(Color::Indexed(n))
        }
        2 => {
            let r = u8::try_from(groups.get(*idx + 2).and_then(|g| g.first().copied())?).ok()?;
            let g = u8::try_from(groups.get(*idx + 3).and_then(|g| g.first().copied())?).ok()?;
            let b = u8::try_from(groups.get(*idx + 4).and_then(|g| g.first().copied())?).ok()?;
            *idx += 4;
            Some(Color::Rgb(r, g, b))
        }
        _ => None,
    }
}

const fn ansi_basic_color(offset: u16, bright: bool) -> Color {
    match (offset, bright) {
        (0, false) => Color::Black,
        (1, false) => Color::Red,
        (2, false) => Color::Green,
        (3, false) => Color::Yellow,
        (4, false) => Color::Blue,
        (5, false) => Color::Magenta,
        (6, false) => Color::Cyan,
        (7, false) => Color::Gray,
        (0, true) => Color::DarkGray,
        (1, true) => Color::LightRed,
        (2, true) => Color::LightGreen,
        (3, true) => Color::LightYellow,
        (4, true) => Color::LightBlue,
        (5, true) => Color::LightMagenta,
        (6, true) => Color::LightCyan,
        (7, true) => Color::White,
        _ => Color::Reset,
    }
}
