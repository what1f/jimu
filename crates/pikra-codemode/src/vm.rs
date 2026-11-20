//! Dedicated-thread QuickJS VM driver with resource limits, interrupts, and dangling-promise detection.

use crate::prelude::build_prelude_source;
use crate::types::{CodemodeError, CodemodeErrorKind, CodemodeOutputItem};
use futures_channel::mpsc as async_mpsc;
use rquickjs::context::EvalOptions;
use rquickjs::prelude::Opt;
use rquickjs::{Context, Ctx, Error as JsError, Exception, Function, Object, Persistent, Runtime, Value};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

/// Number of QuickJS microtask jobs executed between deadline/abort polls.
const JOBS_PER_POLL: usize = 50;

/// Maximum poll slice when waiting for a host tool result so cooperative `AtomicBool` aborts wake promptly.
const IDLE_ABORT_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Thread stack size for the QuickJS VM thread (comfortably above QuickJS's 512 KiB JS stack guard).
const VM_THREAD_STACK_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeclaredTool {
    pub name: String,
    pub raw_name: String,
    pub description: String,
}

pub(crate) struct VmLaunchConfig {
    pub code: String,
    pub tools: Vec<DeclaredTool>,
    pub store: BTreeMap<String, String>,
    pub memory_limit_bytes: usize,
    pub max_stack_size: usize,
    pub timeout_ms: Option<u64>,
    pub abort: Option<Arc<AtomicBool>>,
    pub cancelled: Arc<AtomicBool>,
}

#[derive(Debug)]
pub(crate) enum VmEvent {
    Call {
        id: u64,
        name: String,
        args_json: Option<String>,
    },
    Output(CodemodeOutputItem),
    DoneOk {
        value_json: Option<String>,
        store_writes_json: String,
    },
    DoneErr(CodemodeError),
    Timeout,
    Aborted,
    OutOfMemory(String),
    UnsettledPromise,
    SandboxError(String),
}

#[derive(Debug)]
pub(crate) enum HostMsg {
    ToolResult {
        id: u64,
        ok: bool,
        payload: Option<String>,
    },
    Stop,
}

pub(crate) struct VmHandle {
    pub host_tx: mpsc::Sender<HostMsg>,
    pub event_rx: async_mpsc::UnboundedReceiver<VmEvent>,
}

struct VmShared {
    event_tx: async_mpsc::UnboundedSender<VmEvent>,
    done_sent: AtomicBool,
    pending_calls: AtomicUsize,
}

impl VmShared {
    fn new(event_tx: async_mpsc::UnboundedSender<VmEvent>) -> Self {
        Self {
            event_tx,
            done_sent: AtomicBool::new(false),
            pending_calls: AtomicUsize::new(0),
        }
    }

    fn is_done(&self) -> bool {
        self.done_sent.load(Ordering::Acquire)
    }

    fn send_event(&self, event: VmEvent) {
        let _ = self.event_tx.unbounded_send(event);
    }

    fn send_terminal(&self, event: VmEvent) {
        if !self.done_sent.swap(true, Ordering::AcqRel) {
            let _ = self.event_tx.unbounded_send(event);
        }
    }
}

pub(crate) fn spawn_vm(config: VmLaunchConfig) -> Result<VmHandle, String> {
    let (host_tx, host_rx) = mpsc::channel::<HostMsg>();
    let (event_tx, event_rx) = async_mpsc::unbounded::<VmEvent>();

    thread::Builder::new()
        .name("pikra-codemode-vm".to_string())
        .stack_size(VM_THREAD_STACK_BYTES)
        .spawn(move || {
            run_vm_thread(config, host_rx, event_tx);
        })
        .map_err(|err| format!("failed to spawn QuickJS VM thread: {err}"))?;

    Ok(VmHandle { host_tx, event_rx })
}

fn run_vm_thread(
    config: VmLaunchConfig,
    host_rx: mpsc::Receiver<HostMsg>,
    event_tx: async_mpsc::UnboundedSender<VmEvent>,
) {
    let shared = Arc::new(VmShared::new(event_tx));
    let deadline = config
        .timeout_ms
        .map(|ms| Instant::now() + Duration::from_millis(ms));

    let runtime = match Runtime::new() {
        Ok(rt) => rt,
        Err(err) => {
            shared.send_terminal(VmEvent::SandboxError(format!(
                "failed to create QuickJS runtime: {err}"
            )));
            return;
        }
    };

    runtime.set_memory_limit(config.memory_limit_bytes);
    runtime.set_max_stack_size(config.max_stack_size);

    let int_shared = shared.clone();
    let int_abort = config.abort.clone();
    let int_cancelled = config.cancelled.clone();
    runtime.set_interrupt_handler(Some(Box::new(move || {
        int_shared.is_done()
            || int_cancelled.load(Ordering::Acquire)
            || int_abort
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::Acquire))
            || deadline.is_some_and(|d| Instant::now() >= d)
    })));

    let context = match Context::full(&runtime) {
        Ok(ctx) => ctx,
        Err(err) => {
            shared.send_terminal(VmEvent::SandboxError(format!(
                "failed to create QuickJS context: {err}"
            )));
            return;
        }
    };

    // Run inside a nested function so all `Persistent` handles are guaranteed to drop
    // before `context` and `runtime`.
    if let Err(terminal) = run_vm_session(&runtime, &context, &config, &host_rx, &shared, deadline) {
        shared.send_terminal(terminal);
    }

    drop(context);
    drop(runtime);
}

