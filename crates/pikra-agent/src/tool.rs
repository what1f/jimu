use crate::context::now_ms;
use crate::{
    AfterToolCallCtx, AgentContextSnapshot, AgentEvent, AgentMessage, AgentMiddleware, AgentTool,
    AgentToolResult, BeforeToolCallCtx, ToolExecutionMode, ToolProgressCallback,
};
use futures_util::stream::{FuturesUnordered, StreamExt};
use pikra_ai::{
    AssistantMessage, BoxFuture, CancelToken, Message, ToolCall, ToolDefinition, ToolResultMessage,
};
use pikra_schema::CompiledSchema;
use serde_json::Value;
use std::sync::{Arc, Mutex};

type ClosureToolHandler = Arc<
    dyn Fn(
            String,
            Value,
            CancelToken,
            ToolProgressCallback,
        ) -> BoxFuture<'static, Result<AgentToolResult, String>>
        + Send
        + Sync,
>;

/// Ergonomic closure-backed [`AgentTool`].
#[derive(Clone)]
pub struct ClosureTool {
    definition: ToolDefinition,
    execution_mode: ToolExecutionMode,
    handler: ClosureToolHandler,
}

impl ClosureTool {
    pub fn new<F>(definition: ToolDefinition, handler: F) -> Self
    where
        F: Fn(
                String,
                Value,
                CancelToken,
                ToolProgressCallback,
            ) -> BoxFuture<'static, Result<AgentToolResult, String>>
            + Send
            + Sync
            + 'static,
    {
        Self {
            definition,
            execution_mode: ToolExecutionMode::Parallel,
            handler: Arc::new(handler),
        }
    }

    pub fn with_execution_mode(mut self, mode: ToolExecutionMode) -> Self {
        self.execution_mode = mode;
        self
    }
}

impl AgentTool for ClosureTool {
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
        (self.handler)(tool_call_id, args, cancel, on_update)
    }
}

pub(crate) struct ExecutedToolBatch {
    pub tool_results: Vec<ToolResultMessage>,
    pub terminate: bool,
}

struct FinalizedToolCall {
    tool_call: ToolCall,
    result: AgentToolResult,
}

enum PreparedToolCall {
    Immediate(AgentToolResult),
    Ready {
        tool: Arc<dyn AgentTool>,
        args: Value,
    },
}

fn find_tool<'a>(tools: &'a [Arc<dyn AgentTool>], name: &str) -> Option<&'a Arc<dyn AgentTool>> {
    tools.iter().find(|t| t.definition().name == name)
}

async fn prepare_tool_call(
    tools: &[Arc<dyn AgentTool>],
    middlewares: &[Arc<dyn AgentMiddleware>],
    assistant_message: &AssistantMessage,
    tool_call: &ToolCall,
    snapshot: &AgentContextSnapshot,
    cancel: &CancelToken,
) -> PreparedToolCall {
    if cancel.is_cancelled() {
        return PreparedToolCall::Immediate(AgentToolResult::error("Operation aborted"));
    }

    if let Some(arg_err) = &tool_call.arguments_error {
        return PreparedToolCall::Immediate(AgentToolResult::error(arg_err.clone()));
    }

    let Some(tool) = find_tool(tools, &tool_call.name).cloned() else {
        let available: Vec<String> = tools.iter().map(|t| t.definition().name).collect();
        let msg = if available.is_empty() {
            format!("Unknown tool: {} (no tools available)", tool_call.name)
        } else {
            format!(
                "Unknown tool: {}. Available tools: {}",
                tool_call.name,
                available.join(", ")
            )
        };
        return PreparedToolCall::Immediate(AgentToolResult::error(msg));
    };

    let compiled = match CompiledSchema::compile(tool.definition().parameters) {
        Ok(c) => c,
        Err(err) => {
            return PreparedToolCall::Immediate(AgentToolResult::error(format!(
                "Invalid schema for tool {}: {err}",
                tool_call.name
            )));
        }
    };

    let raw_value = Value::Object(tool_call.arguments.clone());
    let mut validated_args = match compiled.coerce_and_validate(&raw_value) {
        Ok(val) => val,
        Err(validation_errors) => {
            return PreparedToolCall::Immediate(AgentToolResult::error(
                validation_errors.to_string(),
            ));
        }
    };

    for mw in middlewares {
        let ctx = BeforeToolCallCtx {
            assistant_message,
            tool_call,
            args: &validated_args,
            snapshot,
        };
        if let Some(before_res) = mw.before_tool_call(ctx, cancel.clone()).await {
            if cancel.is_cancelled() {
                return PreparedToolCall::Immediate(AgentToolResult::error("Operation aborted"));
            }
            if let Some(override_args) = before_res.args_override {
                validated_args = override_args;
            }
            if before_res.block {
                let reason = before_res
                    .reason
                    .unwrap_or_else(|| "Tool execution was blocked".to_string());
                let mut err_result = AgentToolResult::error(reason);
                err_result.terminate = before_res.terminate;
                return PreparedToolCall::Immediate(err_result);
            }
        }
        if cancel.is_cancelled() {
            return PreparedToolCall::Immediate(AgentToolResult::error("Operation aborted"));
        }
    }

    PreparedToolCall::Ready {
        tool,
        args: validated_args,
    }
}

