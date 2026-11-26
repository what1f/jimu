//! Native in-process Jimu adapter (`#[cfg(feature = "jimu")]`) connecting [`KitSession`]
//! directly to [`crate::harness::TuiHarness`] with zero ACP serialization overhead.

mod approval;
mod events;

pub use approval::{JimuApprovalMiddleware, JimuInteractionSink};

use crate::harness::{
    BackgroundTask, BoxEventStream, BoxFuture, CompactionCap, ContextMeterCap,
    ContextMeterSnapshot, HarnessCapabilities, HistoryMessage, McpCap, ModelOption, ModelPickerCap,
    NoticeLevel, PermissionModeCap, QueueCap, QueuedItem, RewindCap, RewindOutcome, RewindPoint,
    SessionOption, SessionPickerCap, ShellCap, ShellCommandResult, SkillSummary, SkillsCap,
    TasksCap, TuiEvent, TuiHarness, UsageStats, UserInput,
};
use events::{
    KitEventTranslator, context_usage_to_snapshot, parts_to_user_input, queued_summary_to_item,
    restore_history, user_input_to_parts,
};
use futures_channel::mpsc;
use futures_core::Stream;
use futures_util::StreamExt;
use jimu_agent::AgentMessage;
use jimu_ai::{AssistantBlock, ContentPart, Message, Model, ThinkingLevel};
use jimu_kit::KitSession;
use jimu_session::{SessionEntryPayload, SessionModelMetadata, SessionTreeNode};
use jimu_tools::{MonitorManager, ProcessSessionManager};
use std::collections::{HashSet, VecDeque};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::task::{Context, Poll};

/// Host-supplied session lifecycle controller for `/resume` and `/new`.
///
/// Kept behind a trait so `jimu-tui` never hardcodes product-specific session directories.
pub trait JimuSessionController: Send + Sync {
    fn current_session_id(&self) -> Option<String>;
    fn list_sessions(&self) -> BoxFuture<'_, Result<Vec<SessionOption>, String>>;
    fn open_session(&self, session_id: &str) -> BoxFuture<'_, Result<Arc<KitSession>, String>>;
    fn create_session(&self) -> BoxFuture<'_, Result<Arc<KitSession>, String>>;

    /// Optional per-session process manager for the currently active session.
    fn active_process_manager(&self) -> Option<ProcessSessionManager> {
        None
    }

    /// Optional per-session monitor manager for the currently active session.
    fn active_monitor_manager(&self) -> Option<MonitorManager> {
        None
    }
}

enum DriverCommand {
    Submit {
        target_session_id: String,
        input: UserInput,
    },
    ReplaceSession(Arc<KitSession>),
}

pub type ModelCatalogFn = Arc<dyn Fn() -> Vec<Model> + Send + Sync>;
pub type ModelResolverFn = Arc<dyn Fn(&str, Option<&str>) -> Option<Model> + Send + Sync>;
pub type ThinkingLevelsFn = Arc<dyn Fn(&str, &str) -> Option<Vec<ThinkingLevel>> + Send + Sync>;
pub type OnModelChangeFn = Arc<dyn Fn(&Model) + Send + Sync>;
pub type OnThinkingChangeFn = Arc<dyn Fn(ThinkingLevel) + Send + Sync>;

/// Cancellation-safe FIFO async mutex with direct permit handoff used to serialize session
/// switches end-to-end (abort -> open/create -> tool shutdown -> harness swap).
#[derive(Default)]
struct AsyncSwitchMutex {
    state: Mutex<(bool, VecDeque<futures_channel::oneshot::Sender<()>>)>,
}

struct AsyncSwitchGuard<'a>(&'a AsyncSwitchMutex);

impl AsyncSwitchMutex {
    fn release_or_handoff(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while let Some(tx) = state.1.pop_front() {
            if tx.send(()).is_ok() {
                // Permit handed directly to the woken waiter; keep `locked = true`.
                return;
            }
        }
        state.0 = false;
    }

    async fn lock(&self) -> AsyncSwitchGuard<'_> {
        struct Waiter<'a> {
            mutex: &'a AsyncSwitchMutex,
            rx: futures_channel::oneshot::Receiver<()>,
            disarmed: bool,
        }
        impl Drop for Waiter<'_> {
            fn drop(&mut self) {
                if !self.disarmed {
                    self.rx.close();
                    // If the lock holder handed us the permit right before we were cancelled,
                    // forward that permit to the next queued waiter so none ever starve.
                    if matches!(self.rx.try_recv(), Ok(Some(()))) {
                        self.mutex.release_or_handoff();
                    }
                }
            }
        }

        let rx = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if !state.0 {
                state.0 = true;
                return AsyncSwitchGuard(self);
            }
            let (tx, rx) = futures_channel::oneshot::channel();
            state.1.push_back(tx);
            rx
        };
        let mut waiter = Waiter {
            mutex: self,
            rx,
            disarmed: false,
        };
        let _ = (&mut waiter.rx).await;
        waiter.disarmed = true;
        AsyncSwitchGuard(self)
    }
}

impl Drop for AsyncSwitchGuard<'_> {
    fn drop(&mut self) {
        self.0.release_or_handoff();
    }
}

#[derive(Default)]
struct SubmitGate {
    cmd_tx: Option<mpsc::UnboundedSender<DriverCommand>>,
    switching: bool,
}

struct SwitchGateGuard<'a> {
    harness: &'a JimuHarness,
    active: bool,
}

impl<'a> SwitchGateGuard<'a> {
    fn enter(harness: &'a JimuHarness) -> Self {
        harness
            .submit_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .switching = true;
        Self {
            harness,
            active: true,
        }
    }

    /// Atomically swap the active session, enqueue `ReplaceSession(next)` onto the driver,
    /// and exit switching mode under one lock.
    fn finish(mut self, next: Arc<KitSession>) {
        self.active = false;
        self.harness.swap_session_and_flush(next);
    }
}

