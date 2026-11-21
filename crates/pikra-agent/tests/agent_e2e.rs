use futures_channel::oneshot;
use futures_executor::{ThreadPool, block_on};
use futures_util::stream::{self, StreamExt};
use pikra_agent::{
    AfterToolCallCtx, AfterToolCallResult, Agent, AgentConfig, AgentContextSnapshot, AgentEvent,
    AgentMessage, AgentMiddleware, AgentTool, AgentToolResult, BeforeToolCallCtx,
    BeforeToolCallResult, ClosureTool, FinishTurnCtx, QueueMode, RequestOverride,
    ToolExecutionMode, TurnDecision,
};
use pikra_ai::{
    ApiKind, AssistantBlock, AssistantEventStream, AssistantMessage, AssistantMessageEvent,
    BoxFuture, CancelToken, ContentPart, Message, Model, ModelCost, SimpleStreamOptions,
    StopReason, StreamProvider, TextContent, ThinkingLevel, ToolCall, ToolDefinition,
    TranscriptContext, Usage,
};
use pikra_schema::{obj_schema, str_schema};
use serde_json::{Map, Value, json};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn test_model(id: &str) -> Model {
    Model {
        id: id.into(),
        name: id.into(),
        provider: "faux-provider".into(),
        api: ApiKind::OpenaiResponses,
        base_url: "https://example.invalid".into(),
        context_window: 128_000,
        max_output_tokens: 4_096,
        supports_images: true,
        supports_reasoning: true,
        cost: ModelCost::default(),
        headers: Vec::new(),
        compat: None,
    }
}

#[derive(Debug, Clone)]
struct RecordedRequest {
    model: Model,
    context: TranscriptContext,
    reasoning: Option<ThinkingLevel>,
    fast: Option<bool>,
}

type ScriptedResponseFn = Box<
    dyn FnOnce(&Model, &TranscriptContext, &SimpleStreamOptions) -> AssistantEventStream + Send,
>;

struct ScriptedProvider {
    responses: Mutex<VecDeque<ScriptedResponseFn>>,
    requests: Mutex<Vec<RecordedRequest>>,
}