async fn apply_after_tool_call(
    middlewares: &[Arc<dyn AgentMiddleware>],
    assistant_message: &AssistantMessage,
    tool_call: &ToolCall,
    args: &Value,
    mut result: AgentToolResult,
    snapshot: &AgentContextSnapshot,
    cancel: &CancelToken,
) -> AgentToolResult {
    for mw in middlewares {
        let ctx = AfterToolCallCtx {
            assistant_message,
            tool_call,
            args,
            result: &result,
            snapshot,
        };
        if let Some(ov) = mw.after_tool_call(ctx, cancel.clone()).await {
            let content_replaced = ov.content.is_some();
            if let Some(content) = ov.content {
                result.content = content;
            }
            if let Some(structured) = ov.structured_content {
                result.structured_content = Some(structured);
            } else if content_replaced {
                result.structured_content = None;
            }
            if let Some(details) = ov.details {
                result.details = details;
            }
            if let Some(is_error) = ov.is_error {
                result.is_error = is_error;
            }
            if let Some(terminate) = ov.terminate {
                result.terminate = terminate;
            }
        }
    }
    result
}

fn make_scoped_progress_callback(
    tool_call_id: String,
    tool_name: String,
    args: Value,
    emit: Arc<dyn Fn(AgentEvent) + Send + Sync>,
) -> (Arc<Mutex<bool>>, ToolProgressCallback) {
    let gate = Arc::new(Mutex::new(true));
    let gate_clone = Arc::clone(&gate);
    let cb: ToolProgressCallback = Arc::new(move |partial_result| {
        let active = gate_clone.lock().unwrap_or_else(|e| e.into_inner());
        if !*active {
            return;
        }
        emit(AgentEvent::ToolExecutionUpdate {
            tool_call_id: tool_call_id.clone(),
            tool_name: tool_name.clone(),
            args: args.clone(),
            partial_result,
        });
    });
    (gate, cb)
}

fn close_progress_gate(gate: &Arc<Mutex<bool>>) {
    let mut active = gate.lock().unwrap_or_else(|e| e.into_inner());
    *active = false;
}

fn to_tool_result_message(finalized: &FinalizedToolCall) -> ToolResultMessage {
    ToolResultMessage {
        tool_call_id: finalized.tool_call.id.clone(),
        tool_name: finalized.tool_call.name.clone(),
        content: finalized.result.content.clone(),
        is_error: finalized.result.is_error,
        timestamp: now_ms(),
    }
}

fn emit_tool_execution_end(
    finalized: &FinalizedToolCall,
    emit: &(dyn Fn(AgentEvent) + Send + Sync),
) {
    emit(AgentEvent::ToolExecutionEnd {
        tool_call_id: finalized.tool_call.id.clone(),
        tool_name: finalized.tool_call.name.clone(),
        result: finalized.result.clone(),
        is_error: finalized.result.is_error,
    });
}

fn emit_tool_result_message(msg: &ToolResultMessage, emit: &(dyn Fn(AgentEvent) + Send + Sync)) {
    let agent_msg = AgentMessage::Llm(Message::ToolResult(msg.clone()));
    emit(AgentEvent::MessageStart {
        message: agent_msg.clone(),
    });
    emit(AgentEvent::MessageEnd { message: agent_msg });
}

fn should_terminate_batch(finalized: &[FinalizedToolCall]) -> bool {
    !finalized.is_empty() && finalized.iter().all(|f| f.result.terminate)
}