impl Drop for SwitchGateGuard<'_> {
    fn drop(&mut self) {
        if self.active {
            self.harness
                .submit_gate
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .switching = false;
        }
    }
}

/// Runtime-agnostic native adapter wrapping one active [`KitSession`].
///
/// Requires no `tokio::spawn`: the event stream returned by [`TuiHarness::subscribe`] drives
/// foreground prompts, background wakeups, and middleware interaction events directly inside
/// the caller's executor (compatible with `tokio`, `LocalPool`, or any standard async runtime).
pub struct JimuHarness {
    session: RwLock<Arc<KitSession>>,
    session_switch_lock: AsyncSwitchMutex,
    sink: JimuInteractionSink,
    submit_gate: Mutex<SubmitGate>,
    process_manager: Option<ProcessSessionManager>,
    monitor_manager: Option<MonitorManager>,
    session_controller: Option<Arc<dyn JimuSessionController>>,
    permission_mode: Option<Arc<dyn PermissionModeCap>>,
    mcp: Option<Arc<dyn McpCap>>,
    models: Vec<Model>,
    model_catalog: Option<ModelCatalogFn>,
    model_resolver: Option<ModelResolverFn>,
    thinking_levels_resolver: Option<ThinkingLevelsFn>,
    on_model_change: Option<OnModelChangeFn>,
    on_thinking_change: Option<OnThinkingChangeFn>,
    shell_cwd: Option<std::path::PathBuf>,
    serial: AtomicU64,
}

impl JimuHarness {
    /// Wrap a [`KitSession`] with a fresh [`JimuInteractionSink`].
    pub fn new(session: Arc<KitSession>) -> Self {
        Self::with_interaction_sink(session, JimuInteractionSink::new())
    }

    /// Wrap a [`KitSession`] sharing an existing [`JimuInteractionSink`] (typically created
    /// beforehand to construct [`JimuApprovalMiddleware`]).
    pub fn with_interaction_sink(session: Arc<KitSession>, sink: JimuInteractionSink) -> Self {
        Self {
            session: RwLock::new(session),
            session_switch_lock: AsyncSwitchMutex::default(),
            sink,
            submit_gate: Mutex::new(SubmitGate::default()),
            process_manager: None,
            monitor_manager: None,
            session_controller: None,
            permission_mode: None,
            mcp: None,
            models: Vec::new(),
            model_catalog: None,
            model_resolver: None,
            thinking_levels_resolver: None,
            on_model_change: None,
            on_thinking_change: None,
            shell_cwd: None,
            serial: AtomicU64::new(1),
        }
    }

    /// Set the working directory used by `!` local shell commands.
    pub fn with_shell_cwd(mut self, cwd: std::path::PathBuf) -> Self {
        self.shell_cwd = Some(cwd);
        self
    }

