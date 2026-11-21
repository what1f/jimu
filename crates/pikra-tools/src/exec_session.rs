use crate::WorkspaceToolsConfig;
use crate::bash::{ProcessGroupGuard, build_shell_command};
use crate::events::{BackgroundEventCallback, BackgroundEvents, BackgroundToolEvent, WakeGate};
use crate::util::{BoundedOutputBuffer, resolve_path};
use pikra_agent::{AgentTool, AgentToolResult, ToolProgressCallback};
use pikra_ai::{BoxFuture, CancelToken, ToolDefinition};
use pikra_schema::{
    bool_schema, int_schema, obj_schema, str_schema, with_bounds, with_description,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, Interest};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex as AsyncMutex, Notify, mpsc, watch};
use tokio::time::{Instant, sleep_until};

const DEFAULT_MAX_SESSIONS: usize = 16;
const DEFAULT_EXEC_YIELD_MS: u64 = 10_000;
const DEFAULT_STDIN_YIELD_MS: u64 = 5_000;
pub(crate) const OUTPUT_CHUNK_BYTES: usize = 8192;

/// Owns running commands and a bounded set of completed background sessions
/// whose final output can still be collected. Clones share the same sessions.
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
                let state = session.state.lock().unwrap();
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

    /// Stop and reap the managed commands. This manager will reject new commands.
    pub async fn shutdown(&self) {
        let sessions: Vec<_> = {
            let mut sessions = self.0.sessions.lock().unwrap();
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
        progress: mpsc::Sender<Vec<u8>>,
    ) -> Result<Arc<Session>, String> {
        let mut sessions = self.0.sessions.lock().unwrap();
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
        let deadline = args.timeout_ms.and_then(deadline_after_ms);
        let session = Arc::new(Session {
            id,
            cwd,
            control: child.control.clone(),
            input: AsyncMutex::new(input),
            changed: Notify::new(),
            state: Mutex::new(SessionState {
                unread: BoundedOutputBuffer::new(self.0.workspace.max_output_bytes),
                has_output: false,
                backgrounded: args.background,
                exit: None,
            }),
            max_output_bytes: self.0.workspace.max_output_bytes,
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
                    manager.sessions.lock().unwrap().active.remove(&id);
                }
                // A shutting-down runtime may drop the worker before it publishes
                // its result. Unblock any caller that already holds this session.
                cleanup_session
                    .state
                    .lock()
                    .unwrap()
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
            // Use the same lock order as process completion so a command can
            // only become a background session while reserving a slot.
            let sessions = self.0.sessions.lock().unwrap();
            let mut state = session.state.lock().unwrap();
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
        let sessions = self.0.sessions.lock().unwrap();
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
        for session in self.sessions.get_mut().unwrap().active.values() {
            session.control.stop();
        }
    }
}

struct Session {
    id: u32,
    cwd: PathBuf,
    control: Arc<ProcessControl>,
    input: AsyncMutex<Option<ProcessInput>>,
    changed: Notify,
    state: Mutex<SessionState>,
    max_output_bytes: usize,
}

struct SessionState {
    unread: BoundedOutputBuffer,
    has_output: bool,
    backgrounded: bool,
    exit: Option<ProcessExit>,
}

impl Session {
    async fn wait_until(&self, ready: impl Fn(&SessionState) -> bool) {
        loop {
            let changed = self.changed.notified();
            if ready(&self.state.lock().unwrap()) {
                return;
            }
            changed.await;
        }
    }

    fn take_output(&self, state: &mut SessionState) -> BoundedOutputBuffer {
        state.has_output = false;
        std::mem::replace(
            &mut state.unread,
            BoundedOutputBuffer::new(self.max_output_bytes),
        )
    }

    fn render(&self, unread: BoundedOutputBuffer, exit: Option<ProcessExit>) -> AgentToolResult {
        let output = unread.render(self.max_output_bytes);
        let mut result = AgentToolResult::text(output.text);
        let mut details = match exit {
            Some(exit) => json!({"status": "exited", "exit_code": exit.code()}),
            None => json!({"status": "running", "session_id": self.id}),
        };
        details["cwd"] = json!(self.cwd);
        if output.truncated {
            details["truncated"] = json!(true);
            details["omitted_bytes"] = json!(output.omitted_bytes);
        }
        if let Some(exit) = exit {
            if exit.reason == ProcessEnd::Timeout {
                details["timed_out"] = json!(true);
            }
            result.is_error = exit.reason == ProcessEnd::Timeout
                || (exit.reason == ProcessEnd::Exited
                    && !exit.status.is_some_and(|status| status.success()));
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
    progress: mpsc::Sender<Vec<u8>>,
) {
    let read = async {
        let mut buffers = [[0; OUTPUT_CHUNK_BYTES]; 2];
        while let Some((_, chunk)) = output.next(&mut buffers).await {
            {
                let mut state = session.state.lock().unwrap();
                state.unread.push(chunk);
                state.has_output = true;
            }
            session.changed.notify_waiters();
            if let Ok(permit) = progress.try_reserve() {
                permit.send(chunk.to_vec());
            }
        }
    };
    let (exit, ()) = tokio::join!(child.wait(deadline, None), read);
    let backgrounded = if let Some(manager) = manager.upgrade() {
        let mut sessions = manager.sessions.lock().unwrap();
        let mut state = session.state.lock().unwrap();
        state.exit = Some(exit);
        if state.backgrounded && !sessions.closed {
            sessions.finished.push_back((session.id, session.clone()));
            if sessions.finished.len() > manager.max_sessions {
                sessions.finished.pop_front();
            }
        }
        state.backgrounded && !sessions.closed
    } else {
        session.state.lock().unwrap().exit = Some(exit);
        false
    };
    session.changed.notify_waiters();
    session.input.lock().await.take();
    drop(output);

    let events = manager.upgrade().map(|manager| manager.events.clone());
    if backgrounded
        && exit.reason == ProcessEnd::Exited
        && !session.control.stopped.is_cancelled()
        && let Some(events) = events
    {
        events.emit(BackgroundToolEvent::ShellExited {
            session_id: session.id,
            command,
            exit_code: exit.code(),
        });
    }
}

#[derive(Clone)]
pub struct ExecCommandTool {
    manager: ProcessSessionManager,
}

impl ExecCommandTool {
    pub fn new(manager: ProcessSessionManager) -> Self {
        Self { manager }
    }
}

#[derive(Deserialize)]
struct ExecArgs {
    #[serde(alias = "cmd")]
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
        let mut parameters = obj_schema([
            (
                "command",
                with_description(str_schema(), "Shell command to run."),
                false,
            ),
            (
                "cmd",
                with_description(str_schema(), "Alias for command."),
                false,
            ),
            ("cwd", str_schema(), false),
            (
                "yield_time_ms",
                with_bounds(int_schema(), Some(0.0), None),
                false,
            ),
            (
                "timeout_ms",
                with_bounds(int_schema(), Some(1.0), None),
                false,
            ),
            ("background", bool_schema(), false),
            (
                "tty",
                with_description(bool_schema(), "Use an interactive terminal."),
                false,
            ),
        ]);
        parameters["oneOf"] = json!([{"required": ["command"]}, {"required": ["cmd"]}]);
        ToolDefinition {
            name: "exec_command".into(),
            description: "Run a shell command. Use write_stdin to continue a running command."
                .into(),
            parameters,
        }
    }

    fn execute(
        &self,
        _tool_call_id: String,
        args: Value,
        cancel: CancelToken,
        on_update: ToolProgressCallback,
    ) -> BoxFuture<'_, Result<AgentToolResult, String>> {
        Box::pin(async move {
            let args: ExecArgs = serde_json::from_value(args).map_err(|error| error.to_string())?;
            if args.timeout_ms == Some(0) {
                return Err("timeout_ms must be at least 1.".into());
            }
            let (cwd, command) = prepare_shell(
                &self.manager.0.workspace,
                &args.command,
                args.cwd.as_deref(),
                &cancel,
            )
            .await?;
            let (updates, mut progress) = mpsc::channel(16);
            let session = self
                .manager
                .start(args.command.clone(), cwd, command, &args, updates)?;
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
                            on_update(AgentToolResult::text(String::from_utf8_lossy(&chunk)));
                        }
                        _ = session.control.wait_done() => break,
                    }
                }
            }
            let result = self.manager.collect_exec(&session, args.background);
            if result.is_err() {
                session.control.stop();
                session.control.wait_done().await;
            }
            pending.0 = None;
            result
        })
    }
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
                                && !matches!(err.kind(), io::ErrorKind::BrokenPipe)
                                && err.raw_os_error() != Some(libc::EIO)
                            {
                                return Err(format!("Could not send input: {err}"));
                            }
                        }
                    }
                }
                let deadline =
                    deadline_after_ms(args.yield_time_ms.unwrap_or(DEFAULT_STDIN_YIELD_MS));
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err("Operation aborted".into()),
                    _ = session.wait_until(|state| state.has_output || state.exit.is_some()) => {},
                    _ = wait_deadline(deadline) => {},
                }
            }
            let (unread, exit) = {
                let mut state = session.state.lock().unwrap();
                (session.take_output(&mut state), state.exit)
            };
            if exit.is_some() {
                self.manager
                    .0
                    .sessions
                    .lock()
                    .unwrap()
                    .finished
                    .retain(|(id, _)| *id != args.session_id);
            }
            Ok(session.render(unread, exit))
        })
    }
}

