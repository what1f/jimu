use crate::WorkspaceToolsConfig;
use crate::events::{
    BackgroundEventCallback, BackgroundEvents, BackgroundToolEvent, MonitorStopReason, WakeGate,
};
use crate::exec_session::{
    OUTPUT_CHUNK_BYTES, OutputStream, ProcessChild, ProcessControl, ProcessEnd, ProcessOutput,
    deadline_after_ms, prepare_shell, spawn_managed, spawn_process, wait_deadline,
};
use pikra_agent::{AgentTool, AgentToolResult, ToolExecutionMode, ToolProgressCallback};
use pikra_ai::{BoxFuture, CancelToken, ToolDefinition};
use pikra_schema::{
    int_schema, obj_schema, str_schema, string_enum, with_bounds, with_description,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::time::Instant;

const DEFAULT_TIMEOUT_MS: u64 = 1_800_000;
const MAX_TIMEOUT_MS: u64 = 86_400_000;
const TAIL_LINES: usize = 50;
const FLOOD_WINDOW: Duration = Duration::from_secs(60);
static NEXT_MONITOR: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub struct MonitorConfig {
    pub max_active: usize,
    /// Relative paths resolve against the workspace. Defaults to `agent-monitors`
    /// in the system temporary directory.
    pub monitor_log_dir: Option<PathBuf>,
    pub coalesce_window: Duration,
    pub max_lines_per_minute: usize,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            max_active: 10,
            monitor_log_dir: None,
            coalesce_window: Duration::from_millis(200),
            max_lines_per_minute: 200,
        }
    }
}

/// Read-only summary of an active background monitor managed by [`MonitorManager`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningMonitorInfo {
    pub id: String,
    pub name: String,
    pub command: String,
    pub started_at_ms: u64,
    pub log_path: PathBuf,
}

#[derive(Clone)]
pub struct MonitorManager(Arc<MonitorManagerInner>);

struct MonitorManagerInner {
    workspace: Arc<WorkspaceToolsConfig>,
    config: MonitorConfig,
    events: BackgroundEvents,
    monitors: Mutex<Monitors>,
}

#[derive(Default)]
struct Monitors {
    closed: bool,
    entries: BTreeMap<String, MonitorEntry>,
}

struct MonitorEntry {
    info: MonitorInfo,
    control: Arc<ProcessControl>,
    exiting: bool,
}

impl MonitorEntry {
    fn is_active(&self) -> bool {
        !self.exiting && !self.control.stopped.is_cancelled()
    }
}

#[derive(Clone, Serialize)]
struct MonitorInfo {
    id: String,
    name: String,
    command: String,
    notify: MonitorNotify,
    started_at_ms: u64,
    log_path: PathBuf,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum MonitorNotify {
    #[default]
    Lines,
    Exit,
}

#[derive(Deserialize)]
struct StartArgs {
    name: String,
    command: String,
    cwd: Option<String>,
    #[serde(default)]
    notify: MonitorNotify,
    timeout_ms: Option<u64>,
}

impl MonitorManager {
    pub fn new(
        workspace: Arc<WorkspaceToolsConfig>,
        wake_gate: WakeGate,
        callback: Option<BackgroundEventCallback>,
    ) -> Self {
        Self::with_config(workspace, wake_gate, callback, MonitorConfig::default())
    }

    pub fn with_config(
        workspace: Arc<WorkspaceToolsConfig>,
        wake_gate: WakeGate,
        callback: Option<BackgroundEventCallback>,
        config: MonitorConfig,
    ) -> Self {
        Self(Arc::new(MonitorManagerInner {
            workspace,
            config,
            events: BackgroundEvents {
                wake_gate,
                callback,
            },
            monitors: Mutex::default(),
        }))
    }

    /// List currently active monitors.
    pub fn list_running(&self) -> Vec<RunningMonitorInfo> {
        let monitors = self.0.monitors.lock().unwrap_or_else(|e| e.into_inner());
        monitors
            .entries
            .values()
            .filter(|entry| entry.is_active())
            .map(|entry| RunningMonitorInfo {
                id: entry.info.id.clone(),
                name: entry.info.name.clone(),
                command: entry.info.command.clone(),
                started_at_ms: entry.info.started_at_ms,
                log_path: entry.info.log_path.clone(),
            })
            .collect()
    }

