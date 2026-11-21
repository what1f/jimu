//! End-to-end tests for `pikra-session`: JSONL persistence + reopen + fork branching,
//! deduplicated `<system-reminder>` context blocks, base64 image token exclusion, threshold math,
//! and prefix-cache-preserving auto-compaction with folding & context restoration.

use futures_executor::block_on;
use pikra_agent::AgentMessage;
use pikra_ai::{
    ApiKind, AssistantBlock, AssistantMessage, BoxFuture, Client, ContentPart, ImageContent,
    Message, Model, ModelCost, SimpleStreamOptions, StopReason, StreamProvider, TextContent,
    ThinkingLevel, ToolDefinition, Usage, UserMessage,
};
use pikra_schema::{obj_schema, str_schema};
use pikra_session::{
    CompactionConfig, CompactionInput, CompactionOutput, CompactionStrategy, IMAGE_TOKEN_ESTIMATE,
    JsonlSessionStore, SessionConfig, SessionEntryPayload, SessionError, SessionEvent,
    SessionManager, compaction_threshold, effective_reserve_tokens, estimate_context_tokens,
    should_compact,
};
use pikra_testkit::{FauxTransport, assert_prompt_prefix_identical};
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
        supports_images: true,
        supports_reasoning: true,
        cost: ModelCost::default(),
        headers: Vec::new(),
        compat: None,
    }
}

fn resident_tools() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "read_file".into(),
        description: "Read a file".into(),
        parameters: obj_schema([("path", str_schema(), true)]),
    }]
}

fn openai_sse_text(id: &str, text: &str, input_tokens: u64, cached_tokens: u64) -> Vec<String> {
    let text_json = serde_json::to_string(text).unwrap();
    vec![
        format!(
            "data: {{\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{{\"type\":\"message\",\"id\":\"msg_{id}\"}}}}\n\n"
        ),
        format!(
            "data: {{\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":{text_json}}}\n\n"
        ),
        format!(
            "data: {{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{{\"type\":\"message\",\"id\":\"msg_{id}\",\"content\":[{{\"type\":\"output_text\",\"text\":{text_json}}}]}}}}\n\n"
        ),
        format!(
            "data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_{id}\",\"status\":\"completed\",\"usage\":{{\"input_tokens\":{input_tokens},\"output_tokens\":40,\"total_tokens\":{},\"input_tokens_details\":{{\"cached_tokens\":{cached_tokens}}}}}}}}}\n\n",
            input_tokens + 40
        ),
    ]
}