impl ScriptedProvider {
    fn new(responses: Vec<ScriptedResponseFn>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl StreamProvider for ScriptedProvider {
    fn stream(
        &self,
        model: &Model,
        context: &TranscriptContext,
        options: SimpleStreamOptions,
    ) -> AssistantEventStream {
        self.requests.lock().unwrap().push(RecordedRequest {
            model: model.clone(),
            context: context.clone(),
            reasoning: options.reasoning,
            fast: options.fast,
        });
        let next = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("ScriptedProvider ran out of scripted responses");
        next(model, context, &options)
    }
}

fn assistant_stream_from_blocks(
    model: &Model,
    blocks: Vec<AssistantBlock>,
    stop_reason: StopReason,
    usage: Usage,
    error_message: Option<String>,
) -> AssistantEventStream {
    let partial = AssistantMessage {
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_id: None,
        thinking_level: None,
        usage: Usage::default(),
        stop_reason: StopReason::Pending,
        error_message: None,
        diagnostics: Vec::new(),
        timestamp: 1,
        duration_ms: None,
    };

    let mut events = vec![AssistantMessageEvent::Start { partial }];
    for (idx, block) in blocks.iter().enumerate() {
        match block {
            AssistantBlock::Text(tc) => {
                events.push(AssistantMessageEvent::TextStart { content_index: idx });
                events.push(AssistantMessageEvent::TextDelta {
                    content_index: idx,
                    delta: tc.text.clone(),
                });
                events.push(AssistantMessageEvent::TextEnd {
                    content_index: idx,
                    text: tc.text.clone(),
                });
            }
            AssistantBlock::ToolCall(tc) => {
                events.push(AssistantMessageEvent::ToolCallStart {
                    content_index: idx,
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                });
                events.push(AssistantMessageEvent::ToolCallEnd {
                    content_index: idx,
                    tool_call: tc.clone(),
                });
            }
            AssistantBlock::Thinking(th) => {
                events.push(AssistantMessageEvent::ThinkingStart { content_index: idx });
                events.push(AssistantMessageEvent::ThinkingDelta {
                    content_index: idx,
                    delta: th.thinking.clone(),
                });
                events.push(AssistantMessageEvent::ThinkingEnd {
                    content_index: idx,
                    thinking: th.thinking.clone(),
                });
            }
        }
    }

    let final_msg = AssistantMessage {
        content: blocks,
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_id: Some("resp_1".into()),
        thinking_level: None,
        usage,
        stop_reason,
        error_message,
        diagnostics: Vec::new(),
        timestamp: 2,
        duration_ms: Some(10),
    };

    if matches!(stop_reason, StopReason::Error | StopReason::Aborted) {
        events.push(AssistantMessageEvent::Error {
            reason: stop_reason,
            error: final_msg,
        });
    } else {
        events.push(AssistantMessageEvent::Done {
            reason: stop_reason,
            message: final_msg,
        });
    }

    Box::pin(stream::iter(events))
}

fn make_tool_call(id: &str, name: &str, args: Value, arguments_error: Option<&str>) -> ToolCall {
    let map = match args {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: map,
        item_id: None,
        thought_signature: None,
        namespace: None,
        arguments_error: arguments_error.map(str::to_string),
    }
}

fn first_text(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text(t) => Some(t.text.as_str()),
            ContentPart::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

#[test]
fn parallel_tools_out_of_order_completion_and_source_order_persistence() {
    block_on(async {
        let (release_a_tx, release_a_rx) = oneshot::channel::<()>();
        let release_a_tx = Arc::new(Mutex::new(Some(release_a_tx)));
        let release_a_rx = Arc::new(Mutex::new(Some(release_a_rx)));

        let slow_a = Arc::new(ClosureTool::new(
            ToolDefinition {
                name: "slow_a".into(),
                description: "waits for fast_b to finish".into(),
                parameters: obj_schema([("tag", str_schema(), true)]),
            },
            move |_id, args, _cancel, _on_update| {
                let rx_slot = Arc::clone(&release_a_rx);
                Box::pin(async move {
                    let rx = rx_slot.lock().unwrap().take().unwrap();
                    let _ = rx.await;
                    Ok(AgentToolResult::text(format!(
                        "A:{}",
                        args["tag"].as_str().unwrap()
                    )))
                })
            },
        ));

        let fast_b = Arc::new(ClosureTool::new(
            ToolDefinition {
                name: "fast_b".into(),
                description: "finishes first and unblocks slow_a".into(),
                parameters: obj_schema([("tag", str_schema(), true)]),
            },
            move |_id, args, _cancel, on_update| {
                let tx_slot = Arc::clone(&release_a_tx);
                Box::pin(async move {
                    on_update(AgentToolResult::text("B:progress"));
                    if let Some(tx) = tx_slot.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                    Ok(AgentToolResult::text(format!(
                        "B:{}",
                        args["tag"].as_str().unwrap()
                    )))
                })
            },
        ));

        let usage1 = Usage {
            input: 100,
            output: 25,
            total_tokens: 125,
            ..Usage::default()
        };
        let usage2 = Usage {
            input: 160,
            output: 15,
            total_tokens: 175,
            ..Usage::default()
        };

        let provider = ScriptedProvider::new(vec![
            Box::new(move |model, _ctx, _opts| {
                assistant_stream_from_blocks(
                    model,
                    vec![
                        AssistantBlock::ToolCall(make_tool_call(
                            "call_a",
                            "slow_a",
                            json!({"tag": "first"}),
                            None,
                        )),
                        AssistantBlock::ToolCall(make_tool_call(
                            "call_b",
                            "fast_b",
                            json!({"tag": "second"}),
                            None,
                        )),
                        AssistantBlock::ToolCall(make_tool_call(
                            "call_bad",
                            "slow_a",
                            json!({}),
                            Some("malformed JSON args"),
                        )),
                        AssistantBlock::ToolCall(make_tool_call(
                            "call_missing",
                            "nope_tool",
                            json!({}),
                            None,
                        )),
                    ],
                    StopReason::ToolUse,
                    usage1,
                    None,
                )
            }),
            Box::new(move |model, _ctx, _opts| {
                assistant_stream_from_blocks(
                    model,
                    vec![AssistantBlock::Text(TextContent::plain(
                        "All tools settled.",
                    ))],
                    StopReason::Stop,
                    usage2,
                    None,
                )
            }),
        ]);

        let model = test_model("gpt-test");
        let agent = Agent::new(
            provider.clone(),
            AgentConfig::new(model)
                .with_system_prompt("Base stable system prompt")
                .with_tools(vec![slow_a, fast_b])
                .with_tool_execution_mode(ToolExecutionMode::Parallel),
        );

        let mut stream = agent.prompt_stream("run parallel tools").unwrap();
        let mut events = Vec::new();
        while let Some(ev) = stream.next().await {
            events.push(ev);
        }

        // Verify ToolExecutionStart order matches assistant source order
        let start_ids: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolExecutionStart { tool_call_id, .. } => Some(tool_call_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            start_ids,
            vec!["call_a", "call_b", "call_bad", "call_missing"]
        );

        // Verify ToolExecutionEnd fires out-of-order: immediate errors during preflight,
        // then fast_b completes strictly before slow_a!
        let end_ids: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolExecutionEnd { tool_call_id, .. } => Some(tool_call_id.as_str()),
                _ => None,
            })
            .collect();
        let pos_b = end_ids.iter().position(|&id| id == "call_b").unwrap();
        let pos_a = end_ids.iter().position(|&id| id == "call_a").unwrap();
        assert!(
            pos_b < pos_a,
            "expected call_b to finish before call_a, got {end_ids:?}"
        );

        // Verify ToolExecutionUpdate for call_b arrived before ToolExecutionEnd for call_b
        let update_pos = events
            .iter()
            .position(|e| matches!(e, AgentEvent::ToolExecutionUpdate { tool_call_id, .. } if tool_call_id == "call_b"))
            .unwrap();
        let end_b_pos = events
            .iter()
            .position(|e| matches!(e, AgentEvent::ToolExecutionEnd { tool_call_id, .. } if tool_call_id == "call_b"))
            .unwrap();
        assert!(update_pos < end_b_pos);

        // Verify ToolResultMessage MessageEnd events are strictly in assistant source order
        let emitted_tr_ids: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::MessageEnd {
                    message: AgentMessage::Llm(Message::ToolResult(tr)),
                } => Some(tr.tool_call_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            emitted_tr_ids,
            vec!["call_a", "call_b", "call_bad", "call_missing"]
        );

        // Verify TurnEnd.tool_results preserves source order & error details
        let turn1_results = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::TurnEnd { tool_results, .. } if !tool_results.is_empty() => {
                    Some(tool_results)
                }
                _ => None,
            })
            .unwrap();
        let turn1_ids: Vec<&str> = turn1_results
            .iter()
            .map(|tr| tr.tool_call_id.as_str())
            .collect();
        assert_eq!(
            turn1_ids,
            vec!["call_a", "call_b", "call_bad", "call_missing"]
        );
        assert_eq!(first_text(&turn1_results[0].content), "A:first");
        assert_eq!(first_text(&turn1_results[1].content), "B:second");
        assert!(turn1_results[2].is_error);
        assert!(first_text(&turn1_results[2].content).contains("malformed JSON args"));
        assert!(turn1_results[3].is_error);
        let unknown_text = first_text(&turn1_results[3].content);
        assert!(unknown_text.contains("Unknown tool: nope_tool"));
        assert!(unknown_text.contains("slow_a"));
        assert!(unknown_text.contains("fast_b"));

        // Verify transcript in Agent state preserves exact source order
        let transcript_tr_ids: Vec<String> = agent
            .messages()
            .into_iter()
            .filter_map(|m| match m {
                AgentMessage::Llm(Message::ToolResult(tr)) => Some(tr.tool_call_id),
                _ => None,
            })
            .collect();
        assert_eq!(
            transcript_tr_ids,
            vec!["call_a", "call_b", "call_bad", "call_missing"]
        );

        // Verify RequestUsage emitted once per LLM request (2 requests)
        let usages: Vec<(String, String, u64)> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::RequestUsage {
                    model,
                    provider,
                    usage,
                } => Some((model.clone(), provider.clone(), usage.total_tokens)),
                _ => None,
            })
            .collect();
        assert_eq!(
            usages,
            vec![
                ("gpt-test".into(), "faux-provider".into(), 125),
                ("gpt-test".into(), "faux-provider".into(), 175),
            ]
        );

        // Verify stable leading SystemMessage across both LLM calls
        let reqs = provider.requests();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].context.messages[0], reqs[1].context.messages[0]);
        match &reqs[0].context.messages[0] {
            Message::System(sys) => {
                assert_eq!(sys.content, "Base stable system prompt");
                let names: Vec<&str> = sys.tools_added.iter().map(|t| t.name.as_str()).collect();
                assert_eq!(names, vec!["slow_a", "fast_b"]);
            }
            other => panic!("expected leading Message::System, got {other:?}"),
        }
    });
}

