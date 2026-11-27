use crate::WorkspaceToolsConfig;
use crate::events::{
    BackgroundEventCallback, BackgroundEvents, BackgroundToolEvent, ShellStopReason, WakeGate,
};
use crate::util::{TailSpilloverBuffer, TruncatedOutput, display_path, resolve_path};
use jimu_agent::{AgentTool, AgentToolResult, ToolExecutionMode, ToolProgressCallback};
use jimu_ai::{BoxFuture, CancelToken, ToolDefinition};
use jimu_schema::{bool_schema, int_schema, obj_schema, str_schema, with_bounds, with_description};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
#[cfg(unix)]
use std::fs::File;
use std::io;
#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
#[cfg(unix)]
use tokio::io::Interest;
#[cfg(unix)]
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex as AsyncMutex, Notify, mpsc, watch};
use tokio::time::{Instant, sleep_until};

const DEFAULT_MAX_SESSIONS: usize = 16;
const DEFAULT_EXEC_YIELD_MS: u64 = 10_000;
const DEFAULT_STDIN_YIELD_MS: u64 = 5_000;
const MAX_TIMEOUT_MS: u64 = 86_400_000;
const MAX_COMMAND_BYTES: usize = 65_536;
pub(crate) const OUTPUT_CHUNK_BYTES: usize = 8192;