fn run_vm_session(
    runtime: &Runtime,
    context: &Context,
    config: &VmLaunchConfig,
    host_rx: &mpsc::Receiver<HostMsg>,
    shared: &Arc<VmShared>,
    deadline: Option<Instant>,
) -> Result<(), VmEvent> {
    let tools_json = serde_json::to_string(&config.tools)
        .map_err(|err| VmEvent::SandboxError(format!("failed to serialize tool declarations: {err}")))?;
    let store_json = serde_json::to_string(&config.store)
        .map_err(|err| VmEvent::SandboxError(format!("failed to serialize store snapshot: {err}")))?;
    let prelude_source = build_prelude_source();

    // 1. Evaluate prelude, install globals, compile user script, and invoke `run(fn)`.
    let (settle_fn, stalled_fn) = context.with(|ctx| -> Result<(Persistent<Function<'static>>, Persistent<Function<'static>>), VmEvent> {
        let bridge_shared = shared.clone();
        let bridge = Function::new(
            ctx.clone(),
            move |kind: String, a: Opt<Value<'_>>, b: Opt<Value<'_>>, c: Opt<Value<'_>>| {
                handle_bridge(&bridge_shared, &kind, a.0, b.0, c.0);
            },
        )
        .map_err(|err| classify_rquickjs_error(&ctx, err, &config.abort, &config.cancelled, deadline))?;

        let mut prelude_opts = EvalOptions::default();
        prelude_opts.global = true;
        prelude_opts.strict = true;
        prelude_opts.filename = Some("codemode-prelude.js".to_string());

        let prelude_fn: Function<'_> = ctx
            .eval_with_options(prelude_source, prelude_opts)
            .map_err(|err| classify_rquickjs_error(&ctx, err, &config.abort, &config.cancelled, deadline))?;

        let api: Object<'_> = prelude_fn
            .call((bridge, tools_json, store_json))
            .map_err(|err| classify_rquickjs_error(&ctx, err, &config.abort, &config.cancelled, deadline))?;

        let settle: Function<'_> = api
            .get("settle")
            .map_err(|err| classify_rquickjs_error(&ctx, err, &config.abort, &config.cancelled, deadline))?;
        let run: Function<'_> = api
            .get("run")
            .map_err(|err| classify_rquickjs_error(&ctx, err, &config.abort, &config.cancelled, deadline))?;
        let stalled: Function<'_> = api
            .get("stalled")
            .map_err(|err| classify_rquickjs_error(&ctx, err, &config.abort, &config.cancelled, deadline))?;

        let wrapped_code = format!("(async (tools, console) => {{{}\n}})", config.code);
        let mut script_opts = EvalOptions::default();
        script_opts.global = true;
        script_opts.strict = true;
        script_opts.filename = Some("codemode.js".to_string());

        let user_fn: Function<'_> = ctx
            .eval_with_options(wrapped_code, script_opts)
            .map_err(|err| classify_rquickjs_error(&ctx, err, &config.abort, &config.cancelled, deadline))?;

        if let Err(err) = run.call::<_, ()>((user_fn,))
            && !shared.is_done()
        {
            return Err(classify_rquickjs_error(
                &ctx,
                err,
                &config.abort,
                &config.cancelled,
                deadline,
            ));
        }

        Ok((Persistent::save(&ctx, settle), Persistent::save(&ctx, stalled)))
    })?;

    if shared.is_done() {
        return Ok(());
    }

    // 2. Event loop: drain microtasks -> check completion/stall -> await next host tool result.
    loop {
        drain_jobs(runtime, shared, &config.abort, &config.cancelled, deadline)?;

        if shared.is_done() {
            return Ok(());
        }

        if shared.pending_calls.load(Ordering::Acquire) == 0 {
            let is_stalled = context.with(|ctx| -> Result<bool, VmEvent> {
                let stalled = stalled_fn
                    .clone()
                    .restore(&ctx)
                    .map_err(|err| VmEvent::SandboxError(format!("failed to restore stalled(): {err}")))?;
                match stalled.call::<_, bool>(()) {
                    Ok(v) => Ok(v),
                    Err(_) if shared.is_done() => Ok(false),
                    Err(err) => Err(classify_rquickjs_error(
                        &ctx,
                        err,
                        &config.abort,
                        &config.cancelled,
                        deadline,
                    )),
                }
            })?;
            if shared.is_done() || is_stalled {
                shared.send_terminal(VmEvent::UnsettledPromise);
                return Ok(());
            }
        }

        match wait_for_host_msg(host_rx, &config.abort, &config.cancelled, deadline) {
            WaitOutcome::Msg(HostMsg::ToolResult { id, ok, payload }) => {
                shared.pending_calls.fetch_sub(1, Ordering::AcqRel);
                context.with(|ctx| -> Result<(), VmEvent> {
                    let settle = settle_fn
                        .clone()
                        .restore(&ctx)
                        .map_err(|err| VmEvent::SandboxError(format!("failed to restore settle(): {err}")))?;
                    let res = match payload {
                        Some(p) => settle.call::<_, ()>((id as f64, ok, p)),
                        None => settle.call::<_, ()>((id as f64, ok)),
                    };
                    match res {
                        Ok(()) => Ok(()),
                        Err(_) if shared.is_done() => Ok(()),
                        Err(err) => Err(classify_rquickjs_error(
                            &ctx,
                            err,
                            &config.abort,
                            &config.cancelled,
                            deadline,
                        )),
                    }
                })?;
                if shared.is_done() {
                    return Ok(());
                }
            }
            WaitOutcome::Msg(HostMsg::Stop) | WaitOutcome::Disconnected => {
                return Ok(());
            }
            WaitOutcome::Timeout => {
                return Err(VmEvent::Timeout);
            }
            WaitOutcome::Aborted => {
                return Err(VmEvent::Aborted);
            }
        }
    }
}