struct FullTestMiddleware {
    barrier_ok: Arc<AtomicBool>,
    finish_calls: Arc<AtomicUsize>,
    enable_terminate: Arc<AtomicBool>,
}

impl AgentMiddleware for FullTestMiddleware {
    fn prepare_request<'a>(
        &'a self,
        snapshot: &'a AgentContextSnapshot,
        _cancel: CancelToken,
    ) -> BoxFuture<'a, Option<RequestOverride>> {
        let next_prompt = format!("{} [mw]", snapshot.system_prompt);
        Box::pin(async move {
            Some(RequestOverride {
                system_prompt: Some(next_prompt),
                thinking_level: Some(ThinkingLevel::High),
                fast: Some(true),
                ..RequestOverride::default()
            })
        })
    }

    fn transform_context<'a>(
        &'a self,
        mut messages: Vec<AgentMessage>,
        _cancel: CancelToken,
    ) -> BoxFuture<'a, Vec<AgentMessage>> {
        // Inject one LLM-visible custom message and one UI-only custom message
        messages.push(AgentMessage::Custom {
            custom_type: "hint".into(),
            id: Some("c_visible".into()),
            content: vec![ContentPart::Text(TextContent::plain("visible-custom"))],
            details: Value::Null,
            include_in_llm: true,
            display: true,
            timestamp: 10,
        });
        messages.push(AgentMessage::Custom {
            custom_type: "ui_toast".into(),
            id: Some("c_hidden".into()),
            content: vec![ContentPart::Text(TextContent::plain("hidden-custom"))],
            details: Value::Null,
            include_in_llm: false,
            display: true,
            timestamp: 11,
        });
        Box::pin(async move { messages })
    }

    fn before_tool_call<'a>(
        &'a self,
        ctx: BeforeToolCallCtx<'a>,
        _cancel: CancelToken,
    ) -> BoxFuture<'a, Option<BeforeToolCallResult>> {
        // Barrier rule: snapshot.messages MUST already end with the requesting AssistantMessage
        let last_matches = matches!(
            ctx.snapshot.messages.last(),
            Some(AgentMessage::Llm(Message::Assistant(a))) if a == ctx.assistant_message
        );
        if last_matches {
            self.barrier_ok.store(true, Ordering::SeqCst);
        }

        let tool_name = ctx.tool_call.name.clone();
        Box::pin(async move {
            if tool_name == "blocked_tool" {
                return Some(BeforeToolCallResult {
                    block: true,
                    reason: Some("blocked by policy".into()),
                    terminate: false,
                    args_override: None,
                });
            }
            if tool_name == "echo_tool" {
                return Some(BeforeToolCallResult {
                    block: false,
                    reason: None,
                    terminate: false,
                    args_override: Some(json!({ "text": "overridden-arg" })),
                });
            }
            None
        })
    }

    fn after_tool_call<'a>(
        &'a self,
        ctx: AfterToolCallCtx<'a>,
        _cancel: CancelToken,
    ) -> BoxFuture<'a, Option<AfterToolCallResult>> {
        let tool_name = ctx.tool_call.name.clone();
        let should_terminate = self.enable_terminate.load(Ordering::SeqCst);
        Box::pin(async move {
            if tool_name == "echo_tool" {
                return Some(AfterToolCallResult {
                    content: Some(vec![ContentPart::Text(TextContent::plain("[REDACTED]"))]),
                    details: None,
                    structured_content: None,
                    is_error: None,
                    terminate: Some(should_terminate),
                });
            }
            None
        })
    }

    fn finish_turn<'a>(
        &'a self,
        ctx: FinishTurnCtx<'a>,
        _cancel: CancelToken,
    ) -> BoxFuture<'a, Option<TurnDecision>> {
        let call_num = self.finish_calls.fetch_add(1, Ordering::SeqCst);
        let no_tools = ctx.tool_results.is_empty();
        Box::pin(async move {
            // On the first text-only turn, force one Continue; after that let it end naturally.
            if no_tools && call_num == 1 {
                Some(TurnDecision::Continue)
            } else {
                None
            }
        })
    }
}

