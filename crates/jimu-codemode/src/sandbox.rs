//! Host-side async sandbox coordinator (`CodemodeSandbox`).

use crate::declarations::render_declarations;
use crate::identifier::to_codemode_identifier;
use crate::types::{
    CodemodeCall, CodemodeCallStatus, CodemodeConfigError, CodemodeError, CodemodeExecuteOptions,
    CodemodeOutputItem, CodemodeResult, CodemodeSandboxOptions, CodemodeStoreWrites, CodemodeTool,
    CodemodeToolContext, MAX_IN_FLIGHT_CALLS, MAX_QUEUED_CALL_ARG_BYTES,
};
use crate::vm::{DeclaredTool, HostMsg, VmEvent, VmLaunchConfig, spawn_vm};
use futures_channel::oneshot;
use futures_core::Stream;
use futures_util::stream::FuturesUnordered;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc as std_mpsc};
use std::task::Poll;
use std::thread;
use std::time::{Duration, Instant};

/// Stack size for the lightweight host-side deadline/abort watchdog thread.
const WATCHDOG_THREAD_STACK_BYTES: usize = 256 * 1024;

/// Poll interval for checking cooperative `AtomicBool` abort flags in the host watchdog.
const WATCHDOG_ABORT_POLL_INTERVAL: Duration = Duration::from_millis(10);

type WatchdogStop = Arc<(Mutex<bool>, Condvar)>;

enum WatchdogEvent {
    Timeout,
    Aborted,
}

enum LoopStep {
    Vm(VmEvent),
    VmClosed,
    Timeout,
    Aborted,
    ToolDone {
        id: u64,
        call_index: usize,
        elapsed: Duration,
        result: Result<Value, String>,
    },
}

/// Standalone restricted async QuickJS sandbox for model-written JavaScript tool orchestration.
///
/// Each [`CodemodeSandbox::execute`] call creates a fresh, isolated QuickJS VM on a dedicated
/// thread with enforced memory, stack, timeout, and cooperative abort guards.
pub struct CodemodeSandbox {
    options: CodemodeSandboxOptions,
    tools: Vec<Arc<dyn CodemodeTool>>,
    tools_by_name: HashMap<String, Arc<dyn CodemodeTool>>,
}

impl CodemodeSandbox {
    /// Creates an empty sandbox with the provided options.
    pub fn new(options: CodemodeSandboxOptions) -> Self {
        Self {
            options,
            tools: Vec::new(),
            tools_by_name: HashMap::new(),
        }
    }

    /// Creates a sandbox pre-populated with host tools.
    pub fn with_tools(
        options: CodemodeSandboxOptions,
        tools: impl IntoIterator<Item = Arc<dyn CodemodeTool>>,
    ) -> Result<Self, CodemodeConfigError> {
        let mut sandbox = Self::new(options);
        for tool in tools {
            sandbox.register_tool(tool)?;
        }
        Ok(sandbox)
    }

    /// Registers an async host tool under `tools.<ident>` and `tools["<raw_name>"]`.
    pub fn register_tool(
        &mut self,
        tool: Arc<dyn CodemodeTool>,
    ) -> Result<(), CodemodeConfigError> {
        let name = tool.name().to_string();
        if self.tools_by_name.contains_key(&name) {
            return Err(CodemodeConfigError::DuplicateTool(name));
        }
        self.tools_by_name.insert(name, tool.clone());
        self.tools.push(tool);
        Ok(())
    }

    /// Unregisters a host tool by raw name. Returns `true` if removed.
    pub fn unregister_tool(&mut self, name: &str) -> bool {
        if self.tools_by_name.remove(name).is_some() {
            self.tools.retain(|t| t.name() != name);
            true
        } else {
            false
        }
    }

    /// Returns the currently registered host tools in registration order.
    pub fn tools(&self) -> &[Arc<dyn CodemodeTool>] {
        &self.tools
    }

    /// Renders compact TypeScript declarations (`declare const tools: { ... };`) for model prompts.
    pub fn render_declarations(&self) -> String {
        render_declarations(&self.tools)
    }