/// Fail every tool call from an assistant message truncated by `StopReason::Length`.
pub(crate) fn fail_truncated_tool_calls(
    tool_calls: &[ToolCall],
    emit: &(dyn Fn(AgentEvent) + Send + Sync),
) -> ExecutedToolBatch {
    let mut tool_results = Vec::with_capacity(tool_calls.len());
    for tc in tool_calls {
        emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tc.id.clone(),
            tool_name: tc.name.clone(),
            args: Value::Object(tc.arguments.clone()),
        });
        let finalized = FinalizedToolCall {
            tool_call: tc.clone(),
            result: AgentToolResult::error(format!(
                "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                tc.name
            )),
        };
        emit_tool_execution_end(&finalized, emit);
        let msg = to_tool_result_message(&finalized);
        emit_tool_result_message(&msg, emit);
        tool_results.push(msg);
    }
    ExecutedToolBatch {
        tool_results,
        terminate: false,
    }
}

/// Run a single tool call through validation, `before_tool_call`, execution, and `after_tool_call`
/// without emitting agent lifecycle events. Useful for tools invoking other tools programmatically.
pub async fn run_tool_call(
    tools: &[Arc<dyn AgentTool>],
    middlewares: &[Arc<dyn AgentMiddleware>],
    assistant_message: &AssistantMessage,
    tool_call: &ToolCall,
    snapshot: &AgentContextSnapshot,
    cancel: CancelToken,
    on_update: ToolProgressCallback,
) -> AgentToolResult {
    match prepare_tool_call(
        tools,
        middlewares,
        assistant_message,
        tool_call,
        snapshot,
        &cancel,
    )
    .await
    {
        PreparedToolCall::Immediate(res) => res,
        PreparedToolCall::Ready { tool, args } => {
            let raw_result = if cancel.is_cancelled() {
                AgentToolResult::error("Operation aborted")
            } else {
                match tool
                    .execute(
                        tool_call.id.clone(),
                        args.clone(),
                        cancel.clone(),
                        on_update,
                    )
                    .await
                {
                    Ok(r) => r,
                    Err(e) => AgentToolResult::error(e),
                }
            };
            apply_after_tool_call(
                middlewares,
                assistant_message,
                tool_call,
                &args,
                raw_result,
                snapshot,
                &cancel,
            )
            .await
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_tool_batch(
    tools: &[Arc<dyn AgentTool>],
    middlewares: &[Arc<dyn AgentMiddleware>],
    assistant_message: &AssistantMessage,
    tool_calls: &[ToolCall],
    snapshot: &AgentContextSnapshot,
    global_mode: ToolExecutionMode,
    cancel: &CancelToken,
    emit: Arc<dyn Fn(AgentEvent) + Send + Sync>,
) -> ExecutedToolBatch {
    let has_sequential_tool = tool_calls.iter().any(|tc| {
        find_tool(tools, &tc.name)
            .is_some_and(|t| t.execution_mode() == ToolExecutionMode::Sequential)
    });

    if global_mode == ToolExecutionMode::Sequential || has_sequential_tool {
        execute_tool_batch_sequential(
            tools,
            middlewares,
            assistant_message,
            tool_calls,
            snapshot,
            cancel,
            emit,
        )
        .await
    } else {
        execute_tool_batch_parallel(
            tools,
            middlewares,
            assistant_message,
            tool_calls,
            snapshot,
            cancel,
            emit,
        )
        .await
    }
}

async fn execute_tool_batch_sequential(
    tools: &[Arc<dyn AgentTool>],
    middlewares: &[Arc<dyn AgentMiddleware>],
    assistant_message: &AssistantMessage,
    tool_calls: &[ToolCall],
    snapshot: &AgentContextSnapshot,
    cancel: &CancelToken,
    emit: Arc<dyn Fn(AgentEvent) + Send + Sync>,
) -> ExecutedToolBatch {
    let mut finalized_calls = Vec::with_capacity(tool_calls.len());
    let mut tool_results = Vec::with_capacity(tool_calls.len());

    for tc in tool_calls {
        emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tc.id.clone(),
            tool_name: tc.name.clone(),
            args: Value::Object(tc.arguments.clone()),
        });

        let result =
            match prepare_tool_call(tools, middlewares, assistant_message, tc, snapshot, cancel)
                .await
            {
                PreparedToolCall::Immediate(res) => res,
                PreparedToolCall::Ready { tool, args } => {
                    let (gate, on_update) = make_scoped_progress_callback(
                        tc.id.clone(),
                        tc.name.clone(),
                        args.clone(),
                        Arc::clone(&emit),
                    );
                    let raw_result = if cancel.is_cancelled() {
                        AgentToolResult::error("Operation aborted")
                    } else {
                        match tool
                            .execute(tc.id.clone(), args.clone(), cancel.clone(), on_update)
                            .await
                        {
                            Ok(r) => r,
                            Err(e) => AgentToolResult::error(e),
                        }
                    };
                    close_progress_gate(&gate);
                    apply_after_tool_call(
                        middlewares,
                        assistant_message,
                        tc,
                        &args,
                        raw_result,
                        snapshot,
                        cancel,
                    )
                    .await
                }
            };

        let finalized = FinalizedToolCall {
            tool_call: tc.clone(),
            result,
        };
        emit_tool_execution_end(&finalized, emit.as_ref());
        let msg = to_tool_result_message(&finalized);
        emit_tool_result_message(&msg, emit.as_ref());
        tool_results.push(msg);
        finalized_calls.push(finalized);
    }

    ExecutedToolBatch {
        tool_results,
        terminate: should_terminate_batch(&finalized_calls),
    }
}

async fn execute_tool_batch_parallel(
    tools: &[Arc<dyn AgentTool>],
    middlewares: &[Arc<dyn AgentMiddleware>],
    assistant_message: &AssistantMessage,
    tool_calls: &[ToolCall],
    snapshot: &AgentContextSnapshot,
    cancel: &CancelToken,
    emit: Arc<dyn Fn(AgentEvent) + Send + Sync>,
) -> ExecutedToolBatch {
    let mut slots: Vec<Option<FinalizedToolCall>> = (0..tool_calls.len()).map(|_| None).collect();
    let mut in_flight: FuturesUnordered<BoxFuture<'_, (usize, FinalizedToolCall)>> =
        FuturesUnordered::new();

    for (idx, tc) in tool_calls.iter().enumerate() {
        emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tc.id.clone(),
            tool_name: tc.name.clone(),
            args: Value::Object(tc.arguments.clone()),
        });

        match prepare_tool_call(tools, middlewares, assistant_message, tc, snapshot, cancel).await {
            PreparedToolCall::Immediate(res) => {
                let finalized = FinalizedToolCall {
                    tool_call: tc.clone(),
                    result: res,
                };
                emit_tool_execution_end(&finalized, emit.as_ref());
                slots[idx] = Some(finalized);
            }
            PreparedToolCall::Ready { tool, args } => {
                let tc_clone = tc.clone();
                let cancel_clone = cancel.clone();
                let emit_clone = Arc::clone(&emit);
                in_flight.push(Box::pin(async move {
                    let (gate, on_update) = make_scoped_progress_callback(
                        tc_clone.id.clone(),
                        tc_clone.name.clone(),
                        args.clone(),
                        emit_clone,
                    );
                    let raw_result = if cancel_clone.is_cancelled() {
                        AgentToolResult::error("Operation aborted")
                    } else {
                        match tool
                            .execute(
                                tc_clone.id.clone(),
                                args.clone(),
                                cancel_clone.clone(),
                                on_update,
                            )
                            .await
                        {
                            Ok(r) => r,
                            Err(e) => AgentToolResult::error(e),
                        }
                    };
                    close_progress_gate(&gate);
                    let final_result = apply_after_tool_call(
                        middlewares,
                        assistant_message,
                        &tc_clone,
                        &args,
                        raw_result,
                        snapshot,
                        &cancel_clone,
                    )
                    .await;
                    (
                        idx,
                        FinalizedToolCall {
                            tool_call: tc_clone,
                            result: final_result,
                        },
                    )
                }));
            }
        }
    }

    // Drain every in-flight tool future to completion (even if cancelled),
    // emitting ToolExecutionEnd strictly in completion order.
    while let Some((idx, finalized)) = in_flight.next().await {
        emit_tool_execution_end(&finalized, emit.as_ref());
        slots[idx] = Some(finalized);
    }

    // Persist and emit ToolResultMessage artifacts strictly in assistant source order.
    let mut ordered_finalized = Vec::with_capacity(tool_calls.len());
    let mut tool_results = Vec::with_capacity(tool_calls.len());
    for finalized in slots.into_iter().flatten() {
        let msg = to_tool_result_message(&finalized);
        emit_tool_result_message(&msg, emit.as_ref());
        tool_results.push(msg);
        ordered_finalized.push(finalized);
    }

    ExecutedToolBatch {
        tool_results,
        terminate: should_terminate_batch(&ordered_finalized),
    }
}
