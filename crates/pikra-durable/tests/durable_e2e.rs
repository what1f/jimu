//! End-to-end tests for `pikra-durable` against real temp SQLite WAL files,
//! `pikra-testkit::FauxTransport`, and deterministic `ManualClock` (zero sleeps).

use futures_executor::block_on;
use pikra_agent::{AgentToolResult, ClosureTool};
use pikra_ai::{
    ApiKind, AssistantBlock, AssistantEventStream, AssistantMessage, AssistantMessageEvent,
    BoxFuture, Message, Model, ModelCost, Models, ProviderConfig, SimpleStreamOptions, StopReason,
    StreamProvider, TextContent, ToolDefinition, TranscriptContext, Usage,
};
use pikra_durable::{
    Clock, CommitBatch, ConversationStatus, DurableEngine, DurableError, DurableEvent,
    DurableStore, DurableTool, DurableToolContext, DurableToolOutcome, LeaseClaimResult,
    ManualClock, MemoryDurableStore, ParkState, RetryPolicy, RunOutcome, SignalDeliveryResult,
    SignalResolution, SqliteDurableStore, TaskCheckpoint,
};
use pikra_schema::{int_schema, obj_schema, str_schema};
use pikra_session::{CompactionConfig, SessionEntryPayload};
use pikra_testkit::{FauxTransport, ScriptedHttpResponse};
use serde_json::{Value, json};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::tempdir;

fn test_model(context_window: u32, max_output_tokens: u32) -> Model {
    Model {
        id: "gpt-5.4".into(),
        name: "GPT-5.4".into(),
        provider: "openai".into(),
        api: ApiKind::OpenaiResponses,
        base_url: "https://api.openai.com/v1".into(),
        context_window,
        max_output_tokens,
        supports_images: false,
        supports_reasoning: false,
        cost: ModelCost::default(),
        headers: Vec::new(),
        compat: None,
    }
}

fn provider_for(transport: Arc<FauxTransport>) -> Arc<Models> {
    let mut models = Models::new(transport);
    models.register_provider("openai", ProviderConfig::with_api_key("sk-test"));
    Arc::new(models)
}

fn sse_text_response(
    resp_id: &str,
    text: &str,
    input_tokens: u64,
    output_tokens: u64,
) -> Vec<String> {
    let done_item = json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": {
            "type": "message",
            "id": format!("msg_{resp_id}"),
            "content": [{"type": "output_text", "text": text}]
        }
    });
    let completed = json!({
        "type": "response.completed",
        "response": {
            "id": resp_id,
            "status": "completed",
            "usage": {
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "total_tokens": input_tokens + output_tokens
            }
        }
    });
    vec![
        format!(
            "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"{resp_id}\"}}}}\n\n"
        ),
        format!("data: {done_item}\n\n"),
        format!("data: {completed}\n\n"),
    ]
}

fn sse_tool_calls_response(resp_id: &str, calls: &[(&str, &str, Value)]) -> Vec<String> {
    let mut chunks = vec![format!(
        "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"{resp_id}\"}}}}\n\n"
    )];
    for (idx, (call_id, name, args)) in calls.iter().enumerate() {
        let args_str = serde_json::to_string(args).expect("args serialize");
        let item_done = json!({
            "type": "response.output_item.done",
            "output_index": idx,
            "item": {
                "type": "function_call",
                "id": format!("fc_{idx}"),
                "call_id": call_id,
                "name": name,
                "arguments": args_str
            }
        });
        chunks.push(format!("data: {item_done}\n\n"));
    }
    let completed = json!({
        "type": "response.completed",
        "response": {
            "id": resp_id,
            "status": "completed",
            "usage": {"input_tokens": 50, "output_tokens": 20, "total_tokens": 70}
        }
    });
    chunks.push(format!("data: {completed}\n\n"));
    chunks
}

struct CrashingReceiptTool {
    crash_once: Arc<AtomicBool>,
    executions: Arc<AtomicUsize>,
}

impl DurableTool for CrashingReceiptTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "send_receipt".into(),
            description: "Send payment receipt email".into(),
            parameters: obj_schema([("to", str_schema(), true)]),
        }
    }

    fn execute<'a>(
        &'a self,
        _ctx: &'a DurableToolContext,
        args: Value,
    ) -> BoxFuture<'a, Result<DurableToolOutcome, String>> {
        let crash = self.crash_once.swap(false, Ordering::SeqCst);
        let executions = Arc::clone(&self.executions);
        Box::pin(async move {
            if crash {
                panic!("simulated process crash during send_receipt");
            }
            executions.fetch_add(1, Ordering::SeqCst);
            let to = args["to"].as_str().unwrap_or("unknown");
            Ok(DurableToolOutcome::Completed(AgentToolResult::text(
                format!("receipt sent to {to}"),
            )))
        })
    }
}

struct ApprovalGateTool;

