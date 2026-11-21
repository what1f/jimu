use crate::context::{ConvertToLlmFn, now_ms};
use crate::error::AgentError;
use crate::loop_runner::{
    AgentLoopConfig, ApiKeyResolver, QueueSource, QueuedInput, agent_loop_with_queues,
    validate_continuable,
};
use crate::{AgentEvent, AgentMessage, AgentMiddleware, AgentTool, QueueMode, ToolExecutionMode};
use futures_channel::{mpsc, oneshot};
use futures_core::Stream;
use pikra_ai::{
    BoxFuture, CacheRetention, CancelToken, ContentPart, Message, Model, StreamProvider,
    ThinkingLevel, ToolDefinition, UserMessage,
};
use serde_json::{Map, Value};
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

/// Identifier for an event subscription returned by [`Agent::subscribe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubscriptionId(u64);

type EventListener = Arc<dyn Fn(&AgentEvent) + Send + Sync>;

/// Configuration for constructing an [`Agent`].
#[derive(Clone)]
pub struct AgentConfig {
    pub model: Model,
    pub system_prompt: String,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub thinking_level: ThinkingLevel,
    pub fast: bool,
    pub tool_execution_mode: ToolExecutionMode,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub middlewares: Vec<Arc<dyn AgentMiddleware>>,
    pub convert_to_llm: Option<ConvertToLlmFn>,
    pub api_key: Option<String>,
    pub get_api_key: Option<ApiKeyResolver>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    pub cache_retention: CacheRetention,
    pub session_id: Option<String>,
    pub headers: Vec<(String, Option<String>)>,
    pub sampling_params: Map<String, Value>,
    pub initial_messages: Vec<AgentMessage>,
    /// When `true`, if a prompt run fails or is aborted on the very first model request before
    /// any assistant response completes or any tool executes, the newly added prompt message is
    /// rolled back from `Agent` history and `AgentEvent::AgentEnd { user_message_kept: false }`
    /// is emitted. Defaults to `false`.
    pub rollback_first_turn_on_failure: bool,
}

impl AgentConfig {
    pub fn new(model: Model) -> Self {
        Self {
            model,
            system_prompt: String::new(),
            tools: Vec::new(),
            thinking_level: ThinkingLevel::Off,
            fast: false,
            tool_execution_mode: ToolExecutionMode::Parallel,
            steering_mode: QueueMode::OneAtATime,
            follow_up_mode: QueueMode::OneAtATime,
            middlewares: Vec::new(),
            convert_to_llm: None,
            api_key: None,
            get_api_key: None,
            temperature: None,
            max_tokens: None,
            cache_retention: CacheRetention::default(),
            session_id: None,
            headers: Vec::new(),
            sampling_params: Map::new(),
            initial_messages: Vec::new(),
            rollback_first_turn_on_failure: false,
        }
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = prompt.into();
        self
    }

    pub fn with_tools(mut self, tools: Vec<Arc<dyn AgentTool>>) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_tool(mut self, tool: Arc<dyn AgentTool>) -> Self {
        self.tools.push(tool);
        self
    }

    pub fn with_thinking_level(mut self, level: ThinkingLevel) -> Self {
        self.thinking_level = level;
        self
    }

    pub fn with_fast(mut self, fast: bool) -> Self {
        self.fast = fast;
        self
    }

    pub fn with_tool_execution_mode(mut self, mode: ToolExecutionMode) -> Self {
        self.tool_execution_mode = mode;
        self
    }

    pub fn with_steering_mode(mut self, mode: QueueMode) -> Self {
        self.steering_mode = mode;
        self
    }

    pub fn with_follow_up_mode(mut self, mode: QueueMode) -> Self {
        self.follow_up_mode = mode;
        self
    }

    pub fn with_middleware(mut self, middleware: Arc<dyn AgentMiddleware>) -> Self {
        self.middlewares.push(middleware);
        self
    }

    pub fn with_rollback_first_turn_on_failure(mut self, rollback: bool) -> Self {
        self.rollback_first_turn_on_failure = rollback;
        self
    }
}