#[cfg(unix)]
fn kill_process_group(pid: u32) {
    if let Ok(pgid) = i32::try_from(pid)
        && pgid > 0
    {
        // SAFETY: Sending SIGKILL to the negative process group id we created via
        // `Command::process_group(0)` / `setsid()` terminates the child and every descendant.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(windows)]
fn kill_process_group(pid: u32) {
    use std::os::windows::process::CommandExt as _;
    if pid == 0 {
        return;
    }
    let _ = std::process::Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status();
}

#[cfg(not(any(unix, windows)))]
fn kill_process_group(_pid: u32) {}

pub(crate) struct ProcessGroupGuard {
    pid: Option<u32>,
}

impl ProcessGroupGuard {
    pub(crate) fn new(pid: Option<u32>) -> Self {
        Self { pid }
    }

    pub(crate) fn kill_now(&mut self) {
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

pub(crate) fn build_shell_command(shell: Option<&str>, command: &str) -> Command {
    #[cfg(unix)]
    let mut cmd = {
        let bin = shell.unwrap_or("/bin/sh");
        let mut cmd = Command::new(bin);
        cmd.arg("-c").arg(command);
        cmd
    };

    #[cfg(windows)]
    let mut cmd = {
        let bin = shell.unwrap_or("cmd");
        let mut cmd = Command::new(bin);
        cmd.creation_flags(CREATE_NO_WINDOW);
        let name = std::path::Path::new(bin)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(bin)
            .to_ascii_lowercase();
        match name.as_str() {
            "cmd" | "cmd.exe" => {
                // `/D` disables AutoRun registry commands and `/S` strips only the
                // outer quotes around `command`, preserving inner quotes verbatim.
                cmd.raw_arg("/D /S /C").raw_arg(format!("\"{command}\""));
            }
            "pwsh" | "pwsh.exe" | "powershell" | "powershell.exe" => {
                cmd.args(["-NoProfile", "-NonInteractive", "-Command", command]);
            }
            "sh" | "sh.exe" | "bash" | "bash.exe" | "zsh" | "zsh.exe" => {
                cmd.arg("-c").arg(command);
            }
            _ => {
                cmd.arg("/C").arg(command);
            }
        }
        cmd
    };

    #[cfg(not(any(unix, windows)))]
    let mut cmd = {
        let bin = shell.unwrap_or("cmd");
        let mut cmd = Command::new(bin);
        cmd.arg("/C").arg(command);
        cmd
    };

    // OSC 7577 security: never leak the host terminal's program-resume token to agent-run tools.
    cmd.env_remove("OSC7577_TOKEN");
    cmd
}

fn shell_exit_code(status: Option<ExitStatus>, timed_out: bool) -> i32 {
    if timed_out {
        return 124;
    }
    let Some(status) = status else {
        return 1;
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    status.code().unwrap_or(1)
}

#[cfg(unix)]
fn format_signal_notice(status: Option<ExitStatus>) -> String {
    use std::os::unix::process::ExitStatusExt;
    match status.and_then(|s| s.signal()) {
        Some(sig) => format!("Command terminated by signal {sig}."),
        None => "Command failed.".to_string(),
    }
}

#[cfg(not(unix))]
fn format_signal_notice(_status: Option<ExitStatus>) -> String {
    "Command failed.".to_string()
}

/// Owns running commands and a bounded set of completed background sessions
/// whose final output can still be collected. Clones share the same sessions.
/// Read-only summary of an active command session managed by [`ProcessSessionManager`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningProcessInfo {
    pub id: u32,
    pub command: String,
    pub cwd: PathBuf,
    pub backgrounded: bool,
}

#[derive(Clone)]
pub struct ProcessSessionManager(Arc<SessionManagerInner>);

struct SessionManagerInner {
    workspace: Arc<WorkspaceToolsConfig>,
    events: BackgroundEvents,
    max_sessions: usize,
    sessions: Mutex<Sessions>,
}

#[derive(Default)]
struct Sessions {
    closed: bool,
    next_id: u32,
    active: HashMap<u32, Arc<Session>>,
    finished: VecDeque<(u32, Arc<Session>)>,
}

impl Sessions {
    fn check_background_capacity(&self, limit: usize, exclude: Option<u32>) -> Result<(), String> {
        let running = self
            .active
            .iter()
            .filter(|(id, session)| {
                if Some(**id) == exclude {
                    return false;
                }
                let state = session.state.lock().unwrap_or_else(|e| e.into_inner());
                state.backgrounded && state.exit.is_none()
            })
            .count();
        if running >= limit {
            return Err(format!(
                "Too many background commands are running (limit {limit})."
            ));
        }
        Ok(())
    }
}

impl ProcessSessionManager {
    pub fn new(
        workspace: Arc<WorkspaceToolsConfig>,
        wake_gate: WakeGate,
        callback: Option<BackgroundEventCallback>,
    ) -> Self {
        Self::with_max_sessions(workspace, wake_gate, callback, DEFAULT_MAX_SESSIONS)
    }

    pub fn with_max_sessions(
        workspace: Arc<WorkspaceToolsConfig>,
        wake_gate: WakeGate,
        callback: Option<BackgroundEventCallback>,
        max_sessions: usize,
    ) -> Self {
        Self(Arc::new(SessionManagerInner {
            workspace,
            events: BackgroundEvents {
                wake_gate,
                callback,
            },
            max_sessions,
            sessions: Mutex::default(),
        }))
    }

    /// List currently running command sessions in ascending ID order.
    pub fn list_running(&self) -> Vec<RunningProcessInfo> {
        let sessions = self.0.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let mut out: Vec<_> = sessions
            .active
            .values()
            .filter_map(|session| {
                let state = session.state.lock().unwrap_or_else(|e| e.into_inner());
                state.exit.is_none().then(|| RunningProcessInfo {
                    id: session.id,
                    command: session.command.clone(),
                    cwd: session.cwd.clone(),
                    backgrounded: state.backgrounded,
                })
            })
            .collect();
        out.sort_by_key(|info| info.id);
        out
    }

    /// Request termination of a running command session by ID. Returns `true` if found.
    pub fn kill(&self, id: u32) -> bool {
        let session = {
            let sessions = self.0.sessions.lock().unwrap_or_else(|e| e.into_inner());
            sessions.active.get(&id).cloned()
        };
        if let Some(session) = session {
            session.control.stop();
            true
        } else {
            false
        }
    }

    /// Stop and reap the managed commands. This manager will reject new commands.
    pub async fn shutdown(&self) {
        let sessions: Vec<_> = {
            let mut sessions = self.0.sessions.lock().unwrap_or_else(|e| e.into_inner());
            sessions.closed = true;
            sessions.finished.clear();
            sessions.active.values().cloned().collect()
        };
        for session in &sessions {
            session.control.stop();
        }
        for session in sessions {
            session.control.wait_done().await;
        }
    }

    fn start(
        &self,
        command: String,
        cwd: PathBuf,
        process: Command,
        args: &ExecArgs,
        progress: mpsc::Sender<String>,
    ) -> Result<Arc<Session>, String> {
        let mut sessions = self.0.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if sessions.closed {
            return Err("Command sessions are closed.".into());
        }
        if args.background {
            sessions.check_background_capacity(self.0.max_sessions, None)?;
        }
        let id = sessions
            .next_id
            .checked_add(1)
            .ok_or("No command session IDs are available.")?;
        let (child, input, output) = spawn_process(process, true, args.tty)
            .map_err(|err| format!("Could not start command: {err}"))?;
        let timeout_ms = if args.background {
            args.timeout_ms
        } else {
            args.timeout_ms.or(self.0.workspace.default_exec_timeout_ms)
        };
        let deadline = timeout_ms.and_then(deadline_after_ms);
        let max_bytes = self.0.workspace.max_output_bytes;
        let max_lines = self.0.workspace.max_output_lines;
        let session = Arc::new(Session {
            id,
            command: command.clone(),
            cwd,
            control: child.control.clone(),
            input: AsyncMutex::new(input),
            changed: Notify::new(),
            state: Mutex::new(SessionState {
                unread: TailSpilloverBuffer::new(max_bytes, max_lines),
                has_output: false,
                backgrounded: args.background,
                exit: None,
            }),
            max_output_bytes: max_bytes,
            max_output_lines: max_lines,
        });
        sessions.next_id = id;
        sessions.active.insert(id, session.clone());

        drop(sessions);
        let manager = Arc::downgrade(&self.0);
        let cleanup_manager = manager.clone();
        let cleanup_session = session.clone();
        spawn_managed(
            session.control.clone(),
            run_shell(
                manager,
                session.clone(),
                command,
                child,
                output,
                deadline,
                progress,
            ),
            move || {
                if let Some(manager) = cleanup_manager.upgrade() {
                    manager
                        .sessions
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .active
                        .remove(&id);
                }
                cleanup_session
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .exit
                    .get_or_insert(ProcessExit {
                        reason: ProcessEnd::Stopped,
                        status: None,
                    });
                cleanup_session.changed.notify_waiters();
            },
        );
        Ok(session)
    }

    fn collect_exec(
        &self,
        session: &Session,
        explicit_background: bool,
    ) -> Result<AgentToolResult, String> {
        let (unread, exit) = {
            let sessions = self.0.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let mut state = session.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.exit.is_none() && !state.backgrounded {
                if sessions.closed {
                    return Err("Command sessions are closed.".into());
                }
                sessions.check_background_capacity(self.0.max_sessions, Some(session.id))?;
                state.backgrounded = true;
            }
            let unread = session.take_output(&mut state);
            let exit = if explicit_background {
                None
            } else {
                state.exit
            };
            (unread, exit)
        };
        Ok(session.render(unread, exit))
    }

    fn session(&self, id: u32) -> Result<Arc<Session>, String> {
        let sessions = self.0.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions
            .active
            .get(&id)
            .or_else(|| {
                sessions
                    .finished
                    .iter()
                    .find_map(|(key, entry)| (*key == id).then_some(entry))
            })
            .cloned()
            .ok_or_else(|| format!("Command session {id} was not found."))
    }
}

impl Drop for SessionManagerInner {
    fn drop(&mut self) {
        for session in self
            .sessions
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .active
            .values()
        {
            session.control.stop();
        }
    }
}

struct Session {
    id: u32,
    command: String,
    cwd: PathBuf,
    control: Arc<ProcessControl>,
    input: AsyncMutex<Option<ProcessInput>>,
    changed: Notify,
    state: Mutex<SessionState>,
    max_output_bytes: usize,
    max_output_lines: usize,
}

struct SessionState {
    unread: TailSpilloverBuffer,
    has_output: bool,
    backgrounded: bool,
    exit: Option<ProcessExit>,
}

impl Session {
    async fn wait_until(&self, ready: impl Fn(&SessionState) -> bool) {
        loop {
            let changed = self.changed.notified();
            if ready(&self.state.lock().unwrap_or_else(|e| e.into_inner())) {
                return;
            }
            changed.await;
        }
    }

    async fn wait_for_output(&self, after_write: bool) {
        self.wait_until(|state| state.has_output || state.exit.is_some())
            .await;
        if !after_write {
            return;
        }
        loop {
            let changed = self.changed.notified();
            if self
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .exit
                .is_some()
                || tokio::time::timeout(Duration::from_millis(80), changed)
                    .await
                    .is_err()
            {
                break;
            }
        }
    }

    fn take_output(&self, state: &mut SessionState) -> TailSpilloverBuffer {
        state.has_output = false;
        std::mem::replace(
            &mut state.unread,
            TailSpilloverBuffer::new(self.max_output_bytes, self.max_output_lines),
        )
    }

    fn render(&self, unread: TailSpilloverBuffer, exit: Option<ProcessExit>) -> AgentToolResult {
        let (structured_out, structured_trunc) = unread.structured_prefix();
        let structured_out = structured_out.into_owned();
        let notice = match exit {
            Some(exit) if exit.reason == ProcessEnd::Timeout => Some("Command timed out.".into()),
            Some(exit)
                if exit.reason == ProcessEnd::Exited
                    && !exit.status.is_some_and(|s| s.success()) =>
            {
                Some(failure_notice(exit.status))
            }
            _ => None,
        };
        let output = match &notice {
            Some(notice) => render_with_notice(&unread, self.max_output_bytes, notice),
            None => unread.render(self.max_output_bytes),
        };
        let text = if exit.is_none() {
            let status_line = format!("[session_id: {}, status: running]", self.id);
            if output.text.is_empty() {
                status_line
            } else {
                format!("{}\n{status_line}", output.text)
            }
        } else {
            output.text
        };
        let mut result = AgentToolResult::text(text);
        let mut details = match exit {
            Some(exit) => json!({"status": "exited", "exit_code": exit.code()}),
            None => json!({"status": "running", "session_id": self.id}),
        };
        details["cwd"] = json!(self.cwd);
        if output.truncated {
            details["truncated"] = json!(true);
            details["omitted_bytes"] = json!(output.omitted_bytes);
        }
        if let Some(path) = &output.full_output_path {
            details["full_output_path"] = json!(path);
        }
        if let Some(exit) = exit {
            let timed_out = exit.reason == ProcessEnd::Timeout;
            if timed_out {
                details["timed_out"] = json!(true);
            }
            result.is_error = notice.is_some();
            let mut structured = json!({
                "exit_code": shell_exit_code(exit.status, timed_out),
                "output": structured_out,
                "output_truncated": structured_trunc,
            });
            if let Some(path) = &output.full_output_path {
                structured["full_output_path"] = json!(path);
            }
            result.structured_content = Some(structured);
        }
        result.details = details;
        result
    }
}

async fn run_shell(
    manager: Weak<SessionManagerInner>,
    session: Arc<Session>,
    command: String,
    mut child: ProcessChild,
    mut output: ProcessOutput,
    deadline: Option<Instant>,
    progress: mpsc::Sender<String>,
) {
    let read = async {
        let mut buffers = [[0; OUTPUT_CHUNK_BYTES]; 2];
        let mut decoder = ProgressDecoder::default();
        while let Some((stream, chunk)) = output.next(&mut buffers).await {
            {
                let mut state = session.state.lock().unwrap_or_else(|e| e.into_inner());
                state.unread.push(chunk);
                state.has_output = true;
            }
            session.changed.notify_waiters();
            if !progress.is_closed() {
                let text = decoder.push(stream, chunk);
                if !text.is_empty() {
                    let _ = progress.try_send(text);
                }
            }
        }
        decoder.finish(|text| {
            let _ = progress.try_send(text);
        });
    };
    let (exit, ()) = tokio::join!(child.wait(deadline, None), read);
    let backgrounded = if let Some(manager) = manager.upgrade() {
        let mut sessions = manager.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = session.state.lock().unwrap_or_else(|e| e.into_inner());
        state.exit = Some(exit);
        if state.backgrounded && !sessions.closed {
            sessions.finished.push_back((session.id, session.clone()));
            if sessions.finished.len() > manager.max_sessions {
                sessions.finished.pop_front();
            }
        }
        state.backgrounded && !sessions.closed
    } else {
        session.state.lock().unwrap_or_else(|e| e.into_inner()).exit = Some(exit);
        false
    };
    session.changed.notify_waiters();
    session.input.lock().await.take();
    drop(output);

    let events = manager.upgrade().map(|manager| manager.events.clone());
    if backgrounded
        && exit.reason != ProcessEnd::Stopped
        && !session.control.stopped.is_cancelled()
        && let Some(events) = events
    {
        let reason = match exit.reason {
            ProcessEnd::Timeout => ShellStopReason::Timeout,
            _ => ShellStopReason::Exited,
        };
        events.emit(BackgroundToolEvent::ShellExited {
            session_id: session.id,
            command,
            exit_code: exit.code(),
            reason,
        });
    }
}

#[derive(Clone)]
enum ExecBackend {
    Interactive(ProcessSessionManager),
    OneShot {
        workspace: Arc<WorkspaceToolsConfig>,
        structured_cap: usize,
    },
}

#[derive(Clone)]
pub struct ExecCommandTool {
    backend: ExecBackend,
}

impl ExecCommandTool {
    /// Foreground commands use the workspace timeout by default, including after they yield.
    /// Explicit background commands run until completion unless given a timeout.
    pub fn new(manager: ProcessSessionManager) -> Self {
        Self {
            backend: ExecBackend::Interactive(manager),
        }
    }

    /// Construct a stateless one-shot command executor for hosts that do not expose `write_stdin`
    /// (defaults to `WorkspaceToolsConfig::default_exec_timeout_ms`, 120 seconds).
    pub fn one_shot(workspace: Arc<WorkspaceToolsConfig>) -> Self {
        Self::one_shot_with_structured_cap(workspace, crate::STRUCTURED_OUTPUT_MAX_BYTES)
    }

    pub fn one_shot_with_structured_cap(
        workspace: Arc<WorkspaceToolsConfig>,
        structured_cap: usize,
    ) -> Self {
        Self {
            backend: ExecBackend::OneShot {
                workspace,
                structured_cap,
            },
        }
    }
}

#[derive(Deserialize)]
struct ExecArgs {
    command: String,
    cwd: Option<String>,
    yield_time_ms: Option<u64>,
    timeout_ms: Option<u64>,
    #[serde(default)]
    background: bool,
    #[serde(default)]
    tty: bool,
}

struct PendingExec(Option<Arc<ProcessControl>>);

impl Drop for PendingExec {
    fn drop(&mut self) {
        if let Some(control) = &self.0 {
            control.stop();
        }
    }
}

impl AgentTool for ExecCommandTool {
    fn definition(&self) -> ToolDefinition {
        match &self.backend {
            ExecBackend::Interactive(_) => {
                #[cfg_attr(not(unix), allow(unused_mut))]
                let mut fields = vec![
                    (
                        "command",
                        with_description(str_schema(), "Shell command to run."),
                        true,
                    ),
                    ("cwd", str_schema(), false),
                    (
                        "yield_time_ms",
                        with_bounds(int_schema(), Some(0.0), None),
                        false,
                    ),
                    (
                        "timeout_ms",
                        with_bounds(int_schema(), Some(1.0), Some(MAX_TIMEOUT_MS as f64)),
                        false,
                    ),
                    ("background", bool_schema(), false),
                ];
                #[cfg(unix)]
                fields.push((
                    "tty",
                    with_description(bool_schema(), "Use an interactive terminal."),
                    false,
                ));
                ToolDefinition {
                    name: "exec_command".into(),
                    description:
                        "Run a shell command. Use write_stdin to continue a running command."
                            .into(),
                    parameters: obj_schema(fields),
                }
            }
            ExecBackend::OneShot { .. } => ToolDefinition {
                name: "exec_command".into(),
                description:
                    "Run a shell command in the workspace directory with an optional working directory and timeout."
                        .into(),
                parameters: obj_schema([
                    (
                        "command",
                        with_description(str_schema(), "Shell command to execute."),
                        true,
                    ),
                    (
                        "cwd",
                        with_description(
                            str_schema(),
                            "Working directory for the command (relative to workspace cwd or absolute).",
                        ),
                        false,
                    ),
                    (
                        "timeout_ms",
                        with_description(
                            with_bounds(int_schema(), Some(1.0), Some(MAX_TIMEOUT_MS as f64)),
                            "Optional timeout in milliseconds before terminating the process group.",
                        ),
                        false,
                    ),
                ]),
            },
        }
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    fn execute(
        &self,
        _tool_call_id: String,
        args: Value,
        cancel: CancelToken,
        on_update: ToolProgressCallback,
    ) -> BoxFuture<'_, Result<AgentToolResult, String>> {
        Box::pin(async move {
            match &self.backend {
                ExecBackend::Interactive(manager) => {
                    execute_interactive(manager, args, cancel, on_update).await
                }
                ExecBackend::OneShot {
                    workspace,
                    structured_cap,
                } => execute_one_shot(workspace, *structured_cap, args, cancel, on_update).await,
            }
        })
    }
}

async fn execute_interactive(
    manager: &ProcessSessionManager,
    args: Value,
    cancel: CancelToken,
    on_update: ToolProgressCallback,
) -> Result<AgentToolResult, String> {
    let args: ExecArgs = serde_json::from_value(args).map_err(|error| error.to_string())?;
    if let Some(ms) = args.timeout_ms
        && !(1..=MAX_TIMEOUT_MS).contains(&ms)
    {
        return Err(format!(
            "timeout_ms must be an integer between 1 and {MAX_TIMEOUT_MS}."
        ));
    }
    let (cwd, command) = prepare_shell(
        &manager.0.workspace,
        &args.command,
        args.cwd.as_deref(),
        &cancel,
    )
    .await?;
    let (updates, mut progress) = mpsc::channel(16);
    let session = manager.start(args.command.clone(), cwd, command, &args, updates)?;
    let mut pending = PendingExec(Some(session.control.clone()));
    let yield_ms = args.yield_time_ms.unwrap_or(DEFAULT_EXEC_YIELD_MS);
    if !args.background && yield_ms > 0 {
        let deadline = deadline_after_ms(yield_ms);
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    session.control.stop();
                    session.control.wait_done().await;
                    return Err("Operation aborted".into());
                }
                _ = wait_deadline(deadline) => break,
                Some(chunk) = progress.recv() => {
                    on_update(AgentToolResult::text(chunk));
                }
                _ = session.control.wait_done() => break,
            }
        }
    }
    let result = manager.collect_exec(&session, args.background);
    if result.is_err() {
        session.control.stop();
        session.control.wait_done().await;
    }
    pending.0 = None;
    result
}

async fn execute_one_shot(
    workspace: &WorkspaceToolsConfig,
    structured_cap: usize,
    args: Value,
    cancel: CancelToken,
    on_update: ToolProgressCallback,
) -> Result<AgentToolResult, String> {
    if cancel.is_cancelled() {
        return Err("Operation aborted".into());
    }
    let command_str = args
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing required string argument: command".to_string())?;
    let timeout_ms = match args.get("timeout_ms") {
        None | Some(Value::Null) => workspace.default_exec_timeout_ms,
        Some(v) => Some(
            v.as_u64()
                .filter(|&n| (1..=MAX_TIMEOUT_MS).contains(&n))
                .ok_or_else(|| {
                    format!("timeout_ms must be an integer between 1 and {MAX_TIMEOUT_MS}")
                })?,
        ),
    };
    let (cwd, process) = prepare_shell(
        workspace,
        command_str,
        args.get("cwd").and_then(Value::as_str),
        &cancel,
    )
    .await?;
    let shown_cwd = display_path(&workspace.cwd, &cwd);
    let (mut child, _, mut output) = spawn_process(process, false, false)
        .map_err(|e| format!("failed to spawn command: {e}"))?;
    let mut buffer = TailSpilloverBuffer::with_structured_cap(
        workspace.max_output_bytes,
        workspace.max_output_lines,
        structured_cap,
    );
    let deadline = timeout_ms.and_then(deadline_after_ms);
    let mut aborted = false;
    let mut timed_out = false;

    let mut decoder = ProgressDecoder::default();
    let mut chunks = [[0u8; OUTPUT_CHUNK_BYTES]; 2];
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                aborted = true;
                child.control.stop();
                break;
            }
            _ = wait_deadline(deadline) => {
                timed_out = true;
                child.control.stop();
                break;
            }
            next = output.next(&mut chunks) => {
                match next {
                    Some((stream, slice)) => {
                        buffer.push(slice);
                        let text = decoder.push(stream, slice);
                        if !text.is_empty() {
                            on_update(AgentToolResult::text(text));
                        }
                    }
                    None => break,
                }
            }
        }
    }
    decoder.finish(|text| on_update(AgentToolResult::text(text)));
    drop(output);

    let exit = if aborted {
        let _ = child.wait(None, None).await;
        None
    } else if timed_out {
        Some(child.wait(Some(Instant::now()), None).await)
    } else {
        let res = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                aborted = true;
                child.control.stop();
                let _ = child.wait(None, None).await;
                None
            }
            res = child.wait(deadline, None) => Some(res),
        };
        if res.is_some_and(|e| e.reason == ProcessEnd::Timeout) {
            timed_out = true;
        }
        res
    };

    if aborted {
        let notice = "Command aborted.";
        let rendered = render_with_notice(&buffer, workspace.max_output_bytes, notice);
        let mut res = AgentToolResult::error(rendered.text);
        let mut details = json!({
            "cwd": shown_cwd,
            "aborted": true,
            "truncated": rendered.truncated,
            "omitted_bytes": rendered.omitted_bytes,
        });
        if let Some(p) = rendered.full_output_path {
            details["full_output_path"] = json!(p);
        }
        res.details = details;
        return Ok(res);
    }

    let (structured_out, structured_trunc) = buffer.structured_prefix();
    let structured_out = structured_out.into_owned();

    if timed_out {
        let ms = timeout_ms.unwrap_or(0);
        let notice = format!("Command timed out after {ms}ms.");
        let rendered = render_with_notice(&buffer, workspace.max_output_bytes, &notice);
        let mut res = AgentToolResult::error(rendered.text);
        let mut details = json!({
            "cwd": shown_cwd,
            "timed_out": true,
            "timeout_ms": ms,
            "truncated": rendered.truncated,
            "omitted_bytes": rendered.omitted_bytes,
        });
        let mut structured = json!({
            "exit_code": 124,
            "output": structured_out,
            "output_truncated": structured_trunc,
        });
        if let Some(p) = rendered.full_output_path {
            details["full_output_path"] = json!(p);
            structured["full_output_path"] = json!(p);
        }
        res.details = details;
        res.structured_content = Some(structured);
        return Ok(res);
    }

    let status = exit.and_then(|e| e.status);
    let code = status.and_then(|s| s.code());
    if status.is_some_and(|s| s.success()) {
        let rendered = buffer.render(workspace.max_output_bytes);
        let mut res = AgentToolResult::text(rendered.text);
        let mut details = json!({
            "cwd": shown_cwd,
            "exit_code": code.unwrap_or(0),
            "truncated": rendered.truncated,
            "omitted_bytes": rendered.omitted_bytes,
        });
        let mut structured = json!({
            "exit_code": 0,
            "output": structured_out,
            "output_truncated": structured_trunc,
        });
        if let Some(p) = rendered.full_output_path {
            details["full_output_path"] = json!(p);
            structured["full_output_path"] = json!(p);
        }
        res.details = details;
        res.structured_content = Some(structured);
        Ok(res)
    } else {
        let notice = failure_notice(status);
        let rendered = render_with_notice(&buffer, workspace.max_output_bytes, &notice);
        let mut res = AgentToolResult::error(rendered.text);
        let mut details = json!({
            "cwd": shown_cwd,
            "exit_code": code,
            "truncated": rendered.truncated,
            "omitted_bytes": rendered.omitted_bytes,
        });
        let mut structured = json!({
            "exit_code": shell_exit_code(status, false),
            "output": structured_out,
            "output_truncated": structured_trunc,
        });
        if let Some(p) = rendered.full_output_path {
            details["full_output_path"] = json!(p);
            structured["full_output_path"] = json!(p);
        }
        res.details = details;
        res.structured_content = Some(structured);
        Ok(res)
    }
}

