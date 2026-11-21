use futures_util::StreamExt;
use pikra_agent::{Agent, AgentConfig, AgentMessage};
use pikra_ai::{
    ApiKind, AssistantBlock, AssistantMessage, AssistantMessageEvent, ContentPart, Message, Model,
    ModelCost, StopReason, TextContent, ToolCall, ToolResultMessage, Usage,
};
use pikra_testkit::FauxProvider;
use pikra_tools::{BuiltinToolKind, BuiltinToolSet, WorkspaceTools, WorkspaceToolsConfig};
use serde_json::{Map, Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

fn test_model() -> Model {
    Model {
        id: "test-model".into(),
        name: "Test Model".into(),
        provider: "faux".into(),
        api: ApiKind::OpenaiResponses,
        base_url: "https://example.invalid".into(),
        context_window: 128_000,
        max_output_tokens: 4_096,
        supports_images: false,
        supports_reasoning: false,
        cost: ModelCost::default(),
        headers: Vec::new(),
        compat: None,
    }
}

fn make_tool_call(id: &str, name: &str, args: Value) -> ToolCall {
    let arguments = match args {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
        item_id: None,
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    }
}

fn turn_events(
    model: &Model,
    blocks: Vec<AssistantBlock>,
    stop_reason: StopReason,
) -> Vec<AssistantMessageEvent> {
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
    let final_msg = AssistantMessage {
        content: blocks,
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_id: Some("resp_1".into()),
        thinking_level: None,
        usage: Usage::default(),
        stop_reason,
        error_message: None,
        diagnostics: Vec::new(),
        timestamp: 2,
        duration_ms: Some(1),
    };
    vec![
        AssistantMessageEvent::Start { partial },
        AssistantMessageEvent::Done {
            reason: stop_reason,
            message: final_msg,
        },
    ]
}

fn tool_results_from(messages: &[AgentMessage]) -> Vec<&ToolResultMessage> {
    messages
        .iter()
        .filter_map(|m| match m {
            AgentMessage::Llm(Message::ToolResult(tr)) => Some(tr),
            _ => None,
        })
        .collect()
}

fn result_text(tr: &ToolResultMessage) -> String {
    tr.content
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text(t) => Some(t.text.as_str()),
            ContentPart::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

#[tokio::test]
async fn workspace_write_read_edit_find_e2e() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    // Seed ignored directories and a binary file to verify find skipping & read binary rejection.
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::create_dir_all(root.join("target/debug")).unwrap();
    fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
    fs::write(root.join(".git/config"), "UNIQUE_NEEDLE in git\n").unwrap();
    fs::write(root.join("target/debug/out.rs"), "UNIQUE_NEEDLE in target\n").unwrap();
    fs::write(root.join("node_modules/pkg/index.js"), "UNIQUE_NEEDLE in node_modules\n").unwrap();
    fs::write(root.join("blob.bin"), [0x89, b'P', b'N', b'G', 0x00, 0xFF]).unwrap();
    fs::write(root.join("empty.txt"), "").unwrap();
    fs::write(root.join("crlf.txt"), "alpha\r\nbeta\r\ngamma\r\n").unwrap();

    let cfg = WorkspaceToolsConfig::new(root).with_enabled_tools(BuiltinToolSet::from_kinds(&[
        BuiltinToolKind::Find,
        BuiltinToolKind::Write,
        BuiltinToolKind::Read,
        BuiltinToolKind::Edit,
    ]));
    let workspace_tools = WorkspaceTools::new(cfg);

    // Deterministic canonical tool order & exact schema property ordering.
    let defs_a = workspace_tools.definitions();
    let defs_b = workspace_tools.definitions();
    assert_eq!(
        defs_a.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
        vec!["read", "write", "edit", "find"]
    );
    assert_eq!(
        serde_json::to_vec(&defs_a).unwrap(),
        serde_json::to_vec(&defs_b).unwrap()
    );
    let read_props: Vec<&String> = defs_a[0].parameters["properties"]
        .as_object()
        .unwrap()
        .keys()
        .collect();
    assert_eq!(read_props, vec!["path", "offset", "limit"]);

    let model = test_model();
    let provider = Arc::new(FauxProvider::new());

    // Turn 1: write nested file.
    provider.enqueue_events(turn_events(
        &model,
        vec![AssistantBlock::ToolCall(make_tool_call(
            "c_write",
            "write",
            json!({
                "path": "src/app.rs",
                "content": "fn main() {\n    let a = 1;\n    let b = 2;\n    println!(\"UNIQUE_NEEDLE\");\n}\n"
            }),
        ))],
        StopReason::ToolUse,
    ));

    // Turn 2: parallel reads (offset/limit, out-of-range offset, empty file, binary rejection).
    provider.enqueue_events(turn_events(
        &model,
        vec![
            AssistantBlock::ToolCall(make_tool_call(
                "c_read_slice",
                "read",
                json!({ "path": "src/app.rs", "offset": 2, "limit": 2 }),
            )),
            AssistantBlock::ToolCall(make_tool_call(
                "c_read_oor",
                "read",
                json!({ "path": "src/app.rs", "offset": 99 }),
            )),
            AssistantBlock::ToolCall(make_tool_call(
                "c_read_empty",
                "read",
                json!({ "path": "empty.txt" }),
            )),
            AssistantBlock::ToolCall(make_tool_call(
                "c_read_bin",
                "read",
                json!({ "path": "blob.bin" }),
            )),
        ],
        StopReason::ToolUse,
    ));

    // Turn 3: edit error (missing oldText -> model self-corrects next turn), plus CRLF single edit.
    provider.enqueue_events(turn_events(
        &model,
        vec![
            AssistantBlock::ToolCall(make_tool_call(
                "c_edit_miss",
                "edit",
                json!({
                    "path": "src/app.rs",
                    "oldText": "let nonexistent = 42;",
                    "newText": "let x = 42;"
                }),
            )),
            AssistantBlock::ToolCall(make_tool_call(
                "c_edit_crlf",
                "edit",
                json!({
                    "path": "crlf.txt",
                    "oldText": "alpha\nbeta",
                    "newText": "alpha\nBETA_UPDATED"
                }),
            )),
        ],
        StopReason::ToolUse,
    ));

    // Turn 4: self-corrected multi-hunk atomic edit on src/app.rs.
    provider.enqueue_events(turn_events(
        &model,
        vec![AssistantBlock::ToolCall(make_tool_call(
            "c_edit_multi",
            "edit",
            json!({
                "path": "src/app.rs",
                "edits": [
                    { "oldText": "let a = 1;", "newText": "let a = 10;" },
                    { "oldText": "let b = 2;", "newText": "let b = 20;" }
                ]
            }),
        ))],
        StopReason::ToolUse,
    ));

    // Turn 5: find by content substring AND find by glob (both skipping .git/target/node_modules).
    provider.enqueue_events(turn_events(
        &model,
        vec![
            AssistantBlock::ToolCall(make_tool_call(
                "c_find_content",
                "find",
                json!({ "pattern": "UNIQUE_NEEDLE" }),
            )),
            AssistantBlock::ToolCall(make_tool_call(
                "c_find_glob",
                "find",
                json!({ "pattern": "**/*.rs" }),
            )),
        ],
        StopReason::ToolUse,
    ));

    // Turn 6: final assistant text.
    provider.enqueue_events(turn_events(
        &model,
        vec![AssistantBlock::Text(TextContent::plain("Workspace verified."))],
        StopReason::Stop,
    ));

    let agent = Agent::new(
        provider.clone(),
        AgentConfig::new(model)
            .with_system_prompt("workspace-e2e")
            .with_tools(workspace_tools.tools()),
    );

    let messages = agent.prompt("exercise workspace tools").await.unwrap();
    let results = tool_results_from(&messages);

    let by_id = |id: &str| -> &ToolResultMessage {
        results
            .iter()
            .copied()
            .find(|r| r.tool_call_id == id)
            .unwrap_or_else(|| panic!("missing tool result for {id}"))
    };

    assert!(!by_id("c_write").is_error);

    let read_slice = by_id("c_read_slice");
    assert!(!read_slice.is_error);
    let slice_text = result_text(read_slice);
    assert!(slice_text.contains("2:     let a = 1;\n3:     let b = 2;"));
    assert!(slice_text.contains("Showing lines 2-3 of 5"));

    let read_oor = by_id("c_read_oor");
    assert!(!read_oor.is_error);
    assert!(result_text(read_oor).contains("beyond end of file"));

    let read_empty = by_id("c_read_empty");
    assert!(!read_empty.is_error);
    assert_eq!(result_text(read_empty), "(empty file)");

    let read_bin = by_id("c_read_bin");
    assert!(read_bin.is_error);
    assert!(result_text(read_bin).contains("binary"));

    let edit_miss = by_id("c_edit_miss");
    assert!(edit_miss.is_error);
    assert!(result_text(edit_miss).contains("oldText not found"));

    let edit_crlf = by_id("c_edit_crlf");
    assert!(!edit_crlf.is_error);
    assert_eq!(
        fs::read_to_string(root.join("crlf.txt")).unwrap(),
        "alpha\r\nBETA_UPDATED\r\ngamma\r\n"
    );

    let edit_multi = by_id("c_edit_multi");
    assert!(!edit_multi.is_error);
    let edited_app = fs::read_to_string(root.join("src/app.rs")).unwrap();
    assert!(edited_app.contains("let a = 10;"));
    assert!(edited_app.contains("let b = 20;"));

    let find_content = by_id("c_find_content");
    assert!(!find_content.is_error);
    let content_hits = result_text(find_content);
    assert!(content_hits.contains("src/app.rs:4:"));
    assert!(!content_hits.contains(".git"));
    assert!(!content_hits.contains("target"));
    assert!(!content_hits.contains("node_modules"));

    let find_glob = by_id("c_find_glob");
    assert!(!find_glob.is_error);
    assert_eq!(result_text(find_glob).trim(), "src/app.rs");

    // Verify tool definitions stayed byte-identical across all LLM requests (prompt cache invariant).
    let calls = provider.recorded_calls();
    assert_eq!(calls.len(), 6);
    let first_sys = serde_json::to_vec(&calls[0].context.messages[0]).unwrap();
    for call in &calls[1..] {
        assert_eq!(serde_json::to_vec(&call.context.messages[0]).unwrap(), first_sys);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn bash_env_provider_utf8_truncation_and_process_group_cancel() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();

    // Create FIFOs for deterministic, zero-sleep child + grandchild synchronization.
    let alive_fifo = root.join("alive.fifo");
    let block_fifo = root.join("block.fifo");
    for fifo in [&alive_fifo, &block_fifo] {
        let c_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo failed for {}", fifo.display());
    }

    let alive_env = alive_fifo.to_string_lossy().to_string();
    let block_env = block_fifo.to_string_lossy().to_string();

    let max_output_bytes = 200usize;
    let cfg = WorkspaceToolsConfig::new(&root)
        .with_enabled_tools([BuiltinToolKind::Bash])
        .with_max_output_bytes(max_output_bytes)
        .with_env_provider(Arc::new(move || {
            let alive = alive_env.clone();
            let block = block_env.clone();
            Box::pin(async move {
                vec![
                    ("HOST_INJECTED_TOKEN".into(), "from-login-shell-42".into()),
                    ("ALIVE_FIFO".into(), alive),
                    ("BLOCK_FIFO".into(), block),
                ]
            })
        }));

    let model = test_model();
    let provider = Arc::new(FauxProvider::new());

    // Run 1: verify env_provider + AI_AGENT=pikra, UTF-8-safe head+tail truncation on multi-byte output,
    // and non-zero exit code surfacing.
    provider.enqueue_events(turn_events(
        &model,
        vec![
            AssistantBlock::ToolCall(make_tool_call(
                "c_env",
                "bash",
                json!({ "command": "printf '%s|%s' \"$HOST_INJECTED_TOKEN\" \"$AI_AGENT\"" }),
            )),
            AssistantBlock::ToolCall(make_tool_call(
                "c_utf8_trunc",
                "bash",
                json!({
                    "command": "printf 'HEAD_开始_'; i=0; while [ $i -lt 80 ]; do printf '你好世界🚀'; i=$((i+1)); done; printf '_结束_TAIL'"
                }),
            )),
            AssistantBlock::ToolCall(make_tool_call(
                "c_nonzero",
                "bash",
                json!({ "command": "printf 'boom'; exit 7" }),
            )),
        ],
        StopReason::ToolUse,
    ));
    provider.enqueue_events(turn_events(
        &model,
        vec![AssistantBlock::Text(TextContent::plain("bash checks ok"))],
        StopReason::Stop,
    ));

    let agent = Arc::new(Agent::new(
        provider.clone(),
        AgentConfig::new(model.clone())
            .with_system_prompt("bash-e2e")
            .with_tools(WorkspaceTools::build(cfg)),
    ));

    let msgs = agent.prompt("check bash env and truncation").await.unwrap();
    let results = tool_results_from(&msgs);
    let by_id = |id: &str| -> &ToolResultMessage {
        results.iter().copied().find(|r| r.tool_call_id == id).unwrap()
    };

    let env_res = by_id("c_env");
    assert!(!env_res.is_error);
    assert_eq!(result_text(env_res), "from-login-shell-42|pikra");

    let trunc_res = by_id("c_utf8_trunc");
    assert!(!trunc_res.is_error);
    let trunc_text = result_text(trunc_res);
    assert!(
        trunc_text.len() <= max_output_bytes,
        "truncated output exceeded budget: {} > {max_output_bytes}",
        trunc_text.len()
    );
    assert!(trunc_text.starts_with("HEAD_开始_"), "missing UTF-8 head: {trunc_text:?}");
    assert!(trunc_text.ends_with("_结束_TAIL"), "missing UTF-8 tail: {trunc_text:?}");
    assert!(trunc_text.contains("[... truncated "), "missing truncation marker: {trunc_text:?}");
    assert!(trunc_text.contains(" bytes ...]"));
    assert!(!trunc_text.contains('\u{FFFD}'), "UTF-8 char was corrupted: {trunc_text:?}");

    let nonzero_res = by_id("c_nonzero");
    assert!(nonzero_res.is_error);
    let nonzero_text = result_text(nonzero_res);
    assert!(nonzero_text.contains("boom"));
    assert!(nonzero_text.contains("code 7"));

    // Run 2: spawn a shell with a grandchild holding ALIVE_FIFO write-end open and blocking on BLOCK_FIFO.
    // Abort via Agent (CancelToken) once grandchild signals readiness, and verify the kernel closes
    // ALIVE_FIFO immediately (proving kill(-pgid, SIGKILL) killed the grandchild without leaking).
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<i32>();
    let (eof_tx, eof_rx) = tokio::sync::oneshot::channel::<()>();
    let alive_fifo_reader = alive_fifo.clone();
    let reader_thread = std::thread::spawn(move || {
        let file = fs::File::open(&alive_fifo_reader).expect("open alive_fifo for read");
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        reader.read_line(&mut line).expect("read grandchild pid");
        let pid: i32 = line.trim().parse().expect("valid numeric pid");
        let _ = ready_tx.send(pid);
        let mut rest = Vec::new();
        let _ = reader.read_to_end(&mut rest);
        let _ = eof_tx.send(());
    });

    provider.enqueue_events(turn_events(
        &model,
        vec![AssistantBlock::ToolCall(make_tool_call(
            "c_hang_pg",
            "bash",
            json!({
                // Trailing `; true` prevents outer `/bin/sh` from tail-execing the inner `sh`,
                // ensuring a true parent + grandchild process tree in the same process group.
                "command": "sh -c 'exec 3>\"$ALIVE_FIFO\"; printf \"%s\\n\" \"$$\" >&3; exec cat < \"$BLOCK_FIFO\"'; true"
            }),
        ))],
        StopReason::ToolUse,
    ));
    // Run 3 after abort: proves agent settles cleanly without spurious busy error.
    provider.enqueue_events(turn_events(
        &model,
        vec![AssistantBlock::Text(TextContent::plain("recovered cleanly"))],
        StopReason::Stop,
    ));

    let agent_run = Arc::clone(&agent);
    let drain_task = tokio::spawn(async move {
        let mut stream = agent_run.prompt_stream("run hanging grandchild").unwrap();
        let mut events = Vec::new();
        while let Some(ev) = stream.next().await {
            events.push(ev);
        }
        events
    });

    let grandchild_pid = tokio::time::timeout(Duration::from_secs(5), ready_rx)
        .await
        .expect("grandchild did not report ready in time")
        .expect("ready channel closed");
    assert!(grandchild_pid > 0);
    assert_eq!(
        unsafe { libc::kill(grandchild_pid, 0) },
        0,
        "grandchild {grandchild_pid} should be alive before abort"
    );

    tokio::time::timeout(Duration::from_secs(5), agent.abort())
        .await
        .expect("agent.abort() hung instead of killing process group");

    tokio::time::timeout(Duration::from_secs(5), eof_rx)
        .await
        .expect("grandchild process leaked (ALIVE_FIFO write-end never closed after SIGKILL)")
        .expect("eof channel closed");

    let _events = tokio::time::timeout(Duration::from_secs(5), drain_task)
        .await
        .expect("event stream did not close after abort")
        .expect("drain task panicked");
    reader_thread.join().expect("reader thread panicked");

    // Immediate next prompt succeeds without spurious busy error.
    let recovery = agent.prompt("ping after abort").await.unwrap();
    assert!(!recovery.is_empty());
}