#[test]
fn middleware_chain_overrides_blocking_redaction_terminate_and_finish_turn() {
    block_on(async {
        let blocked_executed = Arc::new(AtomicBool::new(false));
        let blocked_flag = Arc::clone(&blocked_executed);
        let echo_received = Arc::new(Mutex::new(String::new()));
        let echo_slot = Arc::clone(&echo_received);

        let blocked_tool = Arc::new(ClosureTool::new(
            ToolDefinition {
                name: "blocked_tool".into(),
                description: "should never run".into(),
                parameters: obj_schema([("x", str_schema(), false)]),
            },
            move |_id, _args, _cancel, _upd| {
                let flag = Arc::clone(&blocked_flag);
                Box::pin(async move {
                    flag.store(true, Ordering::SeqCst);
                    Ok(AgentToolResult::text("should not happen"))
                })
            },
        ));

        let echo_tool = Arc::new(ClosureTool::new(
            ToolDefinition {
                name: "echo_tool".into(),
                description: "echoes text".into(),
                parameters: obj_schema([("text", str_schema(), true)]),
            },
            move |_id, args, _cancel, _upd| {
                let slot = Arc::clone(&echo_slot);
                Box::pin(async move {
                    let s = args["text"].as_str().unwrap().to_string();
                    *slot.lock().unwrap() = s.clone();
                    Ok(AgentToolResult::text(format!("SECRET:{s}")))
                })
            },
        ));

        let barrier_ok = Arc::new(AtomicBool::new(false));
        let finish_calls = Arc::new(AtomicUsize::new(0));
        let enable_terminate = Arc::new(AtomicBool::new(false));

        let mw = Arc::new(FullTestMiddleware {
            barrier_ok: Arc::clone(&barrier_ok),
            finish_calls: Arc::clone(&finish_calls),
            enable_terminate: Arc::clone(&enable_terminate),
        });

        let provider = ScriptedProvider::new(vec![
            // Turn 1: call blocked_tool + echo_tool
            Box::new(|model, _ctx, _opts| {
                assistant_stream_from_blocks(
                    model,
                    vec![
                        AssistantBlock::ToolCall(make_tool_call(
                            "c1",
                            "blocked_tool",
                            json!({}),
                            None,
                        )),
                        AssistantBlock::ToolCall(make_tool_call(
                            "c2",
                            "echo_tool",
                            json!({"text": "raw"}),
                            None,
                        )),
                    ],
                    StopReason::ToolUse,
                    Usage::default(),
                    None,
                )
            }),
            // Turn 2: normal text response -> finish_turn returns TurnDecision::Continue
            Box::new(|model, _ctx, _opts| {
                assistant_stream_from_blocks(
                    model,
                    vec![AssistantBlock::Text(TextContent::plain(
                        "first normal reply",
                    ))],
                    StopReason::Stop,
                    Usage::default(),
                    None,
                )
            }),
            // Turn 3: forced continuation turn
            Box::new(|model, _ctx, _opts| {
                assistant_stream_from_blocks(
                    model,
                    vec![AssistantBlock::Text(TextContent::plain("continued reply"))],
                    StopReason::Stop,
                    Usage::default(),
                    None,
                )
            }),
            // Run 2 (early termination): single echo_tool with after_tool_call setting terminate=true
            Box::new(|model, _ctx, _opts| {
                assistant_stream_from_blocks(
                    model,
                    vec![AssistantBlock::ToolCall(make_tool_call(
                        "c3",
                        "echo_tool",
                        json!({"text": "final"}),
                        None,
                    ))],
                    StopReason::ToolUse,
                    Usage::default(),
                    None,
                )
            }),
        ]);

        let agent = Agent::new(
            provider.clone(),
            AgentConfig::new(test_model("mw-model"))
                .with_system_prompt("sys")
                .with_tools(vec![blocked_tool, echo_tool])
                .with_middleware(mw),
        );

        let run1_msgs = agent.prompt("test middleware").await.unwrap();
        assert!(
            barrier_ok.load(Ordering::SeqCst),
            "before_tool_call must see AssistantMessage"
        );
        assert!(
            !blocked_executed.load(Ordering::SeqCst),
            "blocked tool must not execute"
        );
        assert_eq!(*echo_received.lock().unwrap(), "overridden-arg");

        // Verify 3 LLM requests were made (Turn 1 tools -> Turn 2 reply -> Turn 3 forced Continue)
        let reqs = provider.requests();
        assert_eq!(reqs.len(), 3);
        assert_eq!(reqs[0].reasoning, Some(ThinkingLevel::High));
        assert_eq!(reqs[0].fast, Some(true));
        match &reqs[0].context.messages[0] {
            Message::System(sys) => assert_eq!(sys.content, "sys [mw]"),
            other => panic!("expected system message, got {other:?}"),
        }
        // Custom message with include_in_llm=true present, include_in_llm=false excluded
        let all_user_texts: Vec<String> = reqs[0]
            .context
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::User(u) => Some(first_text(&u.content)),
                _ => None,
            })
            .collect();
        assert!(all_user_texts.contains(&"visible-custom".to_string()));
        assert!(!all_user_texts.contains(&"hidden-custom".to_string()));

        // Verify redacted tool result & blocked tool result in transcript
        let tool_results: Vec<_> = run1_msgs
            .iter()
            .filter_map(|m| match m {
                AgentMessage::Llm(Message::ToolResult(tr)) => Some(tr),
                _ => None,
            })
            .collect();
        assert_eq!(tool_results.len(), 2);
        assert!(tool_results[0].is_error);
        assert_eq!(first_text(&tool_results[0].content), "blocked by policy");
        assert!(!tool_results[1].is_error);
        assert_eq!(first_text(&tool_results[1].content), "[REDACTED]");

        // Now test early termination when ALL tool results in batch set terminate=true
        enable_terminate.store(true, Ordering::SeqCst);
        let _ = agent.prompt("trigger early terminate").await.unwrap();
        // Should have made only 1 additional LLM request (total 4), skipping the follow-up turn!
        assert_eq!(provider.requests().len(), 4);
    });
}