    /// Access the currently active [`KitSession`].
    pub fn session(&self) -> Arc<KitSession> {
        self.session
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Replace the active [`KitSession`] (e.g. after `/resume` or `/new`) atomically under
    /// `submit_gate` so `DriverCommand::ReplaceSession(next)` is strictly ordered with inputs.
    fn swap_session_and_flush(&self, next: Arc<KitSession>) {
        let mut gate = self.submit_gate.lock().unwrap_or_else(|e| e.into_inner());
        *self.session.write().unwrap_or_else(|e| e.into_inner()) = Arc::clone(&next);
        gate.switching = false;
        if let Some(tx) = gate.cmd_tx.clone() {
            let _ = tx.unbounded_send(DriverCommand::ReplaceSession(next));
        }
        drop(gate);
        self.emit_context_snapshot();
        self.emit_queue_snapshot();
    }

    fn resolve_branch_model(&self, meta: Option<&SessionModelMetadata>) -> Option<Model> {
        let meta = meta?;
        if let Some(model) = meta.to_full_model() {
            return Some(model);
        }
        if let Some(resolver) = &self.model_resolver
            && let Some(model) = resolver(
                &format!("{}/{}", meta.provider, meta.model_id),
                Some(&meta.provider),
            )
        {
            return Some(model);
        }
        self.models
            .iter()
            .find(|m| m.provider == meta.provider && m.id == meta.model_id)
            .cloned()
    }

    /// Access the interaction sink for custom tools or middleware.
    pub fn interaction_sink(&self) -> JimuInteractionSink {
        self.sink.clone()
    }

    /// Attach a [`ProcessSessionManager`] so running background shells appear in the status strip.
    pub fn with_process_manager(mut self, manager: ProcessSessionManager) -> Self {
        self.process_manager = Some(manager);
        self
    }

    /// Attach a [`MonitorManager`] so running monitors appear in the status strip.
    pub fn with_monitor_manager(mut self, manager: MonitorManager) -> Self {
        self.monitor_manager = Some(manager);
        self
    }

    /// Attach a host session controller to unlock `/resume` and `/new`.
    pub fn with_session_controller(mut self, controller: Arc<dyn JimuSessionController>) -> Self {
        self.session_controller = Some(controller);
        self
    }

    /// Attach a host permission-mode controller to unlock `/permissions` and `Shift+Tab`.
    pub fn with_permission_mode(mut self, controller: Arc<dyn PermissionModeCap>) -> Self {
        self.permission_mode = Some(controller);
        self
    }

    /// Attach a connected MCP server inventory provider to unlock `/mcp`.
    pub fn with_mcp(mut self, mcp: Arc<dyn McpCap>) -> Self {
        self.mcp = Some(mcp);
        self
    }

    /// Provide selectable models to unlock the `/model` picker capability.
    pub fn with_models(mut self, models: Vec<Model>) -> Self {
        self.models = models;
        self
    }

    /// Provide a dynamic model catalog callback (e.g. backed by `~/.jimu/providers.json`).
    pub fn with_model_catalog(mut self, catalog: ModelCatalogFn) -> Self {
        self.model_catalog = Some(catalog);
        self
    }

    /// Provide a model resolver callback supporting free-form `/model <provider/id>` switching.
    pub fn with_model_resolver(mut self, resolver: ModelResolverFn) -> Self {
        self.model_resolver = Some(resolver);
        self
    }

    /// Provide per-model supported thinking levels (e.g. from `providers.json`).
    pub fn with_thinking_levels_resolver(mut self, resolver: ThinkingLevelsFn) -> Self {
        self.thinking_levels_resolver = Some(resolver);
        self
    }

    /// Register a callback invoked whenever the user switches the active model.
    pub fn with_on_model_change(mut self, callback: OnModelChangeFn) -> Self {
        self.on_model_change = Some(callback);
        self
    }

    /// Register a callback invoked whenever the user changes the thinking level.
    pub fn with_on_thinking_change(mut self, callback: OnThinkingChangeFn) -> Self {
        self.on_thinking_change = Some(callback);
        self
    }

    fn next_id(&self) -> String {
        let n = self.serial.fetch_add(1, Ordering::Relaxed);
        format!("jimu-tui-{n}")
    }

    fn emit_queue_snapshot(&self) {
        let items: Vec<QueuedItem> = self
            .session()
            .pending_inputs()
            .into_iter()
            .map(queued_summary_to_item)
            .collect();
        self.sink.emit(TuiEvent::QueueChanged(items));
    }

    fn emit_context_snapshot(&self) {
        if let Some(usage) = self.session().context_usage() {
            self.sink
                .emit(TuiEvent::ContextUsageUpdate(context_usage_to_snapshot(
                    usage,
                )));
        }
    }

    fn active_process_manager(&self) -> Option<ProcessSessionManager> {
        self.session_controller
            .as_ref()
            .and_then(|c| c.active_process_manager())
            .or_else(|| self.process_manager.clone())
    }

    fn active_monitor_manager(&self) -> Option<MonitorManager> {
        self.session_controller
            .as_ref()
            .and_then(|c| c.active_monitor_manager())
            .or_else(|| self.monitor_manager.clone())
    }
}

impl TuiHarness for JimuHarness {
    fn submit(&self, input: UserInput) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let gate = self.submit_gate.lock().unwrap_or_else(|e| e.into_inner());
            if gate.switching {
                return Err("Session switch in progress; retry after the switch completes".into());
            }
            let Some(tx) = gate.cmd_tx.clone() else {
                return Err("TUI event stream is not subscribed".to_owned());
            };
            let target_session_id = self.session().session_id();
            tx.unbounded_send(DriverCommand::Submit {
                target_session_id,
                input,
            })
            .map_err(|_| "TUI event stream closed".to_owned())
        })
    }

    fn subscribe(&self) -> BoxEventStream {
        let (event_tx, event_rx) = mpsc::unbounded();
        let (turn_event_tx, turn_event_rx) = mpsc::unbounded();
        let (cmd_tx, cmd_rx) = mpsc::unbounded();
        self.sink.attach(event_tx.clone());
        {
            let mut gate = self.submit_gate.lock().unwrap_or_else(|e| e.into_inner());
            gate.cmd_tx = Some(cmd_tx);
        }
        self.emit_context_snapshot();
        self.emit_queue_snapshot();
        let session = self.session();
        let wake_rx = session.subscribe_background_wakeups();
        Box::pin(JimuEventStream {
            session,
            event_tx,
            event_rx,
            turn_event_tx,
            turn_event_rx,
            cmd_rx,
            wake_rx,
            active_turn: None,
            pending_submits: VecDeque::new(),
        })
    }

    fn abort(&self) -> BoxFuture<'_, ()> {
        let session = self.session();
        Box::pin(async move {
            session.abort().await;
            self.emit_queue_snapshot();
            self.emit_context_snapshot();
        })
    }

    fn steer(&self, input: UserInput) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let session = {
                let gate = self.submit_gate.lock().unwrap_or_else(|e| e.into_inner());
                if gate.switching {
                    return Err(
                        "Session switch in progress; retry after the switch completes".into(),
                    );
                }
                self.session()
            };
            let id = self.next_id();
            if input.images.is_empty() {
                session.steer(id, input.text);
            } else {
                session
                    .steer_parts(id, user_input_to_parts(input), false)
                    .await;
            }
            self.emit_queue_snapshot();
            Ok(())
        })
    }

    fn follow_up(&self, input: UserInput) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let session = {
                let gate = self.submit_gate.lock().unwrap_or_else(|e| e.into_inner());
                if gate.switching {
                    return Err(
                        "Session switch in progress; retry after the switch completes".into(),
                    );
                }
                self.session()
            };
            let id = self.next_id();
            if input.images.is_empty() {
                session.follow_up(id, input.text);
            } else {
                session.follow_up_parts(id, user_input_to_parts(input), false);
            }
            self.emit_queue_snapshot();
            Ok(())
        })
    }

    fn initial_history(&self) -> Vec<HistoryMessage> {
        restore_history(self.session().visible_messages()).0
    }

    fn initial_usage(&self) -> UsageStats {
        restore_history(self.session().visible_messages()).1
    }

    fn capabilities(&self) -> HarnessCapabilities<'_> {
        let has_tasks = self.process_manager.is_some()
            || self.monitor_manager.is_some()
            || self.session_controller.as_ref().is_some_and(|c| {
                c.active_process_manager().is_some() || c.active_monitor_manager().is_some()
            });
        HarnessCapabilities {
            context_meter: Some(self),
            queue: Some(self),
            compaction: Some(self),
            model_picker: (!self.models.is_empty()
                || self.model_catalog.is_some()
                || self.model_resolver.is_some())
            .then_some(self as &dyn ModelPickerCap),
            permission_mode: self.permission_mode.as_deref(),
            session_picker: self
                .session_controller
                .is_some()
                .then_some(self as &dyn SessionPickerCap),
            tasks: has_tasks.then_some(self as &dyn TasksCap),
            skills: Some(self),
            mcp: self.mcp.as_deref(),
            rewind: Some(self),
            shell: Some(self),
        }
    }
}

