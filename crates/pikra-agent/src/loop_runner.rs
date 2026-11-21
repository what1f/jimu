use crate::context::{convert_to_llm_with_overrides, now_ms, ConvertToLlmFn};
use crate::error::AgentError;
use crate::tool::{execute_tool_batch, fail_truncated_tool_calls};
use crate::{
    AgentContextSnapshot, AgentEvent, AgentMessage, AgentMiddleware, AgentTool, FinishTurnCtx,
    ToolExecutionMode, TurnDecision,
};
use pikra_ai::{
    AssistantBlock, AssistantMessage, AssistantMessageEvent, BoxFuture, CacheRetention,
    CancelToken, ContentPart, Message, Model, SimpleStreamOptions, StopReason, StreamProvider,
    TextContent, ThinkingContent, ThinkingLevel, ToolCall, ToolDefinition, Usage, UserMessage,
};
use serde_json::{Map, Value};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;

/// Dynamic API key resolver invoked before each LLM call.
pub type ApiKeyResolver = Arc<dyn Fn(&str) -> BoxFuture<'static, Option<String>> + Send + Sync>;

/// A queued steering or follow-up message.
#[derive(Debug, Clone)]
pub struct QueuedInput {
    pub id: String,
    pub text: String,
    pub content: Vec<ContentPart>,
}

impl QueuedInput {
    pub fn text(id: impl Into<String>, text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            id: id.into(),
            content: vec![ContentPart::Text(TextContent::plain(text.clone()))],
            text,
        }
    }

    pub fn from_parts(id: impl Into<String>, content: Vec<ContentPart>) -> Self {
        let text = content
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text(t) => Some(t.text.as_str()),
                ContentPart::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("");
        Self {
            id: id.into(),
            text,
            content,
        }
    }

    pub(crate) fn into_agent_message(self) -> AgentMessage {
        AgentMessage::Llm(Message::User(UserMessage {
            id: Some(self.id),
            content: self.content,
            timestamp: now_ms(),
        }))
    }
}

/// Queue provider for steering and follow-up messages during a loop invocation.
pub trait QueueSource: Send + Sync {
    fn drain_steering(&self) -> Vec<QueuedInput>;
    fn drain_follow_up(&self) -> Vec<QueuedInput>;
    fn discard_steering(&self);
}

struct NoopQueueSource;

impl QueueSource for NoopQueueSource {
    fn drain_steering(&self) -> Vec<QueuedInput> {
        Vec::new()
    }
    fn drain_follow_up(&self) -> Vec<QueuedInput> {
        Vec::new()
    }
    fn discard_steering(&self) {}
}

/// Configuration for the low-level [`agent_loop`] and [`agent_loop_continue`] functions.
#[derive(Clone)]
pub struct AgentLoopConfig {
    pub model: Model,
    pub system_prompt: String,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub thinking_level: ThinkingLevel,
    pub fast: bool,
    pub tool_execution_mode: ToolExecutionMode,
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
    pub rollback_first_turn_on_failure: bool,
}

impl AgentLoopConfig {
    pub fn new(model: Model) -> Self {
        Self {
            model,
            system_prompt: String::new(),
            tools: Vec::new(),
            thinking_level: ThinkingLevel::Off,
            fast: false,
            tool_execution_mode: ToolExecutionMode::Parallel,
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
            rollback_first_turn_on_failure: false,
        }
    }
}

/// Result returned by [`agent_loop`] and [`agent_loop_continue`].
#[derive(Debug, Clone)]
pub struct AgentLoopOutcome {
    /// Complete conversation transcript after the run settled.
    pub messages: Vec<AgentMessage>,
    /// Messages produced during this run invocation (empty if rolled back).
    pub new_messages: Vec<AgentMessage>,
    /// Whether the prompt's user message was retained in the settled transcript.
    pub user_message_kept: bool,
    /// Error message from the last failed/aborted assistant turn, if any.
    pub last_error: Option<String>,
}