#[test]
fn jsonl_persistence_reopen_fork_and_context_deduplication() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let store = Arc::new(JsonlSessionStore::new(&path));

    let events = Arc::new(Mutex::new(Vec::<SessionEvent>::new()));
    let events_sink = Arc::clone(&events);

    let mut mgr = SessionManager::open(
        store,
        SessionConfig::new("Base system instructions.", resident_tools()).with_cwd("/tmp/work"),
    )
    .unwrap();
    mgr.subscribe(move |ev| events_sink.lock().unwrap().push(ev.clone()));

    // First set_context queues; flushing emits ContextBlock + ContextAppended.
    assert!(mgr.set_context("env", "os=macos"));
    let flushed1 = mgr.flush_pending_context().unwrap();
    assert_eq!(flushed1.len(), 1);

    // Repeating exact same (name, value) is a strict NO-OP (preserves prompt cache).
    assert!(!mgr.set_context("env", "os=macos"));
    assert!(mgr.flush_pending_context().unwrap().is_empty());

    // Changing value emits a new <system-reminder>.
    assert!(mgr.set_context("env", "os=linux"));
    mgr.append_context("ephemeral note");
    let flushed2 = mgr.flush_pending_context().unwrap();
    assert_eq!(flushed2.len(), 2);

    // Append turns and branch with fork().
    let u1 = mgr
        .append_message(AgentMessage::Llm(Message::User(UserMessage::text(
            "Turn 1", 10,
        ))))
        .unwrap();
    let a1 = mgr
        .append_message(AgentMessage::Llm(Message::Assistant(AssistantMessage {
            content: vec![AssistantBlock::Text(TextContent::plain("Reply 1"))],
            api: ApiKind::OpenaiResponses,
            provider: "openai".into(),
            model: "gpt-5.4".into(),
            response_id: None,
            thinking_level: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            diagnostics: Vec::new(),
            timestamp: 11,
            duration_ms: None,
        })))
        .unwrap();
    mgr.append_message(AgentMessage::Llm(Message::User(UserMessage::text(
        "Branch A user",
        12,
    ))))
    .unwrap();
    mgr.append_model_change("anthropic", "claude-sonnet-4-6")
        .unwrap();
    mgr.append_thinking_change(ThinkingLevel::High, true)
        .unwrap();

    // Fork back to a1 without deleting Branch A.
    mgr.fork(&a1.id).unwrap();
    let u2_b = mgr
        .append_message(AgentMessage::Llm(Message::User(UserMessage::text(
            "Branch B user",
            20,
        ))))
        .unwrap();
    assert_eq!(u2_b.parent_id.as_deref(), Some(a1.id.as_str()));
    assert!(u1.parent_id.is_some());

    // Strict LF-delimited JSONL (no CRLF).
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(
        !raw.contains('\r'),
        "JSONL file must be strict LF-delimited"
    );
    assert!(raw.ends_with('\n'));

    // Reopen from disk and verify tree + reconstructed context.
    let reopened = SessionManager::open(
        Arc::new(JsonlSessionStore::new(&path)),
        SessionConfig::new("IGNORED_ON_REOPEN", vec![]),
    )
    .unwrap();
    assert_eq!(reopened.leaf_id(), Some(u2_b.id.as_str()));

    let tree = reopened.tree();
    assert_eq!(tree.len(), 1);
    // Walk down to a1 in tree and verify it has 2 sibling children (Branch A and Branch B).
    fn find_children_count(nodes: &[pikra_session::SessionTreeNode], id: &str) -> Option<usize> {
        for n in nodes {
            if n.entry.id == id {
                return Some(n.children.len());
            }
            if let Some(found) = find_children_count(&n.children, id) {
                return Some(found);
            }
        }
        None
    }
    assert_eq!(find_children_count(&tree, &a1.id), Some(2));

    let recon = reopened.reconstruct_context();
    assert_eq!(recon.system_prompt, "Base system instructions.");
    assert_eq!(recon.tools, resident_tools());
    assert!(matches!(
        recon.messages[0],
        AgentMessage::Llm(Message::System(_))
    ));

    let rendered_texts: Vec<String> = recon
        .messages
        .iter()
        .filter_map(|m| match m {
            AgentMessage::Llm(Message::User(u)) => match &u.content[0] {
                ContentPart::Text(t) => Some(t.text.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert!(
        rendered_texts
            .iter()
            .any(|t| t == "<system-reminder name=\"env\">\nos=linux\n</system-reminder>")
    );
    assert!(rendered_texts.iter().any(|t| t == "Branch B user"));
    assert!(!rendered_texts.iter().any(|t| t == "Branch A user"));

    let ctx_events = events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| matches!(e, SessionEvent::ContextAppended { .. }))
        .count();
    assert_eq!(ctx_events, 3);
}

#[test]
fn base64_image_exclusion_and_compaction_threshold_math() {
    // 200k window with huge 128k max_output_tokens must clamp reserve to 20k -> threshold 180k.
    let huge_out_model = test_model(200_000, 128_000);
    let default_cfg = CompactionConfig::default();
    assert_eq!(
        effective_reserve_tokens(&huge_out_model, &default_cfg),
        20_000
    );
    assert_eq!(compaction_threshold(200_000, 20_000), 180_000);
    assert!(!should_compact(179_999, &huge_out_model, &default_cfg));
    assert!(should_compact(180_000, &huge_out_model, &default_cfg));

    // Tiny window falls back to context_window / 2.
    let tiny_model = test_model(10_000, 16_384);
    assert_eq!(compaction_threshold(10_000, 16_384), 5_000);
    assert!(!should_compact(4_999, &tiny_model, &default_cfg));
    assert!(should_compact(5_000, &tiny_model, &default_cfg));

    // 2MB base64 screenshot must count as fixed 1,200 tokens, NOT 500,000 tokens.
    let two_mb_base64 = "A".repeat(2 * 1024 * 1024);
    let messages = vec![AgentMessage::Llm(Message::User(UserMessage {
        is_meta: false,
        id: None,
        content: vec![
            ContentPart::Text(TextContent::plain("Analyze this screenshot")),
            ContentPart::Image(ImageContent {
                data: two_mb_base64,
                mime_type: "image/png".into(),
            }),
        ],
        timestamp: 1,
    }))];

    let est = estimate_context_tokens(&messages);
    assert_eq!(est.total_tokens, IMAGE_TOKEN_ESTIMATE + 6);
    assert!(!should_compact(
        est.total_tokens,
        &huge_out_model,
        &default_cfg
    ));
}

#[test]
fn auto_compaction_preserves_prompt_prefix_folds_repeated_and_restores_context() {
    block_on(async {
        let transport = Arc::new(FauxTransport::new());
        let client = Client::new(transport.clone());

        // Context window 1000, reserve 200 -> threshold 800, summary cap min(120, 1000) = 120 chars.
        let model = test_model(1_000, 4_096);
        let compaction_cfg = CompactionConfig {
            enabled: true,
            compaction_reserve_tokens: Some(200),
            keep_recent_tokens: 0,
            max_summary_chars: 120,
            custom_instructions: None,
        };

        let mut mgr = SessionManager::in_memory(
            SessionConfig::new("You are a disciplined assistant.", resident_tools())
                .with_compaction(compaction_cfg),
        )
        .unwrap();

        let recorded_events = Arc::new(Mutex::new(Vec::<SessionEvent>::new()));
        let sink = Arc::clone(&recorded_events);
        mgr.subscribe(move |ev| sink.lock().unwrap().push(ev.clone()));

        assert!(mgr.set_context("skills", "<available_skills>deploy</available_skills>"));
        assert!(mgr.set_context("env", "cwd=/repo"));
        mgr.flush_pending_context().unwrap();

        mgr.append_message(AgentMessage::Llm(Message::User(UserMessage::text(
            "Start migration work",
            100,
        ))))
        .unwrap();

        // Turn 1 call through real Client -> reports 850 input tokens (exceeding threshold 800).
        transport.enqueue_sse(openai_sse_text("t1", "Step 1 complete.", 850, 200));
        let turn1_ctx = mgr.reconstruct_context().to_transcript_context();
        let a1 = client
            .complete(&model, &turn1_ctx, SimpleStreamOptions::default())
            .await;
        mgr.append_message(AgentMessage::Llm(Message::Assistant(a1)))
            .unwrap();

        // Auto-compaction #1: must send exact Turn 1 prefix + assistant + summarization prompt.
        let long_summary_1 = format!("Summary V1: {}", "x".repeat(200));
        transport.enqueue_sse(openai_sse_text("c1", &long_summary_1, 900, 850));
        let comp1 = mgr
            .auto_compact_if_needed(&client, &model, SimpleStreamOptions::default())
            .await
            .unwrap()
            .expect("compaction #1 must trigger");

        // Summary must be capped to <= 120 chars.
        assert!(comp1.summary.len() <= 120);
        assert!(comp1.summary.starts_with("Summary V1:"));

        // Wire-level prompt cache prefix check between Turn 1 and Compaction #1!
        let reqs = transport.requests();
        assert_eq!(reqs.len(), 2);
        assert_prompt_prefix_identical(&reqs[0], &reqs[1]);

        // Named context blocks ("skills", "env") MUST be automatically queued for restoration.
        assert!(mgr.has_pending_context());
        let restored_entries = mgr.flush_pending_context().unwrap();
        assert_eq!(restored_entries.len(), 2);
        let restored_names: Vec<String> = restored_entries
            .iter()
            .filter_map(|e| match &e.payload {
                SessionEntryPayload::ContextBlock { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            restored_names,
            vec!["skills".to_string(), "env".to_string()]
        );

        // Turn 2 after compaction #1 -> triggers Compaction #2 to verify folding (no stacking).
        mgr.append_message(AgentMessage::Llm(Message::User(UserMessage::text(
            "Now finish step 2",
            200,
        ))))
        .unwrap();
        transport.enqueue_sse(openai_sse_text("t2", "Step 2 complete.", 860, 400));
        let turn2_ctx = mgr.reconstruct_context().to_transcript_context();
        let a2 = client
            .complete(&model, &turn2_ctx, SimpleStreamOptions::default())
            .await;
        mgr.append_message(AgentMessage::Llm(Message::Assistant(a2)))
            .unwrap();

        transport.enqueue_sse(openai_sse_text("c2", "Folded Summary V2", 920, 860));
        let comp2 = mgr
            .auto_compact_if_needed(&client, &model, SimpleStreamOptions::default())
            .await
            .unwrap()
            .expect("compaction #2 must trigger");
        assert_eq!(comp2.summary, "Folded Summary V2");

        // Reconstructed context MUST keep SystemBaseline at index 0 and ONLY the latest summary.
        let final_recon = mgr.reconstruct_context();
        assert_eq!(
            final_recon.system_prompt,
            "You are a disciplined assistant."
        );
        assert_eq!(final_recon.tools, resident_tools());
        assert!(matches!(
            final_recon.messages[0],
            AgentMessage::Llm(Message::System(_))
        ));
        let summary_msgs: Vec<String> = final_recon
            .messages
            .iter()
            .filter_map(|m| match m {
                AgentMessage::Llm(Message::User(u)) => match &u.content[0] {
                    ContentPart::Text(t) if t.text.contains("<compaction-summary>") => {
                        Some(t.text.clone())
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(
            summary_msgs.len(),
            1,
            "repeated compactions must fold, never stack"
        );
        assert!(summary_msgs[0].contains("Folded Summary V2"));
        assert!(!summary_msgs[0].contains("Summary V1"));

        // Verify CompactionStart and CompactionEnd events emitted for both compactions.
        {
            let evs = recorded_events.lock().unwrap();
            assert_eq!(
                evs.iter()
                    .filter(|e| matches!(e, SessionEvent::CompactionStart { .. }))
                    .count(),
                2
            );
            assert_eq!(
                evs.iter()
                    .filter(|e| matches!(e, SessionEvent::CompactionEnd { .. }))
                    .count(),
                2
            );
        }

        // Verify custom CompactionStrategy injection overrides compaction completely.
        struct CustomStrategy;
        impl CompactionStrategy for CustomStrategy {
            fn compact<'a>(
                &'a self,
                _provider: &'a dyn StreamProvider,
                input: CompactionInput,
            ) -> BoxFuture<'a, Result<CompactionOutput, SessionError>> {
                Box::pin(async move {
                    Ok(CompactionOutput {
                        summary: format!(
                            "custom-injected(prev={})",
                            input.previous_summary.is_some()
                        ),
                        first_kept_entry_id: String::new(),
                        usage: None,
                    })
                })
            }
        }

        mgr.set_compaction_strategy(Arc::new(CustomStrategy));
        mgr.append_message(AgentMessage::Llm(Message::User(UserMessage::text(
            "Trigger custom strategy",
            300,
        ))))
        .unwrap();
        let custom_res = mgr
            .compact(&client, &model, SimpleStreamOptions::default())
            .await
            .unwrap()
            .expect("custom compaction should succeed");
        assert_eq!(custom_res.summary, "custom-injected(prev=true)");
    });
}

#[test]
fn review_regressions_baseline_update_flush_retry_output_tokens_and_oversized_turn() {
    use pikra_session::{MemorySessionStore, SessionEntry, SessionHeader, SessionStore};
    use std::sync::atomic::{AtomicUsize, Ordering};

    // 1. (#8) Latest SystemBaseline wins on reconstruct_context & reopen while keeping history.
    let dir = tempdir().unwrap();
    let path = dir.path().join("baseline.jsonl");
    let mut mgr = SessionManager::open(
        Arc::new(JsonlSessionStore::new(&path)),
        SessionConfig::new("System Prompt A", resident_tools()),
    )
    .unwrap();
    mgr.append_message(AgentMessage::Llm(Message::User(UserMessage::text(
        "Hello before update",
        10,
    ))))
    .unwrap();
    let mut updated_tools = resident_tools();
    updated_tools.push(ToolDefinition {
        name: "extra_tool".into(),
        description: "Extra tool".into(),
        parameters: obj_schema(Vec::<(&str, pikra_schema::Schema, bool)>::new()),
    });
    mgr.append_system_baseline("System Prompt B", updated_tools.clone())
        .unwrap();

    let recon = mgr.reconstruct_context();
    assert_eq!(recon.system_prompt, "System Prompt B");
    assert_eq!(recon.tools, updated_tools);
    assert_eq!(recon.messages.len(), 2);
    match &recon.messages[0] {
        AgentMessage::Llm(Message::System(sys)) => {
            assert_eq!(sys.content, "System Prompt B");
            assert_eq!(sys.tools_added, updated_tools);
        }
        other => panic!("expected System message at index 0, got {other:?}"),
    }

    let reopened = SessionManager::open(
        Arc::new(JsonlSessionStore::new(&path)),
        SessionConfig::new("IGNORED", vec![]),
    )
    .unwrap();
    let reopened_recon = reopened.reconstruct_context();
    assert_eq!(reopened_recon.system_prompt, "System Prompt B");
    assert_eq!(reopened_recon.tools, updated_tools);

    // 2. (#19) Transient store error during flush_pending_context preserves unwritten items for retry.
    struct FlakyAppendStore {
        inner: MemorySessionStore,
        fail_on_call: AtomicUsize,
        append_calls: AtomicUsize,
    }
    impl SessionStore for FlakyAppendStore {
        fn load(&self) -> Result<(Option<SessionHeader>, Vec<SessionEntry>), SessionError> {
            self.inner.load()
        }
        fn write_header(&self, header: &SessionHeader) -> Result<(), SessionError> {
            self.inner.write_header(header)
        }
        fn append_entry(&self, entry: &SessionEntry) -> Result<(), SessionError> {
            let n = self.append_calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n == self.fail_on_call.load(Ordering::SeqCst) {
                return Err(SessionError::Io(std::io::Error::other(
                    "simulated transient disk failure",
                )));
            }
            self.inner.append_entry(entry)
        }
    }

    // Call 1 = SystemBaseline, Call 2 = named "k1" (succeeds), Call 3 = named "k2" (fails once).
    let flaky_store = Arc::new(FlakyAppendStore {
        inner: MemorySessionStore::new(),
        fail_on_call: AtomicUsize::new(3),
        append_calls: AtomicUsize::new(0),
    });
    let mut flaky_mgr =
        SessionManager::open(flaky_store, SessionConfig::new("Base", vec![])).unwrap();
    assert!(flaky_mgr.set_context("k1", "v1"));
    assert!(flaky_mgr.set_context("k2", "v2"));
    flaky_mgr.append_context("ephemeral-1");
    assert!(flaky_mgr.flush_pending_context().is_err());
    assert!(
        flaky_mgr.has_pending_context(),
        "unwritten named and ephemeral context blocks must stay pending after error"
    );

    let retry_appended = flaky_mgr.flush_pending_context().unwrap();
    assert_eq!(
        retry_appended.len(),
        2,
        "only k2 and ephemeral-1 should flush on retry"
    );
    assert!(!flaky_mgr.has_pending_context());
    let ctx_names: Vec<String> = flaky_mgr
        .entries()
        .iter()
        .filter_map(|e| match &e.payload {
            SessionEntryPayload::ContextBlock { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(ctx_names, vec!["k1", "k2", "context"]);

    // 3. (#20) estimate_context_tokens includes the latest assistant message's output tokens.
    let large_assistant_with_usage = AgentMessage::Llm(Message::Assistant(AssistantMessage {
        content: vec![AssistantBlock::Text(TextContent::plain("short"))],
        api: ApiKind::OpenaiResponses,
        provider: "openai".into(),
        model: "gpt-5.4".into(),
        response_id: None,
        thinking_level: None,
        usage: Usage {
            input: 7_000,
            output: 10_000,
            cache_read: 0,
            cache_write: 0,
            total_tokens: 17_000,
            ..Usage::default()
        },
        stop_reason: StopReason::Stop,
        error_message: None,
        diagnostics: Vec::new(),
        timestamp: 1,
        duration_ms: None,
    }));
    let est = estimate_context_tokens(&[large_assistant_with_usage]);
    assert_eq!(est.reported_tokens, 7_000);
    assert_eq!(est.trailing_tokens, 10_000);
    assert_eq!(est.total_tokens, 17_000);

    // Fallback to local char estimate when usage.output == 0.
    let large_assistant_no_out = AgentMessage::Llm(Message::Assistant(AssistantMessage {
        content: vec![AssistantBlock::Text(TextContent::plain("a".repeat(40_000)))],
        api: ApiKind::OpenaiResponses,
        provider: "openai".into(),
        model: "gpt-5.4".into(),
        response_id: None,
        thinking_level: None,
        usage: Usage {
            input: 7_000,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            total_tokens: 7_000,
            ..Usage::default()
        },
        stop_reason: StopReason::Stop,
        error_message: None,
        diagnostics: Vec::new(),
        timestamp: 2,
        duration_ms: None,
    }));
    let est_fallback = estimate_context_tokens(&[large_assistant_no_out]);
    assert_eq!(est_fallback.total_tokens, 17_000);

    // 4. (#21) Oversized latest turn exceeding keep_recent_tokens must NOT be kept verbatim.
    let mut oversized_mgr = SessionManager::in_memory(
        SessionConfig::new("Base", vec![]).with_compaction(CompactionConfig {
            enabled: true,
            compaction_reserve_tokens: Some(200),
            keep_recent_tokens: 20,
            max_summary_chars: 1_000,
            custom_instructions: None,
        }),
    )
    .unwrap();
    oversized_mgr
        .append_message(AgentMessage::Llm(Message::User(UserMessage::text(
            "Turn 1", 1,
        ))))
        .unwrap();
    oversized_mgr
        .append_message(AgentMessage::Llm(Message::Assistant(AssistantMessage {
            content: vec![AssistantBlock::Text(TextContent::plain("Ok 1"))],
            api: ApiKind::OpenaiResponses,
            provider: "openai".into(),
            model: "gpt-5.4".into(),
            response_id: None,
            thinking_level: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            diagnostics: Vec::new(),
            timestamp: 2,
            duration_ms: None,
        })))
        .unwrap();
    oversized_mgr
        .append_message(AgentMessage::Llm(Message::User(UserMessage::text(
            "x".repeat(40_000),
            3,
        ))))
        .unwrap();
    oversized_mgr
        .append_message(AgentMessage::Llm(Message::Assistant(AssistantMessage {
            content: vec![AssistantBlock::Text(TextContent::plain("y".repeat(40_000)))],
            api: ApiKind::OpenaiResponses,
            provider: "openai".into(),
            model: "gpt-5.4".into(),
            response_id: None,
            thinking_level: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            diagnostics: Vec::new(),
            timestamp: 4,
            duration_ms: None,
        })))
        .unwrap();

    let model = test_model(100_000, 4_096);
    let prepared = oversized_mgr
        .prepare_compaction(&model, SimpleStreamOptions::default())
        .expect("compaction should be prepared");
    assert!(
        prepared.input.first_kept_entry_id.is_empty(),
        "oversized last turn exceeding keep_recent_tokens must be compacted, got kept id: {}",
        prepared.input.first_kept_entry_id
    );
}