fn failure_notice(status: Option<ExitStatus>) -> String {
    match status.and_then(|s| s.code()) {
        Some(code) => format!("Command exited with code {code}."),
        None => format_signal_notice(status),
    }
}

fn render_with_notice(
    buffer: &TailSpilloverBuffer,
    max_bytes: usize,
    notice: &str,
) -> TruncatedOutput {
    let body_budget = max_bytes.saturating_sub(notice.len() + 1);
    let mut rendered = buffer.render(body_budget);
    // Invalid UTF-8 can expand an otherwise untruncated body. In that case
    // use the buffer's bounded tail rendering and preserve its spill metadata.
    if rendered.text.len() > body_budget {
        rendered = buffer.render(body_budget.min(rendered.original_bytes.saturating_sub(1)));
    }
    if !rendered.text.is_empty() && !rendered.text.ends_with('\n') {
        rendered.text.push('\n');
    }
    rendered
        .text
        .push_str(&notice[..notice.floor_char_boundary(max_bytes)]);
    rendered
}

#[derive(Clone)]
pub struct WriteStdinTool {
    manager: ProcessSessionManager,
}

impl WriteStdinTool {
    pub fn new(manager: ProcessSessionManager) -> Self {
        Self { manager }
    }
}

#[derive(Deserialize)]
struct StdinArgs {
    session_id: u32,
    #[serde(default)]
    chars: String,
    yield_time_ms: Option<u64>,
    #[serde(default)]
    kill: bool,
}