    /// Request termination of an active monitor by ID. Returns `true` if found.
    pub fn kill(&self, id: &str) -> bool {
        let control = {
            let monitors = self.0.monitors.lock().unwrap_or_else(|e| e.into_inner());
            monitors.entries.get(id).map(|entry| entry.control.clone())
        };
        if let Some(control) = control {
            control.stop();
            true
        } else {
            false
        }
    }

    /// Stop and reap all monitors silently. This manager will reject new monitors.
    pub async fn shutdown(&self) {
        let controls: Vec<_> = {
            let mut monitors = self.0.monitors.lock().unwrap_or_else(|e| e.into_inner());
            monitors.closed = true;
            monitors
                .entries
                .values()
                .map(|entry| entry.control.clone())
                .collect()
        };
        for control in &controls {
            control.stop();
        }
        for control in controls {
            control.wait_done().await;
        }
    }

    async fn start(&self, args: StartArgs, cancel: &CancelToken) -> Result<MonitorInfo, String> {
        if args.name.trim().is_empty() {
            return Err("name must not be empty.".into());
        }
        let timeout = args.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        if !(1..=MAX_TIMEOUT_MS).contains(&timeout) {
            return Err(format!(
                "timeout_ms must be between 1 and {MAX_TIMEOUT_MS}."
            ));
        }
        self.0
            .monitors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .check_capacity(self.0.config.max_active)?;

        let (_, process) = prepare_shell(
            &self.0.workspace,
            &args.command,
            args.cwd.as_deref(),
            cancel,
        )
        .await?;
        let (log_dir, is_default_dir) = match &self.0.config.monitor_log_dir {
            Some(path) => (self.0.workspace.cwd.join(path), false),
            None => {
                #[cfg(unix)]
                let name = format!("agent-monitors-{}", unsafe { libc::geteuid() });
                #[cfg(not(unix))]
                let name = "agent-monitors".to_string();
                (std::env::temp_dir().join(name), true)
            }
        };
        let log_dir =
            std::path::absolute(log_dir).map_err(|err| format!("Invalid log directory: {err}"))?;
        if is_default_dir {
            #[cfg(unix)]
            {
                let mut builder = tokio::fs::DirBuilder::new();
                builder.recursive(true).mode(0o700);
                builder
                    .create(&log_dir)
                    .await
                    .map_err(|err| format!("Could not create log directory: {err}"))?;
            }
            #[cfg(not(unix))]
            {
                tokio::fs::create_dir_all(&log_dir)
                    .await
                    .map_err(|err| format!("Could not create log directory: {err}"))?;
            }
        } else {
            tokio::fs::create_dir_all(&log_dir)
                .await
                .map_err(|err| format!("Could not create log directory: {err}"))?;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let seq = NEXT_MONITOR.fetch_add(1, Ordering::Relaxed);
        let id = format!("{}-{}-{seq}", std::process::id(), now.as_nanos());
        let log_path = log_dir.join(format!("{id}.log"));
        let mut open_opts = OpenOptions::new();
        open_opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            open_opts.mode(0o600);
        }
        let log = open_opts
            .open(&log_path)
            .await
            .map_err(|err| format!("Could not create monitor log: {err}"))?;
        let info = MonitorInfo {
            id,
            name: args.name,
            command: args.command,
            notify: args.notify,
            started_at_ms: u64::try_from(now.as_millis()).unwrap_or(u64::MAX),
            log_path,
        };

        let start = || {
            let mut monitors = self.0.monitors.lock().unwrap_or_else(|e| e.into_inner());
            monitors.check_capacity(self.0.config.max_active)?;
            if cancel.is_cancelled() {
                return Err("Operation aborted".into());
            }
            let (child, _, output) = spawn_process(process, false, false)
                .map_err(|err| format!("Could not start monitor: {err}"))?;
            let deadline = deadline_after_ms(timeout);
            monitors.entries.insert(
                info.id.clone(),
                MonitorEntry {
                    info: info.clone(),
                    control: child.control.clone(),
                    exiting: false,
                },
            );
            drop(monitors);
            let manager = Arc::downgrade(&self.0);
            let cleanup_manager = manager.clone();
            let id = info.id.clone();
            spawn_managed(
                child.control.clone(),
                run_monitor(
                    manager,
                    info.clone(),
                    MonitorOutput::new(&self.0.config, &self.0.workspace, info.notify),
                    log,
                    child,
                    output,
                    deadline,
                ),
                move || {
                    if let Some(manager) = cleanup_manager.upgrade() {
                        manager
                            .monitors
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .entries
                            .remove(&id);
                    }
                },
            );
            Ok(())
        };
        if let Err(err) = start() {
            let _ = tokio::fs::remove_file(&info.log_path).await;
            return Err(err);
        }
        Ok(info)
    }