    /// Executes `code` as the body of an `async` function inside a fresh isolated QuickJS VM.
    ///
    /// Never fails with Rust `Err` for script exceptions, timeouts, aborts, OOM, or unsettled
    /// promises; all outcomes are returned as [`CodemodeResult`].
    pub async fn execute(&self, code: &str, options: CodemodeExecuteOptions) -> CodemodeResult {
        if options
            .abort
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            return CodemodeResult::err(
                CodemodeError::aborted("Execution aborted"),
                Vec::new(),
                Vec::new(),
            );
        }

        let effective_timeout_ms = options.timeout_ms.or(self.options.timeout_ms);
        let deadline = effective_timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return CodemodeResult::err(
                CodemodeError::timeout(effective_timeout_ms.unwrap_or(0)),
                Vec::new(),
                Vec::new(),
            );
        }

        let declared_tools: Vec<DeclaredTool> = self
            .tools
            .iter()
            .map(|tool| DeclaredTool {
                name: to_codemode_identifier(tool.name()),
                raw_name: tool.name().to_string(),
                description: tool.description().to_string(),
            })
            .collect();

        let mut serialized_store = BTreeMap::new();
        for (key, value) in &options.store {
            if let Ok(json) = serde_json::to_string(value) {
                serialized_store.insert(key.clone(), json);
            }
        }

        let cancelled = Arc::new(AtomicBool::new(false));
        let tool_abort = Arc::new(AtomicBool::new(false));

        let vm_handle = match spawn_vm(VmLaunchConfig {
            code: code.to_string(),
            tools: declared_tools,
            store: serialized_store,
            memory_limit_bytes: self.options.memory_limit_bytes,
            max_stack_size: self.options.max_stack_size,
            timeout_ms: effective_timeout_ms,
            abort: options.abort.clone(),
            cancelled: cancelled.clone(),
        }) {
            Ok(handle) => handle,
            Err(msg) => {
                return CodemodeResult::err(CodemodeError::sandbox(msg), Vec::new(), Vec::new());
            }
        };

        let (watchdog_stop, mut watchdog_rx) = if deadline.is_some() || options.abort.is_some() {
            match spawn_watchdog(deadline, options.abort.clone()) {
                Ok((stop, rx)) => (Some(stop), Some(rx)),
                Err(msg) => {
                    cancelled.store(true, Ordering::Release);
                    let _ = vm_handle.host_tx.send(HostMsg::Stop);
                    return CodemodeResult::err(
                        CodemodeError::sandbox(msg),
                        Vec::new(),
                        Vec::new(),
                    );
                }
            }
        } else {
            (None, None)
        };

        let mut event_rx = vm_handle.event_rx;
        let host_tx = vm_handle.host_tx;
        let mut output: Vec<CodemodeOutputItem> = Vec::new();
        let mut calls: Vec<CodemodeCall> = Vec::new();
        let mut pending_meta: HashMap<u64, (usize, Instant, usize)> = HashMap::new();
        let mut in_flight_arg_bytes: usize = 0;
        let mut in_flight: FuturesUnordered<Pin<Box<dyn Future<Output = LoopStep> + Send>>> =
            FuturesUnordered::new();

        // Declared AFTER `in_flight` so that if the caller drops the `execute()` future early,
        // `CancelOnDrop::drop` runs FIRST (setting `tool_abort` / `cancelled` and stopping the VM
        // and watchdog) BEFORE any in-flight tool future is dropped.
        let mut guard = CancelOnDrop {
            cancelled: cancelled.clone(),
            tool_abort: tool_abort.clone(),
            host_tx: host_tx.clone(),
            watchdog_stop,
        };

        let outcome: Result<(Option<Value>, CodemodeStoreWrites), CodemodeError> = loop {
            let step = futures_util::future::poll_fn(|cx| {
                if options
                    .abort
                    .as_ref()
                    .is_some_and(|flag| flag.load(Ordering::Acquire))
                {
                    return Poll::Ready(LoopStep::Aborted);
                }
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    return Poll::Ready(LoopStep::Timeout);
                }
                if let Some(rx) = watchdog_rx.as_mut() {
                    match Pin::new(rx).poll(cx) {
                        Poll::Ready(Ok(WatchdogEvent::Aborted)) => {
                            watchdog_rx = None;
                            return Poll::Ready(LoopStep::Aborted);
                        }
                        Poll::Ready(Ok(WatchdogEvent::Timeout)) => {
                            watchdog_rx = None;
                            return Poll::Ready(LoopStep::Timeout);
                        }
                        Poll::Ready(Err(_)) => {
                            watchdog_rx = None;
                        }
                        Poll::Pending => {}
                    }
                }
                if !in_flight.is_empty()
                    && let Poll::Ready(Some(done)) = Pin::new(&mut in_flight).poll_next(cx)
                {
                    return Poll::Ready(done);
                }
                match Pin::new(&mut event_rx).poll_next(cx) {
                    Poll::Ready(Some(ev)) => Poll::Ready(LoopStep::Vm(ev)),
                    Poll::Ready(None) => Poll::Ready(LoopStep::VmClosed),
                    Poll::Pending => Poll::Pending,
                }
            })
            .await;

            match step {
                LoopStep::Timeout => {
                    break Err(CodemodeError::timeout(effective_timeout_ms.unwrap_or(0)));
                }
                LoopStep::Aborted => {
                    break Err(CodemodeError::aborted("Execution aborted"));
                }
                LoopStep::Vm(VmEvent::Output(item)) => {
                    output.push(item);
                }
                LoopStep::Vm(VmEvent::Call {
                    id,
                    name,
                    args_json,
                }) => {
                    let arg_bytes = args_json.as_ref().map_or(0, String::len);
                    if pending_meta.len() >= MAX_IN_FLIGHT_CALLS
                        || in_flight_arg_bytes.saturating_add(arg_bytes) > MAX_QUEUED_CALL_ARG_BYTES
                    {
                        break Err(CodemodeError::script(
                            Some("RangeError".to_string()),
                            format!(
                                "in-flight tool call budget exceeded (max {MAX_IN_FLIGHT_CALLS} calls / {MAX_QUEUED_CALL_ARG_BYTES} bytes)"
                            ),
                            None,
                        ));
                    }

                    let call_index = calls.len();
                    let started_at = Instant::now();
                    calls.push(CodemodeCall {
                        name: name.clone(),
                        status: CodemodeCallStatus::Cancelled,
                        is_error: false,
                        duration_ms: 0,
                    });
                    pending_meta.insert(id, (call_index, started_at, arg_bytes));
                    in_flight_arg_bytes = in_flight_arg_bytes.saturating_add(arg_bytes);

                    let tool = self.tools_by_name.get(&name).cloned();
                    let tool_ctx = CodemodeToolContext {
                        abort: tool_abort.clone(),
                    };

                    let mut fut: Pin<Box<dyn Future<Output = LoopStep> + Send>> =
                        Box::pin(async move {
                            let result = match tool {
                                None => Err(format!("Unknown tool \"{name}\"")),
                                Some(tool) => match args_json {
                                    None => tool.execute(Value::Null, tool_ctx).await,
                                    Some(raw) => match serde_json::from_str::<Value>(&raw) {
                                        Ok(args) => tool.execute(args, tool_ctx).await,
                                        Err(err) => {
                                            Err(format!("Invalid tool arguments JSON: {err}"))
                                        }
                                    },
                                },
                            };
                            LoopStep::ToolDone {
                                id,
                                call_index,
                                elapsed: started_at.elapsed(),
                                result,
                            }
                        });

                    // Eagerly poll the tool future once immediately so fire-and-forget calls
                    // deterministically start before the VM can finish, and ready calls settle
                    // without waiting for another event loop turn.
                    let first_poll =
                        futures_util::future::poll_fn(|cx| Poll::Ready(fut.as_mut().poll(cx)))
                            .await;
                    match first_poll {
                        Poll::Ready(LoopStep::ToolDone {
                            id,
                            call_index,
                            elapsed,
                            result,
                        }) => {
                            finish_tool_call(
                                id,
                                call_index,
                                elapsed,
                                result,
                                &mut pending_meta,
                                &mut in_flight_arg_bytes,
                                &mut calls,
                                &host_tx,
                            );
                        }
                        Poll::Ready(_) => unreachable!("tool future only produces ToolDone"),
                        Poll::Pending => {
                            in_flight.push(fut);
                        }
                    }
                }
                LoopStep::ToolDone {
                    id,
                    call_index,
                    elapsed,
                    result,
                } => {
                    finish_tool_call(
                        id,
                        call_index,
                        elapsed,
                        result,
                        &mut pending_meta,
                        &mut in_flight_arg_bytes,
                        &mut calls,
                        &host_tx,
                    );
                }
                LoopStep::Vm(VmEvent::DoneOk {
                    value_json,
                    store_writes_json,
                }) => {
                    let parsed_value = match value_json {
                        None => Ok(None),
                        Some(raw) => serde_json::from_str::<Value>(&raw)
                            .map(Some)
                            .map_err(|err| {
                                CodemodeError::sandbox(format!(
                                    "Invalid return value JSON from VM: {err}"
                                ))
                            }),
                    };
                    let parsed_writes = parse_store_writes(&store_writes_json);
                    match (parsed_value, parsed_writes) {
                        (Ok(val), Ok(writes)) => break Ok((val, writes)),
                        (Err(e), _) | (_, Err(e)) => break Err(e),
                    }
                }
                LoopStep::Vm(VmEvent::DoneErr(err)) => break Err(err),
                LoopStep::Vm(VmEvent::Timeout) => {
                    break Err(CodemodeError::timeout(effective_timeout_ms.unwrap_or(0)));
                }
                LoopStep::Vm(VmEvent::Aborted) => {
                    break Err(CodemodeError::aborted("Execution aborted"));
                }
                LoopStep::Vm(VmEvent::OutOfMemory(msg)) => {
                    break Err(CodemodeError::out_of_memory(msg));
                }
                LoopStep::Vm(VmEvent::UnsettledPromise) => {
                    break Err(CodemodeError::unsettled_promise());
                }
                LoopStep::Vm(VmEvent::SandboxError(msg)) => {
                    break Err(CodemodeError::sandbox(msg));
                }
                LoopStep::VmClosed => {
                    if options
                        .abort
                        .as_ref()
                        .is_some_and(|flag| flag.load(Ordering::Acquire))
                    {
                        break Err(CodemodeError::aborted("Execution aborted"));
                    }
                    if deadline.is_some_and(|d| Instant::now() >= d) {
                        break Err(CodemodeError::timeout(effective_timeout_ms.unwrap_or(0)));
                    }
                    break Err(CodemodeError::sandbox(
                        "QuickJS VM terminated unexpectedly before completing",
                    ));
                }
            }
        };

        // Tear down any remaining in-flight tool futures and record their elapsed times.
        let now = Instant::now();
        for (_id, (call_index, started_at, _arg_bytes)) in pending_meta.drain() {
            calls[call_index].status = CodemodeCallStatus::Cancelled;
            calls[call_index].duration_ms =
                now.saturating_duration_since(started_at).as_millis() as u64;
        }
        guard.cancel_now();
        drop(in_flight);

        match outcome {
            Ok((value, store_writes)) => CodemodeResult::ok(value, output, calls, store_writes),
            Err(error) => CodemodeResult::err(error, output, calls),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn finish_tool_call(
    id: u64,
    call_index: usize,
    elapsed: Duration,
    result: Result<Value, String>,
    pending_meta: &mut HashMap<u64, (usize, Instant, usize)>,
    in_flight_arg_bytes: &mut usize,
    calls: &mut [CodemodeCall],
    host_tx: &std_mpsc::Sender<HostMsg>,
) {
    if let Some((_, _, arg_bytes)) = pending_meta.remove(&id) {
        *in_flight_arg_bytes = in_flight_arg_bytes.saturating_sub(arg_bytes);
    }
    let duration_ms = elapsed.as_millis() as u64;
    let msg = match result {
        Ok(value) => match serde_json::to_string(&value) {
            Ok(payload) => {
                calls[call_index].status = CodemodeCallStatus::Ok;
                calls[call_index].is_error = false;
                calls[call_index].duration_ms = duration_ms;
                HostMsg::ToolResult {
                    id,
                    ok: true,
                    payload: Some(payload),
                }
            }
            Err(err) => {
                calls[call_index].status = CodemodeCallStatus::Error;
                calls[call_index].is_error = true;
                calls[call_index].duration_ms = duration_ms;
                HostMsg::ToolResult {
                    id,
                    ok: false,
                    payload: Some(format!("Failed to serialize tool return value: {err}")),
                }
            }
        },
        Err(err_msg) => {
            calls[call_index].status = CodemodeCallStatus::Error;
            calls[call_index].is_error = true;
            calls[call_index].duration_ms = duration_ms;
            HostMsg::ToolResult {
                id,
                ok: false,
                payload: Some(err_msg),
            }
        }
    };
    let _ = host_tx.send(msg);
}

fn spawn_watchdog(
    deadline: Option<Instant>,
    abort: Option<Arc<AtomicBool>>,
) -> Result<(WatchdogStop, oneshot::Receiver<WatchdogEvent>), String> {
    let stop = Arc::new((Mutex::new(false), Condvar::new()));
    let stop_bg = stop.clone();
    let (tx, rx) = oneshot::channel();

    thread::Builder::new()
        .name("jimu-codemode-watchdog".to_string())
        .stack_size(WATCHDOG_THREAD_STACK_BYTES)
        .spawn(move || {
            let (lock, cv) = &*stop_bg;
            let mut stopped = lock.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if *stopped {
                    return;
                }
                if abort
                    .as_ref()
                    .is_some_and(|flag| flag.load(Ordering::Acquire))
                {
                    let _ = tx.send(WatchdogEvent::Aborted);
                    return;
                }
                let wait_dur = match deadline {
                    Some(d) => {
                        let now = Instant::now();
                        if now >= d {
                            let _ = tx.send(WatchdogEvent::Timeout);
                            return;
                        }
                        let remaining = d - now;
                        if abort.is_some() {
                            remaining.min(WATCHDOG_ABORT_POLL_INTERVAL)
                        } else {
                            remaining
                        }
                    }
                    None => WATCHDOG_ABORT_POLL_INTERVAL,
                };
                let (next_guard, _) = cv
                    .wait_timeout(stopped, wait_dur)
                    .unwrap_or_else(|e| e.into_inner());
                stopped = next_guard;
            }
        })
        .map_err(|err| format!("failed to spawn codemode watchdog thread: {err}"))?;

    Ok((stop, rx))
}