fn drain_jobs(
    runtime: &Runtime,
    shared: &VmShared,
    abort: &Option<Arc<AtomicBool>>,
    cancelled: &Arc<AtomicBool>,
    deadline: Option<Instant>,
) -> Result<(), VmEvent> {
    let mut jobs_since_check = 0usize;
    while runtime.is_job_pending() {
        if shared.is_done() {
            return Ok(());
        }
        if let Some(reason) = check_interrupt(abort, cancelled, deadline) {
            return Err(reason);
        }
        match runtime.execute_pending_job() {
            Ok(true) => {
                if shared.is_done() {
                    return Ok(());
                }
                jobs_since_check += 1;
                if jobs_since_check >= JOBS_PER_POLL {
                    jobs_since_check = 0;
                    if let Some(reason) = check_interrupt(abort, cancelled, deadline) {
                        return Err(reason);
                    }
                }
            }
            Ok(false) => break,
            Err(job_err) => {
                if shared.is_done() {
                    return Ok(());
                }
                if let Some(reason) = check_interrupt(abort, cancelled, deadline) {
                    return Err(reason);
                }
                let event = job_err
                    .0
                    .with(|ctx| classify_caught_value(&ctx, ctx.catch(), abort, cancelled, deadline));
                return Err(event);
            }
        }
    }
    Ok(())
}