impl AgentTool for WriteStdinTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write_stdin".into(),
            description: "Send input to a running command, read new output, or stop it.".into(),
            parameters: obj_schema([
                (
                    "session_id",
                    with_bounds(int_schema(), Some(1.0), Some(f64::from(u32::MAX))),
                    true,
                ),
                ("chars", str_schema(), false),
                (
                    "yield_time_ms",
                    with_bounds(int_schema(), Some(0.0), None),
                    false,
                ),
                ("kill", bool_schema(), false),
            ]),
        }
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    fn execute(
        &self,
        _tool_call_id: String,
        args: Value,
        cancel: CancelToken,
        _on_update: ToolProgressCallback,
    ) -> BoxFuture<'_, Result<AgentToolResult, String>> {
        Box::pin(async move {
            let args: StdinArgs =
                serde_json::from_value(args).map_err(|error| error.to_string())?;
            if cancel.is_cancelled() {
                return Err("Operation aborted".into());
            }
            let session = self.manager.session(args.session_id)?;
            if args.kill {
                session.control.stop();
                session.control.wait_done().await;
            } else {
                let deadline =
                    deadline_after_ms(args.yield_time_ms.unwrap_or(DEFAULT_STDIN_YIELD_MS));
                if !args.chars.is_empty() {
                    let write = async {
                        if let Some(input) = session.input.lock().await.as_mut() {
                            input.write_all(args.chars.as_bytes()).await
                        } else {
                            Ok(())
                        }
                    };
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return Err("Operation aborted".into()),
                        _ = session.control.stopped.cancelled() => {},
                        _ = session.wait_until(|state| state.exit.is_some()) => {},
                        result = write => {
                            if let Err(err) = result
                                && !is_closed_input_error(&err)
                            {
                                return Err(format!("Could not send input: {err}"));
                            }
                        }
                        _ = wait_deadline(deadline) => {},
                    }
                }
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err("Operation aborted".into()),
                    _ = session.wait_for_output(!args.chars.is_empty()) => {},
                    _ = wait_deadline(deadline) => {},
                }
            }
            let (unread, exit) = {
                let mut state = session.state.lock().unwrap_or_else(|e| e.into_inner());
                (session.take_output(&mut state), state.exit)
            };
            if exit.is_some() {
                self.manager
                    .0
                    .sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .finished
                    .retain(|(id, _)| *id != args.session_id);
            }
            Ok(session.render(unread, exit))
        })
    }
}

