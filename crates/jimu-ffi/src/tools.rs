use crate::session::global_pool;
use crate::transport::SendHttpCallbacks;
use crate::types::{FfiToolSpec, JimuStatus};
use futures_channel::oneshot;
use futures_util::future::select;
use jimu_agent::{AgentTool, AgentToolResult, ToolExecutionMode, ToolProgressCallback};
use jimu_ai::{BoxFuture, CancelToken, ToolDefinition};
use serde_json::Value;
use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub type JimuToolExecuteFn = extern "C" fn(
    ctx: *mut c_void,
    call_handle_id: u64,
    tool_call_id: *const c_char,
    tool_name: *const c_char,
    args_json: *const c_char,
    completion: *mut JimuToolCompletion,
);

pub type JimuToolCancelFn = extern "C" fn(ctx: *mut c_void, call_handle_id: u64);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct JimuToolCallbacks {
    pub ctx: *mut c_void,
    pub execute: Option<JimuToolExecuteFn>,
    pub cancel: Option<JimuToolCancelFn>,
}

#[derive(Clone)]
pub(crate) struct SendToolCallbacks {
    raw: JimuToolCallbacks,
    _keep_alive: Option<Arc<SendHttpCallbacks>>,
}

// SAFETY: Host guarantees `ctx` and callback pointers remain valid and thread-safe for the
// lifetime of `SendHttpCallbacks` (which owns `bridgePtr` release).
unsafe impl Send for SendToolCallbacks {}
unsafe impl Sync for SendToolCallbacks {}

impl SendToolCallbacks {
    pub(crate) fn new(raw: JimuToolCallbacks, keep_alive: Option<Arc<SendHttpCallbacks>>) -> Self {
        Self {
            raw,
            _keep_alive: keep_alive,
        }
    }

    fn fire_execute(
        &self,
        call_handle_id: u64,
        tool_call_id: &CStr,
        tool_name: &CStr,
        args_json: &CStr,
        completion: *mut JimuToolCompletion,
    ) -> bool {
        if let Some(f) = self.raw.execute {
            f(
                self.raw.ctx,
                call_handle_id,
                tool_call_id.as_ptr(),
                tool_name.as_ptr(),
                args_json.as_ptr(),
                completion,
            );
            true
        } else {
            false
        }
    }

    fn fire_cancel(&self, call_handle_id: u64) {
        if let Some(f) = self.raw.cancel {
            f(self.raw.ctx, call_handle_id);
        }
    }
}

/// Opaque tool completion handle owned by the host until [`jimu_tool_complete`] is called.
pub struct JimuToolCompletion {
    done_tx: Mutex<Option<oneshot::Sender<Result<AgentToolResult, String>>>>,
    on_update: ToolProgressCallback,
    finished: Arc<AtomicBool>,
    finish_notify: Mutex<Option<oneshot::Sender<()>>>,
}

impl JimuToolCompletion {
    pub(crate) fn update(&self, partial: AgentToolResult) -> JimuStatus {
        if self.finished.load(Ordering::Acquire) {
            return JimuStatus::ErrClosed;
        }
        (self.on_update)(partial);
        JimuStatus::Ok
    }

    pub(crate) fn complete(self, outcome: Result<AgentToolResult, String>) {
        self.finished.store(true, Ordering::Release);
        if let Some(notify) = self
            .finish_notify
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = notify.send(());
        }
        if let Some(tx) = self
            .done_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = tx.send(outcome);
        }
    }
}

pub(crate) struct FfiAgentTool {
    definition: ToolDefinition,
    execution_mode: ToolExecutionMode,
    callbacks: SendToolCallbacks,
    next_call_id: Arc<AtomicU64>,
}

impl FfiAgentTool {
    pub(crate) fn from_spec(
        spec: &FfiToolSpec,
        callbacks: SendToolCallbacks,
        next_call_id: Arc<AtomicU64>,
    ) -> Arc<dyn AgentTool> {
        Arc::new(Self {
            definition: spec.definition(),
            execution_mode: spec.execution_mode,
            callbacks,
            next_call_id,
        })
    }
}

impl AgentTool for FfiAgentTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        self.execution_mode
    }

    fn execute(
        &self,
        tool_call_id: String,
        args: Value,
        cancel: CancelToken,
        on_update: ToolProgressCallback,
    ) -> BoxFuture<'_, Result<AgentToolResult, String>> {
        let name = self.definition.name.clone();
        let callbacks = self.callbacks.clone();
        let call_handle_id = self.next_call_id.fetch_add(1, Ordering::Relaxed);

        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err("cancelled".to_string());
            }

            let args_str = serde_json::to_string(&args)
                .map_err(|e| format!("failed to serialize tool args: {e}"))?;
            let call_id_c =
                CString::new(tool_call_id).map_err(|e| format!("invalid tool_call_id: {e}"))?;
            let name_c = CString::new(name).map_err(|e| format!("invalid tool_name: {e}"))?;
            let args_c =
                CString::new(args_str).map_err(|e| format!("invalid tool args JSON: {e}"))?;

            let (done_tx, done_rx) = oneshot::channel();
            let (finish_tx, finish_rx) = oneshot::channel();
            let finished = Arc::new(AtomicBool::new(false));

            let completion = Box::new(JimuToolCompletion {
                done_tx: Mutex::new(Some(done_tx)),
                on_update,
                finished: Arc::clone(&finished),
                finish_notify: Mutex::new(Some(finish_tx)),
            });
            let completion_ptr = Box::into_raw(completion);

            // Propagate cancellation immediately to the host so Swift cancels its async Task,
            // while `execute` still awaits `done_rx` so `interrupt()` waits for full tool teardown!
            {
                let cancel_watch = cancel.clone();
                let finished_watch = Arc::clone(&finished);
                let cancel_callbacks = callbacks.clone();
                global_pool().spawn_ok(async move {
                    let _ = select(cancel_watch.cancelled(), finish_rx).await;
                    if cancel_watch.is_cancelled() && !finished_watch.load(Ordering::Acquire) {
                        cancel_callbacks.fire_cancel(call_handle_id);
                    }
                });
            }

            if !callbacks.fire_execute(call_handle_id, &call_id_c, &name_c, &args_c, completion_ptr)
            {
                // SAFETY: Callback was null, so ownership never transferred to host.
                let dropped = unsafe { Box::from_raw(completion_ptr) };
                dropped.complete(Err("host tool execute callback not configured".to_string()));
            } else if cancel.is_cancelled() && !finished.load(Ordering::Acquire) {
                callbacks.fire_cancel(call_handle_id);
            }

            match done_rx.await {
                Ok(result) => result,
                Err(_) => Err("host tool completion dropped".to_string()),
            }
        })
    }
}