enum WaitOutcome {
    Msg(HostMsg),
    Timeout,
    Aborted,
    Disconnected,
}

fn wait_for_host_msg(
    host_rx: &mpsc::Receiver<HostMsg>,
    abort: &Option<Arc<AtomicBool>>,
    cancelled: &Arc<AtomicBool>,
    deadline: Option<Instant>,
) -> WaitOutcome {
    loop {
        if cancelled.load(Ordering::Acquire) {
            return WaitOutcome::Disconnected;
        }
        if abort
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            return WaitOutcome::Aborted;
        }
        let wait_duration = match deadline {
            Some(d) => {
                let now = Instant::now();
                if now >= d {
                    return WaitOutcome::Timeout;
                }
                (d - now).min(IDLE_ABORT_POLL_INTERVAL)
            }
            None => IDLE_ABORT_POLL_INTERVAL,
        };
        match host_rx.recv_timeout(wait_duration) {
            Ok(msg) => return WaitOutcome::Msg(msg),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return WaitOutcome::Disconnected,
        }
    }
}

fn check_interrupt(
    abort: &Option<Arc<AtomicBool>>,
    cancelled: &Arc<AtomicBool>,
    deadline: Option<Instant>,
) -> Option<VmEvent> {
    if abort
        .as_ref()
        .is_some_and(|flag| flag.load(Ordering::Acquire))
    {
        return Some(VmEvent::Aborted);
    }
    if deadline.is_some_and(|d| Instant::now() >= d) {
        return Some(VmEvent::Timeout);
    }
    if cancelled.load(Ordering::Acquire) {
        return Some(VmEvent::Aborted);
    }
    None
}

fn value_to_string(val: Option<Value<'_>>) -> Option<String> {
    let val = val?;
    if val.is_undefined() || val.is_null() {
        return None;
    }
    val.as_string().and_then(|s| s.to_string().ok())
}

fn handle_bridge(
    shared: &VmShared,
    kind: &str,
    a: Option<Value<'_>>,
    b: Option<Value<'_>>,
    c: Option<Value<'_>>,
) {
    match kind {
        "call" => {
            if shared.is_done() {
                return;
            }
            let id = a.and_then(|v| v.as_number()).unwrap_or(0.0) as u64;
            let name = value_to_string(b).unwrap_or_default();
            let args_json = value_to_string(c);
            shared.pending_calls.fetch_add(1, Ordering::AcqRel);
            shared.send_event(VmEvent::Call {
                id,
                name,
                args_json,
            });
        }
        "output" => {
            if shared.is_done() {
                return;
            }
            let subkind = value_to_string(a).unwrap_or_default();
            let data = value_to_string(b).unwrap_or_default();
            let item = match subkind.as_str() {
                "image" => CodemodeOutputItem::Image {
                    data,
                    mime_type: value_to_string(c).unwrap_or_default(),
                },
                "console" => CodemodeOutputItem::Text {
                    text: data,
                    console: true,
                },
                _ => CodemodeOutputItem::Text {
                    text: data,
                    console: false,
                },
            };
            shared.send_event(VmEvent::Output(item));
        }
        "done" => {
            let ok = a.and_then(|v| v.as_bool()).unwrap_or(false);
            if ok {
                let value_json = value_to_string(b);
                let store_writes_json = value_to_string(c).unwrap_or_else(|| "[]".to_string());
                shared.send_terminal(VmEvent::DoneOk {
                    value_json,
                    store_writes_json,
                });
            } else {
                let raw_err = value_to_string(b).unwrap_or_default();
                let err = parse_script_error_json(&raw_err);
                if err.kind == CodemodeErrorKind::OutOfMemory {
                    shared.send_terminal(VmEvent::OutOfMemory(err.message));
                } else {
                    shared.send_terminal(VmEvent::DoneErr(err));
                }
            }
        }
        "oom" => {
            let msg = value_to_string(a).unwrap_or_else(|| "out of memory".to_string());
            shared.send_terminal(VmEvent::OutOfMemory(msg));
        }
        "stalled" => {
            shared.send_terminal(VmEvent::UnsettledPromise);
        }
        _ => {}
    }
}