    async fn stop(&self, id: &str) -> Result<(), String> {
        let control = self
            .0
            .monitors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .get(id)
            .map(|entry| entry.control.clone())
            .ok_or_else(|| format!("Monitor {id} was not found."))?;
        control.stop();
        control.wait_done().await;
        Ok(())
    }

    fn list(&self) -> Vec<MonitorInfo> {
        self.0
            .monitors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .values()
            .filter(|entry| entry.is_active())
            .map(|entry| entry.info.clone())
            .collect()
    }
}

impl Monitors {
    fn check_capacity(&self, limit: usize) -> Result<(), String> {
        if self.closed {
            return Err("Monitors are closed.".into());
        }
        let running = self
            .entries
            .values()
            .filter(|entry| entry.is_active())
            .count();
        if running >= limit {
            return Err(format!("Too many monitors are running (limit {limit})."));
        }
        Ok(())
    }
}

impl Drop for MonitorManagerInner {
    fn drop(&mut self) {
        for entry in self
            .monitors
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .values()
        {
            entry.control.stop();
        }
    }
}

#[derive(Clone, Copy)]
enum MonitorAction {
    Unified,
    Start,
    Stop,
    List,
}

#[derive(Clone)]
pub struct MonitorTool {
    manager: MonitorManager,
    action: MonitorAction,
}

impl MonitorTool {
    pub fn new(manager: MonitorManager) -> Self {
        Self {
            manager,
            action: MonitorAction::Unified,
        }
    }

