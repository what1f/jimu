use futures_executor::block_on;
use jimu_agent::{Agent, AgentConfig, AgentMessage, AgentToolResult, ClosureTool};
use jimu_ai::{
    ApiKind, AssistantBlock, AssistantMessage, AssistantMessageEvent, ContentPart, Message, Model,
    ModelCost, StopReason, TextContent, ThinkingLevel, ToolCall, ToolDefinition, Usage,
};
use jimu_js::{
    DEFAULT_UI_RENDER_TIMEOUT_MS, HookKind, JsExtensionConfig, JsExtensionError,
    JsExtensionRuntime, UiBorder, UiComponent, UiDirection, UiNode, UiTextStyle,
};
use jimu_schema::{obj_schema, str_schema};
use jimu_testkit::FauxProvider;
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
    // Combines an ESM extension (`export default async function(jimu)`) with a script-style
    // extension (`jimu.on(...)`) covering custom async tools and all control-plane hooks.
    let esm_ext = r#"
export default async function (jimu) {
  const prefix = await Promise.resolve("Hello");

  jimu.registerTool({
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

  jimu.on("prepare_request", (snapshot) => ({
    systemPrompt: `${snapshot.systemPrompt} [js-prepared]`,
    thinkingLevel: "medium",
    fast: true,
  }));

  jimu.on("before_tool_call", ({ toolCall, args }) => {
    if (toolCall.name === "shell" && typeof args.cmd === "string" && args.cmd.includes("rm -rf")) {
      return { block: true, reason: "Dangerous command blocked by JS policy" };
    }
    if (toolCall.name === "greet" && args.name === "alice") {
      return { argsOverride: { name: "Alice (verified)" } };
    }
  });

  jimu.on("after_tool_call", ({ result }) => {
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
jimu.on("finish_turn", ({ toolResults }) => {
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
export default function (jimu) {
  jimu.registerTool({
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

  jimu.on("prepare_request", () => ({ systemPrompt: "sys-v1" }));
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
export default function (jimu) {
  jimu.registerTool({
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

  jimu.on("prepare_request", () => ({ systemPrompt: "sys-v2" }));
  jimu.on("transform_context", (messages) =>
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
  jimu.on("before_tool_call", ({ args }) => {
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
jimu.on("prepare_request", () => {
  while (true) {}
});

jimu.on("before_tool_call", () => {
  while (true) {}
});

jimu.registerTool({
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

#[test]
fn js_extension_top_level_await_and_concurrent_reload_generation() {
    // 1. (#31) ESM extension using top-level `await` before `export default`.
    let tla_ext = r#"
const greeting = await Promise.resolve("top-level-ready");

export default function (jimu) {
  jimu.registerTool({
    name: "tla_tool",
    description: "Returns top-level awaited greeting",
    parameters: {
      type: "object",
      properties: { who: { type: "string" } },
      required: ["who"],
    },
    execute(_id, params) {
      return `${greeting}:${params.who}`;
    },
  });
}
"#;
    let runtime = JsExtensionRuntime::new(JsExtensionConfig::new().with_inline("tla.mjs", tla_ext))
        .expect("top-level await ESM extension must load cleanly");
    let tools = runtime.tools();
    assert_eq!(tools.len(), 1);
    let out = block_on(tools[0].execute(
        "tc_tla".into(),
        json!({ "who": "world" }),
        jimu_ai::CancelToken::new(),
        Arc::new(|_| {}),
    ))
    .expect("tla_tool executes");
    match &out.content[0] {
        ContentPart::Text(t) => assert_eq!(t.text, "top-level-ready:world"),
        other => panic!("expected text, got {other:?}"),
    }

    // Rejected top-level await fails initialization cleanly.
    let tla_reject = r#"
await Promise.reject(new Error("init boom"));
export default function () {}
"#;
    assert!(
        JsExtensionRuntime::new(JsExtensionConfig::new().with_inline("tla-err.mjs", tla_reject))
            .is_err()
    );

    // 2. (#32) Concurrent reloads publish monotonic generation consistent with the active VM.
    let dir = tempdir().expect("create tempdir");
    let ext_path = dir.path().join("concurrent.js");
    let write_version = |ver: u32, with_hook: bool| {
        let hook_code = if with_hook {
            format!(r#"jimu.on("prepare_request", () => ({{ systemPrompt: "v{ver}" }}));"#)
        } else {
            String::new()
        };
        let code = format!(
            r#"
export default function (jimu) {{
  jimu.registerTool({{
    name: "ver_tool",
    description: "version {ver}",
    parameters: {{ type: "object", properties: {{}} }},
    execute() {{ return "v{ver}"; }},
  }});
  {hook_code}
}}
"#
        );
        fs::write(&ext_path, code).expect("write version");
    };

    write_version(1, false);
    let shared_rt = Arc::new(
        JsExtensionRuntime::new(JsExtensionConfig::new().with_file(&ext_path)).expect("load v1"),
    );

    // Trigger concurrent reloads across threads while updating file versions, then settle on final version.
    let barrier = Arc::new(std::sync::Barrier::new(4));
    let mut handles = Vec::new();
    for _ in 0..3 {
        let rt = Arc::clone(&shared_rt);
        let b = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            b.wait();
            rt.reload()
        }));
    }
    write_version(2, true);
    barrier.wait();
    for h in handles {
        h.join()
            .expect("thread join")
            .expect("concurrent reload ok");
    }

    // Final published metadata and hook_mask must match what the active VM executes.
    let tool = shared_rt.tools().into_iter().next().expect("ver_tool");
    let exec_res = block_on(tool.execute(
        "tc_ver".into(),
        json!({}),
        jimu_ai::CancelToken::new(),
        Arc::new(|_| {}),
    ))
    .expect("ver_tool executes");
    let active_ver = match &exec_res.content[0] {
        ContentPart::Text(t) => t.text.clone(),
        other => panic!("expected text, got {other:?}"),
    };
    let desc = shared_rt.tool_definitions()[0].description.clone();
    assert_eq!(
        desc,
        format!("version {}", active_ver.trim_start_matches('v')),
        "published registry tool definition must match active VM version"
    );
    assert_eq!(
        shared_rt.has_hook(HookKind::PrepareRequest),
        active_ver == "v2",
        "published hook_mask must match active VM version"
    );
}

#[test]
fn js_ui_mod_onion_chain_theme_and_invalidate_e2e() {
    let dir = tempdir().expect("create tempdir");
    let ext_a_path = dir.path().join("01-ext-a.js");
    let ext_b_path = dir.path().join("02-ext-b.js");

    fs::write(
        &ext_a_path,
        r##"
export default function (jimu) {
  let badge = "idle";
  jimu.ui.theme({ accent: "#00ff88", border: "#333333" });

  jimu.registerTool({
    name: "set_badge",
    description: "Update status badge and invalidate UI",
    parameters: {
      type: "object",
      properties: { label: { type: "string" } },
      required: ["label"],
    },
    execute(_id, params) {
      badge = params.label;
      jimu.ui.invalidate();
      return badge;
    },
  });

  jimu.on("ui.render", (e, next) => {
    if (e.component === "StatusLine") {
      const inner = next({ ...e, props: { ...e.props, tag: "from-A" } });
      return jimu.ui.row(
        { gap: 1 },
        [jimu.ui.text(badge, { color: "accent", bold: true }), inner]
      );
    }
    if (e.component === "Footer") {
      return "custom-footer";
    }
    if (e.component === "Header") {
      return undefined;
    }
    return next(e);
  });
}
"##,
    )
    .expect("write ext-a");

    fs::write(
        &ext_b_path,
        r##"
export default function (jimu) {
  jimu.ui.theme({ accent: "#ff5500", bg: "#111111" });

  jimu.on("ui.render", (e, next) => {
    if (e.component === "StatusLine") {
      return jimu.ui.box(
        { border: "left", borderColor: "accent", paddingLeft: 1 },
        [next(e), jimu.ui.text(e.props.tag || "missing", { dim: true })]
      );
    }
    if (e.component === "AbovePrompt") {
      return jimu.ui.divider({ ch: "─", color: "dim" });
    }
    return next(e);
  });
}
"##,
    )
    .expect("write ext-b");

    let runtime = JsExtensionRuntime::new(
        JsExtensionConfig::new()
            .with_file(&ext_a_path)
            .with_file(&ext_b_path),
    )
    .expect("load UI mod extensions");

    assert!(runtime.has_ui_render());
    for hook in [
        HookKind::PrepareRequest,
        HookKind::TransformContext,
        HookKind::BeforeToolCall,
        HookKind::AfterToolCall,
        HookKind::FinishTurn,
    ] {
        assert!(
            !runtime.has_hook(hook),
            "ui.render must not set Agent HookKind {hook:?}"
        );
    }

    let theme = runtime.ui_theme_overrides();
    assert_eq!(theme.get("accent").map(String::as_str), Some("#ff5500"));
    assert_eq!(theme.get("border").map(String::as_str), Some("#333333"));
    assert_eq!(theme.get("bg").map(String::as_str), Some("#111111"));

    let gen0 = runtime.ui_generation();

    let status_node = block_on(runtime.render_ui(
        UiComponent::StatusLine,
        json!({ "model": "claude" }),
        DEFAULT_UI_RENDER_TIMEOUT_MS,
    ))
    .expect("render StatusLine");
    assert_eq!(
        status_node,
        UiNode::Box {
            direction: UiDirection::Row,
            gap: 1,
            border: UiBorder::None,
            border_color: None,
            bg: None,
            padding_left: 0,
            padding_right: 0,
            children: vec![
                UiNode::Text {
                    text: "idle".into(),
                    style: UiTextStyle {
                        color: Some("accent".into()),
                        bold: Some(true),
                        ..Default::default()
                    },
                },
                UiNode::Box {
                    direction: UiDirection::Column,
                    gap: 0,
                    border: UiBorder::Left,
                    border_color: Some("accent".into()),
                    bg: None,
                    padding_left: 1,
                    padding_right: 0,
                    children: vec![
                        UiNode::Native,
                        UiNode::Text {
                            text: "from-A".into(),
                            style: UiTextStyle {
                                dim: Some(true),
                                ..Default::default()
                            },
                        },
                    ],
                },
            ],
        }
    );

    let footer_node =
        block_on(runtime.render_ui(UiComponent::Footer, json!({}), DEFAULT_UI_RENDER_TIMEOUT_MS))
            .expect("render Footer");
    assert_eq!(
        footer_node,
        UiNode::Text {
            text: "custom-footer".into(),
            style: UiTextStyle::default(),
        }
    );

    let header_node =
        block_on(runtime.render_ui(UiComponent::Header, json!({}), DEFAULT_UI_RENDER_TIMEOUT_MS))
            .expect("render Header");
    assert_eq!(header_node, UiNode::Native);

    let divider_node = block_on(runtime.render_ui(
        UiComponent::AbovePrompt,
        json!({}),
        DEFAULT_UI_RENDER_TIMEOUT_MS,
    ))
    .expect("render AbovePrompt");
    assert_eq!(
        divider_node,
        UiNode::Divider {
            ch: Some("─".into()),
            color: Some("dim".into()),
        }
    );

    // Calling JS tool invokes `jimu.ui.invalidate()`, bumping ui_generation lock-free.
    let tool = runtime.tools().into_iter().next().expect("set_badge tool");
    block_on(tool.execute(
        "tc_badge".into(),
        json!({ "label": "busy" }),
        jimu_ai::CancelToken::new(),
        Arc::new(|_| {}),
    ))
    .expect("set_badge executes");
    let gen1 = runtime.ui_generation();
    assert!(gen1 > gen0, "jimu.ui.invalidate() must bump ui_generation");

    let updated = block_on(runtime.render_ui(
        UiComponent::StatusLine,
        json!({}),
        DEFAULT_UI_RENDER_TIMEOUT_MS,
    ))
    .expect("render updated StatusLine");
    match updated {
        UiNode::Box { children, .. } => match &children[0] {
            UiNode::Text { text, .. } => assert_eq!(text, "busy"),
            other => panic!("expected Text node, got {other:?}"),
        },
        other => panic!("expected Box node, got {other:?}"),
    }

    // Reload updates theme overrides and bumps ui_generation.
    fs::write(
        &ext_b_path,
        r##"
export default function (jimu) {
  jimu.ui.theme({ accent: "#2299ff" });
  jimu.on("ui.render", (e, next) => next(e));
}
"##,
    )
    .expect("rewrite ext-b");
    runtime.reload().expect("reload succeeds");
    assert!(
        runtime.ui_generation() > gen1,
        "reload must bump ui_generation"
    );
    let reloaded_theme = runtime.ui_theme_overrides();
    assert_eq!(
        reloaded_theme.get("accent").map(String::as_str),
        Some("#2299ff")
    );
    assert!(!reloaded_theme.contains_key("bg"));
}

#[test]
fn js_ui_mod_rejects_async_and_oversized_trees_and_recovers_e2e() {
    // Non-string theme value rejected at load time.
    let bad_theme = JsExtensionRuntime::new(
        JsExtensionConfig::new().with_inline("bad-theme.js", r#"jimu.ui.theme({ accent: 123 });"#),
    );
    assert!(bad_theme.is_err());

    let ext = r#"
export default function (jimu) {
  jimu.on("ui.render", (e, next) => {
    if (e.component === "StatusLine") {
      return Promise.resolve(jimu.ui.text("async-nope"));
    }
    if (e.component === "Header") {
      let node = jimu.ui.text("deep");
      for (let i = 0; i < 18; i++) {
        node = jimu.ui.box([node]);
      }
      return node;
    }
    if (e.component === "Footer") {
      const children = [];
      for (let i = 0; i < 260; i++) {
        children.push(jimu.ui.text("n"));
      }
      return jimu.ui.box(children);
    }
    if (e.component === "Spinner") {
      while (true) {}
    }
    if (e.component === "ToolRow") {
      return jimu.ui.text("recovered", { italic: true });
    }
    return next(e);
  });
}
"#;

    let runtime = JsExtensionRuntime::new(JsExtensionConfig::new().with_inline("guards.js", ext))
        .expect("load guard extension");

    let async_err = block_on(runtime.render_ui(UiComponent::StatusLine, json!({}), 40))
        .expect_err("async ui.render must be rejected");
    assert!(
        matches!(async_err, JsExtensionError::Script(ref m) if m.contains("synchronous")),
        "expected synchronous Script error, got {async_err:?}"
    );

    let depth_err = block_on(runtime.render_ui(UiComponent::Header, json!({}), 40))
        .expect_err("deep tree must fail validation");
    assert!(
        matches!(depth_err, JsExtensionError::InvalidUiNode(ref m) if m.contains("depth")),
        "expected depth InvalidUiNode error, got {depth_err:?}"
    );

    let count_err = block_on(runtime.render_ui(UiComponent::Footer, json!({}), 40))
        .expect_err("too many nodes must fail validation");
    assert!(
        matches!(count_err, JsExtensionError::InvalidUiNode(ref m) if m.contains("node count")),
        "expected node count InvalidUiNode error, got {count_err:?}"
    );

    let timeout_err = block_on(runtime.render_ui(UiComponent::Spinner, json!({}), 40))
        .expect_err("infinite loop must time out");
    assert!(
        matches!(timeout_err, JsExtensionError::Timeout { timeout_ms: 40 }),
        "expected Timeout(40), got {timeout_err:?}"
    );

    // VM recovers cleanly after script/validation/timeout errors.
    let recovered = block_on(runtime.render_ui(UiComponent::ToolRow, json!({}), 40))
        .expect("VM must recover after errors");
    assert_eq!(
        recovered,
        UiNode::Text {
            text: "recovered".into(),
            style: UiTextStyle {
                italic: Some(true),
                ..Default::default()
            },
        }
    );

    // Box spacing/padding bounds prevent excessive host expansion while allowing max boundary values.
    let valid_box = UiNode::Box {
        direction: jimu_js::UiDirection::Column,
        gap: jimu_js::MAX_UI_BOX_GAP,
        border: jimu_js::UiBorder::None,
        border_color: None,
        bg: None,
        padding_left: jimu_js::MAX_UI_BOX_PADDING,
        padding_right: jimu_js::MAX_UI_BOX_PADDING,
        children: vec![UiNode::Native],
    };
    jimu_js::validate_ui_node(&valid_box).expect("boundary gap/padding must validate");

    let oversized_gap = UiNode::Box {
        direction: jimu_js::UiDirection::Column,
        gap: jimu_js::MAX_UI_BOX_GAP + 1,
        border: jimu_js::UiBorder::None,
        border_color: None,
        bg: None,
        padding_left: 0,
        padding_right: 0,
        children: vec![UiNode::Native],
    };
    assert!(jimu_js::validate_ui_node(&oversized_gap).is_err());

    let oversized_pad_left = UiNode::Box {
        direction: jimu_js::UiDirection::Row,
        gap: 0,
        border: jimu_js::UiBorder::None,
        border_color: None,
        bg: None,
        padding_left: jimu_js::MAX_UI_BOX_PADDING + 1,
        padding_right: 0,
        children: vec![UiNode::Native],
    };
    assert!(jimu_js::validate_ui_node(&oversized_pad_left).is_err());

    let oversized_pad_right = UiNode::Box {
        direction: jimu_js::UiDirection::Row,
        gap: 0,
        border: jimu_js::UiBorder::None,
        border_color: None,
        bg: None,
        padding_left: 0,
        padding_right: jimu_js::MAX_UI_BOX_PADDING + 1,
        children: vec![UiNode::Native],
    };
    assert!(jimu_js::validate_ui_node(&oversized_pad_right).is_err());
}

#[test]
fn js_ui_mod_zero_worker_fast_path_and_headless_compat_e2e() {
    // 1. Extension without `ui.render`: render_ui must short-circuit with zero worker dispatch.
    let plain_ext = r#"
export default function (jimu) {
  jimu.registerTool({
    name: "ping",
    description: "ping",
    parameters: { type: "object", properties: {} },
    execute() { return "pong"; },
  });
}
"#;
    let plain_rt =
        JsExtensionRuntime::new(JsExtensionConfig::new().with_inline("plain.js", plain_ext))
            .expect("load plain extension");
    assert!(!plain_rt.has_ui_render());
    assert!(plain_rt.ui_theme_overrides().is_empty());

    for comp in [
        UiComponent::StatusLine,
        UiComponent::AbovePrompt,
        UiComponent::Header,
        UiComponent::ToolRow,
    ] {
        let node = block_on(plain_rt.render_ui(comp, json!({}), 1)).expect("fast path Native");
        assert_eq!(node, UiNode::Native);
    }
    assert_eq!(
        plain_rt.ui_render_invocations(),
        0,
        "render_ui without listeners must never dispatch to worker"
    );

    // 2. Headless host loading an extension that contains UI Mod registrations works seamlessly.
    let mixed_ext = r##"
export default function (jimu) {
  jimu.ui.theme({ accent: "#00aaff" });
  jimu.on("ui.render", (e, next) => jimu.ui.row([jimu.ui.text("mod"), next(e)]));
  jimu.registerTool({
    name: "headless_echo",
    description: "echo",
    parameters: {
      type: "object",
      properties: { msg: { type: "string" } },
      required: ["msg"],
    },
    execute(_id, args) { return `echo:${args.msg}`; },
  });
}
"##;
    let headless_rt =
        JsExtensionRuntime::new(JsExtensionConfig::new().with_inline("mixed.js", mixed_ext))
            .expect("headless host loads UI-mod extension cleanly");
    let tool = headless_rt
        .tools()
        .into_iter()
        .next()
        .expect("headless_echo");
    let out = block_on(tool.execute(
        "tc_hl".into(),
        json!({ "msg": "ok" }),
        jimu_ai::CancelToken::new(),
        Arc::new(|_| {}),
    ))
    .expect("tool executes normally in headless host");
    match &out.content[0] {
        ContentPart::Text(t) => assert_eq!(t.text, "echo:ok"),
        other => panic!("expected text, got {other:?}"),
    }
}