struct PendingQueue {
    mode: QueueMode,
    items: VecDeque<QueuedInput>,
}

impl PendingQueue {
    fn new(mode: QueueMode) -> Self {
        Self {
            mode,
            items: VecDeque::new(),
        }
    }

    fn enqueue(&mut self, item: QueuedInput) {
        self.items.push_back(item);
    }

    fn drain(&mut self) -> Vec<QueuedInput> {
        match self.mode {
            QueueMode::All => self.items.drain(..).collect(),
            QueueMode::OneAtATime => self.items.pop_front().into_iter().collect(),
        }
    }

    fn clear(&mut self) {
        self.items.clear();
    }

    fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

struct SharedQueues {
    steering: Mutex<PendingQueue>,
    follow_up: Mutex<PendingQueue>,
}

impl QueueSource for SharedQueues {
    fn drain_steering(&self) -> Vec<QueuedInput> {
        self.steering
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
    }

    fn drain_follow_up(&self) -> Vec<QueuedInput> {
        self.follow_up
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
    }

    fn discard_steering(&self) {
        self.steering
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

struct ActiveRun {
    cancel: CancelToken,
    idle_waiters: Vec<oneshot::Sender<()>>,
}

struct AgentInner {
    model: Model,
    system_prompt: String,
    tools: Vec<Arc<dyn AgentTool>>,
    thinking_level: ThinkingLevel,
    fast: bool,
    tool_execution_mode: ToolExecutionMode,
    middlewares: Vec<Arc<dyn AgentMiddleware>>,
    convert_to_llm: Option<ConvertToLlmFn>,
    api_key: Option<String>,
    get_api_key: Option<ApiKeyResolver>,
    temperature: Option<f64>,
    max_tokens: Option<u32>,
    cache_retention: CacheRetention,
    session_id: Option<String>,
    headers: Vec<(String, Option<String>)>,
    sampling_params: Map<String, Value>,
    rollback_first_turn_on_failure: bool,

    messages: Vec<AgentMessage>,
    streaming_message: Option<AgentMessage>,
    pending_tool_calls: Vec<String>,
    error_message: Option<String>,
    active_run: Option<ActiveRun>,
    listeners: Vec<(SubscriptionId, EventListener)>,
}

impl AgentInner {
    fn snapshot_loop_config(&self) -> AgentLoopConfig {
        AgentLoopConfig {
            model: self.model.clone(),
            system_prompt: self.system_prompt.clone(),
            tools: self.tools.clone(),
            thinking_level: self.thinking_level,
            fast: self.fast,
            tool_execution_mode: self.tool_execution_mode,
            middlewares: self.middlewares.clone(),
            convert_to_llm: self.convert_to_llm.clone(),
            api_key: self.api_key.clone(),
            get_api_key: self.get_api_key.clone(),
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            cache_retention: self.cache_retention,
            session_id: self.session_id.clone(),
            headers: self.headers.clone(),
            sampling_params: self.sampling_params.clone(),
            rollback_first_turn_on_failure: self.rollback_first_turn_on_failure,
        }
    }

    fn finish_run_locked(&mut self) -> Vec<oneshot::Sender<()>> {
        self.streaming_message = None;
        self.pending_tool_calls.clear();
        self.active_run
            .take()
            .map(|r| r.idle_waiters)
            .unwrap_or_default()
    }
}

/// Ensures `active_run` is always cleared and waiters woken even if a run future is dropped early.
struct RunGuard {
    inner: Arc<Mutex<AgentInner>>,
    queues: Arc<SharedQueues>,
    settled: bool,
}

impl RunGuard {
    fn settle(mut self) {
        self.settled = true;
        let waiters = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.finish_run_locked()
        };
        for tx in waiters {
            let _ = tx.send(());
        }
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        if !self.settled {
            self.queues.discard_steering();
            let waiters = {
                let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(active) = &inner.active_run {
                    active.cancel.cancel();
                }
                inner.finish_run_locked()
            };
            for tx in waiters {
                let _ = tx.send(());
            }
        }
    }
}

/// Runtime-agnostic stream of [`AgentEvent`]s returned by [`Agent::prompt_stream`] and
/// [`Agent::continue_stream`].
pub struct AgentEventStream<'a> {
    rx: mpsc::UnboundedReceiver<AgentEvent>,
    driver: Option<BoxFuture<'a, ()>>,
}

impl Stream for AgentEventStream<'_> {
    type Item = AgentEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Poll::Ready(Some(event)) = Pin::new(&mut self.rx).poll_next(cx) {
            return Poll::Ready(Some(event));
        }