impl ShellCap for JimuHarness {
    fn run_shell(
        &self,
        command: String,
        cancel: Arc<std::sync::atomic::AtomicBool>,
    ) -> BoxFuture<'_, Result<ShellCommandResult, String>> {
        let session = self.session();
        let cwd = self
            .shell_cwd
            .clone()
            .or_else(|| std::env::current_dir().ok());
        Box::pin(async move {
            let (tx, rx) = futures_channel::oneshot::channel();
            let cmd_clone = command.clone();
            std::thread::Builder::new()
                .name("jimu-tui-bash".into())
                .spawn(move || {
                    let res = execute_local_shell(&cmd_clone, cwd.as_deref(), &cancel);
                    let _ = tx.send(res);
                })
                .map_err(|err| format!("Failed to spawn shell worker: {err}"))?;
            let result = rx
                .await
                .map_err(|_| "Shell worker terminated unexpectedly".to_owned())??;
            let status_line = if result.interrupted {
                "interrupted".to_owned()
            } else if let Some(code) = result.exit_code {
                format!("exit {code}")
            } else {
                "terminated".to_owned()
            };
            let context_body = if result.output.trim().is_empty() {
                format!(
                    "User ran local shell command (`{status_line}`):\n```sh\n{}\n```\n(no output)",
                    result.command
                )
            } else {
                format!(
                    "User ran local shell command (`{status_line}`):\n```sh\n{}\n```\nOutput:\n```\n{}\n```",
                    result.command, result.output
                )
            };
            session.append_context(context_body);
            Ok(result)
        })
    }
}

fn execute_local_shell(
    command: &str,
    cwd: Option<&std::path::Path>,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<ShellCommandResult, String> {
    use std::io::Read;
    use std::process::{Command, Stdio};

    const MAX_CAPTURE_BYTES: usize = 256 * 1024;

    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", command]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", command]);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            c.process_group(0);
        }
        c
    };
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|err| format!("Failed to run command: {err}"))?;
    let child_pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let done_readers = Arc::new(std::sync::atomic::AtomicU8::new(0));
    let spawn_reader = |pipe: Option<Box<dyn Read + Send>>| {
        let done = Arc::clone(&done_readers);
        std::thread::spawn(move || -> Vec<u8> {
            let mut out = Vec::new();
            let Some(mut reader) = pipe else {
                done.fetch_add(1, Ordering::Release);
                return out;
            };
            let mut buf = [0u8; 4096];
            let mut truncated = false;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if out.len() < MAX_CAPTURE_BYTES {
                            let take = n.min(MAX_CAPTURE_BYTES - out.len());
                            out.extend_from_slice(&buf[..take]);
                            if take < n {
                                truncated = true;
                            }
                        } else {
                            truncated = true;
                        }
                    }
                }
            }
            if truncated {
                out.extend_from_slice(b"\n...[output truncated]");
            }
            done.fetch_add(1, Ordering::Release);
            out
        })
    };

    let stdout_handle = spawn_reader(stdout.map(|s| Box::new(s) as Box<dyn Read + Send>));
    let stderr_handle = spawn_reader(stderr.map(|s| Box::new(s) as Box<dyn Read + Send>));

    let mut interrupted = false;
    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            interrupted = true;
            kill_process_group(child_pid, &mut child);
            break child.wait().ok();
        }
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
            Err(_) => break child.wait().ok(),
        }
    };

    // A backgrounded grandchild (e.g. `sleep 600 &`) may still hold stdout/stderr pipes after the
    // shell exits; keep honoring cancellation while waiting for pipes to reach EOF.
    while done_readers.load(Ordering::Acquire) < 2 {
        if cancel.load(Ordering::Relaxed) {
            interrupted = true;
            kill_process_group(child_pid, &mut child);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let stdout_bytes = stdout_handle.join().unwrap_or_default();
    let stderr_bytes = stderr_handle.join().unwrap_or_default();

    let mut combined = String::from_utf8_lossy(&stdout_bytes).into_owned();
    let stderr_text = String::from_utf8_lossy(&stderr_bytes);
    if !stderr_text.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&stderr_text);
    }

    Ok(ShellCommandResult {
        command: command.to_owned(),
        output: combined.trim_end_matches('\n').to_owned(),
        exit_code: status.and_then(|s| s.code()),
        interrupted,
    })
}

fn kill_process_group(pid: u32, child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        if pid > 0 && pid <= i32::MAX as u32 {
            // SAFETY: Negative PID targets the dedicated process group created via `process_group(0)`.
            unsafe {
                libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
            }
        }
    }
    let _ = child.kill();
}

impl ContextMeterCap for JimuHarness {
    fn context_usage(&self) -> Option<ContextMeterSnapshot> {
        self.session()
            .context_usage()
            .map(context_usage_to_snapshot)
    }
}

impl QueueCap for JimuHarness {
    fn pending_inputs(&self) -> Vec<QueuedItem> {
        self.session()
            .pending_inputs()
            .into_iter()
            .map(queued_summary_to_item)
            .collect()
    }

    fn remove_queued(&self, id: &str) -> bool {
        let removed = self.session().remove_queued_input(id);
        if removed {
            self.emit_queue_snapshot();
        }
        removed
    }

    fn take_queued(&self, id: &str) -> Option<UserInput> {
        let item = self.session().take_queued_input(id)?;
        self.emit_queue_snapshot();
        if item.content.is_empty() {
            Some(UserInput::text(item.text))
        } else {
            Some(parts_to_user_input(&item.content))
        }
    }
}

