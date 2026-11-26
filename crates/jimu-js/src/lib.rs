//! Dynamic QuickJS extension runtime for custom tools and control-plane lifecycle hooks.
//!
//! Hosts can load cross-platform `.js` extensions (ESM `export default async function(jimu)` or
//! script-style global `jimu.registerTool(...)` / `jimu.on(...)`) without modifying Rust source.
//!
//! - Pure interpreter (`rquickjs` 0.14, no JIT), dedicated worker thread (`Send + Sync`), zero `tokio` dependency.
//! - Configurable memory limit (default 32 MiB) and per-hook/tool timeout (default 10s) with `CancelToken`.
//! - Never invoked on per-token `MessageUpdate` streaming deltas; short-circuits in O(1) when no JS
//!   listener is registered for a given control-plane hook.
//! - Atomic `reload()` re-reads file-backed sources and swaps the active VM cleanly between turns.

mod prelude;
mod types;
mod ui;
mod worker;

pub use types::{
    DEFAULT_MAX_STACK_SIZE_BYTES, DEFAULT_MEMORY_LIMIT_BYTES, DEFAULT_TIMEOUT_MS, ExtensionSource,
    HookKind, JsExtensionConfig, JsExtensionError,
};
pub use ui::{
    DEFAULT_UI_RENDER_TIMEOUT_MS, MAX_UI_BOX_GAP, MAX_UI_BOX_PADDING, MAX_UI_DIVIDER_CH_LEN,
    MAX_UI_NODE_COUNT, MAX_UI_NODE_DEPTH, MAX_UI_TEXT_LEN, MAX_UI_TOKEN_LEN, UiBorder, UiComponent,
    UiDirection, UiNode, UiTextStyle, validate_ui_node,
};

use futures_channel::oneshot;
use jimu_agent::{
    AfterToolCallCtx, AfterToolCallResult, AgentContextSnapshot, AgentMessage, AgentMiddleware,
    AgentTool, AgentToolResult, BeforeToolCallCtx, BeforeToolCallResult, FinishTurnCtx,
    RequestOverride, ToolExecutionMode, ToolProgressCallback, TurnDecision,
};
use jimu_ai::{BoxFuture, CancelToken, ToolDefinition};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, mpsc};
use types::{
    JsAfterToolCallPayload, JsBeforeToolCallPayload, JsFinishTurnPayload, JsPrepareRequestSnapshot,
    RegistrySnapshot, parse_after_tool_call_result, parse_before_tool_call_result,
    parse_finish_turn_result, parse_request_override, parse_tool_execute_result, resolve_sources,
};
use ui::parse_and_validate_ui_node;
use worker::{VmLimits, WorkerCmd, WorkerHandle};

const MAX_RECORDED_ERRORS: usize = 64;

struct RuntimeInner {
    config: JsExtensionConfig,
    worker: WorkerHandle,
    registry: RwLock<RegistrySnapshot>,
    hook_mask: AtomicU8,
    has_ui_render: AtomicBool,
    ui_generation: Arc<AtomicU64>,
    errors: Mutex<Vec<JsExtensionError>>,
    js_hook_invocations: AtomicU64,
    ui_render_invocations: AtomicU64,
}

impl RuntimeInner {
    fn has_hook(&self, hook: HookKind) -> bool {
        (self.hook_mask.load(Ordering::Acquire) & hook.bit()) != 0
    }

    fn record_error(&self, err: JsExtensionError) {
        let mut guard = self.errors.lock().unwrap_or_else(|e| e.into_inner());
        if guard.len() >= MAX_RECORDED_ERRORS {
            guard.remove(0);
        }
        guard.push(err);
    }

    async fn invoke_hook_raw(
        &self,
        hook: HookKind,
        payload_json: String,
        cancel: CancelToken,
    ) -> Result<Option<String>, JsExtensionError> {
        if cancel.is_cancelled() {
            return Err(JsExtensionError::Cancelled);
        }
        self.js_hook_invocations.fetch_add(1, Ordering::Relaxed);
        let (reply_tx, reply_rx) = oneshot::channel();
        self.worker
            .tx
            .send(WorkerCmd::InvokeHook {
                hook,
                payload_json,
                cancel,
                reply: reply_tx,
            })
            .map_err(|_| {
                JsExtensionError::RuntimeUnavailable("QuickJS worker channel closed".to_string())
            })?;
        reply_rx.await.map_err(|_| {
            JsExtensionError::RuntimeUnavailable("QuickJS worker dropped reply".to_string())
        })?
    }