#[test]
fn mid_turn_steer_and_abort_waits_for_tool_teardown_without_spurious_busy() {
    let pool = ThreadPool::new().unwrap();

    block_on(async {
        let (step1_entered_tx, step1_entered_rx) = oneshot::channel::<()>();
        let (step1_release_tx, step1_release_rx) = oneshot::channel::<()>();
        let step1_entered_tx = Arc::new(Mutex::new(Some(step1_entered_tx)));
        let step1_release_rx = Arc::new(Mutex::new(Some(step1_release_rx)));

        let (barrier_entered_tx, barrier_entered_rx) = oneshot::channel::<()>();
        let (cancel_observed_tx, cancel_observed_rx) = oneshot::channel::<()>();
        let (teardown_allow_tx, teardown_allow_rx) = oneshot::channel::<()>();
        let barrier_entered_tx = Arc::new(Mutex::new(Some(barrier_entered_tx)));
        let cancel_observed_tx = Arc::new(Mutex::new(Some(cancel_observed_tx)));
        let teardown_allow_rx = Arc::new(Mutex::new(Some(teardown_allow_rx)));
        let teardown_completed = Arc::new(AtomicBool::new(false));
        let teardown_flag = Arc::clone(&teardown_completed);

        let step_one_tool = Arc::new(ClosureTool::new(
            ToolDefinition {
                name: "step_one".into(),
                description: "pauses so caller can queue a mid-turn steer".into(),
                parameters: obj_schema(Vec::<(&str, Value, bool)>::new()),
            },
            move |_id, _args, _cancel, _upd| {
                let entered = Arc::clone(&step1_entered_tx);
                let release = Arc::clone(&step1_release_rx);
                Box::pin(async move {
                    if let Some(tx) = entered.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                    let rx = release.lock().unwrap().take().unwrap();
                    let _ = rx.await;
                    Ok(AgentToolResult::text("step_one_done"))
                })
            },
        ));

        let barrier_tool = Arc::new(ClosureTool::new(
            ToolDefinition {
                name: "barrier_tool".into(),
                description: "waits for cancel and teardown barrier".into(),
                parameters: obj_schema(Vec::<(&str, Value, bool)>::new()),
            },
            move |_id, _args, cancel, _upd| {
                let entered = Arc::clone(&barrier_entered_tx);
                let cancel_seen = Arc::clone(&cancel_observed_tx);
                let teardown_rx = Arc::clone(&teardown_allow_rx);
                let done_flag = Arc::clone(&teardown_flag);
                Box::pin(async move {
                    if let Some(tx) = entered.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                    cancel.cancelled().await;
                    if let Some(tx) = cancel_seen.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                    // Simulate async cleanup that MUST complete before abort().await resolves
                    let rx = teardown_rx.lock().unwrap().take().unwrap();
                    let _ = rx.await;
                    done_flag.store(true, Ordering::SeqCst);
                    Err("barrier_tool aborted cleanly".into())
                })
            },
        ));

        let provider = ScriptedProvider::new(vec![
            // Turn 1: invoke step_one
            Box::new(|model, _ctx, _opts| {
                assistant_stream_from_blocks(
                    model,
                    vec![AssistantBlock::ToolCall(make_tool_call(
                        "t1",
                        "step_one",
                        json!({}),
                        None,
                    ))],
                    StopReason::ToolUse,
                    Usage::default(),
                    None,
                )
            }),
            // Turn 2 (after steer "s1"): invoke barrier_tool
            Box::new(|model, _ctx, _opts| {
                assistant_stream_from_blocks(
                    model,
                    vec![AssistantBlock::ToolCall(make_tool_call(
                        "t2",
                        "barrier_tool",
                        json!({}),
                        None,
                    ))],
                    StopReason::ToolUse,
                    Usage::default(),
                    None,
                )
            }),
            // Run 2 (immediately after abort().await): normal response
            Box::new(|model, _ctx, _opts| {
                assistant_stream_from_blocks(
                    model,
                    vec![AssistantBlock::Text(TextContent::plain("fresh run ok"))],
                    StopReason::Stop,
                    Usage::default(),
                    None,
                )
            }),
        ]);

        let agent = Arc::new(Agent::new(
            provider.clone(),
            AgentConfig::new(test_model("model-v1"))
                .with_system_prompt("steer-abort-test")
                .with_tools(vec![step_one_tool, barrier_tool])
                .with_steering_mode(QueueMode::OneAtATime),
        ));

        let recorded_events = Arc::new(Mutex::new(Vec::<AgentEvent>::new()));
        let ev_sink = Arc::clone(&recorded_events);
        let _sub = agent.subscribe(move |ev| {
            ev_sink.lock().unwrap().push(ev.clone());
        });

        let agent_for_run = Arc::clone(&agent);
        let (run_done_tx, run_done_rx) = oneshot::channel();
        pool.spawn_ok(async move {
            let res = agent_for_run.prompt("initial prompt").await;
            let _ = run_done_tx.send(res);
        });

        // 1. Wait until step_one is in-flight, queue mid-turn steer "s1", then release step_one
        step1_entered_rx.await.unwrap();
        agent.steer("s1", "redirect to barrier");
        step1_release_tx.send(()).unwrap();

        // 2. Wait until Turn 2's barrier_tool is in-flight
        barrier_entered_rx.await.unwrap();

        // Queue an unread steer "s2" that MUST be dropped on abort (never emitted as Steered!)
        agent.steer("s2", "unread steer must be discarded");

        // Trigger abort() concurrently and verify it waits for barrier_tool teardown
        let agent_for_abort = Arc::clone(&agent);
        let (abort_done_tx, abort_done_rx) = oneshot::channel();
        pool.spawn_ok(async move {
            agent_for_abort.abort().await;
            let _ = abort_done_tx.send(());
        });

        // Wait until barrier_tool observes cancellation while still tearing down
        cancel_observed_rx.await.unwrap();
        assert!(
            agent.is_running(),
            "agent must remain running until tool teardown completes"
        );
        assert!(!teardown_completed.load(Ordering::SeqCst));

        // Allow barrier_tool teardown to complete
        teardown_allow_tx.send(()).unwrap();

        // Wait for abort().await to resolve
        abort_done_rx.await.unwrap();

        // Immediately after abort().await:
        // 1) In-flight tool teardown MUST have completed
        assert!(
            teardown_completed.load(Ordering::SeqCst),
            "abort().await must wait for in-flight tool teardown"
        );
        // 2) Active-run lock MUST be cleared -> immediate state mutations & next prompt NEVER return Busy!
        agent
            .set_model(test_model("model-v2"))
            .expect("set_model must not fail with Busy");
        agent
            .set_thinking_level(ThinkingLevel::Medium)
            .expect("set_thinking_level must not fail with Busy");
        agent
            .set_fast(true)
            .expect("set_fast must not fail with Busy");

        let _ = run_done_rx.await.unwrap().unwrap();

        let next_msgs = agent
            .prompt("fresh turn after abort")
            .await
            .expect("next prompt immediately after abort().await must not fail with Busy");
        assert!(next_msgs.iter().any(|m| matches!(
            m,
            AgentMessage::Llm(Message::Assistant(a)) if a.text_output() == "fresh run ok"
        )));

        // Verify Steered event emitted for "s1" before its MessageStart, and NEVER for "s2"
        let events = recorded_events.lock().unwrap().clone();
        let steered_ids: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Steered { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(steered_ids, vec!["s1".to_string()]);

        let s1_steered_pos = events
            .iter()
            .position(|e| matches!(e, AgentEvent::Steered { id, .. } if id == "s1"))
            .unwrap();
        let s1_msg_start_pos = events
            .iter()
            .position(|e| {
                matches!(
                    e,
                    AgentEvent::MessageStart { message: AgentMessage::Llm(Message::User(u)) }
                    if u.id.as_deref() == Some("s1")
                )
            })
            .unwrap();
        assert!(s1_steered_pos < s1_msg_start_pos);

        // Verify Run 2 LLM request used updated model-v2 and did NOT contain discarded steer "s2"
        let reqs = provider.requests();
        assert_eq!(reqs.len(), 3);
        assert_eq!(reqs[2].model.id, "model-v2");
        assert_eq!(reqs[2].reasoning, Some(ThinkingLevel::Medium));
        assert_eq!(reqs[2].fast, Some(true));
        let req2_texts: Vec<String> = reqs[2]
            .context
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::User(u) => Some(first_text(&u.content)),
                _ => None,
            })
            .collect();
        assert!(req2_texts.contains(&"redirect to barrier".to_string()));
        assert!(!req2_texts.contains(&"unread steer must be discarded".to_string()));
    });
}