fn parse_script_error_json(raw: &str) -> CodemodeError {
    #[derive(serde::Deserialize)]
    struct RawErr {
        name: Option<String>,
        message: String,
        stack: Option<String>,
    }

    match serde_json::from_str::<RawErr>(raw) {
        Ok(parsed) => {
            if is_oom_error(parsed.name.as_deref(), &parsed.message) {
                return CodemodeError::out_of_memory(parsed.message);
            }
            CodemodeError::script(parsed.name, parsed.message, parsed.stack)
        }
        Err(_) => CodemodeError::sandbox(format!("malformed script error payload: {raw}")),
    }
}

fn is_oom_error(name: Option<&str>, message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    (name == Some("InternalError") && lower.contains("out of memory"))
        || lower == "out of memory"
}

fn classify_rquickjs_error(
    ctx: &Ctx<'_>,
    err: JsError,
    abort: &Option<Arc<AtomicBool>>,
    cancelled: &Arc<AtomicBool>,
    deadline: Option<Instant>,
) -> VmEvent {
    if let Some(reason) = check_interrupt(abort, cancelled, deadline) {
        return reason;
    }
    match err {
        JsError::Allocation => VmEvent::OutOfMemory("out of memory".to_string()),
        JsError::Exception => classify_caught_value(ctx, ctx.catch(), abort, cancelled, deadline),
        other => VmEvent::SandboxError(other.to_string()),
    }
}

fn classify_caught_value(
    _ctx: &Ctx<'_>,
    caught: Value<'_>,
    abort: &Option<Arc<AtomicBool>>,
    cancelled: &Arc<AtomicBool>,
    deadline: Option<Instant>,
) -> VmEvent {
    if let Some(reason) = check_interrupt(abort, cancelled, deadline) {
        return reason;
    }

    if let Some(obj) = caught.as_object()
        && let Some(exc) = Exception::from_object(obj.clone())
    {
        let name: Option<String> = obj.get("name").ok();
        let message = exc.message().unwrap_or_default();
        let raw_stack = exc.stack();

        if is_oom_error(name.as_deref(), &message) {
            return VmEvent::OutOfMemory(message);
        }
        if name.as_deref() == Some("InternalError") && message.eq_ignore_ascii_case("interrupted") {
            return check_interrupt(abort, cancelled, deadline)
                .unwrap_or(VmEvent::Aborted);
        }

        let head = match &name {
            Some(n) if !message.is_empty() => format!("{n}: {message}"),
            Some(n) => n.clone(),
            None => message.clone(),
        };
        let stack = match raw_stack {
            Some(s) => {
                let filtered: Vec<&str> = s
                    .lines()
                    .filter(|l| !l.trim().is_empty() && !l.contains("codemode-prelude.js"))
                    .collect();
                if filtered.is_empty() {
                    Some(head)
                } else {
                    Some(format!("{head}\n{}", filtered.join("\n")))
                }
            }
            None => Some(head),
        };
        return VmEvent::DoneErr(CodemodeError::script(name, message, stack));
    }

    let message = caught
        .as_string()
        .and_then(|s| s.to_string().ok())
        .unwrap_or_else(|| format!("{caught:?}"));
    if is_oom_error(None, &message) {
        return VmEvent::OutOfMemory(message);
    }
    VmEvent::DoneErr(CodemodeError::script(None, message, None))
}