/// Run an agent loop starting with one or more new prompt messages.
pub async fn agent_loop(
    prompts: Vec<AgentMessage>,
    existing_messages: Vec<AgentMessage>,
    config: AgentLoopConfig,
    provider: Arc<dyn StreamProvider>,
    cancel: CancelToken,
    emit: Arc<dyn Fn(AgentEvent) + Send + Sync>,
) -> AgentLoopOutcome {
    agent_loop_with_queues(
        prompts,
        Vec::new(),
        existing_messages,
        config,
        provider,
        Arc::new(NoopQueueSource),
        cancel,
        emit,
        false,
        None,
    )
    .await
}

/// Continue an agent loop from existing messages without adding a new prompt.
pub async fn agent_loop_continue(
    existing_messages: Vec<AgentMessage>,
    config: AgentLoopConfig,
    provider: Arc<dyn StreamProvider>,
    cancel: CancelToken,
    emit: Arc<dyn Fn(AgentEvent) + Send + Sync>,
) -> Result<AgentLoopOutcome, AgentError> {
    validate_continuable(&existing_messages)?;
    Ok(agent_loop_with_queues(
        Vec::new(),
        Vec::new(),
        existing_messages,
        config,
        provider,
        Arc::new(NoopQueueSource),
        cancel,
        emit,
        true,
        None,
    )
    .await)
}

pub(crate) fn validate_continuable(messages: &[AgentMessage]) -> Result<(), AgentError> {
    if messages.is_empty()
        || messages
            .iter()
            .all(|m| matches!(m, AgentMessage::Llm(Message::System(_))))
    {
        return Err(AgentError::CannotContinue(
            "no non-system messages in transcript".into(),
        ));
    }
    if matches!(messages.last(), Some(AgentMessage::Llm(Message::Assistant(_)))) {
        return Err(AgentError::CannotContinue(
            "cannot continue from message role: assistant".into(),
        ));
    }
    Ok(())
}