#[test]
fn first_turn_failure_rollback_reports_user_message_kept_accurately() {
    block_on(async {
        let provider = ScriptedProvider::new(vec![Box::new(|model, _ctx, _opts| {
            assistant_stream_from_blocks(
                model,
                Vec::new(),
                StopReason::Error,
                Usage::default(),
                Some("upstream 500".into()),
            )
        })]);

        let agent = Agent::new(
            provider,
            AgentConfig::new(test_model("err-model")).with_rollback_first_turn_on_failure(true),
        );

        let mut stream = agent.prompt_stream("will fail on turn 1").unwrap();
        let mut events = Vec::new();
        while let Some(ev) = stream.next().await {
            events.push(ev);
        }

        let agent_end = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::AgentEnd {
                    messages,
                    user_message_kept,
                } => Some((messages.len(), *user_message_kept)),
                _ => None,
            })
            .unwrap();
        assert_eq!(agent_end, (0, false));
        assert!(agent.messages().is_empty());
    });
}

#[test]
fn unpolled_and_paused_streams_settle_via_abort_and_wait_for_idle_without_busy() {
    let pool = ThreadPool::new().unwrap();

    block_on(async {
        // 1. Holding an unpolled prompt_stream: abort().await settles it cleanly and unlocks next prompt.
        let provider = ScriptedProvider::new(vec![
            Box::new(|model, _ctx, _opts| {
                assistant_stream_from_blocks(
                    model,
                    vec![AssistantBlock::Text(TextContent::plain("after abort"))],
                    StopReason::Stop,
                    Usage::default(),
                    None,
                )
            }),
            Box::new(|model, _ctx, _opts| {
                assistant_stream_from_blocks(
                    model,
                    vec![AssistantBlock::Text(TextContent::plain("driven by wait"))],
                    StopReason::Stop,
                    Usage::default(),
                    None,
                )
            }),
        ]);
        let agent = Agent::new(provider, AgentConfig::new(test_model("stream-model")));

        let held_stream = agent.prompt_stream("unpolled abort").unwrap();
        assert!(agent.is_running());
        agent.abort().await;
        assert!(!agent.is_running());

        // Even while `held_stream` is STILL alive, a new prompt succeeds without Busy!
        let next = agent.prompt("fresh prompt").await.unwrap();
        assert!(next.iter().any(|m| matches!(
            m,
            AgentMessage::Llm(Message::Assistant(a)) if a.text_output() == "after abort"
        )));
        drop(held_stream);

        // 2. Holding an unpolled prompt_stream: wait_for_idle().await actively drives it to completion.
        let mut held_idle_stream = agent.prompt_stream("unpolled wait").unwrap();
        agent.wait_for_idle().await;
        assert!(!agent.is_running());
        let mut saw_end = false;
        while let Some(ev) = held_idle_stream.next().await {
            if matches!(ev, AgentEvent::AgentEnd { .. }) {
                saw_end = true;
            }
        }
        assert!(saw_end);

        // 3. Paused stream mid-tool teardown: abort().await drives tool teardown even while stream is unpolled!
        let (entered_tx, entered_rx) = oneshot::channel::<()>();
        let (teardown_tx, teardown_rx) = oneshot::channel::<()>();
        let entered_tx = Arc::new(Mutex::new(Some(entered_tx)));
        let teardown_rx = Arc::new(Mutex::new(Some(teardown_rx)));
        let teardown_done = Arc::new(AtomicBool::new(false));
        let teardown_done_flag = Arc::clone(&teardown_done);

        let blocking_tool = Arc::new(ClosureTool::new(
            ToolDefinition {
                name: "hold".into(),
                description: "blocks until cancel + teardown".into(),
                parameters: obj_schema(Vec::<(&str, Value, bool)>::new()),
            },
            move |_id, _args, cancel, _upd| {
                let entered = Arc::clone(&entered_tx);
                let td_rx = Arc::clone(&teardown_rx);
                let flag = Arc::clone(&teardown_done_flag);
                Box::pin(async move {
                    if let Some(tx) = entered.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                    cancel.cancelled().await;
                    let rx = td_rx.lock().unwrap().take().unwrap();
                    let _ = rx.await;
                    flag.store(true, Ordering::SeqCst);
                    Err("aborted".into())
                })
            },
        ));

        let paused_provider = ScriptedProvider::new(vec![
            Box::new(|model, _ctx, _opts| {
                assistant_stream_from_blocks(
                    model,
                    vec![AssistantBlock::ToolCall(make_tool_call(
                        "c_hold",
                        "hold",
                        json!({}),
                        None,
                    ))],
                    StopReason::ToolUse,
                    Usage::default(),
                    None,
                )
            }),
            Box::new(|model, _ctx, _opts| {
                assistant_stream_from_blocks(
                    model,
                    vec![AssistantBlock::Text(TextContent::plain("recovered"))],
                    StopReason::Stop,
                    Usage::default(),
                    None,
                )
            }),
        ]);
        let paused_agent = Arc::new(Agent::new(
            paused_provider,
            AgentConfig::new(test_model("paused-model")).with_tools(vec![blocking_tool]),
        ));

        let mut paused_stream = paused_agent.prompt_stream("start tool").unwrap();
        // Poll until ToolExecutionStart, then stop polling while holding `paused_stream`.
        while let Some(ev) = paused_stream.next().await {
            if matches!(ev, AgentEvent::ToolExecutionStart { .. }) {
                break;
            }
        }
        // Drive one more poll step concurrently with `abort().await` via pool so tool enters and aborts.
        let agent_for_abort = Arc::clone(&paused_agent);
        let (abort_done_tx, abort_done_rx) = oneshot::channel::<()>();
        pool.spawn_ok(async move {
            agent_for_abort.abort().await;
            let _ = abort_done_tx.send(());
        });
        entered_rx.await.unwrap();
        teardown_tx.send(()).unwrap();
        abort_done_rx.await.unwrap();

        assert!(teardown_done.load(Ordering::SeqCst));
        assert!(!paused_agent.is_running());
        let recovered = paused_agent.prompt("after paused abort").await.unwrap();
        assert!(recovered.iter().any(|m| matches!(
            m,
            AgentMessage::Llm(Message::Assistant(a)) if a.text_output() == "recovered"
        )));
        drop(paused_stream);
    });
}