        if let Some(driver) = self.driver.as_mut() {
            match driver.as_mut().poll(cx) {
                Poll::Ready(()) => {
                    self.driver = None;
                    return match Pin::new(&mut self.rx).poll_next(cx) {
                        Poll::Ready(item) => Poll::Ready(item),
                        Poll::Pending => Poll::Ready(None),
                    };
                }
                Poll::Pending => {
                    if let Poll::Ready(Some(event)) = Pin::new(&mut self.rx).poll_next(cx) {
                        return Poll::Ready(Some(event));
                    }
                    return Poll::Pending;
                }
            }
        }

        Pin::new(&mut self.rx).poll_next(cx)
    }
}

/// Stateful, thread-safe, runtime-agnostic Agent orchestrating multi-turn LLM + tool execution.
pub struct Agent {
    provider: Arc<dyn StreamProvider>,
    inner: Arc<Mutex<AgentInner>>,
    queues: Arc<SharedQueues>,
    next_sub_id: AtomicU64,
}

impl Agent {
    pub fn new(provider: Arc<dyn StreamProvider>, config: AgentConfig) -> Self {
        let queues = Arc::new(SharedQueues {
            steering: Mutex::new(PendingQueue::new(config.steering_mode)),
            follow_up: Mutex::new(PendingQueue::new(config.follow_up_mode)),
        });

        let inner = Arc::new(Mutex::new(AgentInner {
            model: config.model,
            system_prompt: config.system_prompt,
            tools: config.tools,
            thinking_level: config.thinking_level,
            fast: config.fast,
            tool_execution_mode: config.tool_execution_mode,
            middlewares: config.middlewares,
            convert_to_llm: config.convert_to_llm,
            api_key: config.api_key,
            get_api_key: config.get_api_key,
            temperature: config.temperature,
            max_tokens: config.max_tokens,
            cache_retention: config.cache_retention,
            session_id: config.session_id,
            headers: config.headers,
            sampling_params: config.sampling_params,
            rollback_first_turn_on_failure: config.rollback_first_turn_on_failure,
            messages: config.initial_messages,
            streaming_message: None,
            pending_tool_calls: Vec::new(),
            error_message: None,
            active_run: None,
            listeners: Vec::new(),
        }));

        Self {
            provider,
            inner,
            queues,
            next_sub_id: AtomicU64::new(1),
        }
    }

    // ---- Observable State ----

    pub fn model(&self) -> Model {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .model
            .clone()
    }

    pub fn thinking_level(&self) -> ThinkingLevel {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .thinking_level
    }