pub(crate) async fn prepare_shell(
    workspace: &WorkspaceToolsConfig,
    command: &str,
    cwd: Option<&str>,
    cancel: &CancelToken,
) -> Result<(PathBuf, Command), String> {
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
        process.env("AI_AGENT", "pikra");
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
        self.group.lock().unwrap().kill_now();
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
        // A shell may exit while descendants still own its pipes. Terminate the
        // group before waiting for EOF, even when the leader exited naturally.
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
    Terminal(Arc<AsyncFd<File>>),
}

impl ProcessInput {
    async fn write_all(&mut self, mut data: &[u8]) -> io::Result<()> {
        match self {
            Self::Pipe(pipe) => pipe.write_all(data).await,
            Self::Terminal(master) => {
                while !data.is_empty() {
                    let written = master
                        .async_io(Interest::WRITABLE, |mut fd| fd.write(data))
                        .await?;
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

pub(crate) enum ProcessOutput {
    Pipes(Option<ChildStdout>, Option<ChildStderr>),
    Terminal(Arc<AsyncFd<File>>),
}

impl ProcessOutput {
    pub async fn next<'a>(
        &mut self,
        buffers: &'a mut [[u8; OUTPUT_CHUNK_BYTES]; 2],
    ) -> Option<(OutputStream, &'a [u8])> {
        let [out, err] = buffers;
        match self {
            Self::Terminal(master) => {
                let count = master
                    .async_io(Interest::READABLE, |mut fd| fd.read(out))
                    .await
                    .ok()?;
                (count > 0).then_some((OutputStream::Stdout, &out[..count]))
            }
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
    let mut child = command.spawn()?;
    let control = Arc::new(ProcessControl {
        group: Mutex::new(ProcessGroupGuard::new(child.id())),
        stopped: CancelToken::new(),
        done: watch::channel(false).0,
    });
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
    Ok((ProcessChild { control, child }, input, output))
}

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