pub(crate) type BeforeAgentEndHook = Box<dyn FnOnce(&[AgentMessage], bool) + Send>;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn agent_loop_with_queues(
    prompts: Vec<AgentMessage>,
    initial_steers: Vec<QueuedInput>,
    existing_messages: Vec<AgentMessage>,
    config: AgentLoopConfig,
    provider: Arc<dyn StreamProvider>,
    queues: Arc<dyn QueueSource>,
    cancel: CancelToken,
    emit: Arc<dyn Fn(AgentEvent) + Send + Sync>,
    skip_initial_steering: bool,
    before_agent_end: Option<BeforeAgentEndHook>,
) -> AgentLoopOutcome {
    let base_len = existing_messages.len();
    let had_prompt_user_message = !initial_steers.is_empty()
        || prompts
            .iter()
            .any(|m| matches!(m, AgentMessage::Llm(Message::User(_))));

    let mut current_messages = existing_messages;
    let mut new_messages = Vec::new();

    emit(AgentEvent::AgentStart);
    emit(AgentEvent::TurnStart);

    for prompt_msg in prompts {
        current_messages.push(prompt_msg.clone());
        new_messages.push(prompt_msg.clone());
        emit(AgentEvent::MessageStart {
            message: prompt_msg.clone(),
        });
        emit(AgentEvent::MessageEnd {
            message: prompt_msg,
        });
    }

    let mut pending_steers = initial_steers;
    if !skip_initial_steering && !cancel.is_cancelled() {
        pending_steers.extend(queues.drain_steering());
    }
    let mut pending_follow_ups: Vec<QueuedInput> = Vec::new();

    let mut is_first_turn = true;
    let mut any_tool_executed = false;
    let mut turns_with_ok_assistant = 0usize;
    let mut last_error: Option<String> = None;
    let mut explicit_continuation = false;
    let mut first_turn_failed_cleanly = false;

    let tool_defs: Vec<ToolDefinition> = config.tools.iter().map(|t| t.definition()).collect();

    'outer: loop {
        let mut has_more_tool_calls = true;

        while has_more_tool_calls
            || !pending_steers.is_empty()
            || !pending_follow_ups.is_empty()
            || explicit_continuation
        {
            if !is_first_turn {
                emit(AgentEvent::TurnStart);
            }

            // Admit steering messages (emit Steered before MessageStart/MessageEnd)
            for steer in std::mem::take(&mut pending_steers) {
                emit(AgentEvent::Steered {
                    id: steer.id.clone(),
                    text: steer.text.clone(),
                });
                let msg = steer.into_agent_message();
                current_messages.push(msg.clone());
                new_messages.push(msg.clone());
                emit(AgentEvent::MessageStart {
                    message: msg.clone(),
                });
                emit(AgentEvent::MessageEnd { message: msg });
            }

            // Admit follow-up messages
            for fu in std::mem::take(&mut pending_follow_ups) {
                let msg = fu.into_agent_message();
                current_messages.push(msg.clone());
                new_messages.push(msg.clone());
                emit(AgentEvent::MessageStart {
                    message: msg.clone(),
                });
                emit(AgentEvent::MessageEnd { message: msg });
            }

            // Prepare request snapshot & run prepare_request middleware chain
            let mut request_snapshot = AgentContextSnapshot {
                system_prompt: config.system_prompt.clone(),
                tools: tool_defs.clone(),
                messages: current_messages.clone(),
                model: config.model.clone(),
                thinking_level: config.thinking_level,
                fast: config.fast,
            };
            let mut system_overridden = false;
            let mut tools_overridden = false;

            if !cancel.is_cancelled() {
                for mw in &config.middlewares {
                    if let Some(ov) = mw.prepare_request(&request_snapshot, cancel.clone()).await {
                        if let Some(sp) = ov.system_prompt {
                            request_snapshot.system_prompt = sp;
                            system_overridden = true;
                        }
                        if let Some(tools) = ov.tools {
                            request_snapshot.tools = tools;
                            tools_overridden = true;
                        }
                        if let Some(msgs) = ov.messages {
                            request_snapshot.messages = msgs;
                        }
                        if let Some(model) = ov.model {
                            request_snapshot.model = model;
                        }
                        if let Some(tl) = ov.thinking_level {
                            request_snapshot.thinking_level = tl;
                        }
                        if let Some(fast) = ov.fast {
                            request_snapshot.fast = fast;
                        }
                    }
                    if cancel.is_cancelled() {
                        break;
                    }
                }
            }

            // Run transform_context middleware chain
            let mut transformed_messages = request_snapshot.messages.clone();
            if !cancel.is_cancelled() {
                for mw in &config.middlewares {
                    transformed_messages = mw
                        .transform_context(transformed_messages, cancel.clone())
                        .await;
                    if cancel.is_cancelled() {
                        break;
                    }
                }
            }

            // Stream assistant response
            let assistant_msg = if cancel.is_cancelled() {
                let aborted = make_aborted_assistant(
                    &request_snapshot.model,
                    request_snapshot.thinking_level,
                    "Operation aborted",
                );
                let agent_msg = AgentMessage::Llm(Message::Assistant(aborted.clone()));
                emit(AgentEvent::MessageStart {
                    message: agent_msg.clone(),
                });
                emit(AgentEvent::MessageEnd { message: agent_msg });
                emit(AgentEvent::RequestUsage {
                    model: aborted.model.clone(),
                    provider: aborted.provider.clone(),
                    usage: aborted.usage.clone(),
                });
                aborted
            } else {
                let transcript = if let Some(custom_convert) = &config.convert_to_llm {
                    custom_convert(
                        &request_snapshot.system_prompt,
                        &request_snapshot.tools,
                        &transformed_messages,
                    )
                } else {
                    convert_to_llm_with_overrides(
                        &request_snapshot.system_prompt,
                        &request_snapshot.tools,
                        &transformed_messages,
                        system_overridden,
                        tools_overridden,
                    )
                };

                let resolved_api_key = match &config.get_api_key {
                    Some(resolver) => resolver(&request_snapshot.model.provider)
                        .await
                        .or_else(|| config.api_key.clone()),
                    None => config.api_key.clone(),
                };

                let stream_options = SimpleStreamOptions {
                    api_key: resolved_api_key,
                    reasoning: if request_snapshot.thinking_level == ThinkingLevel::Off {
                        None
                    } else {
                        Some(request_snapshot.thinking_level)
                    },
                    fast: Some(request_snapshot.fast),
                    temperature: config.temperature,
                    max_tokens: config.max_tokens,
                    cache_retention: config.cache_retention,
                    session_id: config.session_id.clone(),
                    headers: config.headers.clone(),
                    sampling_params: config.sampling_params.clone(),
                    cancel: cancel.clone(),
                };

                let event_stream =
                    provider.stream(&request_snapshot.model, &transcript, stream_options);
                stream_assistant_response(
                    event_stream,
                    &request_snapshot.model,
                    request_snapshot.thinking_level,
                    &cancel,
                    emit.as_ref(),
                )
                .await
            };

            // Barrier: commit assistant message to transcript BEFORE before_tool_call / finish_turn
            current_messages.push(AgentMessage::Llm(Message::Assistant(assistant_msg.clone())));
            new_messages.push(AgentMessage::Llm(Message::Assistant(assistant_msg.clone())));

            if let Some(err) = &assistant_msg.error_message {
                last_error = Some(err.clone());
            }

            // Hard exit on Error or Aborted assistant responses
            if matches!(
                assistant_msg.stop_reason,
                StopReason::Error | StopReason::Aborted
            ) {
                if is_first_turn && !any_tool_executed && turns_with_ok_assistant == 0 {
                    first_turn_failed_cleanly = true;
                }
                let turn_snapshot = AgentContextSnapshot {
                    system_prompt: request_snapshot.system_prompt.clone(),
                    tools: request_snapshot.tools.clone(),
                    messages: current_messages.clone(),
                    model: request_snapshot.model.clone(),
                    thinking_level: request_snapshot.thinking_level,
                    fast: request_snapshot.fast,
                };
                let _ = run_finish_turn(
                    &config.middlewares,
                    &assistant_msg,
                    &[],
                    &turn_snapshot,
                    &cancel,
                )
                .await;
                emit(AgentEvent::TurnEnd {
                    message: assistant_msg,
                    tool_results: Vec::new(),
                });
                break 'outer;
            }

            turns_with_ok_assistant += 1;

            // Execute tool calls (if any)
            let tool_calls: Vec<ToolCall> = assistant_msg
                .tool_calls()
                .into_iter()
                .cloned()
                .collect();

            let mut tool_results = Vec::new();
            has_more_tool_calls = false;

            if !tool_calls.is_empty() {
                let tool_snapshot = AgentContextSnapshot {
                    system_prompt: request_snapshot.system_prompt.clone(),
                    tools: request_snapshot.tools.clone(),
                    messages: current_messages.clone(),
                    model: request_snapshot.model.clone(),
                    thinking_level: request_snapshot.thinking_level,
                    fast: request_snapshot.fast,
                };

                let batch = if assistant_msg.stop_reason == StopReason::Length {
                    fail_truncated_tool_calls(&tool_calls, emit.as_ref())
                } else {
                    execute_tool_batch(
                        &config.tools,
                        &config.middlewares,
                        &assistant_msg,
                        &tool_calls,
                        &tool_snapshot,
                        config.tool_execution_mode,
                        &cancel,
                        Arc::clone(&emit),
                    )
                    .await
                };

                any_tool_executed = true;
                has_more_tool_calls = !batch.terminate;
                for tr in batch.tool_results {
                    current_messages.push(AgentMessage::Llm(Message::ToolResult(tr.clone())));
                    new_messages.push(AgentMessage::Llm(Message::ToolResult(tr.clone())));
                    tool_results.push(tr);
                }
            }

            // Run finish_turn middleware chain BEFORE TurnEnd
            let finish_snapshot = AgentContextSnapshot {
                system_prompt: request_snapshot.system_prompt,
                tools: request_snapshot.tools,
                messages: current_messages.clone(),
                model: request_snapshot.model,
                thinking_level: request_snapshot.thinking_level,
                fast: request_snapshot.fast,
            };
            let decision = run_finish_turn(
                &config.middlewares,
                &assistant_msg,
                &tool_results,
                &finish_snapshot,
                &cancel,
            )
            .await;

            emit(AgentEvent::TurnEnd {
                message: assistant_msg,
                tool_results,
            });

            is_first_turn = false;

            // Hard exit if cancelled during tool execution or finish_turn
            if cancel.is_cancelled() {
                break 'outer;
            }

            if decision == Some(TurnDecision::End) {
                break 'outer;
            }

            explicit_continuation = decision == Some(TurnDecision::Continue);

            pending_steers = queues.drain_steering();
            if has_more_tool_calls || !pending_steers.is_empty() {
                explicit_continuation = false;
            }

            if !has_more_tool_calls && pending_steers.is_empty() {
                break;
            }
        }

        if cancel.is_cancelled() {
            break 'outer;
        }

        let follow_ups = queues.drain_follow_up();
        if !follow_ups.is_empty() {
            explicit_continuation = false;
            pending_follow_ups = follow_ups;
            continue 'outer;
        }

        if explicit_continuation {
            // One context-only turn requested by finish_turn
            explicit_continuation = false;
            continue 'outer;
        }

        break 'outer;
    }

    // Settling invariant: discard any remaining unread steering items so they never ambush the next turn.
    queues.discard_steering();

    let rolled_back = config.rollback_first_turn_on_failure
        && first_turn_failed_cleanly
        && had_prompt_user_message;

    if rolled_back {
        current_messages.truncate(base_len);
        new_messages.clear();
    }

    let user_message_kept = if had_prompt_user_message {
        !rolled_back
    } else {
        current_messages
            .iter()
            .any(|m| matches!(m, AgentMessage::Llm(Message::User(_))))
    };

    if let Some(hook) = before_agent_end {
        hook(&current_messages, rolled_back);
    }

    emit(AgentEvent::AgentEnd {
        messages: new_messages.clone(),
        user_message_kept,
    });

    AgentLoopOutcome {
        messages: current_messages,
        new_messages,
        user_message_kept,
        last_error,
    }
}

