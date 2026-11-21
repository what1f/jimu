use futures_executor::block_on;
use pikra_agent::{Agent, AgentConfig, AgentMessage, AgentToolResult, ClosureTool};
use pikra_ai::{
    ApiKind, AssistantBlock, AssistantMessage, AssistantMessageEvent, ContentPart, Message, Model,
    ModelCost, StopReason, TextContent, ThinkingLevel, ToolCall, ToolDefinition, Usage,
};
use pikra_js::{HookKind, JsExtensionConfig, JsExtensionError, JsExtensionRuntime};
use pikra_schema::{obj_schema, str_schema};
use pikra_testkit::FauxProvider;
use serde_json::{Map, Value, json};
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::tempdir;

fn test_model() -> Model {
    Model {
        id: "faux-js-model".into(),
        name: "Faux JS Model".into(),
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

fn tool_call(id: &str, name: &str, args: Value) -> ToolCall {
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

fn stream_events(
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
    let mut events = vec![AssistantMessageEvent::Start { partial }];
    for (idx, block) in blocks.iter().enumerate() {
        match block {
            AssistantBlock::Text(tc) => {
                events.push(AssistantMessageEvent::TextStart { content_index: idx });
                // Emit multiple token deltas to verify QuickJS is never invoked per streaming delta.
                for chunk in tc.text.as_bytes().chunks(3) {
                    events.push(AssistantMessageEvent::TextDelta {
                        content_index: idx,
                        delta: String::from_utf8_lossy(chunk).into_owned(),
                    });
                }
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
        usage: Usage::default(),
        stop_reason,
        error_message: None,
        diagnostics: Vec::new(),
        timestamp: 2,
        duration_ms: Some(1),
    };
    events.push(AssistantMessageEvent::Done {
        reason: stop_reason,
        message: final_msg,
    });
    events
}

fn tool_result_texts(messages: &[AgentMessage]) -> Vec<(String, bool, String)> {
    messages
        .iter()
        .filter_map(|m| match m {
            AgentMessage::Llm(Message::ToolResult(tr)) => {
                let text = tr
                    .content
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text(t) => Some(t.text.as_str()),
                        ContentPart::Image(_) => None,
                    })
                    .collect::<String>();
                Some((tr.tool_call_id.clone(), tr.is_error, text))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn js_extension_full_lifecycle_tool_and_hooks_e2e() {
    // Combines an ESM extension (`export default async function(pikra)`) with a script-style
    // extension (`pikra.on(...)`) covering custom async tools and all control-plane hooks.
    let esm_ext = r#"
export default async function (pikra) {
  const prefix = await Promise.resolve("Hello");

  pikra.registerTool({
    name: "greet",
    description: "Greet a user asynchronously",
    parameters: {
      type: "object",
      properties: { name: { type: "string" } },
      required: ["name"],
    },
    executionMode: "sequential",
    async execute(toolCallId, params) {
      const greeting = await Promise.resolve(`${prefix}, ${params.name}! token=SECRET_TOKEN_123 (${toolCallId})`);
      return {
        content: [{ type: "text", text: greeting }],
        details: { greeted: params.name },
        structuredContent: { name: params.name },
      };
    },
  });

  pikra.on("prepare_request", (snapshot) => ({
    systemPrompt: `${snapshot.systemPrompt} [js-prepared]`,
    thinkingLevel: "medium",
    fast: true,
  }));

  pikra.on("before_tool_call", ({ toolCall, args }) => {
    if (toolCall.name === "shell" && typeof args.cmd === "string" && args.cmd.includes("rm -rf")) {
      return { block: true, reason: "Dangerous command blocked by JS policy" };
    }
    if (toolCall.name === "greet" && args.name === "alice") {
      return { argsOverride: { name: "Alice (verified)" } };
    }
  });

  pikra.on("after_tool_call", ({ result }) => {
    const first = result.content?.[0]?.text ?? "";
    if (first.includes("SECRET_TOKEN_123")) {
      return {
        content: [{ type: "text", text: first.replace("SECRET_TOKEN_123", "[REDACTED]") }],
      };
    }
  });
}
"#;

    let script_ext = r#"
let extraTurnUsed = false;
pikra.on("finish_turn", ({ toolResults }) => {
  if (toolResults.length === 0 && !extraTurnUsed) {
    extraTurnUsed = true;
    return { action: "continue" };
  }
  if (extraTurnUsed && toolResults.length === 0) {
    return { action: "end" };
  }
});
"#;

    let runtime = Arc::new(
        JsExtensionRuntime::new(
            JsExtensionConfig::new()
                .with_inline("esm-ext.js", esm_ext)
                .with_inline("script-ext.js", script_ext),
        )
        .expect("runtime loads ESM and script extensions"),
    );

    assert!(runtime.has_hook(HookKind::PrepareRequest));
    assert!(!runtime.has_hook(HookKind::TransformContext));
    assert!(runtime.has_hook(HookKind::BeforeToolCall));
    assert!(runtime.has_hook(HookKind::AfterToolCall));
    assert!(runtime.has_hook(HookKind::FinishTurn));

    let shell_executions = Arc::new(AtomicUsize::new(0));
    let shell_counter = Arc::clone(&shell_executions);
    let shell_tool = Arc::new(ClosureTool::new(
        ToolDefinition {
            name: "shell".into(),
            description: "Run shell command".into(),
            parameters: obj_schema(vec![("cmd", str_schema(), true)]),
        },
        move |_id, _args, _cancel, _on_update| {
            shell_counter.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(AgentToolResult::text("executed")) })
        },
    ));

    let model = test_model();
    let provider = Arc::new(FauxProvider::new());

    // Turn 1: model issues a dangerous `shell` call (blocked) and a `greet` call (args overridden + secret redacted).
    provider.enqueue_events(stream_events(
        &model,
        vec![
            AssistantBlock::ToolCall(tool_call("tc_1", "shell", json!({ "cmd": "rm -rf /tmp" }))),
            AssistantBlock::ToolCall(tool_call("tc_2", "greet", json!({ "name": "alice" }))),
        ],
        StopReason::ToolUse,
    ));
    // Turn 2: model returns text; `finish_turn` asks for one `continue` turn.
    provider.enqueue_events(stream_events(
        &model,
        vec![AssistantBlock::Text(TextContent::plain(
            "Initial summary before final check",
        ))],
        StopReason::Stop,
    ));
    // Turn 3: model returns final text; `finish_turn` returns `end`.
    provider.enqueue_events(stream_events(
        &model,
        vec![AssistantBlock::Text(TextContent::plain(
            "All verified and finalized.",
        ))],
        StopReason::Stop,
    ));

    let mut tools = runtime.tools();
    tools.push(shell_tool);

    let agent = Agent::new(
        Arc::clone(&provider) as _,
        AgentConfig::new(model)
            .with_system_prompt("base-system")
            .with_tools(tools)
            .with_middleware(Arc::clone(&runtime) as _),
    );

    let new_messages = block_on(agent.prompt("Please greet alice and clean up"))
        .expect("multi-turn prompt succeeds");

    // 1. Dangerous `shell` call was blocked before execution.
    assert_eq!(shell_executions.load(Ordering::SeqCst), 0);

    // 2. Verify tool results: blocked shell error + overridden & redacted greet output.
    let results = tool_result_texts(&new_messages);
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].0, "tc_1");
    assert!(results[0].1);
    assert!(results[0].2.contains("Dangerous command blocked"));

    assert_eq!(results[1].0, "tc_2");
    assert!(!results[1].1);
    assert_eq!(
        results[1].2,
        "Hello, Alice (verified)! token=[REDACTED] (tc_2)"
    );

    // 3. Verify `prepare_request` overrides & `finish_turn` continuation produced 3 LLM calls.
    let calls = provider.recorded_calls();
    assert_eq!(calls.len(), 3);
    for call in &calls {
        assert_eq!(call.options.reasoning, Some(ThinkingLevel::Medium));
        assert_eq!(call.options.fast, Some(true));
        let sys_text = match &call.context.messages[0] {
            Message::System(s) => s.content.as_str(),
            _ => "",
        };
        assert_eq!(sys_text, "base-system [js-prepared]");
    }

    // 4. Performance guardrail: only control-plane hooks invoked QuickJS (never per-token deltas).
    // 3x prepare_request + 2x before_tool_call + 1x after_tool_call (blocked shell skipped execute) + 3x finish_turn = 9.
    assert_eq!(runtime.js_hook_invocations(), 9);
    assert!(runtime.last_error().is_none());
}

#[test]
fn js_extension_file_reload_swaps_behavior_mid_session() {
    let dir = tempdir().expect("create tempdir");
    let ext_path = dir.path().join("ext.js");

    fs::write(
        &ext_path,
        r#"
export default function (pikra) {
  pikra.registerTool({
    name: "greet",
    description: "Greet v1",
    parameters: {
      type: "object",
      properties: { name: { type: "string" } },
      required: ["name"],
    },
    execute(_id, params) {
      return `v1:${params.name}`;
    },
  });

  pikra.on("prepare_request", () => ({ systemPrompt: "sys-v1" }));
}
"#,
    )
    .expect("write v1 extension");

    let runtime = Arc::new(
        JsExtensionRuntime::new(JsExtensionConfig::new().with_file(dir.path()))
            .expect("load directory-backed extension"),
    );

    let model = test_model();
    let provider = Arc::new(FauxProvider::new());
    let agent = Agent::new(
        Arc::clone(&provider) as _,
        AgentConfig::new(model.clone())
            .with_system_prompt("orig")
            .with_tools(runtime.tools())
            .with_middleware(Arc::clone(&runtime) as _),
    );

    // Session turn 1 with v1 extension.
    provider.enqueue_events(stream_events(
        &model,
        vec![AssistantBlock::ToolCall(tool_call(
            "tc_v1",
            "greet",
            json!({ "name": "world" }),
        ))],
        StopReason::ToolUse,
    ));
    provider.enqueue_events(stream_events(
        &model,
        vec![AssistantBlock::Text(TextContent::plain("done v1"))],
        StopReason::Stop,
    ));

    let turn1_msgs = block_on(agent.prompt("run v1")).expect("turn 1 succeeds");
    let turn1_tools = tool_result_texts(&turn1_msgs);
    assert_eq!(
        turn1_tools,
        vec![("tc_v1".into(), false, "v1:world".into())]
    );

    // Update extension file on disk to v2 with new tool behavior + new hook rules.
    fs::write(
        &ext_path,
        r#"
export default function (pikra) {
  pikra.registerTool({
    name: "greet",
    description: "Greet v2",
    parameters: {
      type: "object",
      properties: { name: { type: "string" } },
      required: ["name"],
    },
    async execute(_id, params) {
      return `v2:${params.name.toUpperCase()}`;
    },
  });

  pikra.on("prepare_request", () => ({ systemPrompt: "sys-v2" }));
  pikra.on("transform_context", (messages) =>
    messages.map((m) => {
      if (m.kind === "llm" && m.role === "user" && m.content?.[0]?.type === "text") {
        return {
          ...m,
          content: [{ type: "text", text: `${m.content[0].text} [ctx-v2]` }],
        };
      }
      return m;
    })
  );
  pikra.on("before_tool_call", ({ args }) => {
    if (args.name === "forbidden") {
      return { block: true, reason: "blocked by v2 hook" };
    }
  });
}
"#,
    )
    .expect("write v2 extension");

    runtime.reload().expect("reload v2 succeeds");
    assert!(runtime.has_hook(HookKind::BeforeToolCall));
    assert!(runtime.has_hook(HookKind::TransformContext));

    // Verify failed reload is atomic and preserves v2 active VM.
    fs::write(&ext_path, "export default function ( { BROKEN SYNTAX").expect("write broken ext");
    assert!(runtime.reload().is_err());

    // Session turn 2 runs on the same Agent with reloaded v2 behavior intact.
    provider.enqueue_events(stream_events(
        &model,
        vec![
            AssistantBlock::ToolCall(tool_call("tc_v2_ok", "greet", json!({ "name": "world" }))),
            AssistantBlock::ToolCall(tool_call(
                "tc_v2_blk",
                "greet",
                json!({ "name": "forbidden" }),
            )),
        ],
        StopReason::ToolUse,
    ));
    provider.enqueue_events(stream_events(
        &model,
        vec![AssistantBlock::Text(TextContent::plain("done v2"))],
        StopReason::Stop,
    ));

    let turn2_msgs = block_on(agent.prompt("run v2")).expect("turn 2 succeeds");
    let turn2_tools = tool_result_texts(&turn2_msgs);
    assert_eq!(
        turn2_tools,
        vec![
            ("tc_v2_ok".into(), false, "v2:WORLD".into()),
            ("tc_v2_blk".into(), true, "blocked by v2 hook".into()),
        ]
    );

    let calls = provider.recorded_calls();
    assert_eq!(calls.len(), 4);
    let sys_of = |idx: usize| match &calls[idx].context.messages[0] {
        Message::System(s) => s.content.clone(),
        _ => String::new(),
    };
    assert_eq!(sys_of(0), "sys-v1");
    assert_eq!(sys_of(2), "sys-v2");
    let saw_transformed_user = calls[2].context.messages.iter().any(|m| match m {
        Message::User(u) => u.content.iter().any(|p| match p {
            ContentPart::Text(t) => t.text.contains("[ctx-v2]"),
            ContentPart::Image(_) => false,
        }),
        _ => false,
    });
    assert!(
        saw_transformed_user,
        "expected transform_context v2 marker in turn 2 request"
    );
}

#[test]
fn js_extension_runaway_hook_interrupted_by_timeout() {
    let runaway_ext = r#"
pikra.on("prepare_request", () => {
  while (true) {}
});

pikra.on("before_tool_call", () => {
  while (true) {}
});

pikra.registerTool({
  name: "echo",
  description: "Echo text",
  parameters: {
    type: "object",
    properties: { text: { type: "string" } },
    required: ["text"],
  },
  execute(_id, params) {
    return params.text;
  },
});
"#;

    let runtime = Arc::new(
        JsExtensionRuntime::new(
            JsExtensionConfig::new()
                .with_timeout_ms(50)
                .with_inline("runaway.js", runaway_ext),
        )
        .expect("load runaway extension"),
    );

    let model = test_model();
    let provider = Arc::new(FauxProvider::new());
    provider.enqueue_events(stream_events(
        &model,
        vec![AssistantBlock::ToolCall(tool_call(
            "tc_hang",
            "echo",
            json!({ "text": "hi" }),
        ))],
        StopReason::ToolUse,
    ));
    provider.enqueue_events(stream_events(
        &model,
        vec![AssistantBlock::Text(TextContent::plain(
            "Recovered after hook timeout",
        ))],
        StopReason::Stop,
    ));

    let agent = Agent::new(
        Arc::clone(&provider) as _,
        AgentConfig::new(model)
            .with_system_prompt("safe-default")
            .with_tools(runtime.tools())
            .with_middleware(Arc::clone(&runtime) as _),
    );

    let msgs = block_on(agent.prompt("trigger runaway hooks"))
        .expect("agent finishes cleanly despite runaway JS hooks");

    let errors = runtime.take_errors();
    assert!(
        errors.len() >= 2,
        "expected timeout errors from prepare_request and before_tool_call, got: {errors:?}"
    );
    assert!(
        errors
            .iter()
            .all(|e| matches!(e, JsExtensionError::Timeout { timeout_ms: 50 }))
    );

    // `before_tool_call` timeout fails safe by blocking the tool call rather than panicking or hanging.
    let tool_results = tool_result_texts(&msgs);
    assert_eq!(tool_results.len(), 1);
    assert!(tool_results[0].1);
    assert!(tool_results[0].2.contains("timed out"));

    // Final assistant response still settled cleanly.
    let last_text = msgs.iter().rev().find_map(|m| match m {
        AgentMessage::Llm(Message::Assistant(a)) => Some(a.text_output()),
        _ => None,
    });
    assert_eq!(last_text.as_deref(), Some("Recovered after hook timeout"));
}