    async fn invoke_tool_raw(
        &self,
        name: String,
        tool_call_id: String,
        args_json: String,
        cancel: CancelToken,
    ) -> Result<String, JsExtensionError> {
        if cancel.is_cancelled() {
            return Err(JsExtensionError::Cancelled);
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        self.worker
            .tx
            .send(WorkerCmd::InvokeTool {
                name,
                tool_call_id,
                args_json,
                cancel,
                reply: reply_tx,
            })
            .map_err(|_| {
                JsExtensionError::RuntimeUnavailable("QuickJS worker channel closed".to_string())
            })?;
        reply_rx.await.map_err(|_| {
            JsExtensionError::RuntimeUnavailable("QuickJS worker dropped reply".to_string())
        })?
    }
}

impl Drop for RuntimeInner {
    fn drop(&mut self) {
        self.worker.shutdown();
    }
}

/// Thread-safe (`Send + Sync`) QuickJS extension runtime implementing [`AgentMiddleware`].
///
/// Cloning `JsExtensionRuntime` is cheap (shares the underlying worker thread and active VM).
#[derive(Clone)]
pub struct JsExtensionRuntime {
    inner: Arc<RuntimeInner>,
}

impl JsExtensionRuntime {
    /// Load all configured extension sources into a fresh QuickJS VM on a dedicated worker thread.
    pub fn new(config: JsExtensionConfig) -> Result<Self, JsExtensionError> {
        let sources = resolve_sources(&config.sources)?;
        let limits = VmLimits {
            memory_limit_bytes: config.memory_limit_bytes,
            max_stack_size_bytes: config.max_stack_size_bytes,
            timeout_ms: config.timeout_ms,
        };
        let ui_generation = Arc::new(AtomicU64::new(1));
        let (worker, snapshot) = WorkerHandle::spawn(limits, sources, Arc::clone(&ui_generation))?;
        let hook_mask = AtomicU8::new(snapshot.hook_mask);
        let has_ui_render = AtomicBool::new(snapshot.has_ui_render);
        Ok(Self {
            inner: Arc::new(RuntimeInner {
                config,
                worker,
                registry: RwLock::new(snapshot),
                hook_mask,
                has_ui_render,
                ui_generation,
                errors: Mutex::new(Vec::new()),
                js_hook_invocations: AtomicU64::new(0),
                ui_render_invocations: AtomicU64::new(0),
            }),
        })
    }

    /// Re-read all file-backed extension sources and atomically swap the active QuickJS VM.
    ///
    /// If reading or evaluating any extension fails, the previously active VM, tools, and hooks
    /// remain intact and an error is returned.
    pub fn reload(&self) -> Result<(), JsExtensionError> {
        self.reload_excluding(std::collections::HashSet::new())
    }

    /// Atomically reload extensions after validating that no newly registered tool collides with
    /// `forbidden_tool_names` (or duplicates another tool in the reloaded bundle).
    pub fn reload_excluding(
        &self,
        forbidden_tool_names: std::collections::HashSet<String>,
    ) -> Result<(), JsExtensionError> {
        let sources = resolve_sources(&self.inner.config.sources)?;
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inner
            .worker
            .tx
            .send(WorkerCmd::Reload {
                sources,
                forbidden_tool_names,
                reply: reply_tx,
            })
            .map_err(|_| {
                JsExtensionError::RuntimeUnavailable("QuickJS worker channel closed".to_string())
            })?;

        let new_snapshot = reply_rx.recv().map_err(|_| {
            JsExtensionError::RuntimeUnavailable("QuickJS worker dropped reload reply".to_string())
        })??;

        let new_mask = new_snapshot.hook_mask;
        let new_has_ui = new_snapshot.has_ui_render;
        let mut published = false;
        {
            let mut reg = self
                .inner
                .registry
                .write()
                .unwrap_or_else(|e| e.into_inner());
            if new_snapshot.generation >= reg.generation {
                *reg = new_snapshot;
                self.inner.hook_mask.store(new_mask, Ordering::Release);
                self.inner
                    .has_ui_render
                    .store(new_has_ui, Ordering::Release);
                published = true;
            }
        }
        if published {
            self.inner.ui_generation.fetch_add(1, Ordering::Release);
        }
        Ok(())
    }