impl DurableTool for ApprovalGateTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "approval_gate".into(),
            description: "Wait for approval signal and optional cooldown timer".into(),
            parameters: obj_schema([
                ("key", str_schema(), true),
                ("cooldown_ms", int_schema(), true),
            ]),
        }
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a DurableToolContext,
        args: Value,
    ) -> BoxFuture<'a, Result<DurableToolOutcome, String>> {
        let now = ctx.now_ms();
        Box::pin(async move {
            let signal_key = args["key"].as_str().unwrap_or_default().to_string();
            Ok(DurableToolOutcome::AwaitSignal {
                signal_key,
                timeout_at_ms: Some(now + 300_000),
                checkpoint: json!({ "stage": "await_signal" }),
            })
        })
    }

    fn resume_with_signal<'a>(
        &'a self,
        ctx: &'a DurableToolContext,
        args: Value,
        _checkpoint: Value,
        resolution: SignalResolution,
    ) -> BoxFuture<'a, Result<DurableToolOutcome, String>> {
        let now = ctx.now_ms();
        Box::pin(async move {
            match resolution {
                SignalResolution::TimedOut => Ok(DurableToolOutcome::Completed(
                    AgentToolResult::error("approval timed out"),
                )),
                SignalResolution::Received(payload) => {
                    let approver = payload["approver"]
                        .as_str()
                        .unwrap_or("unknown")
                        .to_string();
                    let cooldown_ms = args["cooldown_ms"].as_u64().unwrap_or(0);
                    if cooldown_ms > 0 {
                        Ok(DurableToolOutcome::SleepUntil {
                            wake_at_ms: now + cooldown_ms,
                            checkpoint: json!({ "approver": approver }),
                        })
                    } else {
                        Ok(DurableToolOutcome::Completed(AgentToolResult::text(
                            format!("approved immediately by {approver}"),
                        )))
                    }
                }
            }
        })
    }

    fn resume_after_sleep<'a>(
        &'a self,
        ctx: &'a DurableToolContext,
        _args: Value,
        checkpoint: Value,
    ) -> BoxFuture<'a, Result<DurableToolOutcome, String>> {
        let now = ctx.now_ms();
        Box::pin(async move {
            let approver = checkpoint["approver"].as_str().unwrap_or("unknown");
            Ok(DurableToolOutcome::Completed(AgentToolResult::text(
                format!("approved by {approver} after cooldown at {now}"),
            )))
        })
    }
}

fn make_charge_tool(counter: Arc<AtomicUsize>) -> ClosureTool {
    let def = ToolDefinition {
        name: "charge_card".into(),
        description: "Charge customer credit card".into(),
        parameters: obj_schema([("amount", int_schema(), true)]),
    };
    ClosureTool::new(def, move |_id, args, _cancel, _on_update| {
        let counter = Arc::clone(&counter);
        Box::pin(async move {
            let count = counter.fetch_add(1, Ordering::SeqCst) + 1;
            let amount = args["amount"].as_i64().unwrap_or(0);
            Ok(AgentToolResult::text(format!(
                "charged ${amount} (tx #{count})"
            )))
        })
    })
}