    /// Build monitor_start, monitor_stop and monitor_list with the same manager.
    pub fn split(manager: MonitorManager) -> [Self; 3] {
        [
            MonitorAction::Start,
            MonitorAction::Stop,
            MonitorAction::List,
        ]
        .map(|action| Self {
            manager: manager.clone(),
            action,
        })
    }
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
enum MonitorArgs {
    Start(StartArgs),
    Stop { id: String },
    List,
}

impl AgentTool for MonitorTool {
    fn definition(&self) -> ToolDefinition {
        let start_parameters = |required| {
            [
                ("name", str_schema(), required),
                ("command", str_schema(), required),
                ("cwd", str_schema(), false),
                (
                    "notify",
                    with_description(
                        string_enum(["lines", "exit"]),
                        "When to report output. Defaults to lines.",
                    ),
                    false,
                ),
                (
                    "timeout_ms",
                    with_bounds(int_schema(), Some(1.0), Some(MAX_TIMEOUT_MS as f64)),
                    false,
                ),
            ]
        };
        let (name, description, parameters) = match self.action {
            MonitorAction::Unified => (
                "monitor",
                "Start, stop, or list background monitors. Starting requires name and command. Stopping requires id.",
                obj_schema(
                    [
                        ("action", string_enum(["start", "stop", "list"]), true),
                        ("id", str_schema(), false),
                    ]
                    .into_iter()
                    .chain(start_parameters(false)),
                ),
            ),
            MonitorAction::Start => (
                "monitor_start",
                "Run a background command and receive updates.",
                obj_schema(start_parameters(true)),
            ),
            MonitorAction::Stop => (
                "monitor_stop",
                "Stop a background monitor.",
                obj_schema([("id", str_schema(), true)]),
            ),
            MonitorAction::List => (
                "monitor_list",
                "List running monitors.",
                obj_schema(std::iter::empty::<(&str, Value, bool)>()),
            ),
        };
        ToolDefinition {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    fn execute(
        &self,
        _tool_call_id: String,
        mut args: Value,
        cancel: CancelToken,
        _on_update: ToolProgressCallback,
    ) -> BoxFuture<'_, Result<AgentToolResult, String>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err("Operation aborted".into());
            }
            let action = match self.action {
                MonitorAction::Unified => None,
                MonitorAction::Start => Some("start"),
                MonitorAction::Stop => Some("stop"),
                MonitorAction::List => Some("list"),
            };
            if let Some(action) = action {
                args.as_object_mut()
                    .ok_or("Expected an object for monitor arguments.")?
                    .insert("action".into(), json!(action));
            }
            let args: MonitorArgs =
                serde_json::from_value(args).map_err(|error| error.to_string())?;
            let (text, details) = match args {
                MonitorArgs::Start(args) => {
                    let info = self.manager.start(args, &cancel).await?;
                    (
                        format!("Monitor started: {} ({})", info.name, info.id),
                        json!(info),
                    )
                }
                MonitorArgs::Stop { id } => {
                    self.manager.stop(&id).await?;
                    ("Monitor stopped.".into(), json!({"id": id}))
                }
                MonitorArgs::List => {
                    let monitors = self.manager.list();
                    let text = if monitors.is_empty() {
                        "No monitors are running.".into()
                    } else {
                        serde_json::to_string(&monitors).map_err(|err| err.to_string())?
                    };
                    (text, json!({"monitors": monitors}))
                }
            };
            let mut result = AgentToolResult::text(text);
            result.details = details;
            Ok(result)
        })
    }
}

async fn run_monitor(
    manager: Weak<MonitorManagerInner>,
    info: MonitorInfo,
    mut capture: MonitorOutput,
    mut log: File,
    mut child: ProcessChild,
    mut output: ProcessOutput,
    deadline: Option<Instant>,
) {
    let control = child.control.clone();
    let interrupt = CancelToken::new();
    let read = async {
        let mut buffers = [[0; OUTPUT_CHUNK_BYTES]; 2];
        loop {
            tokio::select! {
                biased;
                _ = wait_deadline(capture.batch_deadline) => {
                    capture.emit_lines(&manager, &info, &control);
                }
                chunk = output.next(&mut buffers) => {
                    let Some((stream, chunk)) = chunk else { break };
                    let was_flooded = capture.flooded;
                    capture.push(stream, chunk, &interrupt);
                    if capture.flooded && !was_flooded {
                        control.kill_group();
                    }
                    if log.write_all(chunk).await.is_err() {
                        interrupt.cancel();
                        break;
                    }
                }
            }
        }
        capture.finish(&interrupt);
        let _ = log.flush().await;
        capture.emit_lines(&manager, &info, &control);
    };
    let (exit, ()) = tokio::join!(child.wait(deadline, Some(&interrupt)), read);

    drop(log);
    drop(output);
    let events = manager.upgrade().and_then(|manager| {
        let mut monitors = manager.monitors.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = monitors.entries.get_mut(&info.id) {
            entry.exiting = true;
        }
        (!monitors.closed).then(|| manager.events.clone())
    });
    if exit.reason != ProcessEnd::Stopped
        && !control.stopped.is_cancelled()
        && let Some(events) = events
    {
        let reason = match exit.reason {
            ProcessEnd::Timeout => MonitorStopReason::Timeout,
            _ if capture.flooded => MonitorStopReason::Flood,
            _ => MonitorStopReason::Exited,
        };
        events.emit(BackgroundToolEvent::MonitorExited {
            id: info.id,
            name: info.name,
            exit_code: exit.code(),
            reason,
            tail_lines: capture.tail.make_contiguous().join("\n"),
            log_path: info.log_path,
        });
    }
}