    /// Return all currently registered JS tools as [`AgentTool`] trait objects.
    ///
    /// Returned tool handles dispatch dynamically to the runtime's currently active VM, so tools
    /// with the same name automatically pick up reloaded behavior after [`Self::reload`].
    pub fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        let reg = self
            .inner
            .registry
            .read()
            .unwrap_or_else(|e| e.into_inner());
        reg.tools
            .iter()
            .map(|meta| {
                Arc::new(JsAgentTool {
                    name: meta.definition.name.clone(),
                    fallback_definition: meta.definition.clone(),
                    fallback_mode: meta.execution_mode,
                    runtime: Arc::clone(&self.inner),
                }) as Arc<dyn AgentTool>
            })
            .collect()
    }

    /// Return [`ToolDefinition`]s for all currently registered JS tools.
    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        let reg = self
            .inner
            .registry
            .read()
            .unwrap_or_else(|e| e.into_inner());
        reg.tools.iter().map(|m| m.definition.clone()).collect()
    }

    /// Check whether at least one JS listener is registered for the given control-plane hook.
    pub fn has_hook(&self, hook: HookKind) -> bool {
        self.inner.has_hook(hook)
    }

    /// Check whether at least one JS extension registered a `ui.render` interceptor.
    pub fn has_ui_render(&self) -> bool {
        self.inner.has_ui_render.load(Ordering::Acquire)
    }

    /// Return merged UI theme token overrides registered via `jimu.ui.theme(...)`.
    pub fn ui_theme_overrides(&self) -> BTreeMap<String, String> {
        self.inner
            .registry
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .ui_theme_overrides
            .clone()
    }

    /// Lock-free monotonic UI revision counter incremented on `jimu.ui.invalidate()` and reload.
    pub fn ui_generation(&self) -> u64 {
        self.inner.ui_generation.load(Ordering::Acquire)
    }

    /// Total number of `ui.render` calls dispatched to the QuickJS worker thread.
    pub fn ui_render_invocations(&self) -> u64 {
        self.inner.ui_render_invocations.load(Ordering::Relaxed)
    }

    /// Invoke the synchronous `ui.render` onion chain for a [`UiComponent`] slot.
    ///
    /// - Zero-overhead fast path: when `!self.has_ui_render()`, returns `Ok(UiNode::Native)`
    ///   immediately without touching the QuickJS worker thread.
    /// - `timeout_ms`: per-render timeout in milliseconds (pass `0` to fall back to the VM default
    ///   timeout; interactive hosts typically pass [`DEFAULT_UI_RENDER_TIMEOUT_MS`]).
    /// - Returned nodes are validated against [`validate_ui_node`] hard limits before returning.
    pub async fn render_ui(
        &self,
        component: UiComponent,
        props: Value,
        timeout_ms: u64,
    ) -> Result<UiNode, JsExtensionError> {
        if !self.has_ui_render() {
            return Ok(UiNode::Native);
        }
        let props_json = serde_json::to_string(&props)
            .map_err(|err| JsExtensionError::Serialization(err.to_string()))?;
        self.inner
            .ui_render_invocations
            .fetch_add(1, Ordering::Relaxed);
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .worker
            .tx
            .send(WorkerCmd::RenderUi {
                component,
                props_json,
                timeout_ms,
                reply: reply_tx,
            })
            .map_err(|_| {
                JsExtensionError::RuntimeUnavailable("QuickJS worker channel closed".to_string())
            })?;
        let raw_json = match reply_rx.await {
            Ok(Ok(json)) => json,
            Ok(Err(err)) => {
                self.inner.record_error(err.clone());
                return Err(err);
            }
            Err(_) => {
                let err = JsExtensionError::RuntimeUnavailable(
                    "QuickJS worker dropped UI render reply".to_string(),
                );
                self.inner.record_error(err.clone());
                return Err(err);
            }
        };
        parse_and_validate_ui_node(&raw_json).inspect_err(|err| {
            self.inner.record_error(err.clone());
        })
    }

    /// Names/paths of the currently loaded extension sources.
    pub fn loaded_extensions(&self) -> Vec<String> {
        self.inner
            .registry
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .loaded_extensions
            .clone()
    }

    /// Total number of control-plane hook invocations dispatched to QuickJS.
    pub fn js_hook_invocations(&self) -> u64 {
        self.inner.js_hook_invocations.load(Ordering::Relaxed)
    }

    /// Most recent runtime/hook error observed, if any.
    pub fn last_error(&self) -> Option<JsExtensionError> {
        self.inner
            .errors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last()
            .cloned()
    }

    /// Drain and return all recorded runtime/hook errors.
    pub fn take_errors(&self) -> Vec<JsExtensionError> {
        let mut guard = self.inner.errors.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *guard)
    }
}