impl CompactionCap for JimuHarness {
    fn compact(&self) -> BoxFuture<'_, Result<bool, String>> {
        let session = self.session();
        Box::pin(async move {
            let res = session.compact().await.map_err(|e| e.to_string())?;
            self.emit_context_snapshot();
            Ok(res.is_some())
        })
    }
}

impl SkillsCap for JimuHarness {
    fn list_skills(&self) -> Vec<SkillSummary> {
        let session = self.session();
        session.reload_skills();
        session
            .skills()
            .skills()
            .iter()
            .map(|s| SkillSummary {
                name: s.name.clone(),
                description: s.description.clone(),
            })
            .collect()
    }

    fn reload_skills(&self) -> bool {
        self.session().reload_skills()
    }

    fn invoke_skill(&self, name: &str, args: &str) -> Option<Result<String, String>> {
        let session = self.session();
        session.reload_skills();
        let catalog = session.skills();
        let skill = catalog.get(name)?;
        Some(
            skill
                .render_invocation(args)
                .map_err(|err| format!("Failed to load skill `{name}`: {err}")),
        )
    }
}

impl SessionPickerCap for JimuHarness {
    fn current_session_id(&self) -> Option<String> {
        self.session_controller
            .as_ref()
            .and_then(|ctrl| ctrl.current_session_id())
    }

    fn list_sessions(&self) -> BoxFuture<'_, Result<Vec<SessionOption>, String>> {
        let ctrl = self.session_controller.clone();
        Box::pin(async move {
            let ctrl = ctrl.ok_or_else(|| "session switching is unavailable".to_owned())?;
            ctrl.list_sessions().await
        })
    }

    fn switch_session(&self, session_id: &str) -> BoxFuture<'_, Result<(), String>> {
        let ctrl = self.session_controller.clone();
        let id = session_id.to_owned();
        Box::pin(async move {
            let ctrl = ctrl.ok_or_else(|| "session switching is unavailable".to_owned())?;
            let _switch_guard = self.session_switch_lock.lock().await;
            let gate_guard = SwitchGateGuard::enter(self);
            self.session().abort().await;
            let next = ctrl.open_session(&id).await?;
            gate_guard.finish(next);
            Ok(())
        })
    }

    fn new_session(&self) -> BoxFuture<'_, Result<(), String>> {
        let ctrl = self.session_controller.clone();
        Box::pin(async move {
            let ctrl = ctrl.ok_or_else(|| "session creation is unavailable".to_owned())?;
            let _switch_guard = self.session_switch_lock.lock().await;
            let gate_guard = SwitchGateGuard::enter(self);
            self.session().abort().await;
            let next = ctrl.create_session().await?;
            gate_guard.finish(next);
            Ok(())
        })
    }
}

impl RewindCap for JimuHarness {
    fn list_rewind_points(&self) -> Vec<RewindPoint> {
        let session = self.session();
        let tree = session.tree();
        let leaf_id = session.leaf_id();
        build_rewind_points(&tree, leaf_id.as_deref())
    }

    fn rewind_to(&self, id: &str) -> BoxFuture<'_, Result<RewindOutcome, String>> {
        let session = self.session();
        let target_id = id.to_owned();
        Box::pin(async move {
            // Check before aborting: if the user pressed Enter on the active branch's current
            // non-user visible leaf, treat it as a pure no-op without cancelling in-flight work
            // (such as auto-compaction or an active turn) or dropping trailing metadata entries.
            {
                let tree = session.tree();
                let Some(found) = find_tree_entry(&tree, &target_id) else {
                    return Err(format!("checkpoint `{target_id}` not found"));
                };
                let is_user_msg = matches!(
                    &found.payload,
                    SessionEntryPayload::Message {
                        message: AgentMessage::Llm(Message::User(_)),
                    }
                );
                if !is_user_msg {
                    let leaf_id = session.leaf_id();
                    let points = build_rewind_points(&tree, leaf_id.as_deref());
                    if points
                        .iter()
                        .any(|p| p.id == target_id && p.is_current_leaf)
                    {
                        return Ok(RewindOutcome::Unchanged);
                    }
                }
            }

            session.abort().await;
            let tree = session.tree();
            let Some(found) = find_tree_entry(&tree, &target_id) else {
                return Err(format!("checkpoint `{target_id}` not found"));
            };
            let leaf_id = session.leaf_id();
            let points = build_rewind_points(&tree, leaf_id.as_deref());
            let already_current_leaf = points
                .iter()
                .any(|p| p.id == target_id && p.is_current_leaf);
            // Selecting a user message rewinds to right before it (its parent entry) and
            // refills the user's text into the composer for edit-and-resend (creating a new branch).
            // Selecting an assistant or compaction node switches the active leaf directly to it
            // (no-op if it is already the active branch's current visible leaf, preserving any
            // trailing metadata entries like ThinkingChange/ModelChange).
            let (fork_target, refill) = match &found.payload {
                SessionEntryPayload::Message {
                    message: AgentMessage::Llm(Message::User(user)),
                } => {
                    let text = extract_user_text(&user.content);
                    if let Some(parent) = found.parent_id.as_deref() {
                        (Some(parent.to_owned()), (!text.is_empty()).then_some(text))
                    } else {
                        (Some(found.id.clone()), None)
                    }
                }
                _ if already_current_leaf => (None, None),
                _ => (Some(found.id.clone()), None),
            };
            let Some(target) = fork_target else {
                return Ok(RewindOutcome::Unchanged);
            };
            session
                .fork_with_model_resolver(&target, |meta| self.resolve_branch_model(meta))
                .map_err(|e| e.to_string())?;
            self.emit_context_snapshot();
            self.emit_queue_snapshot();
            Ok(RewindOutcome::Rewound { draft: refill })
        })
    }
}