#[test]
fn crash_recovery_replays_completed_side_effect_tools_and_deduplicates_submissions() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("durable.sqlite");
    let clock = Arc::new(ManualClock::new(10_000));
    let charge_count = Arc::new(AtomicUsize::new(0));
    let receipt_count = Arc::new(AtomicUsize::new(0));
    let crash_once = Arc::new(AtomicBool::new(true));

    // Process 1: transient 503 retry -> tool batch -> charge_card commits -> send_receipt crashes.
    {
        let store = Arc::new(SqliteDurableStore::open(&db_path).expect("open sqlite"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&db_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "newly created SQLite DB must use mode 0600");
        }

        let transport = Arc::new(FauxTransport::new());
        transport.enqueue(ScriptedHttpResponse::http_error(503, "temporarily busy"));
        transport.enqueue_sse(sse_tool_calls_response(
            "resp_tools",
            &[
                ("call_charge", "charge_card", json!({"amount": 42})),
                (
                    "call_receipt",
                    "send_receipt",
                    json!({"to": "alice@example.com"}),
                ),
            ],
        ));

        let engine = DurableEngine::new(store, provider_for(transport.clone()))
            .with_clock(clock.clone())
            .with_retry_policy(RetryPolicy::new(3))
            .with_agent_tool(make_charge_tool(Arc::clone(&charge_count)))
            .with_tool(CrashingReceiptTool {
                crash_once: Arc::clone(&crash_once),
                executions: Arc::clone(&receipt_count),
            });

        engine
            .create_conversation(
                "conv-pay",
                test_model(128_000, 4_096),
                "You are a billing agent.",
            )
            .expect("create conversation");

        let crash_res = catch_unwind(AssertUnwindSafe(|| {
            block_on(engine.submit("conv-pay", "sub-1", "Charge $42 and email alice"))
        }));
        assert!(
            crash_res.is_err(),
            "expected simulated crash during send_receipt"
        );
        assert_eq!(transport.request_count(), 2, "503 retry + tool-use turn");
        assert_eq!(charge_count.load(Ordering::SeqCst), 1);
        assert_eq!(receipt_count.load(Ordering::SeqCst), 0);
    }

    // Process 2: reopen DB file from disk, recover, replay charge_card without re-running it.
    let store2 = Arc::new(SqliteDurableStore::open(&db_path).expect("reopen sqlite"));
    let assistant_entry_id = match store2.get_checkpoint("conv-pay").unwrap() {
        Some(TaskCheckpoint::ExecutingTools {
            assistant_entry_id, ..
        }) => assistant_entry_id,
        other => panic!("expected ExecutingTools checkpoint, got {other:?}"),
    };
    assert!(
        store2
            .get_completed_tool("conv-pay", &assistant_entry_id, "call_charge")
            .unwrap()
            .is_some()
    );
    assert!(
        store2
            .get_completed_tool("conv-pay", &assistant_entry_id, "call_receipt")
            .unwrap()
            .is_none()
    );

    let transport2 = Arc::new(FauxTransport::new());
    transport2.enqueue_sse(sse_text_response(
        "resp_done",
        "Charged $42 and sent receipt to alice@example.com.",
        120,
        24,
    ));

    let events = Arc::new(Mutex::new(Vec::<DurableEvent>::new()));
    let events_sink = Arc::clone(&events);
    let engine2 = DurableEngine::new(store2.clone(), provider_for(transport2.clone()))
        .with_clock(clock.clone())
        .with_agent_tool(make_charge_tool(Arc::clone(&charge_count)))
        .with_tool(CrashingReceiptTool {
            crash_once: Arc::clone(&crash_once),
            executions: Arc::clone(&receipt_count),
        });
    engine2.subscribe(move |ev| {
        events_sink.lock().unwrap().push(ev.clone());
    });

    let outcome = block_on(engine2.recover("conv-pay")).expect("recover");
    match outcome {
        RunOutcome::Completed {
            submission_id,
            final_text,
            deduplicated,
            ..
        } => {
            assert_eq!(submission_id, "sub-1");
            assert_eq!(
                final_text,
                "Charged $42 and sent receipt to alice@example.com."
            );
            assert!(!deduplicated);
        }
        other => panic!("expected Completed, got {other:?}"),
    }

    assert_eq!(
        charge_count.load(Ordering::SeqCst),
        1,
        "charge_card must NOT re-execute after recovery"
    );
    assert_eq!(
        receipt_count.load(Ordering::SeqCst),
        1,
        "send_receipt must execute exactly once after recovery"
    );
    assert_eq!(transport2.request_count(), 1);

    let recorded = events.lock().unwrap();
    assert!(recorded.iter().any(|e| matches!(
        e,
        DurableEvent::ToolCompleted {
            tool_call_id,
            replayed: true,
            ..
        } if tool_call_id == "call_charge"
    )));
    assert!(recorded.iter().any(|e| matches!(
        e,
        DurableEvent::ToolCompleted {
            tool_call_id,
            replayed: false,
            ..
        } if tool_call_id == "call_receipt"
    )));

    // Verify transcript order: Baseline, User (once!), Assistant, ToolResult(call_charge), ToolResult(call_receipt), Assistant.
    let entries = store2.load_entries("conv-pay").unwrap();
    let tool_result_ids: Vec<&str> = entries
        .iter()
        .filter_map(|e| match &e.payload {
            SessionEntryPayload::Message {
                message: pikra_agent::AgentMessage::Llm(Message::ToolResult(tr)),
            } => Some(tr.tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(tool_result_ids, vec!["call_charge", "call_receipt"]);
    let user_count = entries
        .iter()
        .filter(|e| {
            matches!(
                &e.payload,
                SessionEntryPayload::Message {
                    message: pikra_agent::AgentMessage::Llm(Message::User(_))
                }
            )
        })
        .count();
    assert_eq!(
        user_count, 1,
        "retry and recovery must never duplicate user prompt"
    );

    // Duplicate submission_id returns cached completion with 0 extra LLM calls.
    let dup = block_on(engine2.submit("conv-pay", "sub-1", "Charge $42 and email alice"))
        .expect("duplicate submit");
    match dup {
        RunOutcome::Completed {
            final_text,
            deduplicated,
            ..
        } => {
            assert_eq!(
                final_text,
                "Charged $42 and sent receipt to alice@example.com."
            );
            assert!(deduplicated);
        }
        other => panic!("expected deduplicated Completed, got {other:?}"),
    }
    assert_eq!(
        transport2.request_count(),
        1,
        "0 extra LLM calls on duplicate"
    );
}

#[test]
fn durable_signal_and_timer_park_across_restarts() {
    block_on(async {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("signals.sqlite");
        let clock = Arc::new(ManualClock::new(1_000_000));

        // Process 1: tool parks on AwaitSignal("approval:req-7"), engine drops completely.
        {
            let store = Arc::new(SqliteDurableStore::open(&db_path).expect("open sqlite"));
            let transport = Arc::new(FauxTransport::new());
            transport.enqueue_sse(sse_tool_calls_response(
                "resp_gate_1",
                &[(
                    "call_gate_1",
                    "approval_gate",
                    json!({"key": "approval:req-7", "cooldown_ms": 60_000}),
                )],
            ));

            let engine = DurableEngine::new(store.clone(), provider_for(transport))
                .with_clock(clock.clone())
                .with_tool(ApprovalGateTool);
            engine
                .create_conversation("conv-wf", test_model(128_000, 4_096), "Deploy coordinator.")
                .unwrap();

            let outcome = engine
                .submit("conv-wf", "sub-wf-1", "Deploy v2.0")
                .await
                .unwrap();
            assert!(matches!(
                outcome,
                RunOutcome::Parked {
                    park: ParkState::AwaitSignal { ref signal_key, .. },
                    ..
                } if signal_key == "approval:req-7"
            ));
            assert_eq!(
                store.get_conversation("conv-wf").unwrap().unwrap().status,
                ConversationStatus::Parked
            );
        }

        // Process 2: reopen SQLite, deliver idempotent signal, wake into SleepUntil, advance clock, finish.
        let store2 = Arc::new(SqliteDurableStore::open(&db_path).expect("reopen sqlite"));
        let transport2 = Arc::new(FauxTransport::new());
        let engine2 = DurableEngine::new(store2.clone(), provider_for(transport2.clone()))
            .with_clock(clock.clone())
            .with_tool(ApprovalGateTool);

        // Recovering before signal arrives stays parked cleanly.
        assert!(matches!(
            engine2.recover("conv-wf").await.unwrap(),
            RunOutcome::Parked {
                park: ParkState::AwaitSignal { .. },
                ..
            }
        ));

        let d1 = engine2
            .deliver_signal(
                "conv-wf",
                "approval:req-7",
                "sig-req-7",
                json!({"approver": "alice"}),
            )
            .unwrap();
        let d2 = engine2
            .deliver_signal(
                "conv-wf",
                "approval:req-7",
                "sig-req-7",
                json!({"approver": "alice"}),
            )
            .unwrap();
        assert!(matches!(d1, SignalDeliveryResult::Delivered(_)));
        assert!(matches!(d2, SignalDeliveryResult::Duplicate(_)));

        // Wake from signal -> tool chains into SleepUntil(T + 60_000).
        let after_sig = engine2.recover("conv-wf").await.unwrap();
        assert!(matches!(
            after_sig,
            RunOutcome::Parked {
                park: ParkState::SleepUntil {
                    wake_at_ms: 1_060_000,
                    ..
                },
                ..
            }
        ));

        // Still sleeping at T + 30_000.
        clock.advance(30_000);
        assert!(matches!(
            engine2.recover("conv-wf").await.unwrap(),
            RunOutcome::Parked {
                park: ParkState::SleepUntil {
                    wake_at_ms: 1_060_000,
                    ..
                },
                ..
            }
        ));

        // Advance to T + 60_000 -> wake timer -> complete turn.
        clock.advance(30_000);
        assert_eq!(clock.now_ms(), 1_060_000);
        transport2.enqueue_sse(sse_text_response(
            "resp_deploy_done",
            "Deployed v2.0 after approval and cooldown.",
            90,
            18,
        ));
        let finished = engine2.recover("conv-wf").await.unwrap();
        match finished {
            RunOutcome::Completed { final_text, .. } => {
                assert_eq!(final_text, "Deployed v2.0 after approval and cooldown.");
            }
            other => panic!("expected Completed after timer wakeup, got {other:?}"),
        }
        let req_body = transport2.requests()[0].body_json.to_string();
        assert!(req_body.contains("approved by alice after cooldown at 1060000"));

        // Pre-buffered signal arriving BEFORE AwaitSignal consumes immediately with zero park.
        engine2
            .deliver_signal(
                "conv-wf",
                "approval:req-8",
                "sig-req-8",
                json!({"approver": "bob"}),
            )
            .unwrap();
        transport2.enqueue_sse(sse_tool_calls_response(
            "resp_gate_2",
            &[(
                "call_gate_2",
                "approval_gate",
                json!({"key": "approval:req-8", "cooldown_ms": 0}),
            )],
        ));
        transport2.enqueue_sse(sse_text_response(
            "resp_fast_done",
            "Fast-tracked v2.1 with pre-buffered approval.",
            100,
            15,
        ));

        let fast_outcome = engine2
            .submit("conv-wf", "sub-wf-2", "Fast-track v2.1")
            .await
            .unwrap();
        match fast_outcome {
            RunOutcome::Completed { final_text, .. } => {
                assert_eq!(final_text, "Fast-tracked v2.1 with pre-buffered approval.");
            }
            other => {
                panic!("expected immediate completion from pre-buffered signal, got {other:?}")
            }
        }
        let sig8 = store2
            .list_signals("conv-wf")
            .unwrap()
            .into_iter()
            .find(|s| s.signal_id == "sig-req-8")
            .expect("sig-req-8 stored");
        assert!(
            sig8.consumed_at_ms.is_some(),
            "pre-buffered signal must be marked consumed"
        );
    });
}

#[test]
fn durable_compaction_and_cas_documents() {
    block_on(async {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("compact.sqlite");
        let clock = Arc::new(ManualClock::new(2_000_000));

        let store = Arc::new(SqliteDurableStore::open(&db_path).expect("open sqlite"));
        let transport = Arc::new(FauxTransport::new());
        let compaction_events = Arc::new(AtomicUsize::new(0));
        let comp_counter = Arc::clone(&compaction_events);

        let engine = DurableEngine::new(store.clone(), provider_for(transport.clone()))
            .with_clock(clock.clone())
            .with_compaction(CompactionConfig {
                enabled: true,
                compaction_reserve_tokens: Some(80),
                keep_recent_tokens: 40,
                max_summary_chars: 2_000,
                custom_instructions: None,
            });
        engine.subscribe(move |ev| {
            if matches!(ev, DurableEvent::Compacted { .. }) {
                comp_counter.fetch_add(1, Ordering::SeqCst);
            }
        });

        // Context window 200 with reserve 80 => compaction threshold = max(120, 100) = 120 tokens.
        engine
            .create_conversation("conv-doc", test_model(200, 64), "You are a planner.")
            .unwrap();

        // 1. Optimistic CAS documents + atomic batch rollback on CAS conflict.
        assert_eq!(engine.get_document("conv-doc", "plan").unwrap(), None);
        let rev1 = engine
            .put_document("conv-doc", "plan", None, json!({"step": 1}))
            .unwrap();
        assert_eq!(rev1, 1);

        let err_create_dup = engine
            .put_document("conv-doc", "plan", None, json!({"step": 99}))
            .unwrap_err();
        assert!(matches!(
            err_create_dup,
            DurableError::Conflict {
                expected: None,
                actual: Some(1),
                ..
            }
        ));

        let rev2 = engine
            .put_document("conv-doc", "plan", Some(1), json!({"step": 2}))
            .unwrap();
        assert_eq!(rev2, 2);

        // Stale CAS inside a multi-op CommitBatch rolls back the entire transaction.
        let mut failing_batch = CommitBatch::new("conv-doc", clock.now_ms());
        failing_batch.update_status = Some(ConversationStatus::Failed);
        failing_batch
            .document_writes
            .push(pikra_durable::DocumentWrite {
                key: "plan".into(),
                expected_revision: Some(1),
                value: json!({"step": 3}),
            });
        assert!(matches!(
            store.commit(failing_batch),
            Err(DurableError::Conflict {
                expected: Some(1),
                actual: Some(2),
                ..
            })
        ));
        assert_eq!(
            store.get_conversation("conv-doc").unwrap().unwrap().status,
            ConversationStatus::Idle,
            "failed CAS commit must roll back status update atomically"
        );

        // 2. Durable auto-compaction preserving transcript continuity across turns.
        // Turn 1 reports 150 input tokens (>= 120 threshold).
        transport.enqueue_sse(sse_text_response("resp_t1", "Completed step 1.", 150, 20));
        let t1 = engine
            .submit("conv-doc", "sub-1", "Start step 1 of the rollout plan.")
            .await
            .unwrap();
        assert!(matches!(t1, RunOutcome::Completed { .. }));
        assert_eq!(compaction_events.load(Ordering::SeqCst), 0);

        // Turn 2 triggers auto-compaction first, then generates Turn 2 over the compacted transcript.
        transport.enqueue_sse(sse_text_response(
            "resp_compact",
            "## Goal\nRollout plan\n## Progress\nStep 1 completed.",
            160,
            25,
        ));
        transport.enqueue_sse(sse_text_response(
            "resp_t2",
            "Step 2 completed cleanly.",
            70,
            15,
        ));
        let t2 = engine
            .submit("conv-doc", "sub-2", "Continue to step 2.")
            .await
            .unwrap();
        match t2 {
            RunOutcome::Completed { final_text, .. } => {
                assert_eq!(final_text, "Step 2 completed cleanly.");
            }
            other => panic!("expected Completed on turn 2, got {other:?}"),
        }
        assert_eq!(compaction_events.load(Ordering::SeqCst), 1);

        // Turn 2 LLM request (request index 2: 0=turn1, 1=compaction, 2=turn2) contains compaction summary + turn 2 prompt.
        let reqs = transport.requests();
        assert_eq!(reqs.len(), 3);
        let turn2_req_str = reqs[2].body_json.to_string();
        assert!(turn2_req_str.contains("<compaction-summary>"));
        assert!(turn2_req_str.contains("Step 1 completed."));
        assert!(turn2_req_str.contains("Continue to step 2."));

        // Reopen DB from disk: verify CAS doc + compaction checkpoint persist across restarts.
        drop(engine);
        drop(store);
        let reopened = Arc::new(SqliteDurableStore::open(&db_path).expect("reopen sqlite"));
        assert_eq!(
            reopened.get_document("conv-doc", "plan").unwrap(),
            Some((2, json!({"step": 2})))
        );
        let engine2 = DurableEngine::new(reopened, provider_for(Arc::new(FauxTransport::new())))
            .with_clock(clock);
        let reconstructed = engine2.reconstruct_context("conv-doc").unwrap();
        let ckpt = reconstructed
            .latest_compaction
            .expect("compaction checkpoint must survive restart");
        assert!(ckpt.summary.contains("Step 1 completed."));
        assert!(reconstructed.messages.iter().any(|m| matches!(
            m,
            pikra_agent::AgentMessage::Llm(Message::Assistant(a)) if a.text_output() == "Step 2 completed cleanly."
        )));
    });
}

struct BlockingStreamProvider {
    calls: Arc<AtomicUsize>,
    entered_tx: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    release_rx: Mutex<Option<std::sync::mpsc::Receiver<String>>>,
}

impl BlockingStreamProvider {
    fn new() -> (
        Arc<Self>,
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::Sender<String>,
    ) {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        (
            Arc::new(Self {
                calls: Arc::new(AtomicUsize::new(0)),
                entered_tx: Mutex::new(Some(entered_tx)),
                release_rx: Mutex::new(Some(release_rx)),
            }),
            entered_rx,
            release_tx,
        )
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl StreamProvider for BlockingStreamProvider {
    fn stream(
        &self,
        model: &Model,
        _context: &TranscriptContext,
        _options: SimpleStreamOptions,
    ) -> AssistantEventStream {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let model = model.clone();
        let entered_tx = self.entered_tx.lock().unwrap().take();
        let release_rx = self.release_rx.lock().unwrap().take();
        Box::pin(futures_util::stream::once(async move {
            if let Some(sig) = entered_tx {
                let _ = sig.send(());
            }
            let text = release_rx
                .and_then(|rx| rx.recv().ok())
                .unwrap_or_else(|| "released".to_string());
            let final_msg = AssistantMessage {
                content: vec![AssistantBlock::Text(TextContent::plain(text))],
                api: model.api.clone(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                response_id: Some("resp_blocking".into()),
                thinking_level: None,
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                diagnostics: Vec::new(),
                timestamp: 1,
                duration_ms: Some(1),
            };
            AssistantMessageEvent::Done {
                reason: StopReason::Stop,
                message: final_msg,
            }
        }))
    }
}

#[test]
fn concurrent_engine_lease_prevents_double_execution_and_fences_expired_owner() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("lease.sqlite");
    let clock = Arc::new(ManualClock::new(10_000));
    let store1 = Arc::new(SqliteDurableStore::open(&db_path).expect("open sqlite 1"));
    let store2 = Arc::new(SqliteDurableStore::open(&db_path).expect("open sqlite 2"));

    // Part 1: Live lease blocks concurrent recover() and submit() from a second engine.
    let (blocking_provider, entered_rx, release_tx) = BlockingStreamProvider::new();
    let engine1 = Arc::new(
        DurableEngine::new(store1.clone(), blocking_provider.clone())
            .with_clock(clock.clone())
            .with_lease_ttl_ms(60_000),
    );
    let transport2 = Arc::new(FauxTransport::new());
    let engine2 = DurableEngine::new(store2.clone(), provider_for(transport2.clone()))
        .with_clock(clock.clone())
        .with_lease_ttl_ms(60_000);

    engine1
        .create_conversation("conv-lease", test_model(128_000, 4_096), "Lease test.")
        .unwrap();

    let e1_handle = {
        let e1 = Arc::clone(&engine1);
        std::thread::spawn(move || block_on(e1.submit("conv-lease", "sub-1", "Hello")))
    };

    // Wait deterministically until engine1 is inside provider.stream().
    entered_rx.recv().expect("engine1 entered provider");
    let conv_during = store2.get_conversation("conv-lease").unwrap().unwrap();
    assert_eq!(
        conv_during.lease_owner.as_deref(),
        Some(engine1.engine_id())
    );
    assert_eq!(conv_during.lease_expires_at_ms, Some(70_000));

    // Engine 2 recover() skips live-leased conversation instead of double-executing it.
    assert_eq!(
        block_on(engine2.recover("conv-lease")).unwrap(),
        RunOutcome::Idle
    );
    assert_eq!(transport2.request_count(), 0);

    // Engine 2 submit() on same or new submission returns Busy while engine1 holds a live lease.
    assert!(matches!(
        block_on(engine2.submit("conv-lease", "sub-1", "Hello")),
        Err(DurableError::Busy { .. })
    ));
    assert!(matches!(
        block_on(engine2.submit("conv-lease", "sub-2", "Another")),
        Err(DurableError::Busy { .. })
    ));

    // Release engine1 -> completes cleanly and releases the lease.
    release_tx.send("reply from engine1".into()).unwrap();
    let outcome1 = e1_handle.join().unwrap().unwrap();
    assert!(matches!(
        outcome1,
        RunOutcome::Completed { ref final_text, .. } if final_text == "reply from engine1"
    ));
    assert_eq!(blocking_provider.call_count(), 1);
    let conv_after = store2.get_conversation("conv-lease").unwrap().unwrap();
    assert_eq!(conv_after.lease_owner, None);
    assert_eq!(conv_after.lease_expires_at_ms, None);

    // Part 2: Expired lease takeover by engine2 fences stalled engine1 from committing a duplicate reply.
    let (stalled_provider, stalled_entered_rx, stalled_release_tx) = BlockingStreamProvider::new();
    let stalled_engine = Arc::new(
        DurableEngine::new(store1.clone(), stalled_provider)
            .with_clock(clock.clone())
            .with_lease_ttl_ms(60_000),
    );
    let stalled_handle = {
        let e = Arc::clone(&stalled_engine);
        std::thread::spawn(move || block_on(e.submit("conv-lease", "sub-2", "Stalled prompt")))
    };
    stalled_entered_rx
        .recv()
        .expect("stalled engine entered provider");

    // Advance virtual clock past lease_ttl_ms so engine2 can take over via recover().
    clock.advance(60_000);
    transport2.enqueue_sse(sse_text_response(
        "resp_takeover",
        "reply from engine2 takeover",
        40,
        10,
    ));
    let takeover_outcome = block_on(engine2.recover("conv-lease")).unwrap();
    assert!(matches!(
        takeover_outcome,
        RunOutcome::Completed { ref final_text, .. } if final_text == "reply from engine2 takeover"
    ));

    // Unblock stalled_engine: its stale CommitBatch must fail with Conflict, never duplicating the reply.
    stalled_release_tx
        .send("stale reply from engine1".into())
        .unwrap();
    let stalled_res = stalled_handle.join().unwrap();
    assert!(
        matches!(stalled_res, Err(DurableError::Conflict { .. })),
        "expected Conflict fencing stale owner, got {stalled_res:?}"
    );

    let assistant_replies: Vec<String> = store2
        .load_entries("conv-lease")
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.payload {
            SessionEntryPayload::Message {
                message: pikra_agent::AgentMessage::Llm(Message::Assistant(a)),
            } => Some(a.text_output()),
            _ => None,
        })
        .collect();
    assert_eq!(
        assistant_replies,
        vec![
            "reply from engine1".to_string(),
            "reply from engine2 takeover".to_string()
        ]
    );

    // Part 3: Verify MemoryDurableStore enforces identical lease exclusivity and fencing.
    let mem_store = Arc::new(MemoryDurableStore::new());
    let mem_clock = Arc::new(ManualClock::new(1_000));
    let mem_e1 = DurableEngine::new(
        mem_store.clone(),
        provider_for(Arc::new(FauxTransport::new())),
    )
    .with_clock(mem_clock.clone())
    .with_lease_ttl_ms(10_000);
    mem_e1
        .create_conversation("conv-mem", test_model(128_000, 4_096), "Memory lease test.")
        .unwrap();
    let claim1 = mem_store
        .try_claim_conversation("conv-mem", "owner-a", 1_000, 10_000)
        .unwrap();
    let ver1 = match claim1 {
        LeaseClaimResult::Acquired {
            checkpoint_version, ..
        } => checkpoint_version,
        other => panic!("expected Acquired, got {other:?}"),
    };
    assert!(matches!(
        mem_store
            .try_claim_conversation("conv-mem", "owner-b", 5_000, 10_000)
            .unwrap(),
        LeaseClaimResult::HeldByOther { ref lease_owner, .. } if lease_owner == "owner-a"
    ));
    let claim2 = mem_store
        .try_claim_conversation("conv-mem", "owner-b", 11_000, 10_000)
        .unwrap();
    assert!(matches!(
        claim2,
        LeaseClaimResult::Acquired {
            checkpoint_version, ..
        } if checkpoint_version > ver1
    ));
    let mut stale_batch = CommitBatch::new("conv-mem", 11_001);
    stale_batch.expected_lease_owner = Some("owner-a".into());
    stale_batch.expected_checkpoint_version = Some(ver1);
    assert!(matches!(
        mem_store.commit(stale_batch),
        Err(DurableError::Conflict { .. })
    ));
    assert!(
        !mem_store
            .release_conversation("conv-mem", "owner-a", 11_002)
            .unwrap()
    );
    assert!(
        mem_store
            .release_conversation("conv-mem", "owner-b", 11_002)
            .unwrap()
    );
}

struct StagedDocTool {
    crash_once: Arc<AtomicBool>,
}

impl DurableTool for StagedDocTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "staged_doc".into(),
            description: "Read-modify-write state document with crash/error/park modes".into(),
            parameters: obj_schema([("mode", str_schema(), true)]),
        }
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a DurableToolContext,
        args: Value,
    ) -> BoxFuture<'a, Result<DurableToolOutcome, String>> {
        let crash_once = Arc::clone(&self.crash_once);
        Box::pin(async move {
            let mode = args["mode"].as_str().unwrap_or("ok");
            let current = ctx.get_document("counter").map_err(|e| e.to_string())?;
            let (cur_rev, cur_val) = match current {
                Some((r, v)) => (Some(r), v["n"].as_u64().unwrap_or(0)),
                None => (None, 0),
            };

            let rev1 = ctx
                .put_document("counter", cur_rev, json!({ "n": cur_val + 1 }))
                .map_err(|e| e.to_string())?;
            // Read-your-own-writes inside the same tool execution.
            let seen = ctx.get_document("counter").map_err(|e| e.to_string())?;
            assert_eq!(seen, Some((rev1, json!({ "n": cur_val + 1 }))));

            match mode {
                "crash_then_ok" => {
                    let rev2 = ctx
                        .put_document("counter", Some(rev1), json!({ "n": cur_val + 2 }))
                        .map_err(|e| e.to_string())?;
                    assert_eq!(rev2, rev1 + 1);
                    if crash_once.swap(false, Ordering::SeqCst) {
                        panic!("simulated crash after staging put_document");
                    }
                    Ok(DurableToolOutcome::Completed(AgentToolResult::text(
                        format!("counter={}", cur_val + 2),
                    )))
                }
                "error" => Err("tool aborted after staging document".into()),
                "park" => Ok(DurableToolOutcome::AwaitSignal {
                    signal_key: "resume:doc".into(),
                    timeout_at_ms: None,
                    checkpoint: json!({ "staged": cur_val + 1 }),
                }),
                _ => Ok(DurableToolOutcome::Completed(AgentToolResult::text("ok"))),
            }
        })
    }
}