fn is_closed_input_error(err: &io::Error) -> bool {
    if matches!(err.kind(), io::ErrorKind::BrokenPipe) {
        return true;
    }
    #[cfg(unix)]
    if err.raw_os_error() == Some(libc::EIO) {
        return true;
    }
    false
}

pub(crate) async fn prepare_shell(
    workspace: &WorkspaceToolsConfig,
    command: &str,
    cwd: Option<&str>,
    cancel: &CancelToken,
) -> Result<(PathBuf, Command), String> {
    if command.is_empty() || command.len() > MAX_COMMAND_BYTES || command.contains('\0') {
        return Err("Command must contain 1 to 65536 bytes without NUL.".into());
    }
    if command.trim().is_empty() {
        return Err("command must not be empty.".into());
    }
    let cwd = match cwd {
        Some(cwd) => resolve_path(&workspace.cwd, cwd)?,
        None => workspace.cwd.clone(),
    };
    let cwd =
        std::path::absolute(cwd).map_err(|err| format!("Invalid working directory: {err}"))?;
    let prepare = async {
        let meta = tokio::fs::metadata(&cwd)
            .await
            .map_err(|err| format!("Could not open working directory: {err}"))?;
        if !meta.is_dir() {
            return Err("The working directory must be a directory.".to_owned());
        }
        let mut process = build_shell_command(workspace.shell.as_deref(), command);
        process.current_dir(&cwd);
        if let Some(provider) = &workspace.env_provider {
            process.envs(provider().await);
        }
        Ok(process)
    };
    let process = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err("Operation aborted".into()),
        result = prepare => result?,
    };
    Ok((cwd, process))
}