/// Keep complete lines for notifications and bounded prefixes for long lines.
struct MonitorOutput {
    line_events: bool,
    coalesce_window: Duration,
    max_lines_per_minute: usize,
    line_bytes: usize,
    stdout: PartialLine,
    stderr: PartialLine,
    tail: VecDeque<String>,
    recent: VecDeque<Instant>,
    batch: Vec<String>,
    batch_deadline: Option<Instant>,
    flooded: bool,
}

#[derive(Default)]
struct PartialLine {
    bytes: Vec<u8>,
    pending: bool,
}

impl PartialLine {
    fn finish(&mut self) -> Option<String> {
        if !std::mem::take(&mut self.pending) {
            return None;
        }
        let line = self.bytes.strip_suffix(b"\r").unwrap_or(&self.bytes);
        let text = String::from_utf8_lossy(line).into_owned();
        self.bytes.clear();
        Some(text)
    }
}

impl MonitorOutput {
    fn new(
        config: &MonitorConfig,
        workspace: &WorkspaceToolsConfig,
        notify: MonitorNotify,
    ) -> Self {
        Self {
            line_events: notify == MonitorNotify::Lines,
            coalesce_window: config.coalesce_window,
            max_lines_per_minute: config.max_lines_per_minute,
            line_bytes: workspace.max_output_bytes.min(OUTPUT_CHUNK_BYTES),
            stdout: PartialLine::default(),
            stderr: PartialLine::default(),
            tail: VecDeque::with_capacity(TAIL_LINES),
            recent: VecDeque::new(),
            batch: Vec::new(),
            batch_deadline: None,
            flooded: false,
        }
    }

    fn push(&mut self, stream: OutputStream, chunk: &[u8], interrupt: &CancelToken) {
        if self.line_events && stream == OutputStream::Stderr {
            return;
        }
        for part in chunk.split_inclusive(|&byte| byte == b'\n') {
            let current = match stream {
                OutputStream::Stdout => &mut self.stdout,
                OutputStream::Stderr => &mut self.stderr,
            };
            current.pending = true;
            let body = part.strip_suffix(b"\n").unwrap_or(part);
            let take = body
                .len()
                .min(self.line_bytes.saturating_sub(current.bytes.len()));
            current.bytes.extend_from_slice(&body[..take]);
            if body.len() != part.len() {
                let line = current.finish().unwrap();
                self.line(line, interrupt);
            }
        }
    }

    fn finish(&mut self, interrupt: &CancelToken) {
        for line in [self.stdout.finish(), self.stderr.finish()]
            .into_iter()
            .flatten()
        {
            self.line(line, interrupt);
        }
    }

    fn line(&mut self, line: String, interrupt: &CancelToken) {
        if self.tail.len() == TAIL_LINES {
            self.tail.pop_front();
        }
        if self.line_events && !self.flooded {
            let now = Instant::now();
            while self
                .recent
                .front()
                .is_some_and(|at| now.duration_since(*at) >= FLOOD_WINDOW)
            {
                self.recent.pop_front();
            }
            if self.recent.len() >= self.max_lines_per_minute {
                self.flooded = true;
                self.batch.clear();
                self.batch_deadline = None;
                interrupt.cancel();
            } else {
                self.recent.push_back(now);
                self.batch.push(line.clone());
                self.batch_deadline = self
                    .batch_deadline
                    .or_else(|| now.checked_add(self.coalesce_window));
            }
        }
        self.tail.push_back(line);
    }

    fn emit_lines(
        &mut self,
        manager: &Weak<MonitorManagerInner>,
        info: &MonitorInfo,
        control: &ProcessControl,
    ) {
        self.batch_deadline = None;
        let lines = std::mem::take(&mut self.batch);
        if lines.is_empty() {
            return;
        }
        let events = manager.upgrade().and_then(|manager| {
            (!manager
                .monitors
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .closed)
                .then(|| manager.events.clone())
        });
        if !control.stopped.is_cancelled()
            && let Some(events) = events
        {
            events.emit(BackgroundToolEvent::MonitorLines {
                id: info.id.clone(),
                name: info.name.clone(),
                lines: lines.join("\n"),
                log_path: info.log_path.clone(),
            });
        }
    }
}