struct JsAgentTool {
    name: String,
    fallback_definition: ToolDefinition,
    fallback_mode: ToolExecutionMode,
    runtime: Arc<RuntimeInner>,
}

impl AgentTool for JsAgentTool {
    fn definition(&self) -> ToolDefinition {
        let reg = self
            .runtime
            .registry
            .read()
            .unwrap_or_else(|e| e.into_inner());
        reg.tools
            .iter()
            .find(|m| m.definition.name == self.name)
            .map(|m| m.definition.clone())
            .unwrap_or_else(|| self.fallback_definition.clone())
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        let reg = self
            .runtime
            .registry
            .read()
            .unwrap_or_else(|e| e.into_inner());
        reg.tools
            .iter()
            .find(|m| m.definition.name == self.name)
            .map(|m| m.execution_mode)
            .unwrap_or(self.fallback_mode)
    }

    fn execute(
        &self,
        tool_call_id: String,
        args: Value,
        cancel: CancelToken,
        _on_update: ToolProgressCallback,
    ) -> BoxFuture<'_, Result<AgentToolResult, String>> {
        let name = self.name.clone();
        let runtime = Arc::clone(&self.runtime);
        Box::pin(async move {
            let args_json = serde_json::to_string(&args)
                .map_err(|err| format!("failed to serialize tool args: {err}"))?;
            match runtime
                .invoke_tool_raw(name, tool_call_id, args_json, cancel)
                .await
            {
                Ok(result_json) => parse_tool_execute_result(&result_json).map_err(|err| {
                    runtime.record_error(err.clone());
                    err.to_string()
                }),
                Err(err) => {
                    runtime.record_error(err.clone());
                    Err(err.to_string())
                }
            }
        })
    }
}