#[test]
fn agent_end_subscriber_sees_idle_state_and_can_start_next_prompt_without_busy() {
    block_on(async {
        let provider = ScriptedProvider::new(vec![
            Box::new(|model, _ctx, _opts| {
                assistant_stream_from_blocks(
                    model,
                    vec![AssistantBlock::Text(TextContent::plain("turn 1"))],
                    StopReason::Stop,
                    Usage::default(),
                    None,
                )
            }),
            Box::new(|model, _ctx, _opts| {
                assistant_stream_from_blocks(
                    model,
                    vec![AssistantBlock::Text(TextContent::plain("turn 2"))],
                    StopReason::Stop,
                    Usage::default(),
                    None,
                )
            }),
        ]);

        let agent = Arc::new(Agent::new(
            provider,
            AgentConfig::new(test_model("end-sub-model")),
        ));
        let agent_cb = Arc::clone(&agent);
        let saw_idle_in_cb = Arc::new(AtomicBool::new(false));
        let saw_idle_flag = Arc::clone(&saw_idle_in_cb);
        let next_stream_started = Arc::new(AtomicBool::new(false));
        let started_flag = Arc::clone(&next_stream_started);
        let spawned_second = Arc::new(AtomicBool::new(false));

        let _sub = agent.subscribe(move |ev| {
            if matches!(ev, AgentEvent::AgentEnd { .. })
                && !spawned_second.swap(true, Ordering::SeqCst)
            {
                saw_idle_flag.store(!agent_cb.is_running(), Ordering::SeqCst);
                agent_cb
                    .set_model(test_model("end-sub-model-v2"))
                    .expect("set_model inside AgentEnd must not fail with Busy");
                let stream = agent_cb
                    .prompt_stream("triggered from AgentEnd")
                    .expect("prompt_stream inside AgentEnd must not fail with Busy");
                // Keep the newly started run active until the outer test drives it to completion.
                std::mem::forget(stream);
                started_flag.store(true, Ordering::SeqCst);
            }
        });

        let _ = agent.prompt("first").await.unwrap();
        assert!(saw_idle_in_cb.load(Ordering::SeqCst));
        assert!(next_stream_started.load(Ordering::SeqCst));
        // The second run started inside AgentEnd must STILL be active (not clobbered by Run 1's guard!).
        assert!(agent.is_running());
        agent.wait_for_idle().await;
        assert!(!agent.is_running());
        assert!(agent.messages().iter().any(|m| matches!(
            m,
            AgentMessage::Llm(Message::Assistant(a)) if a.text_output() == "turn 2"
        )));
    });
}

