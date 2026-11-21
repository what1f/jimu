use crate::clock::Clock;
use crate::error::DurableError;
use crate::store::DurableStore;
use pikra_agent::{AgentTool, AgentToolResult, ToolProgressCallback};
use pikra_ai::{BoxFuture, CancelToken, ToolDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

/// Runtime context supplied to [`DurableTool`] execution and resume callbacks.
#[derive(Clone)]
pub struct DurableToolContext {
    pub conversation_id: String,
    pub submission_id: String,
    pub tool_call_id: String,
    pub clock: Arc<dyn Clock>,
    pub store: Arc<dyn DurableStore>,
    pub cancel: CancelToken,
}

impl DurableToolContext {
    pub fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    pub fn get_document(&self, key: &str) -> Result<Option<(u64, Value)>, DurableError> {
        self.store.get_document(&self.conversation_id, key)
    }

    pub fn put_document(
        &self,
        key: &str,
        expected_revision: Option<u64>,
        value: Value,
    ) -> Result<u64, DurableError> {
        self.store.put_document(
            &self.conversation_id,
            key,
            expected_revision,
            value,
            self.now_ms(),
        )
    }
}

/// Resolution passed to [`DurableTool::resume_with_signal`] when woken by a signal or timeout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SignalResolution {
    Received(Value),
    TimedOut,
}

/// Outcome returned by a [`DurableTool`] invocation or resume step.
#[derive(Debug, Clone, PartialEq)]
pub enum DurableToolOutcome {
    /// Tool finished and produced a finalized result to commit into the side-effect ledger.
    Completed(AgentToolResult),
    /// Suspend execution until `clock.now_ms() >= wake_at_ms`, then call `resume_after_sleep`.
    SleepUntil { wake_at_ms: u64, checkpoint: Value },
    /// Suspend execution until an external signal arrives on `signal_key` (or `timeout_at_ms`
    /// elapses), then call `resume_with_signal`.
    AwaitSignal {
        signal_key: String,
        timeout_at_ms: Option<u64>,
        checkpoint: Value,
    },
}

/// Tool contract supporting immediate completion as well as durable sleep and signal suspension.
pub trait DurableTool: Send + Sync {
    fn definition(&self) -> ToolDefinition;

    fn execute<'a>(
        &'a self,
        ctx: &'a DurableToolContext,
        args: Value,
    ) -> BoxFuture<'a, Result<DurableToolOutcome, String>>;

    fn resume_after_sleep<'a>(
        &'a self,
        _ctx: &'a DurableToolContext,
        _args: Value,
        _checkpoint: Value,
    ) -> BoxFuture<'a, Result<DurableToolOutcome, String>> {
        Box::pin(async { Err("tool does not implement resume_after_sleep".to_string()) })
    }

    fn resume_with_signal<'a>(
        &'a self,
        _ctx: &'a DurableToolContext,
        _args: Value,
        _checkpoint: Value,
        _resolution: SignalResolution,
    ) -> BoxFuture<'a, Result<DurableToolOutcome, String>> {
        Box::pin(async { Err("tool does not implement resume_with_signal".to_string()) })
    }
}

/// Adapter allowing any existing [`pikra_agent::AgentTool`] to run inside [`crate::DurableEngine`].
pub struct AgentToolAdapter<T: AgentTool> {
    inner: T,
}

impl<T: AgentTool> AgentToolAdapter<T> {
    pub fn new(inner: T) -> Self {
        Self { inner }
    }

    pub fn into_inner(self) -> T {
        self.inner
    }

    pub fn inner(&self) -> &T {
        &self.inner
    }
}

impl<T: AgentTool> DurableTool for AgentToolAdapter<T> {
    fn definition(&self) -> ToolDefinition {
        self.inner.definition()
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a DurableToolContext,
        args: Value,
    ) -> BoxFuture<'a, Result<DurableToolOutcome, String>> {
        let noop_update: ToolProgressCallback = Arc::new(|_| {});
        let fut = self.inner.execute(
            ctx.tool_call_id.clone(),
            args,
            ctx.cancel.clone(),
            noop_update,
        );
        Box::pin(async move { fut.await.map(DurableToolOutcome::Completed) })
    }
}