#[test]
fn staged_tool_document_writes_commit_atomically_and_never_orphan_on_crash_or_error() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("staged_docs.sqlite");
    let clock = Arc::new(ManualClock::new(50_000));
    let crash_once = Arc::new(AtomicBool::new(true));

    // 1. Crash after two staged put_document calls -> zero writes hit SQLite.
    {
        let store = Arc::new(SqliteDurableStore::open(&db_path).expect("open sqlite"));
        let transport = Arc::new(FauxTransport::new());
        transport.enqueue_sse(sse_tool_calls_response(
            "resp_doc_1",
            &[(
                "call_doc_1",
                "staged_doc",
                json!({ "mode": "crash_then_ok" }),
            )],
        ));
        let engine = DurableEngine::new(store.clone(), provider_for(transport))
            .with_clock(clock.clone())
            .with_tool(StagedDocTool {
                crash_once: Arc::clone(&crash_once),
            });
        engine
            .create_conversation("conv-doc-tx", test_model(128_000, 4_096), "Doc tx.")
            .unwrap();

        let crashed = catch_unwind(AssertUnwindSafe(|| {
            block_on(engine.submit("conv-doc-tx", "sub-1", "Increment twice"))
        }));
        assert!(crashed.is_err());
        assert_eq!(
            store.get_document("conv-doc-tx", "counter").unwrap(),
            None,
            "crashed tool must leave zero orphaned document writes in store"
        );
    }

    // 2. Recover: tool re-runs cleanly and commits counter=2 (rev 2) atomically with completion.
    let store2 = Arc::new(SqliteDurableStore::open(&db_path).expect("reopen sqlite"));
    let transport2 = Arc::new(FauxTransport::new());
    transport2.enqueue_sse(sse_text_response("resp_doc_done", "done", 30, 10));
    let engine2 = DurableEngine::new(store2.clone(), provider_for(transport2.clone()))
        .with_clock(clock.clone())
        .with_tool(StagedDocTool {
            crash_once: Arc::clone(&crash_once),
        });
    assert!(matches!(
        block_on(engine2.recover("conv-doc-tx")).unwrap(),
        RunOutcome::Completed { .. }
    ));
    assert_eq!(
        store2.get_document("conv-doc-tx", "counter").unwrap(),
        Some((2, json!({ "n": 2 }))),
        "recovery must apply read-modify-write exactly once"
    );

    // 3. Tool returning Err after staging put_document must not persist the staged write.
    transport2.enqueue_sse(sse_tool_calls_response(
        "resp_doc_err",
        &[("call_doc_err", "staged_doc", json!({ "mode": "error" }))],
    ));
    transport2.enqueue_sse(sse_text_response(
        "resp_doc_err_done",
        "handled error",
        30,
        10,
    ));
    assert!(matches!(
        block_on(engine2.submit("conv-doc-tx", "sub-2", "Trigger error")).unwrap(),
        RunOutcome::Completed { .. }
    ));
    assert_eq!(
        store2.get_document("conv-doc-tx", "counter").unwrap(),
        Some((2, json!({ "n": 2 }))),
        "tool returning Err must discard staged document writes"
    );

    // 4. Tool returning AwaitSignal commits staged document atomically with Parked checkpoint.
    transport2.enqueue_sse(sse_tool_calls_response(
        "resp_doc_park",
        &[("call_doc_park", "staged_doc", json!({ "mode": "park" }))],
    ));
    assert!(matches!(
        block_on(engine2.submit("conv-doc-tx", "sub-3", "Park with doc")).unwrap(),
        RunOutcome::Parked {
            park: ParkState::AwaitSignal { .. },
            ..
        }
    ));
    assert_eq!(
        store2.get_document("conv-doc-tx", "counter").unwrap(),
        Some((3, json!({ "n": 3 }))),
        "parking tool must commit staged document writes atomically with ParkState"
    );
}