#[test]
fn explicit_system_prompt_and_tools_updates_override_reconstructed_leading_system_message() {
    block_on(async {
        let tool_a = Arc::new(ClosureTool::new(
            ToolDefinition {
                name: "tool_a".into(),
                description: "A".into(),
                parameters: obj_schema(Vec::<(&str, Value, bool)>::new()),
            },
            |_id, _args, _cancel, _upd| Box::pin(async { Ok(AgentToolResult::text("a")) }),
        ));
        let tool_b = Arc::new(ClosureTool::new(
            ToolDefinition {
                name: "tool_b".into(),
                description: "B".into(),
                parameters: obj_schema(Vec::<(&str, Value, bool)>::new()),
            },
            |_id, _args, _cancel, _upd| Box::pin(async { Ok(AgentToolResult::text("b")) }),
        ));

        let initial_system = Message::System(pikra_ai::SystemMessage {
            content: "System A".into(),
            sections: vec![("env".into(), Some("OS=macOS".into()))],
            tools_added: vec![tool_a.definition()],
            tools_removed: Vec::new(),
            timestamp: 1,
        });

        let provider = ScriptedProvider::new(vec![
            Box::new(|model, _ctx, _opts| {
                assistant_stream_from_blocks(
                    model,
                    vec![AssistantBlock::Text(TextContent::plain("r1"))],
                    StopReason::Stop,
                    Usage::default(),
                    None,
                )
            }),
            Box::new(|model, _ctx, _opts| {
                assistant_stream_from_blocks(
                    model,
                    vec![AssistantBlock::Text(TextContent::plain("r2"))],
                    StopReason::Stop,
                    Usage::default(),
                    None,
                )
            }),
            Box::new(|model, _ctx, _opts| {
                assistant_stream_from_blocks(
                    model,
                    vec![AssistantBlock::Text(TextContent::plain("r3"))],
                    StopReason::Stop,
                    Usage::default(),
                    None,
                )
            }),
        ]);

        let mut cfg = AgentConfig::new(test_model("sys-override"))
            .with_system_prompt("Different config default")
            .with_tools(vec![tool_a.clone()]);
        cfg.initial_messages = vec![AgentMessage::Llm(initial_system)];

        let agent = Agent::new(provider.clone(), cfg);

        // 1. Untouched reconstructed session preserves messages[0] byte-for-byte.
        let _ = agent.prompt("turn 1").await.unwrap();
        let req0 = &provider.requests()[0];
        let resolved0 = req0.context.resolve();
        assert!(resolved0.system_prompt.starts_with("System A"));
        assert!(resolved0.system_prompt.contains("OS=macOS"));
        assert_eq!(
            resolved0
                .tools
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            vec!["tool_a"]
        );

        // 2. Explicit set_system_prompt("System B") + set_tools([tool_a, tool_b]) override messages[0].
        agent.set_system_prompt("System B").unwrap();
        agent
            .set_tools(vec![tool_a.clone(), tool_b.clone()])
            .unwrap();
        let _ = agent.prompt("turn 2").await.unwrap();
        let req1 = &provider.requests()[1];
        let resolved1 = req1.context.resolve();
        assert!(resolved1.system_prompt.starts_with("System B"));
        assert!(resolved1.system_prompt.contains("OS=macOS"));
        assert_eq!(
            resolved1
                .tools
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            vec!["tool_a", "tool_b"]
        );

        // 3. Explicit set_system_prompt("") clears base system content while keeping named sections.
        agent.set_system_prompt("").unwrap();
        agent.set_tools(Vec::new()).unwrap();
        let _ = agent.prompt("turn 3").await.unwrap();
        let req2 = &provider.requests()[2];
        let resolved2 = req2.context.resolve();
        assert_eq!(resolved2.system_prompt, "OS=macOS");
        assert!(resolved2.tools.is_empty());
    });
}