async fn run_finish_turn(
    middlewares: &[Arc<dyn AgentMiddleware>],
    assistant_message: &AssistantMessage,
    tool_results: &[pikra_ai::ToolResultMessage],
    snapshot: &AgentContextSnapshot,
    cancel: &CancelToken,
) -> Option<TurnDecision> {
    let mut final_decision: Option<TurnDecision> = None;
    for mw in middlewares {
        let ctx = FinishTurnCtx {
            assistant_message,
            tool_results,
            snapshot,
        };
        if let Some(decision) = mw.finish_turn(ctx, cancel.clone()).await {
            match decision {
                TurnDecision::End => return Some(TurnDecision::End),
                TurnDecision::Continue => {
                    final_decision = Some(TurnDecision::Continue);
                }
            }
        }
    }
    final_decision
}

fn empty_assistant(model: &Model, thinking_level: ThinkingLevel, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_id: None,
        thinking_level: Some(thinking_level),
        usage: Usage::default(),
        stop_reason,
        error_message: None,
        diagnostics: Vec::new(),
        timestamp: now_ms(),
        duration_ms: None,
    }
}

fn make_aborted_assistant(
    model: &Model,
    thinking_level: ThinkingLevel,
    reason: &str,
) -> AssistantMessage {
    let mut msg = empty_assistant(model, thinking_level, StopReason::Aborted);
    msg.error_message = Some(reason.to_string());
    msg
}