impl TasksCap for JimuHarness {
    fn list_tasks(&self) -> Vec<BackgroundTask> {
        let mut out = Vec::new();
        if let Some(pm) = self.active_process_manager() {
            for proc_info in pm.list_running() {
                out.push(BackgroundTask {
                    id: format!("proc:{}", proc_info.id),
                    kind: if proc_info.backgrounded {
                        "shell-bg".into()
                    } else {
                        "shell".into()
                    },
                    title: format!("shell #{}", proc_info.id),
                    command: proc_info.command,
                });
            }
        }
        if let Some(mm) = self.active_monitor_manager() {
            for mon in mm.list_running() {
                out.push(BackgroundTask {
                    id: format!("mon:{}", mon.id),
                    kind: "monitor".into(),
                    title: mon.name,
                    command: mon.command,
                });
            }
        }
        out
    }

    fn kill_task(&self, id: &str) -> bool {
        if let Some(raw) = id.strip_prefix("proc:")
            && let Ok(num) = raw.parse::<u32>()
            && let Some(pm) = self.active_process_manager()
        {
            return pm.kill(num);
        }
        if let Some(raw) = id.strip_prefix("mon:")
            && let Some(mm) = self.active_monitor_manager()
        {
            return mm.kill(raw);
        }
        false
    }
}

impl ModelPickerCap for JimuHarness {
    fn current_model(&self) -> ModelOption {
        model_to_option(&self.session().model())
    }

    fn available_models(&self) -> Vec<ModelOption> {
        let models = self
            .model_catalog
            .as_ref()
            .map_or_else(|| self.models.clone(), |f| f());
        models.iter().map(model_to_option).collect()
    }

    fn set_model(&self, model_id: &str) -> BoxFuture<'_, Result<(), String>> {
        let model_id = model_id.to_owned();
        let session = self.session();
        let current_provider = session.model().provider;
        let models = self
            .model_catalog
            .as_ref()
            .map_or_else(|| self.models.clone(), |f| f());
        let target = self
            .model_resolver
            .as_ref()
            .and_then(|resolve| resolve(&model_id, Some(&current_provider)))
            .or_else(|| {
                models
                    .iter()
                    .find(|m| {
                        format!("{}/{}", m.provider, m.id) == model_id
                            || m.id == model_id
                            || m.name == model_id
                    })
                    .cloned()
            });
        let on_model_change = self.on_model_change.clone();
        Box::pin(async move {
            let model = target.ok_or_else(|| format!("unknown model `{model_id}`"))?;
            session
                .set_model(model.clone())
                .map_err(|e| e.to_string())?;
            if let Some(cb) = on_model_change {
                cb(&model);
            }
            self.emit_context_snapshot();
            Ok(())
        })
    }

    fn thinking_levels(&self) -> Vec<String> {
        let model = self.session().model();
        self.thinking_levels_for(&format!("{}/{}", model.provider, model.id))
    }

    fn thinking_levels_for(&self, model_id: &str) -> Vec<String> {
        let default_levels = || {
            vec![
                "off".into(),
                "low".into(),
                "medium".into(),
                "high".into(),
                "xhigh".into(),
                "max".into(),
            ]
        };
        let Some(resolver) = &self.thinking_levels_resolver else {
            return default_levels();
        };
        let (provider, id) = model_id.split_once('/').unwrap_or(("", model_id));
        match resolver(provider, id) {
            Some(levels) if !levels.is_empty() => {
                levels.into_iter().map(thinking_level_label).collect()
            }
            _ => default_levels(),
        }
    }

    fn current_thinking_level(&self) -> Option<String> {
        Some(thinking_level_label(self.session().thinking_level()))
    }

    fn set_thinking_level(&self, level: &str) -> BoxFuture<'_, Result<(), String>> {
        let parsed = ThinkingLevel::from_str(level);
        let session = self.session();
        let on_thinking_change = self.on_thinking_change.clone();
        Box::pin(async move {
            let level = parsed?;
            session
                .set_thinking_level(level)
                .map_err(|e| e.to_string())?;
            if let Some(cb) = on_thinking_change {
                cb(level);
            }
            Ok(())
        })
    }

    fn fast_mode(&self) -> bool {
        self.session().fast()
    }
}

fn thinking_level_label(level: ThinkingLevel) -> String {
    match level {
        ThinkingLevel::Off => "off",
        ThinkingLevel::None => "none",
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::Xhigh => "xhigh",
        ThinkingLevel::Max => "max",
    }
    .into()
}

fn model_to_option(model: &Model) -> ModelOption {
    ModelOption {
        id: model.id.clone(),
        name: model.name.clone(),
        provider: model.provider.clone(),
        supports_thinking: model.supports_reasoning,
    }
}

// ---------------------------------------------------------------------------
// Tree projection for /rewind (/tree) and double-Esc
// ---------------------------------------------------------------------------

struct VisibleNode {
    id: String,
    parent_id: Option<String>,
    role: &'static str,
    label: String,
    timestamp: Option<String>,
    prompt_text: Option<String>,
    contains_active_leaf: bool,
    is_current_leaf: bool,
    children: Vec<VisibleNode>,
}

fn find_tree_entry<'a>(
    nodes: &'a [SessionTreeNode],
    id: &str,
) -> Option<&'a jimu_session::SessionEntry> {
    for node in nodes {
        if node.entry.id == id {
            return Some(&node.entry);
        }
        if let Some(found) = find_tree_entry(&node.children, id) {
            return Some(found);
        }
    }
    None
}

fn collect_active_path(
    nodes: &[SessionTreeNode],
    leaf_id: &str,
    out: &mut HashSet<String>,
) -> bool {
    for node in nodes {
        if node.entry.id == leaf_id || collect_active_path(&node.children, leaf_id, out) {
            out.insert(node.entry.id.clone());
            return true;
        }
    }
    false
}