#[test]
fn durable_meta_survives_restart_and_preserves_context_deduplication() {
    block_on(async {
        let dir = tempdir().unwrap();
        let path = dir.path().join("meta.sqlite");
        let store = Arc::new(SqliteDurableStore::open(&path).unwrap());
        let http = Arc::new(FauxTransport::new());
        http.enqueue_sse(sse_text_response("meta", "Acknowledged", 40, 4));
        let engine = DurableEngine::new(store.clone(), provider_for(http.clone()));
        engine
            .create_conversation("meta", test_model(128_000, 4_096), "Base")
            .unwrap();
        assert!(
            engine
                .set_context("meta", "env", "active env")
                .await
                .unwrap()
        );
        let head = store
            .load_entries("meta")
            .unwrap()
            .last()
            .unwrap()
            .id
            .clone();
        assert!(
            !engine
                .set_context("meta", "env", "active env")
                .await
                .unwrap()
        );
        assert_eq!(store.load_entries("meta").unwrap().last().unwrap().id, head);
        engine
            .append_context("meta", "env", "one-off env")
            .await
            .unwrap();
        engine
            .append_meta("meta", "raw durable context")
            .await
            .unwrap();
        engine
            .submit_meta("meta", "first", "hidden durable prompt")
            .await
            .unwrap();
        drop(engine);
        drop(store);

        let store = Arc::new(SqliteDurableStore::open(&path).unwrap());
        let engine = DurableEngine::new(store, provider_for(http.clone()));
        engine
            .submit_meta("meta", "first", "ignore this retry")
            .await
            .unwrap();
        assert!(
            !engine
                .set_context("meta", "env", "active env")
                .await
                .unwrap()
        );
        let context = engine.reconstruct_context("meta").unwrap();
        let messages: Vec<_> = context
            .messages
            .iter()
            .filter_map(|message| match message {
                pikra_agent::AgentMessage::Llm(Message::User(user)) => Some(user),
                _ => None,
            })
            .collect();
        assert_eq!(messages.len(), 4);
        assert!(messages.iter().all(|user| user.is_meta));
        assert_eq!(http.request_count(), 1);
        let request = &http.requests()[0].body_json;
        let input = request["input"].as_array().unwrap();
        let users: Vec<_> = input
            .iter()
            .filter(|item| item["role"] == "user")
            .map(|item| item["content"][0]["text"].as_str().unwrap())
            .collect();
        assert_eq!(
            users,
            [
                "<system-reminder name=\"env\">\nactive env\n</system-reminder>",
                "<system-reminder name=\"env\">\none-off env\n</system-reminder>",
                "raw durable context",
                "hidden durable prompt",
            ]
        );
        let body = request.to_string();
        assert!(!body.contains("is_meta") && !body.contains("isMeta"));
    });
}