pub(crate) fn deadline_after_ms(ms: u64) -> Option<Instant> {
    Instant::now().checked_add(Duration::from_millis(ms))
}

pub(crate) async fn wait_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) if deadline <= Instant::now() => {}
        Some(deadline) => sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

pub(crate) struct ProcessControl {
    group: Mutex<ProcessGroupGuard>,
    pub stopped: CancelToken,
    done: watch::Sender<bool>,
}

impl ProcessControl {
    pub fn stop(&self) {
        self.stopped.cancel();
        self.kill_group();
    }

    pub(crate) fn kill_group(&self) {
        self.group
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .kill_now();
    }

    pub async fn wait_done(&self) {
        let _ = self.done.subscribe().wait_for(|&done| done).await;
    }
}

pub(crate) fn spawn_managed(
    control: Arc<ProcessControl>,
    task: impl Future<Output = ()> + Send + 'static,
    cleanup: impl FnOnce() + Send + 'static,
) {
    let completion = ProcessCompletion {
        control,
        cleanup: Some(cleanup),
    };
    tokio::spawn(async move {
        let _completion = completion;
        task.await;
    });
}

struct ProcessCompletion<F: FnOnce()> {
    control: Arc<ProcessControl>,
    cleanup: Option<F>,
}

impl<F: FnOnce()> Drop for ProcessCompletion<F> {
    fn drop(&mut self) {
        self.control.kill_group();
        if let Some(cleanup) = self.cleanup.take() {
            cleanup();
        }
        self.control.done.send_replace(true);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessEnd {
    Exited,
    Timeout,
    Stopped,
    Interrupted,
}

#[derive(Clone, Copy)]
pub(crate) struct ProcessExit {
    pub reason: ProcessEnd,
    status: Option<ExitStatus>,
}

impl ProcessExit {
    pub fn code(self) -> Option<i32> {
        self.status.and_then(|status| status.code())
    }
}

pub(crate) struct ProcessChild {
    pub control: Arc<ProcessControl>,
    child: Child,
}

impl ProcessChild {
    pub async fn wait(
        &mut self,
        deadline: Option<Instant>,
        interrupt: Option<&CancelToken>,
    ) -> ProcessExit {
        let (reason, status) = tokio::select! {
            biased;
            _ = self.control.stopped.cancelled() => (ProcessEnd::Stopped, None),
            _ = wait_deadline(deadline) => (ProcessEnd::Timeout, None),
            _ = async {
                match interrupt {
                    Some(token) => token.cancelled().await,
                    None => std::future::pending().await,
                }
            } => (ProcessEnd::Interrupted, None),
            result = self.child.wait() => (ProcessEnd::Exited, result.ok()),
        };
        self.control.kill_group();
        let status = match status {
            Some(status) => Some(status),
            None => {
                let _ = self.child.start_kill();
                self.child.wait().await.ok()
            }
        };
        ProcessExit { reason, status }
    }
}

impl Drop for ProcessChild {
    fn drop(&mut self) {
        self.control.kill_group();
    }
}

pub(crate) enum ProcessInput {
    Pipe(ChildStdin),
    #[cfg(unix)]
    Terminal(Arc<AsyncFd<File>>),
}

impl ProcessInput {
    async fn write_all(
        &mut self,
        #[cfg_attr(not(unix), allow(unused_mut))] mut data: &[u8],
    ) -> io::Result<()> {
        match self {
            Self::Pipe(pipe) => pipe.write_all(data).await,
            #[cfg(unix)]
            Self::Terminal(master) => {
                while !data.is_empty() {
                    let written = match master
                        .async_io(Interest::WRITABLE, |mut fd| {
                            loop {
                                match fd.write(data) {
                                    Err(err) if err.kind() == io::ErrorKind::Interrupted => {
                                        continue;
                                    }
                                    other => break other,
                                }
                            }
                        })
                        .await
                    {
                        Ok(written) => written,
                        Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                        Err(err) => return Err(err),
                    };
                    if written == 0 {
                        return Err(io::ErrorKind::WriteZero.into());
                    }
                    data = &data[written..];
                }
                Ok(())
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputStream {
    Stdout,
    Stderr,
}

// Each pipe can split a different UTF-8 character at the end of a read.
#[derive(Default)]
struct ProgressDecoder {
    pending: [Vec<u8>; 2],
}

impl ProgressDecoder {
    fn push(&mut self, stream: OutputStream, bytes: &[u8]) -> String {
        let pending = &mut self.pending[stream as usize];
        let joined;
        let bytes = if pending.is_empty() {
            bytes
        } else {
            joined = [pending.as_slice(), bytes].concat();
            pending.clear();
            &joined
        };
        let mut text = String::with_capacity(bytes.len());
        let mut chunks = bytes.utf8_chunks().peekable();
        while let Some(chunk) = chunks.next() {
            text.push_str(chunk.valid());
            let invalid = chunk.invalid();
            if !invalid.is_empty() {
                if chunks.peek().is_none()
                    && std::str::from_utf8(invalid).is_err_and(|err| err.error_len().is_none())
                {
                    pending.extend_from_slice(invalid);
                    break;
                }
                text.push(char::REPLACEMENT_CHARACTER);
            }
        }
        text
    }

    fn finish(self, mut emit: impl FnMut(String)) {
        for pending in self.pending {
            if !pending.is_empty() {
                emit(char::REPLACEMENT_CHARACTER.to_string());
            }
        }
    }
}

pub(crate) enum ProcessOutput {
    Pipes(Option<ChildStdout>, Option<ChildStderr>),
    #[cfg(unix)]
    Terminal(Arc<AsyncFd<File>>),
}

impl ProcessOutput {
    pub async fn next<'a>(
        &mut self,
        buffers: &'a mut [[u8; OUTPUT_CHUNK_BYTES]; 2],
    ) -> Option<(OutputStream, &'a [u8])> {
        let [out, err] = buffers;
        match self {
            #[cfg(unix)]
            Self::Terminal(master) => loop {
                match master
                    .async_io(Interest::READABLE, |mut fd| {
                        loop {
                            match fd.read(out) {
                                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                                other => break other,
                            }
                        }
                    })
                    .await
                {
                    Ok(count) => {
                        return (count > 0).then_some((OutputStream::Stdout, &out[..count]));
                    }
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => return None,
                }
            },
            Self::Pipes(stdout, stderr) => loop {
                tokio::select! {
                    count = read_pipe(stdout, out), if stdout.is_some() => {
                        if count > 0 {
                            return Some((OutputStream::Stdout, &out[..count]));
                        }
                        *stdout = None;
                    }
                    count = read_pipe(stderr, err), if stderr.is_some() => {
                        if count > 0 {
                            return Some((OutputStream::Stderr, &err[..count]));
                        }
                        *stderr = None;
                    }
                    else => return None,
                }
            },
        }
    }
}

async fn read_pipe(pipe: &mut Option<impl AsyncRead + Unpin>, buffer: &mut [u8]) -> usize {
    match pipe {
        Some(pipe) => pipe.read(buffer).await.unwrap_or(0),
        None => 0,
    }
}

pub(crate) fn spawn_process(
    mut command: Command,
    writable: bool,
    tty: bool,
) -> io::Result<(ProcessChild, Option<ProcessInput>, ProcessOutput)> {
    command.kill_on_drop(true);
    #[cfg(unix)]
    let terminal = if tty {
        Some(attach_pty(&mut command)?)
    } else {
        command
            .process_group(0)
            .stdin(if writable {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        None
    };
    #[cfg(not(unix))]
    {
        if tty {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Interactive terminal (tty) is only supported on Unix platforms",
            ));
        }
        command
            .stdin(if writable {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
    }
    let mut child = command.spawn()?;
    let control = Arc::new(ProcessControl {
        group: Mutex::new(ProcessGroupGuard::new(child.id())),
        stopped: CancelToken::new(),
        done: watch::channel(false).0,
    });
    #[cfg(unix)]
    let (input, output) = match terminal {
        Some(master) => (
            Some(ProcessInput::Terminal(master.clone())),
            ProcessOutput::Terminal(master),
        ),
        None => (
            child.stdin.take().map(ProcessInput::Pipe),
            ProcessOutput::Pipes(child.stdout.take(), child.stderr.take()),
        ),
    };
    #[cfg(not(unix))]
    let (input, output) = (
        child.stdin.take().map(ProcessInput::Pipe),
        ProcessOutput::Pipes(child.stdout.take(), child.stderr.take()),
    );
    Ok((ProcessChild { control, child }, input, output))
}

#[cfg(unix)]
fn attach_pty(command: &mut Command) -> io::Result<Arc<AsyncFd<File>>> {
    let (mut master, mut slave) = (-1, -1);
    let mut size = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: openpty initializes both file descriptors on success and reads the
    // provided window size. Ownership of each returned fd transfers to File.
    let (master, slave) = unsafe {
        if libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        ) < 0
        {
            return Err(io::Error::last_os_error());
        }
        (File::from_raw_fd(master), File::from_raw_fd(slave))
    };
    // SAFETY: the descriptors remain owned and valid for these fcntl calls.
    unsafe {
        for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
            if libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let fd = master.as_raw_fd();
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    command
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    // SAFETY: this runs after stdio remapping in the child. setsid creates
    // the process group, and fd 0 is the open PTY slave to make controlling.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    // SAFETY: File uniquely owns the master descriptor, which is set to nonblocking
    // above and supports reactor readiness for reads and writes.
    let master = unsafe { AsyncFd::register(master) }.map_err(|error| error.into_parts().1)?;
    Ok(Arc::new(master))
}