fn extract_user_text(content: &[ContentPart]) -> String {
    let mut parts = Vec::new();
    for item in content {
        match item {
            ContentPart::Text(t) if !t.text.trim().is_empty() => parts.push(t.text.as_str()),
            ContentPart::Image(_) => parts.push("[image]"),
            _ => {}
        }
    }
    parts.join(" ")
}

fn preview_line(raw: &str, max_chars: usize) -> String {
    let collapsed: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    for (i, ch) in collapsed.chars().enumerate() {
        if i >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

fn format_timestamp_ms(ms: u64) -> Option<String> {
    if ms == 0 {
        return None;
    }
    let dt = chrono::DateTime::from_timestamp_millis(ms as i64)?;
    Some(
        dt.with_timezone(&chrono::Local)
            .format("%H:%M:%S")
            .to_string(),
    )
}

fn visible_entry_info(
    entry: &jimu_session::SessionEntry,
) -> Option<(&'static str, String, Option<String>)> {
    match &entry.payload {
        SessionEntryPayload::Message {
            message: AgentMessage::Llm(Message::User(user)),
        } => {
            if user.is_meta {
                return None;
            }
            let full = extract_user_text(&user.content);
            let label = preview_line(&full, 72);
            if label.is_empty() {
                None
            } else {
                Some(("user", label, Some(full)))
            }
        }
        SessionEntryPayload::Message {
            message: AgentMessage::Llm(Message::Assistant(assistant)),
        } => {
            let mut text = String::new();
            let mut tools = Vec::new();
            for block in &assistant.content {
                match block {
                    AssistantBlock::Text(t) if !t.text.trim().is_empty() => {
                        if !text.is_empty() {
                            text.push(' ');
                        }
                        text.push_str(t.text.trim());
                    }
                    AssistantBlock::ToolCall(tc) => tools.push(tc.name.as_str()),
                    _ => {}
                }
            }
            let label = if !text.is_empty() {
                preview_line(&text, 72)
            } else if !tools.is_empty() {
                preview_line(&format!("tool: {}", tools.join(", ")), 72)
            } else {
                return None;
            };
            Some(("assistant", label, None))
        }
        SessionEntryPayload::Compaction { summary, .. } => {
            Some(("compaction", preview_line(summary, 72), None))
        }
        _ => None,
    }
}

fn project_visible(
    node: &SessionTreeNode,
    active_ids: &HashSet<String>,
    leaf_id: Option<&str>,
) -> Vec<VisibleNode> {
    let mut children = Vec::new();
    for child in &node.children {
        children.extend(project_visible(child, active_ids, leaf_id));
    }
    if let Some((role, label, prompt_text)) = visible_entry_info(&node.entry) {
        let on_active = active_ids.contains(&node.entry.id);
        let child_has_active = children.iter().any(|c| c.contains_active_leaf);
        let is_current_leaf =
            leaf_id == Some(node.entry.id.as_str()) || (on_active && !child_has_active);
        vec![VisibleNode {
            id: node.entry.id.clone(),
            parent_id: node.entry.parent_id.clone(),
            role,
            label,
            timestamp: format_timestamp_ms(node.entry.timestamp),
            prompt_text,
            contains_active_leaf: on_active || child_has_active,
            is_current_leaf,
            children,
        }]
    } else {
        children
    }
}

fn build_rewind_points(tree: &[SessionTreeNode], leaf_id: Option<&str>) -> Vec<RewindPoint> {
    let mut active_ids = HashSet::new();
    if let Some(leaf) = leaf_id {
        collect_active_path(tree, leaf, &mut active_ids);
    }
    let mut roots = Vec::new();
    for root in tree {
        roots.extend(project_visible(root, &active_ids, leaf_id));
    }
    if roots.is_empty() {
        return Vec::new();
    }

    // Pi-style compact tree layout:
    // - Single-child chains stay flat at the current column (never drifting right per turn).
    // - Indentation and `├─ ` / `└─ ` connectors appear only at actual branch points (or multi-root).
    // - Ancestor branch columns render `│  ` while sibling branches remain below, else `   `.
    // - Active branch sorts first among siblings so the current path stays immediately visible.
    fn emit_tree(
        node: &VisibleNode,
        gutters: &mut Vec<bool>,
        show_connector: bool,
        is_last: bool,
        out: &mut Vec<RewindPoint>,
    ) {
        let mut tree_prefix =
            String::with_capacity((gutters.len() + usize::from(show_connector)) * 3);
        for &has_line in gutters.iter() {
            tree_prefix.push_str(if has_line { "│  " } else { "   " });
        }
        if show_connector {
            tree_prefix.push_str(if is_last { "└─ " } else { "├─ " });
        }
        let depth = gutters.len() + usize::from(show_connector);
        out.push(RewindPoint {
            id: node.id.clone(),
            parent_id: node.parent_id.clone(),
            role: node.role.into(),
            label: node.label.clone(),
            timestamp: node.timestamp.clone(),
            depth,
            tree_prefix,
            is_active_branch: node.contains_active_leaf,
            is_leaf: node.children.is_empty(),
            is_current_leaf: node.is_current_leaf,
            prompt_text: node.prompt_text.clone(),
        });

        if show_connector {
            gutters.push(!is_last);
        }
        let child_count = node.children.len();
        if child_count == 1 {
            emit_tree(&node.children[0], gutters, false, true, out);
        } else if child_count > 1 {
            let mut order: Vec<usize> = (0..child_count).collect();
            order.sort_by_key(|&i| !node.children[i].contains_active_leaf);
            for (pos, idx) in order.into_iter().enumerate() {
                emit_tree(
                    &node.children[idx],
                    gutters,
                    true,
                    pos + 1 == child_count,
                    out,
                );
            }
        }
        if show_connector {
            gutters.pop();
        }
    }

    let mut out = Vec::new();
    let mut gutters = Vec::new();
    if roots.len() == 1 {
        emit_tree(&roots[0], &mut gutters, false, true, &mut out);
    } else {
        let mut order: Vec<usize> = (0..roots.len()).collect();
        order.sort_by_key(|&i| !roots[i].contains_active_leaf);
        let total = order.len();
        for (pos, idx) in order.into_iter().enumerate() {
            emit_tree(&roots[idx], &mut gutters, true, pos + 1 == total, &mut out);
        }
    }
    out
}

/// Self-driving event stream that pumps `KitSession` turns inside `poll_next` without requiring
/// `tokio::spawn`.
struct JimuEventStream {
    session: Arc<KitSession>,
    event_tx: mpsc::UnboundedSender<TuiEvent>,
    event_rx: mpsc::UnboundedReceiver<TuiEvent>,
    turn_event_tx: mpsc::UnboundedSender<TuiEvent>,
    turn_event_rx: mpsc::UnboundedReceiver<TuiEvent>,
    cmd_rx: mpsc::UnboundedReceiver<DriverCommand>,
    wake_rx: mpsc::UnboundedReceiver<()>,
    active_turn: Option<BoxFuture<'static, ()>>,
    pending_submits: VecDeque<UserInput>,
}

impl JimuEventStream {
    fn start_foreground_turn(&mut self, input: UserInput) {
        let session = self.session.clone();
        let tx = self.turn_event_tx.clone();
        self.active_turn = Some(Box::pin(async move {
            let parts = user_input_to_parts(input);
            match session.prompt_parts_stream(parts) {
                Ok(mut stream) => {
                    let mut translator = KitEventTranslator::default();
                    let mut buf = Vec::new();
                    while let Some(ev) = stream.next().await {
                        translator.translate(ev, &mut buf);
                        for out in buf.drain(..) {
                            let _ = tx.unbounded_send(out);
                        }
                        if let Some(usage) = session.context_usage() {
                            let _ = tx.unbounded_send(TuiEvent::ContextUsageUpdate(
                                context_usage_to_snapshot(usage),
                            ));
                        }
                        let queue: Vec<QueuedItem> = session
                            .pending_inputs()
                            .into_iter()
                            .map(queued_summary_to_item)
                            .collect();
                        let _ = tx.unbounded_send(TuiEvent::QueueChanged(queue));
                    }
                }
                Err(err) => {
                    let _ = tx.unbounded_send(TuiEvent::Error(err.to_string()));
                }
            }
        }));
    }

    fn start_background_turn(&mut self) {
        let session = self.session.clone();
        let tx = self.turn_event_tx.clone();
        self.active_turn = Some(Box::pin(async move {
            match session.run_background_turn() {
                Ok(Some(mut stream)) => {
                    let mut translator = KitEventTranslator::default();
                    let mut buf = Vec::new();
                    while let Some(ev) = stream.next().await {
                        translator.translate(ev, &mut buf);
                        for out in buf.drain(..) {
                            let _ = tx.unbounded_send(out);
                        }
                        if let Some(usage) = session.context_usage() {
                            let _ = tx.unbounded_send(TuiEvent::ContextUsageUpdate(
                                context_usage_to_snapshot(usage),
                            ));
                        }
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    let _ = tx.unbounded_send(TuiEvent::Error(err.to_string()));
                }
            }
        }));
    }
}

impl Stream for JimuEventStream {
    type Item = TuiEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            // 1. Drain any pending driver commands (foreground submits or session swaps).
            while let Poll::Ready(Some(cmd)) = Pin::new(&mut this.cmd_rx).poll_next(cx) {
                match cmd {
                    DriverCommand::Submit {
                        target_session_id,
                        input,
                    } => {
                        if target_session_id == this.session.session_id() {
                            this.pending_submits.push_back(input);
                        } else {
                            let _ = this.event_tx.unbounded_send(TuiEvent::SessionNotice {
                                level: NoticeLevel::Warning,
                                message: "Message was not sent because the active session changed"
                                    .into(),
                            });
                        }
                    }
                    DriverCommand::ReplaceSession(next) => {
                        this.active_turn = None;
                        // Replace the turn-event channel so buffered late events from the
                        // outgoing session's turn can never leak into the new session.
                        let (turn_event_tx, turn_event_rx) = mpsc::unbounded();
                        this.turn_event_tx = turn_event_tx;
                        this.turn_event_rx = turn_event_rx;
                        if !this.pending_submits.is_empty() {
                            this.pending_submits.clear();
                            let _ = this.event_tx.unbounded_send(TuiEvent::SessionNotice {
                                level: NoticeLevel::Warning,
                                message:
                                    "Pending message was not sent because the session switched"
                                        .into(),
                            });
                        }
                        this.wake_rx = next.subscribe_background_wakeups();
                        this.session = next;
                    }
                }
            }

            // 2. Poll the active turn future if one is running.
            if let Some(fut) = this.active_turn.as_mut()
                && fut.as_mut().poll(cx).is_ready()
            {
                this.active_turn = None;
            }

            // 3. Start next foreground submit or background wakeup when idle.
            if this.active_turn.is_none() {
                if let Some(input) = this.pending_submits.pop_front() {
                    this.start_foreground_turn(input);
                    continue;
                }
                if let Poll::Ready(Some(())) = Pin::new(&mut this.wake_rx).poll_next(cx) {
                    this.start_background_turn();
                    continue;
                }
            }

            // 4. Deliver any queued turn events, then global/host events.
            match Pin::new(&mut this.turn_event_rx).poll_next(cx) {
                Poll::Ready(Some(ev)) => return Poll::Ready(Some(ev)),
                Poll::Ready(None) | Poll::Pending => {}
            }
            match Pin::new(&mut this.event_rx).poll_next(cx) {
                Poll::Ready(Some(ev)) => return Poll::Ready(Some(ev)),
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}