fn ensure_block_slot(blocks: &mut Vec<AssistantBlock>, index: usize, default_block: AssistantBlock) {
    while blocks.len() < index {
        blocks.push(AssistantBlock::Text(TextContent::plain("")));
    }
    if blocks.len() == index {
        blocks.push(default_block);
    }
}

fn apply_stream_event(partial: &mut AssistantMessage, event: &AssistantMessageEvent) {
    match event {
        AssistantMessageEvent::TextStart { content_index } => {
            ensure_block_slot(
                &mut partial.content,
                *content_index,
                AssistantBlock::Text(TextContent::plain("")),
            );
            partial.content[*content_index] = AssistantBlock::Text(TextContent::plain(""));
        }
        AssistantMessageEvent::TextDelta {
            content_index,
            delta,
        } => {
            ensure_block_slot(
                &mut partial.content,
                *content_index,
                AssistantBlock::Text(TextContent::plain("")),
            );
            if let AssistantBlock::Text(tc) = &mut partial.content[*content_index] {
                tc.text.push_str(delta);
            } else {
                partial.content[*content_index] =
                    AssistantBlock::Text(TextContent::plain(delta.clone()));
            }
        }
        AssistantMessageEvent::TextEnd {
            content_index,
            text,
        } => {
            ensure_block_slot(
                &mut partial.content,
                *content_index,
                AssistantBlock::Text(TextContent::plain("")),
            );
            if let AssistantBlock::Text(tc) = &mut partial.content[*content_index] {
                tc.text = text.clone();
            } else {
                partial.content[*content_index] =
                    AssistantBlock::Text(TextContent::plain(text.clone()));
            }
        }
        AssistantMessageEvent::ThinkingStart { content_index } => {
            ensure_block_slot(
                &mut partial.content,
                *content_index,
                AssistantBlock::Thinking(ThinkingContent {
                    thinking: String::new(),
                    thinking_signature: None,
                    redacted: false,
                }),
            );
            partial.content[*content_index] = AssistantBlock::Thinking(ThinkingContent {
                thinking: String::new(),
                thinking_signature: None,
                redacted: false,
            });
        }
        AssistantMessageEvent::ThinkingDelta {
            content_index,
            delta,
        } => {
            ensure_block_slot(
                &mut partial.content,
                *content_index,
                AssistantBlock::Thinking(ThinkingContent {
                    thinking: String::new(),
                    thinking_signature: None,
                    redacted: false,
                }),
            );
            if let AssistantBlock::Thinking(th) = &mut partial.content[*content_index] {
                th.thinking.push_str(delta);
            }
        }
        AssistantMessageEvent::ThinkingEnd {
            content_index,
            thinking,
        } => {
            ensure_block_slot(
                &mut partial.content,
                *content_index,
                AssistantBlock::Thinking(ThinkingContent {
                    thinking: String::new(),
                    thinking_signature: None,
                    redacted: false,
                }),
            );
            if let AssistantBlock::Thinking(th) = &mut partial.content[*content_index] {
                th.thinking = thinking.clone();
            }
        }
        AssistantMessageEvent::ToolCallStart {
            content_index,
            id,
            name,
        } => {
            ensure_block_slot(
                &mut partial.content,
                *content_index,
                AssistantBlock::ToolCall(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: Map::new(),
                    item_id: None,
                    thought_signature: None,
                    namespace: None,
                    arguments_error: None,
                }),
            );
            partial.content[*content_index] = AssistantBlock::ToolCall(ToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: Map::new(),
                item_id: None,
                thought_signature: None,
                namespace: None,
                arguments_error: None,
            });
        }
        AssistantMessageEvent::ToolCallDelta { .. } => {}
        AssistantMessageEvent::ToolCallEnd {
            content_index,
            tool_call,
        } => {
            ensure_block_slot(
                &mut partial.content,
                *content_index,
                AssistantBlock::ToolCall(tool_call.clone()),
            );
            partial.content[*content_index] = AssistantBlock::ToolCall(tool_call.clone());
        }
        AssistantMessageEvent::Start { .. }
        | AssistantMessageEvent::Done { .. }
        | AssistantMessageEvent::Error { .. } => {}
    }
}