impl AgentMiddleware for JsExtensionRuntime {
    fn prepare_request<'a>(
        &'a self,
        snapshot: &'a AgentContextSnapshot,
        cancel: CancelToken,
    ) -> BoxFuture<'a, Option<RequestOverride>> {
        if !self.inner.has_hook(HookKind::PrepareRequest) {
            return Box::pin(async { None });
        }
        let payload = JsPrepareRequestSnapshot::from(snapshot);
        let payload_json = match serde_json::to_string(&payload) {
            Ok(s) => s,
            Err(err) => {
                self.inner
                    .record_error(JsExtensionError::Serialization(err.to_string()));
                return Box::pin(async { None });
            }
        };
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            match inner
                .invoke_hook_raw(HookKind::PrepareRequest, payload_json, cancel)
                .await
            {
                Ok(Some(json)) => match parse_request_override(&json) {
                    Ok(ov) => ov,
                    Err(err) => {
                        inner.record_error(err);
                        None
                    }
                },
                Ok(None) => None,
                Err(err) => {
                    inner.record_error(err);
                    None
                }
            }
        })
    }

    fn transform_context<'a>(
        &'a self,
        messages: Vec<AgentMessage>,
        cancel: CancelToken,
    ) -> BoxFuture<'a, Vec<AgentMessage>> {
        if !self.inner.has_hook(HookKind::TransformContext) {
            return Box::pin(async move { messages });
        }
        let payload_json = match serde_json::to_string(&messages) {
            Ok(s) => s,
            Err(err) => {
                self.inner
                    .record_error(JsExtensionError::Serialization(err.to_string()));
                return Box::pin(async move { messages });
            }
        };
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            match inner
                .invoke_hook_raw(HookKind::TransformContext, payload_json, cancel)
                .await
            {
                Ok(Some(json)) => match serde_json::from_str::<Vec<AgentMessage>>(&json) {
                    Ok(transformed) => transformed,
                    Err(err) => {
                        inner.record_error(JsExtensionError::Serialization(format!(
                            "invalid transform_context return value: {err}"
                        )));
                        messages
                    }
                },
                Ok(None) => messages,
                Err(err) => {
                    inner.record_error(err);
                    messages
                }
            }
        })
    }

    fn before_tool_call<'a>(
        &'a self,
        ctx: BeforeToolCallCtx<'a>,
        cancel: CancelToken,
    ) -> BoxFuture<'a, Option<BeforeToolCallResult>> {
        if !self.inner.has_hook(HookKind::BeforeToolCall) {
            return Box::pin(async { None });
        }
        let payload = JsBeforeToolCallPayload::from(&ctx);
        let payload_json = match serde_json::to_string(&payload) {
            Ok(s) => s,
            Err(err) => {
                let err = JsExtensionError::Serialization(err.to_string());
                let reason = err.to_string();
                self.inner.record_error(err);
                return Box::pin(async move {
                    Some(BeforeToolCallResult {
                        block: true,
                        reason: Some(reason),
                        terminate: false,
                        args_override: None,
                    })
                });
            }
        };
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            match inner
                .invoke_hook_raw(HookKind::BeforeToolCall, payload_json, cancel)
                .await
            {
                Ok(Some(json)) => match parse_before_tool_call_result(&json) {
                    Ok(res) => res,
                    Err(err) => {
                        let reason = err.to_string();
                        inner.record_error(err);
                        Some(BeforeToolCallResult {
                            block: true,
                            reason: Some(reason),
                            terminate: false,
                            args_override: None,
                        })
                    }
                },
                Ok(None) => None,
                Err(err) => {
                    let reason = err.to_string();
                    inner.record_error(err);
                    Some(BeforeToolCallResult {
                        block: true,
                        reason: Some(reason),
                        terminate: false,
                        args_override: None,
                    })
                }
            }
        })
    }

    fn after_tool_call<'a>(
        &'a self,
        ctx: AfterToolCallCtx<'a>,
        cancel: CancelToken,
    ) -> BoxFuture<'a, Option<AfterToolCallResult>> {
        if !self.inner.has_hook(HookKind::AfterToolCall) {
            return Box::pin(async { None });
        }
        let fail_closed = |reason: String| AfterToolCallResult {
            content: Some(vec![jimu_ai::ContentPart::Text(
                jimu_ai::TextContent::plain(format!(
                    "Tool output withheld because after_tool_call hook failed: {reason}"
                )),
            )]),
            structured_content: None,
            details: Some(serde_json::Value::Null),
            is_error: Some(true),
            terminate: None,
        };
        let payload = JsAfterToolCallPayload::from(&ctx);
        let payload_json = match serde_json::to_string(&payload) {
            Ok(s) => s,
            Err(err) => {
                let reason = err.to_string();
                self.inner
                    .record_error(JsExtensionError::Serialization(reason.clone()));
                return Box::pin(async move { Some(fail_closed(reason)) });
            }
        };
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            match inner
                .invoke_hook_raw(HookKind::AfterToolCall, payload_json, cancel)
                .await
            {
                Ok(Some(json)) => match parse_after_tool_call_result(&json) {
                    Ok(res) => res,
                    Err(err) => {
                        let reason = err.to_string();
                        inner.record_error(err);
                        Some(fail_closed(reason))
                    }
                },
                Ok(None) => None,
                Err(err) => {
                    let reason = err.to_string();
                    inner.record_error(err);
                    Some(fail_closed(reason))
                }
            }
        })
    }

    fn finish_turn<'a>(
        &'a self,
        ctx: FinishTurnCtx<'a>,
        cancel: CancelToken,
    ) -> BoxFuture<'a, Option<TurnDecision>> {
        if !self.inner.has_hook(HookKind::FinishTurn) {
            return Box::pin(async { None });
        }
        let payload = JsFinishTurnPayload::from(&ctx);
        let payload_json = match serde_json::to_string(&payload) {
            Ok(s) => s,
            Err(err) => {
                self.inner
                    .record_error(JsExtensionError::Serialization(err.to_string()));
                return Box::pin(async { None });
            }
        };
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            match inner
                .invoke_hook_raw(HookKind::FinishTurn, payload_json, cancel)
                .await
            {
                Ok(Some(json)) => match parse_finish_turn_result(&json) {
                    Ok(res) => res,
                    Err(err) => {
                        inner.record_error(err);
                        None
                    }
                },
                Ok(None) => None,
                Err(err) => {
                    inner.record_error(err);
                    None
                }
            }
        })
    }
}