    pub fn fast(&self) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).fast
    }

    pub fn system_prompt(&self) -> String {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .system_prompt
            .clone()
    }

    pub fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .tools
            .clone()
    }

    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .tools
            .iter()
            .map(|t| t.definition())
            .collect()
    }

    pub fn messages(&self) -> Vec<AgentMessage> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .messages
            .clone()
    }

    pub fn is_running(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active_run
            .is_some()
    }

    pub fn streaming_message(&self) -> Option<AgentMessage> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .streaming_message
            .clone()
    }

    pub fn pending_tool_calls(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending_tool_calls
            .clone()
    }

    pub fn error_message(&self) -> Option<String> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .error_message
            .clone()
    }

    pub fn steering_mode(&self) -> QueueMode {
        self.queues
            .steering
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mode
    }

    pub fn follow_up_mode(&self) -> QueueMode {
        self.queues
            .follow_up
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mode
    }

    pub fn has_queued_messages(&self) -> bool {
        let s = !self
            .queues
            .steering
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty();
        let f = !self
            .queues
            .follow_up
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty();
        s || f
    }

    // ---- State Mutations (Guarded against Busy) ----

    fn with_idle_inner<R>(&self, f: impl FnOnce(&mut AgentInner) -> R) -> Result<R, AgentError> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.active_run.is_some() {
            return Err(AgentError::Busy);
        }
        Ok(f(&mut inner))
    }

    pub fn set_model(&self, model: Model) -> Result<(), AgentError> {
        self.with_idle_inner(|inner| {
            inner.model = model;
        })
    }

    pub fn set_thinking_level(&self, level: ThinkingLevel) -> Result<(), AgentError> {
        self.with_idle_inner(|inner| {
            inner.thinking_level = level;
        })
    }

    pub fn set_fast(&self, fast: bool) -> Result<(), AgentError> {
        self.with_idle_inner(|inner| {
            inner.fast = fast;
        })
    }

    pub fn set_api_key(&self, api_key: Option<String>) -> Result<(), AgentError> {
        self.with_idle_inner(|inner| {
            inner.api_key = api_key;
        })
    }

    pub fn set_system_prompt(&self, prompt: impl Into<String>) -> Result<(), AgentError> {
        let prompt = prompt.into();
        self.with_idle_inner(|inner| {
            inner.system_prompt = prompt;
        })
    }

    pub fn set_tools(&self, tools: Vec<Arc<dyn AgentTool>>) -> Result<(), AgentError> {
        self.with_idle_inner(|inner| {
            inner.tools = tools;
        })
    }

    pub fn replace_messages(&self, messages: Vec<AgentMessage>) -> Result<(), AgentError> {
        self.with_idle_inner(|inner| {
            inner.messages = messages;
        })
    }

    pub fn reset(&self) -> Result<(), AgentError> {
        self.with_idle_inner(|inner| {
            inner.messages.clear();
            inner.streaming_message = None;
            inner.pending_tool_calls.clear();
            inner.error_message = None;
        })?;
        self.clear_all_queues();
        Ok(())
    }

    pub fn add_middleware(&self, middleware: Arc<dyn AgentMiddleware>) -> Result<(), AgentError> {
        self.with_idle_inner(|inner| {
            inner.middlewares.push(middleware);
        })
    }

    pub fn set_tool_execution_mode(&self, mode: ToolExecutionMode) -> Result<(), AgentError> {
        self.with_idle_inner(|inner| {
            inner.tool_execution_mode = mode;
        })
    }

    // ---- Steering & Follow-Up Queues (Always Thread-Safe) ----

    pub fn steer(&self, id: impl Into<String>, text: impl Into<String>) {
        self.queues
            .steering
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .enqueue(QueuedInput::text(id, text));
    }

    pub fn steer_with_parts(&self, id: impl Into<String>, content: Vec<ContentPart>) {
        self.queues
            .steering
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .enqueue(QueuedInput::from_parts(id, content));
    }

    pub fn follow_up(&self, id: impl Into<String>, text: impl Into<String>) {
        self.queues
            .follow_up
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .enqueue(QueuedInput::text(id, text));
    }

    pub fn follow_up_with_parts(&self, id: impl Into<String>, content: Vec<ContentPart>) {
        self.queues
            .follow_up
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .enqueue(QueuedInput::from_parts(id, content));
    }

    pub fn set_steering_mode(&self, mode: QueueMode) {
        self.queues
            .steering
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mode = mode;
    }

    pub fn set_follow_up_mode(&self, mode: QueueMode) {
        self.queues
            .follow_up
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mode = mode;
    }

    pub fn clear_steering_queue(&self) {
        self.queues
            .steering
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Drain all remaining unconsumed steering items in FIFO order as `(id, content)`.
    pub fn drain_steering(&self) -> Vec<(String, Vec<ContentPart>)> {
        self.queues
            .steering
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .items
            .drain(..)
            .map(|item| (item.id, item.content))
            .collect()
    }

    pub fn clear_follow_up_queue(&self) {
        self.queues
            .follow_up
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    pub fn clear_all_queues(&self) {
        self.clear_steering_queue();
        self.clear_follow_up_queue();
    }

    // ---- Event Subscription ----

    pub fn subscribe(
        &self,
        listener: impl Fn(&AgentEvent) + Send + Sync + 'static,
    ) -> SubscriptionId {
        let id = SubscriptionId(self.next_sub_id.fetch_add(1, Ordering::Relaxed));
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .listeners
            .push((id, Arc::new(listener)));
        id
    }

    pub fn subscribe_channel(&self) -> (SubscriptionId, mpsc::UnboundedReceiver<AgentEvent>) {
        let (tx, rx) = mpsc::unbounded();
        let id = self.subscribe(move |ev| {
            let _ = tx.unbounded_send(ev.clone());
        });
        (id, rx)
    }

    pub fn unsubscribe(&self, id: SubscriptionId) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let before = inner.listeners.len();
        inner.listeners.retain(|(sub_id, _)| *sub_id != id);
        inner.listeners.len() != before
    }

    // ---- Run Control ----

    /// Cancel the active run immediately without awaiting teardown.
    pub fn abort_now(&self) {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(active) = &inner.active_run {
            active.cancel.cancel();
        }
    }

    /// Cancel the active run and wait until all in-flight tools finish, unread steering items are
    /// discarded, `AgentEnd` is emitted, and the active-run lock is released.
    pub async fn abort(&self) {
        let rx = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(active) = inner.active_run.as_mut() {
                active.cancel.cancel();
                let (tx, rx) = oneshot::channel();
                active.idle_waiters.push(tx);
                Some(rx)
            } else {
                None
            }
        };
        if let Some(rx) = rx {
            let _ = rx.await;
        }
    }

    /// Resolve once any active run has fully settled and released its lock.
    pub async fn wait_for_idle(&self) {
        let rx = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(active) = inner.active_run.as_mut() {
                let (tx, rx) = oneshot::channel();
                active.idle_waiters.push(tx);
                Some(rx)
            } else {
                None
            }
        };
        if let Some(rx) = rx {
            let _ = rx.await;
        }
    }

    /// Run a text prompt to completion, returning the messages generated during this run.
    pub async fn prompt(&self, text: impl Into<String>) -> Result<Vec<AgentMessage>, AgentError> {
        let msg = AgentMessage::Llm(Message::User(UserMessage::text(text, now_ms())));
        self.prompt_messages(vec![msg]).await
    }

    /// Run a multimodal user prompt to completion.
    pub async fn prompt_parts(
        &self,
        content: Vec<ContentPart>,
    ) -> Result<Vec<AgentMessage>, AgentError> {
        let msg = AgentMessage::Llm(Message::User(UserMessage {
            id: None,
            content,
            timestamp: now_ms(),
        }));
        self.prompt_messages(vec![msg]).await
    }

    /// Run one or more custom/LLM messages as a prompt to completion.
    pub async fn prompt_messages(
        &self,
        messages: Vec<AgentMessage>,
    ) -> Result<Vec<AgentMessage>, AgentError> {
        if messages.is_empty() {
            return Err(AgentError::EmptyPrompt);
        }
        let prepared = self.begin_run(messages, false)?;
        Ok(self.drive_prepared_run(prepared, None).await)
    }

    /// Run a text prompt as a [`Stream`] of [`AgentEvent`]s.
    pub fn prompt_stream(
        &self,
        text: impl Into<String>,
    ) -> Result<AgentEventStream<'_>, AgentError> {
        let msg = AgentMessage::Llm(Message::User(UserMessage::text(text, now_ms())));
        self.prompt_messages_stream(vec![msg])
    }

    /// Run one or more messages as a [`Stream`] of [`AgentEvent`]s.
    pub fn prompt_messages_stream(
        &self,
        messages: Vec<AgentMessage>,
    ) -> Result<AgentEventStream<'_>, AgentError> {
        if messages.is_empty() {
            return Err(AgentError::EmptyPrompt);
        }
        let prepared = self.begin_run(messages, false)?;
        Ok(self.spawn_stream_driver(prepared))
    }

    /// Continue from the current transcript without adding a new prompt.
    pub async fn continue_run(&self) -> Result<Vec<AgentMessage>, AgentError> {
        let prepared = self.begin_continue_run()?;
        Ok(self.drive_prepared_run(prepared, None).await)
    }

    /// Continue from the current transcript as a [`Stream`] of [`AgentEvent`]s.
    pub fn continue_stream(&self) -> Result<AgentEventStream<'_>, AgentError> {
        let prepared = self.begin_continue_run()?;
        Ok(self.spawn_stream_driver(prepared))
    }

    fn begin_run(
        &self,
        prompts: Vec<AgentMessage>,
        skip_initial_steering: bool,
    ) -> Result<PreparedRun, AgentError> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.active_run.is_some() {
            return Err(AgentError::Busy);
        }

        let cancel = CancelToken::new();
        inner.active_run = Some(ActiveRun {
            cancel: cancel.clone(),
            idle_waiters: Vec::new(),
        });
        inner.streaming_message = None;
        inner.pending_tool_calls.clear();
        inner.error_message = None;

        let config = inner.snapshot_loop_config();
        let existing_messages = inner.messages.clone();
        drop(inner);

        Ok(PreparedRun {
            prompts,
            initial_steers: Vec::new(),
            existing_messages,
            config,
            cancel,
            skip_initial_steering,
            guard: RunGuard {
                inner: Arc::clone(&self.inner),
                queues: Arc::clone(&self.queues),
                settled: false,
            },
        })
    }

    fn begin_continue_run(&self) -> Result<PreparedRun, AgentError> {
        // Acquire active_run atomically under `inner` lock before touching queues.
        let (last_is_assistant, config, existing_messages, cancel) = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if inner.active_run.is_some() {
                return Err(AgentError::Busy);
            }
            if inner.messages.is_empty()
                || inner
                    .messages
                    .iter()
                    .all(|m| matches!(m, AgentMessage::Llm(Message::System(_))))
            {
                return Err(AgentError::CannotContinue(
                    "no non-system messages in transcript".into(),
                ));
            }
            let last_is_assistant = matches!(
                inner.messages.last(),
                Some(AgentMessage::Llm(Message::Assistant(_)))
            );
            if !last_is_assistant {
                validate_continuable(&inner.messages)?;
            }

            let cancel = CancelToken::new();
            inner.active_run = Some(ActiveRun {
                cancel: cancel.clone(),
                idle_waiters: Vec::new(),
            });
            inner.streaming_message = None;
            inner.pending_tool_calls.clear();
            inner.error_message = None;

            (
                last_is_assistant,
                inner.snapshot_loop_config(),
                inner.messages.clone(),
                cancel,
            )
        };

        let guard = RunGuard {
            inner: Arc::clone(&self.inner),
            queues: Arc::clone(&self.queues),
            settled: false,
        };

        if last_is_assistant {
            let queued_steers = self.queues.drain_steering();
            if !queued_steers.is_empty() {
                return Ok(PreparedRun {
                    prompts: Vec::new(),
                    initial_steers: queued_steers,
                    existing_messages,
                    config,
                    cancel,
                    skip_initial_steering: true,
                    guard,
                });
            }
            let queued_follow_ups = self.queues.drain_follow_up();
            if !queued_follow_ups.is_empty() {
                let msgs = queued_follow_ups
                    .into_iter()
                    .map(QueuedInput::into_agent_message)
                    .collect();
                return Ok(PreparedRun {
                    prompts: msgs,
                    initial_steers: Vec::new(),
                    existing_messages,
                    config,
                    cancel,
                    skip_initial_steering: false,
                    guard,
                });
            }
            guard.settle();
            return Err(AgentError::CannotContinue(
                "cannot continue from message role: assistant".into(),
            ));
        }

        Ok(PreparedRun {
            prompts: Vec::new(),
            initial_steers: Vec::new(),
            existing_messages,
            config,
            cancel,
            skip_initial_steering: false,
            guard,
        })
    }

    fn spawn_stream_driver(&self, prepared: PreparedRun) -> AgentEventStream<'_> {
        let (tx, rx) = mpsc::unbounded();
        let driver = Box::pin(async move {
            let _ = self.drive_prepared_run(prepared, Some(tx)).await;
        });
        AgentEventStream {
            rx,
            driver: Some(driver),
        }
    }

    async fn drive_prepared_run(
        &self,
        prepared: PreparedRun,
        stream_tx: Option<mpsc::UnboundedSender<AgentEvent>>,
    ) -> Vec<AgentMessage> {
        let inner_for_emit = Arc::clone(&self.inner);
        let emit: Arc<dyn Fn(AgentEvent) + Send + Sync> = Arc::new(move |event| {
            let listeners = {
                let mut inner = inner_for_emit.lock().unwrap_or_else(|e| e.into_inner());
                match &event {
                    AgentEvent::MessageStart { message } => {
                        inner.streaming_message = Some(message.clone());
                    }
                    AgentEvent::MessageUpdate { message, .. } => {
                        inner.streaming_message =
                            Some(AgentMessage::Llm(Message::Assistant(message.clone())));
                    }
                    AgentEvent::MessageEnd { message } => {
                        inner.streaming_message = None;
                        inner.messages.push(message.clone());
                    }
                    AgentEvent::ToolExecutionStart { tool_call_id, .. } => {
                        if !inner.pending_tool_calls.contains(tool_call_id) {
                            inner.pending_tool_calls.push(tool_call_id.clone());
                        }
                    }
                    AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                        inner.pending_tool_calls.retain(|id| id != tool_call_id);
                    }
                    AgentEvent::TurnEnd { message, .. } => {
                        if let Some(err) = &message.error_message {
                            inner.error_message = Some(err.clone());
                        }
                    }
                    AgentEvent::AgentEnd { .. } => {
                        inner.streaming_message = None;
                        inner.pending_tool_calls.clear();
                    }
                    AgentEvent::AgentStart
                    | AgentEvent::TurnStart
                    | AgentEvent::RequestUsage { .. }
                    | AgentEvent::ToolExecutionUpdate { .. }
                    | AgentEvent::Steered { .. } => {}
                }
                inner
                    .listeners
                    .iter()
                    .map(|(_, l)| Arc::clone(l))
                    .collect::<Vec<_>>()
            };

            for listener in listeners {
                listener(&event);
            }
            if let Some(tx) = &stream_tx {
                let _ = tx.unbounded_send(event);
            }
        });

        let inner_for_sync = Arc::clone(&self.inner);
        let before_agent_end: crate::loop_runner::BeforeAgentEndHook =
            Box::new(move |final_messages, rolled_back| {
                let mut inner = inner_for_sync.lock().unwrap_or_else(|e| e.into_inner());
                if rolled_back {
                    inner.messages = final_messages.to_vec();
                }
            });

        let queues_dyn: Arc<dyn QueueSource> = Arc::clone(&self.queues) as Arc<dyn QueueSource>;
        let outcome = agent_loop_with_queues(
            prepared.prompts,
            prepared.initial_steers,
            prepared.existing_messages,
            prepared.config,
            Arc::clone(&self.provider),
            queues_dyn,
            prepared.cancel,
            emit,
            prepared.skip_initial_steering,
            Some(before_agent_end),
        )
        .await;

        // Release active_run lock BEFORE resolving prompt() / abort() / wait_for_idle().
        prepared.guard.settle();
        outcome.new_messages
    }
}

struct PreparedRun {
    prompts: Vec<AgentMessage>,
    initial_steers: Vec<QueuedInput>,
    existing_messages: Vec<AgentMessage>,
    config: AgentLoopConfig,
    cancel: CancelToken,
    skip_initial_steering: bool,
    guard: RunGuard,
}