struct CancelOnDrop {
    cancelled: Arc<AtomicBool>,
    tool_abort: Arc<AtomicBool>,
    host_tx: std_mpsc::Sender<HostMsg>,
    watchdog_stop: Option<WatchdogStop>,
}

impl CancelOnDrop {
    fn cancel_now(&mut self) {
        self.tool_abort.store(true, Ordering::Release);
        self.cancelled.store(true, Ordering::Release);
        let _ = self.host_tx.send(HostMsg::Stop);
        if let Some(stop) = self.watchdog_stop.take() {
            let (lock, cv) = &*stop;
            let mut stopped = lock.lock().unwrap_or_else(|e| e.into_inner());
            *stopped = true;
            cv.notify_all();
        }
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.cancel_now();
    }
}

fn parse_store_writes(raw: &str) -> Result<CodemodeStoreWrites, CodemodeError> {
    let entries: Value = serde_json::from_str(raw)
        .map_err(|err| CodemodeError::sandbox(format!("Invalid store writes JSON: {err}")))?;
    let Some(arr) = entries.as_array() else {
        return Err(CodemodeError::sandbox(
            "Store writes payload is not an array",
        ));
    };
    let mut writes = CodemodeStoreWrites::default();
    for entry in arr {
        let Some(pair) = entry.as_array() else {
            return Err(CodemodeError::sandbox("Malformed store write entry"));
        };
        let Some(key) = pair.first().and_then(Value::as_str) else {
            return Err(CodemodeError::sandbox("Malformed store write key"));
        };
        match pair.len() {
            1 => {
                writes.delete.push(key.to_string());
            }
            2 => {
                let Some(val_json) = pair[1].as_str() else {
                    return Err(CodemodeError::sandbox("Malformed store write value"));
                };
                let val: Value = serde_json::from_str(val_json).map_err(|err| {
                    CodemodeError::sandbox(format!("Invalid JSON for stored key \"{key}\": {err}"))
                })?;
                writes.set.insert(key.to_string(), val);
            }
            _ => return Err(CodemodeError::sandbox("Malformed store write tuple length")),
        }
    }
    Ok(writes)
}