async fn next_event_or_cancel(
    stream: &mut pikra_ai::AssistantEventStream,
    cancel: &CancelToken,
) -> Option<Option<AssistantMessageEvent>> {
    futures_util::future::poll_fn(|cx| {
        if let Poll::Ready(item) = stream.as_mut().poll_next(cx) {
            return Poll::Ready(Some(item));
        }
        if cancel.is_cancelled() {
            return Poll::Ready(None);
        }
        let mut fut = cancel.cancelled();
        if Pin::new(&mut fut).poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        Poll::Pending
    })
    .await
}

async fn stream_assistant_response(
    mut stream: pikra_ai::AssistantEventStream,
    model: &Model,
    thinking_level: ThinkingLevel,
    cancel: &CancelToken,
    emit: &(dyn Fn(AgentEvent) + Send + Sync),
) -> AssistantMessage {
    let mut partial: Option<AssistantMessage> = None;
    let mut started = false;
    let mut settled: Option<AssistantMessage> = None;

    while let Some(stream_item) = next_event_or_cancel(&mut stream, cancel).await {
        let Some(event) = stream_item else {
            break;
        };

        match event {
            AssistantMessageEvent::Start { partial: p } => {
                partial = Some(p.clone());
                if !started {
                    started = true;
                    emit(AgentEvent::MessageStart {
                        message: AgentMessage::Llm(Message::Assistant(p.clone())),
                    });
                }
                emit(AgentEvent::MessageUpdate {
                    message: p.clone(),
                    assistant_message_event: AssistantMessageEvent::Start { partial: p },
                });
            }
            AssistantMessageEvent::Done { reason, mut message } => {
                if message.stop_reason == StopReason::Pending {
                    message.stop_reason = reason;
                }
                if message.thinking_level.is_none() {
                    message.thinking_level = Some(thinking_level);
                }
                if message.model.is_empty() {
                    message.model = model.id.clone();
                }
                if message.provider.is_empty() {
                    message.provider = model.provider.clone();
                }
                if !started {
                    started = true;
                    emit(AgentEvent::MessageStart {
                        message: AgentMessage::Llm(Message::Assistant(message.clone())),
                    });
                }
                emit(AgentEvent::MessageUpdate {
                    message: message.clone(),
                    assistant_message_event: AssistantMessageEvent::Done {
                        reason,
                        message: message.clone(),
                    },
                });
                settled = Some(message);
                break;
            }
            AssistantMessageEvent::Error { reason, mut error } => {
                if error.stop_reason == StopReason::Pending {
                    error.stop_reason = reason;
                }
                if error.thinking_level.is_none() {
                    error.thinking_level = Some(thinking_level);
                }
                if error.model.is_empty() {
                    error.model = model.id.clone();
                }
                if error.provider.is_empty() {
                    error.provider = model.provider.clone();
                }
                if !started {
                    started = true;
                    emit(AgentEvent::MessageStart {
                        message: AgentMessage::Llm(Message::Assistant(error.clone())),
                    });
                }
                emit(AgentEvent::MessageUpdate {
                    message: error.clone(),
                    assistant_message_event: AssistantMessageEvent::Error {
                        reason,
                        error: error.clone(),
                    },
                });
                settled = Some(error);
                break;
            }
            other => {
                let p = partial.get_or_insert_with(|| {
                    empty_assistant(model, thinking_level, StopReason::Pending)
                });
                if !started {
                    started = true;
                    emit(AgentEvent::MessageStart {
                        message: AgentMessage::Llm(Message::Assistant(p.clone())),
                    });
                }
                apply_stream_event(p, &other);
                emit(AgentEvent::MessageUpdate {
                    message: p.clone(),
                    assistant_message_event: other,
                });
            }
        }
    }

    // Drop stream promptly so any underlying transport request is cancelled.
    drop(stream);

    let final_message = match settled {
        Some(msg) => msg,
        None => {
            let mut msg = partial
                .unwrap_or_else(|| empty_assistant(model, thinking_level, StopReason::Pending));
            if cancel.is_cancelled() {
                msg.stop_reason = StopReason::Aborted;
                if msg.error_message.is_none() {
                    msg.error_message = Some("Operation aborted".to_string());
                }
            } else {
                msg.stop_reason = StopReason::Error;
                if msg.error_message.is_none() {
                    msg.error_message =
                        Some("Stream ended before terminal Done or Error event".to_string());
                }
            }
            if !started {
                emit(AgentEvent::MessageStart {
                    message: AgentMessage::Llm(Message::Assistant(msg.clone())),
                });
            }
            msg
        }
    };

    emit(AgentEvent::MessageEnd {
        message: AgentMessage::Llm(Message::Assistant(final_message.clone())),
    });
    emit(AgentEvent::RequestUsage {
        model: final_message.model.clone(),
        provider: final_message.provider.clone(),
        usage: final_message.usage.clone(),
    });

    final_message
}
