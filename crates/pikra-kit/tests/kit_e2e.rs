//! End-to-end tests for `pikra-kit`: SkillCatalog discovery, deferred tool gateway
//! (`tool_search` deduplication + direct `call_tool` before/after `upsert_tools`/`remove_tools`),
//! prompt cache prefix preservation, QuickJS `exec` codemode multi-tool execution,
//! Streamable HTTP MCP client (`application/json` & SSE), and idle configuration updates.

use bytes::Bytes;
use futures_executor::block_on;
use futures_util::StreamExt;
use pikra_agent::{AgentEvent, AgentTool, AgentToolResult, ClosureTool};
use pikra_ai::{
    ApiKind, CancelToken, Client, ContentPart, Message, Model, ModelCost, ThinkingLevel,
    ToolDefinition,
};
use pikra_kit::{
    BackgroundNotification, CALL_TOOL_NAME, CODEMODE_EXEC_TOOL_NAME, INSTRUCTIONS_CONTEXT_NAME,
    InstructionCatalog, InstructionDiscoveryConfig, KitEvent, KitSession, KitSessionConfig,
    KitToolSpec, KitTurnOrigin, McpClient, McpToolRegistrationMode, SKILL_SEARCH_NAME,
    SkillCatalog, SkillDiscoveryConfig, SkillEnableRule, SkillSelector, TOOL_SEARCH_NAME,
    WalkBoundary,
};
#[cfg(all(feature = "stdio", not(target_arch = "wasm32")))]
use pikra_kit::{StdioMcpConfig, StdioMcpTransport};
use pikra_schema::{int_schema, obj_schema, str_schema};
use pikra_testkit::{FauxTransport, ScriptedHttpResponse, assert_prompt_prefix_identical};
use serde_json::{Value, json};
use std::sync::Arc;
use tempfile::tempdir;

fn test_model() -> Model {
    Model {
        id: "gpt-5.4".into(),
        name: "GPT-5.4".into(),
        provider: "openai".into(),
        api: ApiKind::OpenaiResponses,
        base_url: "https://api.openai.com/v1".into(),
        context_window: 200_000,
        max_output_tokens: 16_384,
        supports_images: true,
        supports_reasoning: true,
        cost: ModelCost::default(),
        headers: Vec::new(),
        compat: None,
    }
}

fn openai_sse_text(id: &str, text: &str) -> Vec<String> {
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
            "data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_{id}\",\"status\":\"completed\",\"usage\":{{\"input_tokens\":100,\"output_tokens\":20,\"total_tokens\":120}}}}}}\n\n"
        ),
    ]
}

fn openai_sse_tool_call(id: &str, call_id: &str, name: &str, args: &Value) -> Vec<String> {
    let args_escaped = serde_json::to_string(&args.to_string()).unwrap();
    vec![
        format!(
            "data: {{\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{{\"type\":\"function_call\",\"id\":\"fc_{id}\",\"call_id\":\"{call_id}\",\"name\":\"{name}\"}}}}\n\n"
        ),
        format!(
            "data: {{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{{\"type\":\"function_call\",\"id\":\"fc_{id}\",\"call_id\":\"{call_id}\",\"name\":\"{name}\",\"arguments\":{args_escaped}}}}}\n\n"
        ),
        format!(
            "data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_{id}\",\"status\":\"completed\",\"usage\":{{\"input_tokens\":100,\"output_tokens\":25,\"total_tokens\":125}}}}}}\n\n"
        ),
    ]
}

fn make_add_tool(name: &str, description: &str) -> Arc<dyn AgentTool> {
    let def = ToolDefinition {
        name: name.into(),
        description: description.into(),
        parameters: obj_schema([("a", int_schema(), true), ("b", int_schema(), true)]),
    };
    Arc::new(ClosureTool::new(def, |_id, args, _cancel, _prog| {
        Box::pin(async move {
            let a = args["a"].as_i64().unwrap_or(0);
            let b = args["b"].as_i64().unwrap_or(0);
            Ok(AgentToolResult {
                content: vec![ContentPart::Text(pikra_ai::TextContent::plain(format!(
                    "{}",
                    a + b
                )))],
                details: Value::Null,
                structured_content: Some(json!({ "sum": a + b })),
                is_error: false,
                terminate: false,
            })
        })
    }))
}

fn make_echo_tool(name: &str, description: &str) -> Arc<dyn AgentTool> {
    let def = ToolDefinition {
        name: name.into(),
        description: description.into(),
        parameters: obj_schema([("msg", str_schema(), true)]),
    };
    Arc::new(ClosureTool::new(def, |_id, args, _cancel, _prog| {
        Box::pin(async move {
            let msg = args["msg"].as_str().unwrap_or("").to_string();
            Ok(AgentToolResult::text(format!("echo:{msg}")))
        })
    }))
}

fn json_http_response(
    status: u16,
    headers: Vec<(String, String)>,
    body: Value,
) -> ScriptedHttpResponse {
    let mut all_headers = headers;
    all_headers.push(("content-type".into(), "application/json".into()));
    let bytes = Bytes::from(serde_json::to_vec(&body).unwrap());
    ScriptedHttpResponse::stream(
        status,
        all_headers,
        Box::pin(futures_util::stream::iter(vec![Ok(bytes)])),
    )
}

#[test]
fn skills_deferred_tools_upsert_remove_and_prompt_cache_stability() {
    block_on(async {
        let dir = tempdir().unwrap();
        // Nested SKILL.md with frontmatter + top-level .md skill without frontmatter.
        let deploy_dir = dir.path().join("deploy-app");
        std::fs::create_dir_all(&deploy_dir).unwrap();
        std::fs::write(
            deploy_dir.join("SKILL.md"),
            "---\nname: deploy-app\ndescription: Deploy services safely to production\n---\n# Deploy\nRun checks first.\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("quick-triage.md"),
            "# Quick Triage Guide\nInspect logs immediately.\n",
        )
        .unwrap();

        let transport = Arc::new(FauxTransport::new());
        let provider = Arc::new(Client::new(transport.clone()));

        let config = KitSessionConfig::new(test_model())
            .with_system_prompt("You are Pikra Kit.")
            .with_skill_root(dir.path())
            .with_resident_tool(make_echo_tool("resident_echo", "Echo a string"))
            .with_deferred_tool(make_add_tool("math_add", "Add two integers together"));

        let session = KitSession::new(provider, config).unwrap();

        // Resident tools MUST NOT contain deferred `math_add`.
        let resident_names: Vec<String> = session
            .resident_tool_definitions()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert!(resident_names.contains(&"resident_echo".to_string()));
        assert!(resident_names.contains(&TOOL_SEARCH_NAME.to_string()));
        assert!(resident_names.contains(&CALL_TOOL_NAME.to_string()));
        assert!(!resident_names.contains(&"math_add".to_string()));

        // Turn 1: model directly calls `call_tool` on `math_add` WITHOUT calling `tool_search` first!
        transport.enqueue_sse(openai_sse_tool_call(
            "t1a",
            "call_direct",
            CALL_TOOL_NAME,
            &json!({ "name": "math_add", "arguments": { "a": 17, "b": 25 } }),
        ));
        transport.enqueue_sse(openai_sse_text("t1b", "The sum is 42."));

        let turn1_msgs = session.prompt("Add 17 and 25").await.unwrap();
        let direct_tool_result = turn1_msgs
            .iter()
            .find_map(|m| match m {
                pikra_agent::AgentMessage::Llm(Message::ToolResult(tr)) => Some(tr),
                _ => None,
            })
            .expect("direct call_tool must produce a ToolResultMessage");
        assert!(!direct_tool_result.is_error);
        match &direct_tool_result.content[0] {
            ContentPart::Text(t) => assert_eq!(t.text, "42"),
            other => panic!("expected text result, got {other:?}"),
        }

        // Verify skills & deferred_tools context blocks were injected in request 0 and resident tools never included math_add.
        let reqs_after_t1 = transport.requests();
        assert_eq!(reqs_after_t1.len(), 2);
        let wire_tool_names: Vec<&str> = reqs_after_t1[0].body_json["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(!wire_tool_names.contains(&"math_add"));
        let req0_str = reqs_after_t1[0].body_json.to_string();
        assert!(req0_str.contains("<available_skills>"));
        assert!(req0_str.contains("deploy-app"));
        assert!(req0_str.contains("quick-triage"));
        assert!(req0_str.contains("math_add: Add two integers together"));

        // Dynamic upsert_tools while idle -> resident tools stay byte-identical; deferred catalog updates via <system-reminder>.
        session
            .upsert_tools(vec![make_echo_tool(
                "dyn_greet",
                "Greet a user dynamically",
            )])
            .unwrap();

        // Turn 2: call `tool_search` twice for `dyn_greet` to verify schema deduplication, then call `dyn_greet`.
        transport.enqueue_sse(openai_sse_tool_call(
            "t2a",
            "call_s1",
            TOOL_SEARCH_NAME,
            &json!({ "query": "select:dyn_greet", "limit": 5 }),
        ));
        transport.enqueue_sse(openai_sse_tool_call(
            "t2b",
            "call_s2",
            TOOL_SEARCH_NAME,
            &json!({ "query": "greet", "limit": 5 }),
        ));
        transport.enqueue_sse(openai_sse_tool_call(
            "t2c",
            "call_dyn",
            CALL_TOOL_NAME,
            &json!({ "name": "dyn_greet", "arguments": { "msg": "hi" } }),
        ));
        transport.enqueue_sse(openai_sse_text("t2d", "Greeted!"));

        let turn2_msgs = session.prompt("Search and greet").await.unwrap();
        let tool_results: Vec<String> = turn2_msgs
            .iter()
            .filter_map(|m| match m {
                pikra_agent::AgentMessage::Llm(Message::ToolResult(tr)) => match &tr.content[0] {
                    ContentPart::Text(t) => Some(t.text.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(tool_results.len(), 3);

        // First search returns full schema; second search deduplicates into `already_loaded`.
        let s1: Value = serde_json::from_str(&tool_results[0]).unwrap();
        assert_eq!(s1["tools"].as_array().unwrap().len(), 1);
        assert_eq!(s1["tools"][0]["name"], "dyn_greet");
        assert!(s1["tools"][0]["parameters"]["properties"]["msg"].is_object());

        let s2: Value = serde_json::from_str(&tool_results[1]).unwrap();
        assert!(s2["tools"].as_array().unwrap().is_empty());
        assert_eq!(s2["already_loaded"], json!(["dyn_greet"]));

        assert_eq!(tool_results[2], "echo:hi");

        // Remove `dyn_greet` while idle and verify `call_tool` now errors while resident prefix stays identical.
        assert_eq!(session.remove_tools(&["dyn_greet"]).unwrap(), 1);
        transport.enqueue_sse(openai_sse_tool_call(
            "t3a",
            "call_removed",
            CALL_TOOL_NAME,
            &json!({ "name": "dyn_greet", "arguments": { "msg": "gone" } }),
        ));
        transport.enqueue_sse(openai_sse_text("t3b", "Tool was removed."));
        let turn3_msgs = session.prompt("Try calling removed tool").await.unwrap();
        let removed_res = turn3_msgs
            .iter()
            .find_map(|m| match m {
                pikra_agent::AgentMessage::Llm(Message::ToolResult(tr)) => Some(tr),
                _ => None,
            })
            .unwrap();
        assert!(removed_res.is_error);

        // Verify prompt cache prefix stayed 100% identical across ALL turns including upsert_tools & remove_tools!
        let all_reqs = transport.requests();
        for pair in all_reqs.windows(2) {
            assert_prompt_prefix_identical(&pair[0], &pair[1]);
        }

        // Verify idle configuration updates succeed with zero spurious Busy.
        session.set_thinking_level(ThinkingLevel::Medium).unwrap();
        session.set_fast(true).unwrap();
        let mut alt_model = test_model();
        alt_model.id = "gpt-5.4-mini".into();
        session.set_model(alt_model).unwrap();
        assert_eq!(session.model().id, "gpt-5.4-mini");
        assert_eq!(session.thinking_level(), ThinkingLevel::Medium);
        assert!(session.fast());
        session.abort().await;
    });
}

#[test]
fn codemode_exec_bridges_resident_and_deferred_tools_without_recursion() {
    block_on(async {
        let transport = Arc::new(FauxTransport::new());
        let provider = Arc::new(Client::new(transport.clone()));

        let config = KitSessionConfig::new(test_model())
            .with_system_prompt("Use exec when orchestrating tools.")
            .with_resident_tool(make_echo_tool("resident_echo", "Echo text"))
            .with_deferred_tool(make_add_tool("deferred_add", "Add two integers"))
            .with_codemode(true);

        let session = KitSession::new(provider, config).unwrap();
        assert!(
            session
                .resident_tool_definitions()
                .iter()
                .any(|t| t.name == CODEMODE_EXEC_TOOL_NAME)
        );

        let script = r#"
            const sum = await tools.deferred_add({ a: 10, b: 32 });
            const echoed = await tools.resident_echo({ msg: "val=" + sum.sum });
            let execBlocked = false;
            try { void tools.exec; } catch (e) { execBlocked = true; }
            const hasExec = ALL_TOOLS.some(t => t.name === "exec") || !execBlocked;
            store("last_sum", sum.sum);
            return { echoed, sum: sum.sum, hasExec };
        "#;

        transport.enqueue_sse(openai_sse_tool_call(
            "cm1",
            "call_exec",
            CODEMODE_EXEC_TOOL_NAME,
            &json!({ "code": script }),
        ));
        transport.enqueue_sse(openai_sse_text("cm2", "Codemode completed."));

        // Exercise `prompt_stream` as well.
        let mut stream = session.prompt_stream("Run codemode script").unwrap();
        let mut exec_result_text = None;
        while let Some(ev) = stream.next().await {
            if let KitEvent::Agent(AgentEvent::ToolExecutionEnd {
                tool_name,
                result,
                is_error,
                ..
            }) = ev
                && tool_name == CODEMODE_EXEC_TOOL_NAME
            {
                assert!(!is_error, "codemode script failed: {result:?}");
                if let Some(ContentPart::Text(t)) = result.content.first() {
                    exec_result_text = Some(t.text.clone());
                }
            }
        }

        let parsed: Value = serde_json::from_str(&exec_result_text.expect("exec output")).unwrap();
        assert_eq!(parsed["echoed"], "echo:val=42");
        assert_eq!(parsed["sum"], 42);
        assert_eq!(
            parsed["hasExec"], false,
            "exec must be excluded to prevent recursion"
        );
    });
}

#[test]
fn streamable_http_mcp_client_json_and_sse_integrated_into_kit_session() {
    block_on(async {
        let mcp_http = Arc::new(FauxTransport::new());

        // 1. initialize -> JSON response with Mcp-Session-Id header.
        mcp_http.enqueue(json_http_response(
            200,
            vec![("Mcp-Session-Id".into(), "sess_abc123".into())],
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "faux-mcp", "version": "1.0.0" }
                }
            }),
        ));
        // 2. notifications/initialized -> 202 Accepted empty body.
        mcp_http.enqueue(ScriptedHttpResponse::stream(
            202,
            vec![],
            Box::pin(futures_util::stream::empty()),
        ));
        // 3. tools/list -> text/event-stream (SSE) response!
        mcp_http.enqueue_sse([
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{\"progress\":1}}\n\n",
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[{\"name\":\"lookup_weather\",\"description\":\"Get city weather\",\"inputSchema\":{\"type\":\"object\",\"properties\":{\"city\":{\"type\":\"string\"}},\"required\":[\"city\"]}}]}}\n\n",
        ]);
        // 4. tools/call -> text/event-stream (SSE) response!
        mcp_http.enqueue_sse([
            "data: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"Tokyo: 22C sunny\"}],\"isError\":false}}\n\n",
        ]);

        let mcp_client = Arc::new(McpClient::with_http(
            mcp_http.clone(),
            "https://mcp.example.invalid/mcp",
            vec![("authorization".into(), "Bearer secret".into())],
        ));
        let init = mcp_client
            .initialize("pikra-test", "0.1.0", CancelToken::new())
            .await
            .unwrap();
        assert_eq!(init.server_info.name, "faux-mcp");

        // Attach MCP tools as deferred tools into KitSession and execute via `call_tool`.
        let llm_http = Arc::new(FauxTransport::new());
        let llm_provider = Arc::new(Client::new(llm_http.clone()));
        let session = KitSession::new(llm_provider, KitSessionConfig::new(test_model())).unwrap();
        llm_http.enqueue_sse(openai_sse_text("m0", "Ready."));
        session.prompt("Start.").await.unwrap();

        let attached = session
            .attach_mcp_client(
                Arc::clone(&mcp_client),
                McpToolRegistrationMode::Auto,
                Some("mcp"),
                CancelToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(attached, vec!["mcp_lookup_weather".to_string()]);
        assert_eq!(session.deferred_tools()[0].name, "mcp_lookup_weather");

        llm_http.enqueue_sse(openai_sse_tool_call(
            "m1",
            "call_mcp_weather",
            CALL_TOOL_NAME,
            &json!({ "name": "mcp_lookup_weather", "arguments": { "city": "Tokyo" } }),
        ));
        llm_http.enqueue_sse(openai_sse_text("m2", "It is 22C and sunny in Tokyo."));

        let msgs = session
            .prompt("What is the weather in Tokyo?")
            .await
            .unwrap();
        let tr = msgs
            .iter()
            .find_map(|m| match m {
                pikra_agent::AgentMessage::Llm(Message::ToolResult(tr)) => Some(tr),
                _ => None,
            })
            .unwrap();
        assert!(!tr.is_error);
        match &tr.content[0] {
            ContentPart::Text(t) => assert_eq!(t.text, "Tokyo: 22C sunny"),
            other => panic!("expected text, got {other:?}"),
        }

        let llm_requests = llm_http.requests();
        assert_prompt_prefix_identical(&llm_requests[0], &llm_requests[1]);

        // Verify MCP HTTP protocol headers (Mcp-Session-Id & MCP-Protocol-Version propagated!).
        let mcp_reqs = mcp_http.requests();
        assert_eq!(mcp_reqs.len(), 4);
        assert_eq!(mcp_reqs[0].body_json["method"], "initialize");
        assert_eq!(mcp_reqs[1].body_json["method"], "notifications/initialized");
        assert_eq!(mcp_reqs[1].header("mcp-session-id"), Some("sess_abc123"));
        assert_eq!(
            mcp_reqs[1].header("mcp-protocol-version"),
            Some("2025-03-26")
        );
        assert_eq!(mcp_reqs[2].body_json["method"], "tools/list");
        assert_eq!(mcp_reqs[3].body_json["method"], "tools/call");
        assert_eq!(mcp_reqs[3].body_json["params"]["name"], "lookup_weather");
        assert_eq!(
            mcp_reqs[3].body_json["params"]["arguments"]["city"],
            "Tokyo"
        );
    });
}

#[cfg(all(feature = "stdio", not(target_arch = "wasm32")))]
#[test]
fn stdio_mcp_transport_real_python_subprocess_integrated_into_kit_session() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let py_server = r#"
import json, sys

for raw in sys.stdin:
    line = raw.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    req_id = msg.get("id")
    if method == "initialize":
        resp = {
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "protocolVersion": "2025-03-26",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "py-stdio-mcp", "version": "0.1.0"},
            },
        }
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif method == "notifications/initialized":
        continue
    elif method == "tools/list":
        # Emit an unsolicited notification before the real response to verify correlation!
        sys.stdout.write(json.dumps({
            "jsonrpc": "2.0",
            "method": "notifications/message",
            "params": {"level": "info", "data": "listing tools"},
        }) + "\n")
        resp = {
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "tools": [{
                    "name": "multiply",
                    "description": "Multiply two integers",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "x": {"type": "integer"},
                            "y": {"type": "integer"}
                        },
                        "required": ["x", "y"]
                    }
                }]
            },
        }
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif method == "tools/call":
        args = msg.get("params", {}).get("arguments", {})
        prod = int(args["x"]) * int(args["y"])
        resp = {
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "content": [{"type": "text", "text": f"product={prod}"}],
                "structuredContent": {"product": prod},
                "isError": False,
            },
        }
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
"#;

        let transport = Arc::new(
            StdioMcpTransport::spawn(StdioMcpConfig {
                command: "python3".into(),
                args: vec!["-u".into(), "-c".into(), py_server.into()],
                env: Vec::new(),
                cwd: None,
            })
            .unwrap(),
        );
        let mcp_client = Arc::new(McpClient::new(transport.clone()));

        let init = mcp_client
            .initialize("pikra-stdio-e2e", "0.1.0", CancelToken::new())
            .await
            .unwrap();
        assert_eq!(init.server_info.name, "py-stdio-mcp");

        let llm_http = Arc::new(FauxTransport::new());
        let llm_provider = Arc::new(Client::new(llm_http.clone()));
        let session = KitSession::new(llm_provider, KitSessionConfig::new(test_model())).unwrap();

        let attached = session
            .attach_mcp_client(
                Arc::clone(&mcp_client),
                McpToolRegistrationMode::Deferred,
                Some("stdio"),
                CancelToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(attached, vec!["stdio_multiply".to_string()]);

        llm_http.enqueue_sse(openai_sse_tool_call(
            "s1",
            "call_mul",
            CALL_TOOL_NAME,
            &json!({ "name": "stdio_multiply", "arguments": { "x": 6, "y": 7 } }),
        ));
        llm_http.enqueue_sse(openai_sse_text("s2", "6 * 7 = 42."));

        let msgs = session.prompt("Multiply 6 and 7").await.unwrap();
        let tr = msgs
            .iter()
            .find_map(|m| match m {
                pikra_agent::AgentMessage::Llm(Message::ToolResult(tr)) => Some(tr),
                _ => None,
            })
            .unwrap();
        assert!(!tr.is_error);
        match &tr.content[0] {
            ContentPart::Text(t) => assert_eq!(t.text, "product=42"),
            other => panic!("expected text, got {other:?}"),
        }

        // Verify pre-cancelled request returns McpError::Cancelled immediately.
        let cancelled = CancelToken::new();
        cancelled.cancel();
        let err = mcp_client.list_tools(cancelled).await.unwrap_err();
        assert!(matches!(err, pikra_kit::McpError::Cancelled));

        // Clean shutdown terminates the subprocess cleanly.
        transport.close().await;
    });
}

#[test]
fn codemode_exec_serializes_sequential_tools_and_breaks_arc_cycle() {
    use pikra_agent::ToolExecutionMode;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ModeAwareTool {
        def: ToolDefinition,
        mode: ToolExecutionMode,
        in_flight: Arc<AtomicUsize>,
        max_in_flight: Arc<AtomicUsize>,
        parallel_gate: Option<Arc<std::sync::Mutex<Option<futures_channel::oneshot::Sender<()>>>>>,
        parallel_wait:
            Option<Arc<std::sync::Mutex<Option<futures_channel::oneshot::Receiver<()>>>>>,
    }

    impl AgentTool for ModeAwareTool {
        fn definition(&self) -> ToolDefinition {
            self.def.clone()
        }

        fn execution_mode(&self) -> ToolExecutionMode {
            self.mode
        }

        fn execute(
            &self,
            _tool_call_id: String,
            args: Value,
            _cancel: CancelToken,
            _on_update: pikra_agent::ToolProgressCallback,
        ) -> pikra_ai::BoxFuture<'_, Result<AgentToolResult, String>> {
            let in_flight = Arc::clone(&self.in_flight);
            let max_in_flight = Arc::clone(&self.max_in_flight);
            let gate = self.parallel_gate.clone();
            let wait = self.parallel_wait.clone();
            let mode = self.mode;
            Box::pin(async move {
                let current = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                max_in_flight.fetch_max(current, Ordering::SeqCst);

                if mode == ToolExecutionMode::Parallel {
                    // Two-party deterministic rendezvous: only resolves if BOTH calls overlap!
                    if current >= 2 {
                        if let Some(tx) = gate.and_then(|g| g.lock().unwrap().take()) {
                            let _ = tx.send(());
                        }
                    } else if let Some(rx) = wait.and_then(|w| w.lock().unwrap().take()) {
                        let _ = rx.await;
                    }
                } else {
                    // Yield several polls so a concurrent Sequential call would definitely overlap if unguarded.
                    for _ in 0..4 {
                        let mut yielded = false;
                        futures_util::future::poll_fn(|cx| {
                            if yielded {
                                std::task::Poll::Ready(())
                            } else {
                                yielded = true;
                                cx.waker().wake_by_ref();
                                std::task::Poll::Pending
                            }
                        })
                        .await;
                    }
                }

                in_flight.fetch_sub(1, Ordering::SeqCst);
                Ok(AgentToolResult::text(
                    args.get("n")
                        .and_then(Value::as_i64)
                        .unwrap_or(0)
                        .to_string(),
                ))
            })
        }
    }

    block_on(async {
        let seq_in_flight = Arc::new(AtomicUsize::new(0));
        let seq_max = Arc::new(AtomicUsize::new(0));
        let seq_tool: Arc<dyn AgentTool> = Arc::new(ModeAwareTool {
            def: ToolDefinition {
                name: "seq_edit".into(),
                description: "Sequential mutating tool".into(),
                parameters: obj_schema([("n", int_schema(), true)]),
            },
            mode: ToolExecutionMode::Sequential,
            in_flight: Arc::clone(&seq_in_flight),
            max_in_flight: Arc::clone(&seq_max),
            parallel_gate: None,
            parallel_wait: None,
        });

        let (par_tx, par_rx) = futures_channel::oneshot::channel();
        let par_in_flight = Arc::new(AtomicUsize::new(0));
        let par_max = Arc::new(AtomicUsize::new(0));
        let par_tool: Arc<dyn AgentTool> = Arc::new(ModeAwareTool {
            def: ToolDefinition {
                name: "par_read".into(),
                description: "Parallel read tool".into(),
                parameters: obj_schema([("n", int_schema(), true)]),
            },
            mode: ToolExecutionMode::Parallel,
            in_flight: Arc::clone(&par_in_flight),
            max_in_flight: Arc::clone(&par_max),
            parallel_gate: Some(Arc::new(std::sync::Mutex::new(Some(par_tx)))),
            parallel_wait: Some(Arc::new(std::sync::Mutex::new(Some(par_rx)))),
        });

        let transport = Arc::new(FauxTransport::new());
        let provider = Arc::new(Client::new(transport.clone()));
        let session = KitSession::new(
            provider,
            KitSessionConfig::new(test_model())
                .with_resident_tool(Arc::clone(&seq_tool))
                .with_deferred_tool(Arc::clone(&par_tool))
                .with_codemode(true),
        )
        .unwrap();

        let script = r#"
            const seqRes = await Promise.all([
                tools.seq_edit({ n: 1 }),
                tools.seq_edit({ n: 2 }),
                tools.seq_edit({ n: 3 }),
            ]);
            const parRes = await Promise.all([
                tools.par_read({ n: 10 }),
                tools.par_read({ n: 20 }),
            ]);
            return { seqRes, parRes };
        "#;
        transport.enqueue_sse(openai_sse_tool_call(
            "sq1",
            "call_exec_seq",
            CODEMODE_EXEC_TOOL_NAME,
            &json!({ "code": script }),
        ));
        transport.enqueue_sse(openai_sse_text("sq2", "Done."));

        session.prompt("run seq and par").await.unwrap();
        assert_eq!(
            seq_max.load(Ordering::SeqCst),
            1,
            "Sequential tools inside codemode Promise.all must never overlap"
        );
        assert_eq!(
            par_max.load(Ordering::SeqCst),
            2,
            "Parallel tools inside codemode Promise.all must still run concurrently"
        );

        // (#23) Dropping KitSession must release all user tools (no Arc cycle via CodemodeExecTool).
        assert!(Arc::strong_count(&seq_tool) > 1);
        drop(session);
        assert_eq!(
            Arc::strong_count(&seq_tool),
            1,
            "dropping KitSession with codemode enabled must not leak resident tools"
        );
        assert_eq!(Arc::strong_count(&par_tool), 1);
    });
}

#[test]
fn review_regressions_baseline_sync_compaction_guard_unpolled_stream_mcp_and_skills_clear() {
    use pikra_session::{
        CompactionInput, CompactionOutput, CompactionStrategy, JsonlSessionStore, SessionError,
    };

    // 1. (#8 + #38) set_system_prompt, resident MCP attach, reopen, and deleting last skill.
    block_on(async {
        let dir = tempdir().unwrap();
        let session_path = dir.path().join("kit.jsonl");
        let skills_dir = dir.path().join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        let skill_file = skills_dir.join("only-skill.md");
        std::fs::write(&skill_file, "# Only Skill\nDoes one thing.\n").unwrap();

        let llm_http = Arc::new(FauxTransport::new());
        let llm_provider = Arc::new(Client::new(llm_http.clone()));
        let store = Arc::new(JsonlSessionStore::new(&session_path));

        let session = KitSession::new(
            llm_provider.clone(),
            KitSessionConfig::new(test_model())
                .with_system_prompt("Prompt A")
                .with_skill_discovery(SkillDiscoveryConfig {
                    include_user_skills: false,
                    ..SkillDiscoveryConfig::default()
                })
                .with_skill_root(&skills_dir)
                .with_store(store)
                .with_codemode(true),
        )
        .unwrap();

        llm_http.enqueue_sse(openai_sse_text("b1", "Reply A"));
        session.prompt("Turn 1").await.unwrap();
        assert_eq!(llm_http.requests()[0].body_json["instructions"], "Prompt A");
        assert!(
            llm_http.requests()[0]
                .body_json
                .to_string()
                .contains("only-skill")
        );

        // Update system prompt & attach resident MCP tool.
        session.set_system_prompt("Prompt B").unwrap();

        let mcp_http = Arc::new(FauxTransport::new());
        mcp_http.enqueue(json_http_response(
            200,
            vec![("Mcp-Session-Id".into(), "sess_res".into())],
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "res-mcp", "version": "1.0.0" }
                }
            }),
        ));
        mcp_http.enqueue(ScriptedHttpResponse::stream(
            202,
            vec![],
            Box::pin(futures_util::stream::empty()),
        ));
        mcp_http.enqueue(json_http_response(
            200,
            vec![],
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": {
                    "tools": [{
                        "name": "ping_res",
                        "description": "Resident MCP ping",
                        "inputSchema": { "type": "object", "properties": {} }
                    }]
                }
            }),
        ));
        let mcp_client = Arc::new(McpClient::with_http(
            mcp_http,
            "https://mcp.example.invalid/mcp",
            vec![],
        ));
        mcp_client
            .initialize("pikra-test", "0.1.0", CancelToken::new())
            .await
            .unwrap();
        session
            .attach_mcp_client(
                mcp_client,
                McpToolRegistrationMode::Resident,
                Some("mcp"),
                CancelToken::new(),
            )
            .await
            .unwrap();

        // (#38) Delete the last skill and reload_skills().
        std::fs::remove_file(&skill_file).unwrap();
        assert!(
            session.reload_skills(),
            "deleting the last skill must report a removal"
        );

        llm_http.enqueue_sse(openai_sse_text("b2", "Reply B"));
        session.prompt("Turn 2").await.unwrap();

        let reqs = llm_http.requests();
        assert_eq!(reqs[1].body_json["instructions"], "Prompt B");
        let turn2_tools: Vec<&str> = reqs[1].body_json["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(turn2_tools.contains(&"mcp_ping_res"));
        assert!(turn2_tools.contains(&CODEMODE_EXEC_TOOL_NAME));
        assert!(
            reqs[1]
                .body_json
                .to_string()
                .contains("The only-skill skill has been removed")
        );
        // Conversation history from Turn 1 preserved.
        assert!(reqs[1].body_json.to_string().contains("Reply A"));
        // Subsequent reload with still 0 skills is a NO-OP.
        assert!(!session.reload_skills());

        // Reopen persisted session and verify Prompt B + resident MCP tool persisted.
        let reopened = KitSession::new(
            llm_provider,
            KitSessionConfig::new(test_model())
                .with_system_prompt("IGNORED")
                .with_store(Arc::new(JsonlSessionStore::new(&session_path))),
        )
        .unwrap();
        assert_eq!(reopened.system_prompt(), "Prompt B");
    });

    // 2. (#18) Dropping an unpolled prompt_stream does not drop queued set_context blocks.
    block_on(async {
        let llm_http = Arc::new(FauxTransport::new());
        let session = KitSession::new(
            Arc::new(Client::new(llm_http.clone())),
            KitSessionConfig::new(test_model()),
        )
        .unwrap();

        assert!(session.set_context("env", "staging-unpolled"));
        {
            let unpolled = session.prompt_stream("never polled").unwrap();
            assert!(session.is_running());
            drop(unpolled);
        }
        assert!(!session.is_running());

        llm_http.enqueue_sse(openai_sse_text("u1", "Got staging context."));
        session.prompt("actual prompt").await.unwrap();
        let reqs = llm_http.requests();
        assert_eq!(reqs.len(), 1);
        assert!(
            reqs[0].body_json.to_string().contains("staging-unpolled"),
            "context block must still be delivered after dropping an unpolled prompt_stream"
        );
    });

    // 3. (#17) Manual compact() guards against concurrent prompt/stream/mutations until committed.
    block_on(async {
        struct GatedCompaction {
            started: std::sync::Mutex<Option<futures_channel::oneshot::Sender<()>>>,
            release: std::sync::Mutex<Option<futures_channel::oneshot::Receiver<()>>>,
        }
        impl CompactionStrategy for GatedCompaction {
            fn compact<'a>(
                &'a self,
                _provider: &'a dyn pikra_ai::StreamProvider,
                input: CompactionInput,
            ) -> pikra_ai::BoxFuture<'a, Result<CompactionOutput, SessionError>> {
                Box::pin(async move {
                    let started_tx = self.started.lock().unwrap().take();
                    if let Some(tx) = started_tx {
                        let _ = tx.send(());
                    }
                    let release_rx = self.release.lock().unwrap().take();
                    if let Some(rx) = release_rx {
                        let _ = rx.await;
                    }
                    Ok(CompactionOutput {
                        summary: "Compacted summary".into(),
                        first_kept_entry_id: input.first_kept_entry_id,
                        usage: None,
                    })
                })
            }
        }

        let (started_tx, started_rx) = futures_channel::oneshot::channel();
        let (release_tx, release_rx) = futures_channel::oneshot::channel();
        let strategy = Arc::new(GatedCompaction {
            started: std::sync::Mutex::new(Some(started_tx)),
            release: std::sync::Mutex::new(Some(release_rx)),
        });

        let llm_http = Arc::new(FauxTransport::new());
        let session = KitSession::new(
            Arc::new(Client::new(llm_http.clone())),
            KitSessionConfig::new(test_model()).with_compaction_strategy(strategy),
        )
        .unwrap();

        llm_http.enqueue_sse(openai_sse_text("c1", "Turn 1 reply"));
        session.prompt("Turn 1").await.unwrap();
        let leaf = session.leaf_id().unwrap();

        let compact_fut = session.compact();
        let probe_fut = async {
            started_rx.await.unwrap();
            assert!(session.is_running());
            assert!(matches!(
                session.prompt("concurrent").await,
                Err(pikra_kit::KitError::Busy)
            ));
            assert!(matches!(
                session.prompt_stream("concurrent"),
                Err(pikra_kit::KitError::Busy)
            ));
            assert!(matches!(
                session.fork(&leaf),
                Err(pikra_kit::KitError::Busy)
            ));
            assert!(matches!(
                session.set_model(test_model()),
                Err(pikra_kit::KitError::Busy)
            ));
            assert!(matches!(
                session.upsert_tools(vec![]),
                Err(pikra_kit::KitError::Busy)
            ));
            assert!(matches!(
                session.remove_tools(&["x"]),
                Err(pikra_kit::KitError::Busy)
            ));
            release_tx.send(()).unwrap();
        };

        let (compact_res, ()) = futures_util::future::join(compact_fut, probe_fut).await;
        assert!(compact_res.unwrap().is_some());
        assert!(!session.is_running());

        llm_http.enqueue_sse(openai_sse_text("c2", "After compaction"));
        session.prompt("Turn 2").await.unwrap();
    });

    // 4. (#29 & #30) MCP HTTP clears stale session_id on 404/initialize & replies to SSE ping.
    block_on(async {
        let mcp_http = Arc::new(FauxTransport::new());
        // 1. First initialize -> sess_old
        mcp_http.enqueue(json_http_response(
            200,
            vec![("Mcp-Session-Id".into(), "sess_old".into())],
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "faux-mcp", "version": "1.0.0" }
                }
            }),
        ));
        // 2. notifications/initialized -> 202
        mcp_http.enqueue(ScriptedHttpResponse::stream(
            202,
            vec![],
            Box::pin(futures_util::stream::empty()),
        ));
        // 3. tools/list with stale sess_old -> 404 Not Found
        mcp_http.enqueue(json_http_response(
            404,
            vec![],
            json!({ "error": "session expired" }),
        ));
        // 4. Re-initialize -> must NOT send Mcp-Session-Id; server issues sess_new
        mcp_http.enqueue(json_http_response(
            200,
            vec![("Mcp-Session-Id".into(), "sess_new".into())],
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "result": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "faux-mcp", "version": "1.0.0" }
                }
            }),
        ));
        // 5. notifications/initialized -> 202
        mcp_http.enqueue(ScriptedHttpResponse::stream(
            202,
            vec![],
            Box::pin(futures_util::stream::empty()),
        ));
        // 6. tools/call SSE stream: server sends `ping` request BEFORE returning tool result!
        mcp_http.enqueue_sse([
            "data: {\"jsonrpc\":\"2.0\",\"id\":\"srv-ping-1\",\"method\":\"ping\"}\n\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":4,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"pong-ok\"}],\"isError\":false}}\n\n",
        ]);
        // 7. HTTP 202 response for client's POST reply to server's `ping`
        mcp_http.enqueue(ScriptedHttpResponse::stream(
            202,
            vec![],
            Box::pin(futures_util::stream::empty()),
        ));

        let transport = Arc::new(pikra_kit::StreamableHttpMcpTransport::new(
            mcp_http.clone(),
            "https://mcp.example.invalid/mcp",
            vec![],
        ));
        let client = McpClient::new(transport.clone());

        client
            .initialize("pikra-test", "0.1.0", CancelToken::new())
            .await
            .unwrap();
        assert_eq!(transport.session_id().as_deref(), Some("sess_old"));

        // 404 must clear stored session_id.
        assert!(client.list_tools(CancelToken::new()).await.is_err());
        assert_eq!(transport.session_id(), None);

        // Re-initialize sends NO stale Mcp-Session-Id and captures sess_new.
        client
            .initialize("pikra-test", "0.1.0", CancelToken::new())
            .await
            .unwrap();
        assert_eq!(transport.session_id().as_deref(), Some("sess_new"));

        // tools/call over SSE must reply to server `ping` and return tool result.
        let call_res = client
            .call_tool("echo", json!({}), CancelToken::new())
            .await
            .unwrap();
        assert!(!call_res.is_error);
        match &call_res.content[0] {
            ContentPart::Text(t) => assert_eq!(t.text, "pong-ok"),
            other => panic!("expected text, got {other:?}"),
        }

        let reqs = mcp_http.requests();
        assert_eq!(reqs.len(), 7);
        assert_eq!(reqs[2].header("mcp-session-id"), Some("sess_old"));
        assert_eq!(
            reqs[3].header("mcp-session-id"),
            None,
            "re-initialize must not send expired Mcp-Session-Id"
        );
        assert_eq!(reqs[4].header("mcp-session-id"), Some("sess_new"));
        // Request 6 is the client's JSON-RPC reply to the server's SSE ping.
        assert_eq!(
            reqs[6].body_json,
            json!({ "jsonrpc": "2.0", "id": "srv-ping-1", "result": {} })
        );
        assert_eq!(reqs[6].header("mcp-session-id"), Some("sess_new"));
    });
}

#[test]
fn instruction_discovery_scopes_and_normalizes_files_before_applying_budget() {
    use std::fs;

    let dir = tempdir().unwrap();
    let home = dir.path().join("home");
    let user = home.join(".agents");
    let custom = home.join("custom");
    let repo = dir.path().join("repo");
    let worktree = repo.join("worktree");
    let leaf = worktree.join("leaf");
    let additional = worktree.join("additional");
    for path in [&user, &custom, &leaf, &additional, &repo.join(".git")] {
        fs::create_dir_all(path).unwrap();
    }
    let explicit = dir.path().join("explicit.md");
    for (path, content) in [
        (user.join("AGENTS.md"), "global"),
        (custom.join("AGENTS.md"), "custom"),
        (repo.join("AGENTS.md"), "\u{feff} shared\r\nrule\r\n"),
        (worktree.join("AGENTS.md"), "shared\nrule"),
        (worktree.join(".git"), "gitdir: ignored"),
        (leaf.join("AGENTS.md"), "overridden"),
        (leaf.join("AGENTS.override.md"), "leaf"),
        (leaf.join("CLAUDE.md"), "custom filename"),
        (additional.join("AGENTS.md"), "additional"),
        (explicit.clone(), "a瑞b"),
    ] {
        fs::write(path, content).unwrap();
    }
    let mut config = InstructionDiscoveryConfig {
        cwd: Some(leaf.clone()),
        additional_directories: vec![additional],
        home_dir_override: Some(home.clone()),
        user_dirs: vec!["~/custom".into()],
        extra_files: vec![explicit.clone()],
        ..Default::default()
    };
    #[cfg(unix)]
    {
        let link = dir.path().join("explicit-link.md");
        std::os::unix::fs::symlink(&explicit, &link).unwrap();
        config.extra_files.push(link);
    }
    let contents = |config: &_| {
        InstructionCatalog::discover(config)
            .entries()
            .iter()
            .map(|entry| entry.content.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        contents(&config),
        [
            "global",
            "custom",
            "shared\nrule",
            "leaf",
            "additional",
            "a瑞b"
        ]
    );
    config.walk_boundary = WalkBoundary::FilesystemRoot;
    assert_eq!(
        contents(&config),
        [
            "global",
            "custom",
            " shared\nrule\n",
            "leaf",
            "additional",
            "a瑞b"
        ]
    );
    config.deduplicate_identical_content = false;
    assert_eq!(
        contents(&config),
        [
            "global",
            "custom",
            " shared\nrule\n",
            "shared\nrule",
            "leaf",
            "additional",
            "a瑞b"
        ]
    );
    config.walk_boundary = WalkBoundary::CurrentDirOnly;
    config.include_agents_dir = false;
    assert_eq!(contents(&config), ["custom", "leaf", "additional", "a瑞b"]);
    config.include_user_instructions = false;
    config.additional_directories.clear();
    config.extra_files.clear();
    config.filenames = vec!["../AGENTS.md".into(), "CLAUDE.md".into()];
    assert_eq!(contents(&config), ["custom filename"]);

    let unmarked = dir.path().join("unmarked/leaf");
    fs::create_dir_all(&unmarked).unwrap();
    fs::write(dir.path().join("unmarked/AGENTS.md"), "outside").unwrap();
    fs::write(unmarked.join("AGENTS.md"), "inside").unwrap();
    config = InstructionDiscoveryConfig {
        cwd: Some(unmarked),
        include_user_instructions: false,
        ..Default::default()
    };
    assert_eq!(contents(&config), ["inside"]);
    config.project_root_markers = vec!["ROOT".into()];
    fs::write(dir.path().join("unmarked/ROOT"), "").unwrap();
    assert_eq!(contents(&config), ["outside", "inside"]);

    config.cwd = None;
    config.extra_files = vec![explicit];
    config.max_bytes = Some(3);
    let limited = InstructionCatalog::discover(&config);
    assert_eq!(limited.entries()[0].content, "a");
    assert!(limited.entries()[0].truncated && limited.truncated());
    assert_eq!(limited.total_bytes(), 1);
    config.max_bytes = None;
    assert_eq!(contents(&config), ["a瑞b"]);
    config.max_bytes = Some(0);
    assert!(
        InstructionCatalog::discover(&config)
            .format_for_context()
            .is_none()
    );
}

#[test]
fn instruction_context_is_refreshed_and_survives_compaction_fork_and_reopen() {
    use pikra_session::{CompactionConfig, JsonlSessionStore};
    use std::fs;

    block_on(async {
        let dir = tempdir().unwrap();
        let file = dir.path().join("AGENTS.md");
        fs::write(&file, "initial workspace guidance").unwrap();
        let http = Arc::new(FauxTransport::new());
        let provider = Arc::new(Client::new(http.clone()));
        let mut config = KitSessionConfig::new(test_model())
            .with_store(Arc::new(JsonlSessionStore::new(
                dir.path().join("session.jsonl"),
            )))
            .with_compaction(CompactionConfig {
                keep_recent_tokens: 1,
                ..Default::default()
            })
            .with_instruction_discovery(InstructionDiscoveryConfig {
                include_user_instructions: false,
                ..Default::default()
            });
        config.cwd = Some(dir.path().display().to_string());
        let session = KitSession::new(provider.clone(), config.clone()).unwrap();
        assert_eq!(
            session.instructions().entries()[0].content,
            "initial workspace guidance"
        );
        assert!(!session.reload_instructions());
        for id in ["first", "second", "summary", "after", "fork", "reopened"] {
            http.enqueue_sse(openai_sse_text(id, "Acknowledged."));
        }

        session.prompt("First task").await.unwrap();
        let first_leaf = session.leaf_id().unwrap();
        fs::write(&file, "revised workspace guidance").unwrap();
        assert!(session.reload_instructions());
        assert!(!session.reload_instructions());
        session.prompt("Second task").await.unwrap();
        assert!(session.compact().await.unwrap().is_some());
        session.prompt("After compaction").await.unwrap();
        session.fork(&first_leaf).unwrap();
        session.prompt("After fork").await.unwrap();
        drop(session);

        fs::remove_file(file).unwrap();
        let session = KitSession::new(provider, config).unwrap();
        assert!(session.instructions().is_empty());
        session.prompt("After reopening").await.unwrap();
        let requests = http.requests();
        assert_prompt_prefix_identical(&requests[0], &requests[1]);
        for index in [0, 1, 3, 4] {
            let body = requests[index].body_json.to_string();
            let guidance = if index == 0 {
                "initial workspace guidance"
            } else {
                "revised workspace guidance"
            };
            assert!(body.contains(guidance), "{body}");
            assert!(body.contains(&format!("name=\\\"{INSTRUCTIONS_CONTEXT_NAME}\\\"")));
        }
        assert!(
            requests[5]
                .body_json
                .to_string()
                .contains("Previously supplied project instructions no longer apply.")
        );
    });
}

#[test]
fn scoped_skills_search_reload_reopen_and_compact_without_prefix_drift() {
    use pikra_session::JsonlSessionStore;
    use std::{fs, path::Path};

    let write_skill = |root: &Path, name: &str, description: &str| {
        fs::create_dir_all(root).unwrap();
        let path = root.join(format!("{name}.md"));
        fs::write(
            &path,
            format!("---\nname: {name}\ndescription: {description}\n---\nInstructions\n"),
        )
        .unwrap();
        path
    };

    block_on(async {
        let dir = tempdir().unwrap();
        let home = dir.path().join("home");
        let repo = home.join("work/repo");
        let cwd = repo.join("team");
        let extra = dir.path().join("explicit-with-a-long-locatable-root");
        let bundled = dir.path().join("bundled");
        let keep = write_skill(&extra, "a-build", "Build the selected app");
        let removed = write_skill(&extra, "b-release", "Publish releases");
        write_skill(&cwd.join(".dora/skills"), "a-build", "Lower priority");
        write_skill(&cwd.join(".agents/skills"), "local", "Local workflow");
        write_skill(&repo.join(".agents/skills"), "project", "Project workflow");
        write_skill(
            &home.join("work/.agents/skills"),
            "out-of-repo",
            "Outside the repository",
        );
        write_skill(&home.join(".agents/skills"), "user", "User workflow");
        write_skill(&bundled, "bundled", &"部署发布与运维".repeat(80));
        let blocked = write_skill(&extra, "blocked", "Disabled by the final rule");
        let manual = write_skill(&extra, "manual-only", "Explicit approval workflow");
        let frontmatter_only = extra.join("frontmatter-only.md");
        fs::write(
            &frontmatter_only,
            "---\ndisable-model-invocation: true\n---\nPrivate instructions",
        )
        .unwrap();
        // Directory-style SKILL.md uses a dedicated openai.yaml sidecar.
        let manual_dir = extra.join("manual");
        fs::create_dir_all(manual_dir.join("agents")).unwrap();
        fs::rename(manual, manual_dir.join("SKILL.md")).unwrap();
        fs::write(
            manual_dir.join("agents/openai.yaml"),
            "policy:\n  allow_implicit_invocation: false # explicit only\n",
        )
        .unwrap();
        fs::write(repo.join(".git"), "gitdir: checkout").unwrap();
        #[cfg(unix)]
        {
            let managed = dir.path().join("managed");
            fs::create_dir(&managed).unwrap();
            fs::write(
                managed.join("SKILL.md"),
                "---\nname: managed\n---\nManaged skill",
            )
            .unwrap();
            std::os::unix::fs::symlink(&managed, extra.join("managed")).unwrap();
            std::os::unix::fs::symlink(&extra, extra.join("recursive")).unwrap();
        }

        let catalog_budget = extra.to_string_lossy().len() + 420;
        let discovery = SkillDiscoveryConfig {
            cwd: Some(cwd),
            project_skill_dirs: vec![".dora/skills".into()],
            home_dir_override: Some(home),
            bundled_roots: vec![bundled],
            rules: vec![
                SkillEnableRule {
                    selector: SkillSelector::ByName("blocked".into()),
                    enabled: true,
                },
                SkillEnableRule {
                    selector: SkillSelector::ByPath(blocked),
                    enabled: false,
                },
            ],
            max_catalog_bytes: catalog_budget,
            shorten_paths: true,
            ..SkillDiscoveryConfig::default()
        };
        let isolated = SkillCatalog::discover_with_config(&SkillDiscoveryConfig {
            walk_boundary: WalkBoundary::CurrentDirOnly,
            include_user_skills: false,
            ..discovery.clone()
        });
        assert!(isolated.get("local").is_some());
        assert!(isolated.get("project").is_none() && isolated.get("user").is_none());

        let http = Arc::new(FauxTransport::new());
        let provider = Arc::new(Client::new(http.clone()));
        let config = KitSessionConfig::new(test_model())
            .with_skill_root(&extra)
            .with_skill_discovery(discovery)
            .with_store(Arc::new(JsonlSessionStore::new(
                dir.path().join("session.jsonl"),
            )));
        let session = KitSession::new(provider.clone(), config.clone()).unwrap();
        let catalog = session.skills();
        for name in ["local", "project", "user", "bundled"] {
            assert!(catalog.get(name).is_some(), "missing scope: {name}");
        }
        #[cfg(unix)]
        assert!(catalog.get("managed").is_some());
        assert!(catalog.get("out-of-repo").is_none() && catalog.get("blocked").is_none());
        assert_eq!(catalog.get("a-build").unwrap().file_path, keep);
        assert_eq!(catalog.search("部署发布", 1)[0].name, "bundled");
        let xml = catalog.format_for_context().unwrap();
        assert!(xml.len() <= catalog_budget && xml.contains("<truncated count="));
        assert!(xml.contains("<skill_root alias=\"r0\">") && xml.contains("r0/a-build.md"));
        assert!(!xml.contains("bundled") && !xml.contains("manual-only"));

        http.enqueue_sse(openai_sse_tool_call(
            "ss1",
            "search",
            SKILL_SEARCH_NAME,
            &json!({"query":"部署发布"}),
        ));
        http.enqueue_sse(openai_sse_text("ss2", "Found the bundled skill."));
        let messages = session
            .prompt("请用$manual-only 和 $frontmatter-only。")
            .await
            .unwrap();
        assert!(messages.iter().any(|message| matches!(message,
            pikra_agent::AgentMessage::Llm(Message::ToolResult(result))
            if matches!(&result.content[0], ContentPart::Text(text) if text.text.contains("bundled")))));
        let first = &http.requests()[0];
        let wire = first.body_json.to_string();
        for name in ["manual-only", "frontmatter-only"] {
            assert!(wire.contains(&format!("<skill_mention name=\\\"{name}\\\"")));
        }

        // Updating a skill in place changes its revision but causes no prompt reminder.
        fs::write(
            &keep,
            "---\nname: a-build\ndescription: Updated description\n---\nUpdated instructions",
        )
        .unwrap();
        assert!(!session.reload_skills());
        http.enqueue_sse(openai_sse_text("ss3", "Still here."));
        session.prompt("Continue.").await.unwrap();
        assert!(
            !http.requests()[2]
                .body_json
                .to_string()
                .contains("# Skills changed")
        );
        drop(session);

        fs::remove_file(removed).unwrap();
        write_skill(&extra, "c-new", "New workflow");
        let session = KitSession::new(provider, config).unwrap();
        http.enqueue_sse(openai_sse_text("ss4", "Received changes."));
        session.prompt("What changed?").await.unwrap();
        let requests = http.requests();
        let diff = requests[3].body_json.to_string();
        assert!(
            diff.contains("# Skills changed")
                && diff.contains("The b-release skill has been removed")
        );
        assert!(diff.contains("<name>c-new</name>") && !diff.contains("Updated description"));
        for pair in requests.windows(2) {
            assert_prompt_prefix_identical(&pair[0], &pair[1]);
        }

        http.enqueue_sse(openai_sse_text("summary", "Prior work summarized."));
        assert!(session.compact().await.unwrap().is_some());
        http.enqueue_sse(openai_sse_text("ss5", "Ready."));
        session.prompt("After compaction.").await.unwrap();
        let latest_catalog = session
            .entries()
            .into_iter()
            .rev()
            .find_map(|entry| match entry.payload {
                pikra_session::SessionEntryPayload::ContextBlock { name, text, .. }
                    if name == "skills" =>
                {
                    Some(text)
                }
                _ => None,
            })
            .unwrap();
        assert!(
            latest_catalog.contains("<name>c-new</name>") && !latest_catalog.contains("b-release"),
            "{latest_catalog}"
        );
    });
}

#[test]
fn auto_tools_rank_subwords_and_propagate_delegated_progress() {
    struct Middleware;
    impl pikra_agent::AgentMiddleware for Middleware {}

    block_on(async {
        let http = Arc::new(FauxTransport::new());
        let pinned = make_echo_tool("pinned", "Always available");
        let small = make_echo_tool("small", "Fits the budget");
        let deploy = Arc::new(ClosureTool::new(
            ToolDefinition {
                name: "deployProject".into(),
                description: "部署生产服务".into(),
                parameters: obj_schema([("deploymentTarget", int_schema(), true)]),
            },
            |_id, args, _cancel, progress| {
                Box::pin(async move {
                    let mut result = AgentToolResult::text(args["deploymentTarget"].to_string());
                    result.details = json!({"status":"working"});
                    progress(result.clone());
                    result.details = json!({"status":"done"});
                    Ok(result)
                })
            },
        ));
        let mut config = KitSessionConfig::new(test_model())
            .with_tool(KitToolSpec::auto(small.clone()))
            .with_tool(KitToolSpec::auto(deploy))
            .with_resident_tool(pinned.clone())
            .with_deferred_tool(make_echo_tool("other", "Unrelated"));
        config.auto_defer_threshold_bytes = Some(
            [pinned, small]
                .iter()
                .map(|tool| serde_json::to_vec(&tool.definition()).unwrap().len())
                .sum(),
        );
        config.middlewares.push(Arc::new(Middleware));
        let session = KitSession::new(Arc::new(Client::new(http.clone())), config).unwrap();
        let names: Vec<_> = session
            .resident_tool_definitions()
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        assert!(
            names.iter().any(|name| name == "small") && names.iter().any(|name| name == "pinned")
        );
        assert!(!names.iter().any(|name| name == "deployProject"));
        http.enqueue_sse(openai_sse_tool_call(
            "tools1",
            "search",
            TOOL_SEARCH_NAME,
            &json!({"query":"deploy project 部署 target","limit":1}),
        ));
        http.enqueue_sse(openai_sse_tool_call(
            "tools2",
            "delegate",
            CALL_TOOL_NAME,
            &json!({"name":"deployProject","arguments":{"deploymentTarget":"42"}}),
        ));
        http.enqueue_sse(openai_sse_text("tools3", "Deployed."));
        let mut stream = session
            .prompt_stream("Find and invoke the deployment tool.")
            .unwrap();
        let mut saw_schema = false;
        let mut statuses = Vec::new();
        while let Some(event) = stream.next().await {
            let (result, name) = match event {
                KitEvent::Agent(AgentEvent::ToolExecutionUpdate {
                    tool_name,
                    partial_result,
                    ..
                }) => (partial_result, tool_name),
                KitEvent::Agent(AgentEvent::ToolExecutionEnd {
                    tool_name, result, ..
                }) => (result, tool_name),
                _ => continue,
            };
            if name == TOOL_SEARCH_NAME {
                saw_schema =
                    result.structured_content.unwrap()["tools"][0]["name"] == "deployProject";
            } else if name == CALL_TOOL_NAME {
                assert_eq!(
                    result.details["delegated_tool"],
                    json!({"name":"deployProject","arguments":{"deploymentTarget":42}})
                );
                statuses.push(
                    result.details["inner_details"]["status"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                );
            }
        }
        assert!(saw_schema);
        assert_eq!(statuses, ["working", "done"]);
        for pair in http.requests().windows(2) {
            assert_prompt_prefix_identical(&pair[0], &pair[1]);
        }
    });
}

#[test]
fn background_events_coalesce_and_wait_for_clean_foreground_completion() {
    use futures_util::FutureExt;

    block_on(async {
        let http = Arc::new(FauxTransport::new());
        let session = KitSession::new(
            Arc::new(Client::new(http.clone())),
            KitSessionConfig::new(test_model()),
        )
        .unwrap();
        let sender = session.background_sender();
        let send = |index| {
            sender.send(BackgroundNotification {
                source_id: "ci".into(),
                kind: "monitor".into(),
                summary: format!("Output {index}"),
                xml_payload: format!("<monitor_event>{index}</monitor_event>"),
                timestamp_ms: index,
            })
        };
        let mut wakeups = session.subscribe_background_wakeups();
        let gate = http.enqueue_gated_sse();
        let mut foreground = session.prompt_stream("Foreground first.").unwrap();
        assert_eq!(foreground.origin(), KitTurnOrigin::Foreground);
        while !matches!(
            Some(foreground.next().await.expect("foreground must start")),
            Some(KitEvent::Agent(AgentEvent::AgentStart))
        ) {}
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    send(1);
                    send(2);
                })
                .join()
                .unwrap();
        });
        assert!(wakeups.next().now_or_never().is_none());
        assert!(matches!(
            session.run_background_turn(),
            Err(pikra_kit::KitError::Busy)
        ));
        assert!(session.drain_background_prompt().is_none());
        for chunk in openai_sse_text("foreground", "Complete") {
            assert!(gate.push_str(chunk));
        }
        gate.close();
        while foreground.next().await.is_some() {}
        assert_eq!(wakeups.next().now_or_never(), Some(Some(())));
        assert_eq!(
            session.subscribe_background_wakeups().next().now_or_never(),
            Some(Some(()))
        );

        // Dropping an unpolled reservation does not discard the batch.
        let background = session.run_background_turn().unwrap().unwrap();
        assert_eq!(background.origin(), KitTurnOrigin::Background);
        drop(background);
        assert_eq!(wakeups.next().now_or_never(), Some(Some(())));
        http.enqueue_sse(openai_sse_text("background", "Processed"));
        let mut background = session.run_background_turn().unwrap().unwrap();
        send(3);
        while background.next().await.is_some() {}
        let requests = http.requests();
        assert_eq!(requests.len(), 2);
        let payload = requests[1].body_json.to_string();
        assert!(payload.contains(r"<background_events>\n<monitor_event>1</monitor_event>\n<monitor_event>2</monitor_event>\n<monitor_event>3</monitor_event>\n</background_events>"));
        assert!(session.run_background_turn().unwrap().is_none());

        http.enqueue_gated_sse();
        let mut aborted = session.prompt_stream("Stop this turn.").unwrap();
        while !matches!(
            Some(aborted.next().await.expect("foreground must start")),
            Some(KitEvent::Agent(AgentEvent::AgentStart))
        ) {}
        send(4);
        session.abort().await;
        while aborted.next().await.is_some() {}
        assert!(session.is_background_paused() && session.run_background_turn().unwrap().is_none());
        send(5);
        assert!(wakeups.next().now_or_never().is_none());
        http.enqueue_sse(openai_sse_text("resume", "New foreground turn."));
        session.prompt("Continue from here.").await.unwrap();
        assert_eq!(wakeups.next().now_or_never(), Some(Some(())));
        let drained = session.drain_background_prompt().unwrap();
        assert!(
            drained.contains("<monitor_event>4</monitor_event>\n<monitor_event>5</monitor_event>")
        );
        assert!(session.run_background_turn().unwrap().is_none());

        send(6);
        assert_eq!(wakeups.next().now_or_never(), Some(Some(())));
        http.enqueue(ScriptedHttpResponse::Error(
            pikra_ai::TransportError::Timeout,
        ));
        session.prompt("A request that times out.").await.unwrap();
        assert!(session.is_background_paused() && session.run_background_turn().unwrap().is_none());
        assert!(wakeups.next().now_or_never().is_none());
        session.resume_background();
        assert_eq!(wakeups.next().now_or_never(), Some(Some(())));
        assert!(
            session
                .drain_background_prompt()
                .unwrap()
                .contains("<monitor_event>6</monitor_event>")
        );
    });
}

#[test]
fn meta_context_is_delivered_and_persisted_without_visible_user_bubbles() {
    use pikra_agent::AgentMessage;
    use pikra_session::{JsonlSessionStore, SessionEntryPayload};

    block_on(async {
        let dir = tempdir().unwrap();
        let http = Arc::new(FauxTransport::new());
        let provider = Arc::new(Client::new(http.clone()));
        let config = KitSessionConfig::new(test_model())
            .with_skill_discovery(SkillDiscoveryConfig {
                include_agents_skills: false,
                include_user_skills: false,
                ..Default::default()
            })
            .with_store(Arc::new(JsonlSessionStore::new(
                dir.path().join("meta.jsonl"),
            )));
        let session = KitSession::new(provider.clone(), config.clone()).unwrap();
        assert!(session.set_context("env", "active-env"));
        session.append_context("one-off-default");
        session.append_named_context("env", "one-off-env");
        session.append_meta("raw-meta");

        let gate = http.enqueue_gated_sse();
        for id in ["steer", "follow-up", "human", "background", "summary"] {
            http.enqueue_sse(openai_sse_text(id, id));
        }
        let mut stream = session.prompt_meta_stream("direct-meta").unwrap();
        let chunks = openai_sse_text("direct", "Got the context");
        for chunk in &chunks[..2] {
            assert!(gate.push_str(chunk));
        }
        loop {
            let event = stream.next().await.unwrap();
            assert!(!matches!(event, KitEvent::Error(_)), "{event:?}");
            if matches!(
                event,
                KitEvent::Agent(AgentEvent::MessageUpdate {
                    assistant_message_event: pikra_ai::AssistantMessageEvent::TextDelta { .. },
                    ..
                })
            ) {
                break;
            }
        }
        session.steer_meta("s", "steer-meta");
        session.follow_up_meta("f", "follow-up-meta");
        for chunk in &chunks[2..] {
            assert!(gate.push_str(chunk));
        }
        gate.close();

        let mut steer_stages = Vec::new();
        while let Some(event) = stream.next().await {
            match event {
                KitEvent::Agent(AgentEvent::Steered { .. }) | KitEvent::Error(_) => {
                    panic!("{event:?}")
                }
                KitEvent::Agent(AgentEvent::MessageStart {
                    message: AgentMessage::Llm(Message::User(user)),
                }) if user.id.as_deref() == Some("s") => steer_stages.push(("start", user.is_meta)),
                KitEvent::Agent(AgentEvent::MessageEnd {
                    message: AgentMessage::Llm(Message::User(user)),
                }) if user.id.as_deref() == Some("s") => steer_stages.push(("end", user.is_meta)),
                _ => {}
            }
        }
        drop(stream);
        assert_eq!(steer_stages, [("start", true), ("end", true)]);

        let human = "<system-reminder>This is visible user text.</system-reminder>";
        session.prompt(human).await.unwrap();
        session.background_sender().send(BackgroundNotification {
            source_id: "fixture".into(),
            kind: "monitor".into(),
            summary: "Background change".into(),
            xml_payload: "<monitor_event>background-meta</monitor_event>".into(),
            timestamp_ms: 1,
        });
        let mut stream = session.run_background_turn().unwrap().unwrap();
        while let Some(event) = stream.next().await {
            assert!(!matches!(event, KitEvent::Error(_)), "{event:?}");
        }
        drop(stream);

        let meta_texts = [
            "<system-reminder name=\"env\">\nactive-env\n</system-reminder>",
            "<system-reminder name=\"context\">\none-off-default\n</system-reminder>",
            "<system-reminder name=\"env\">\none-off-env\n</system-reminder>",
            "raw-meta",
            "direct-meta",
            "steer-meta",
            "follow-up-meta",
            "<background_events>\n<monitor_event>background-meta</monitor_event>\n</background_events>",
        ];
        let users = |messages: Vec<AgentMessage>| {
            messages
                .into_iter()
                .filter_map(|message| match message {
                    AgentMessage::Llm(Message::User(user)) => match user.content.as_slice() {
                        [ContentPart::Text(text)] => Some((text.text.clone(), user.is_meta)),
                        _ => panic!("expected a text message"),
                    },
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let snapshot = users(session.messages());
        assert_eq!(snapshot.len(), meta_texts.len() + 1);
        for text in meta_texts {
            assert!(snapshot.contains(&(text.to_owned(), true)), "{text}");
        }
        assert_eq!(
            users(session.visible_messages()),
            [(human.to_owned(), false)]
        );
        let requests = http.requests();
        assert_eq!(requests.len(), 5);
        for request in &requests {
            let body = request.body_json.to_string();
            assert!(!body.contains("is_meta") && !body.contains("isMeta"));
        }
        let final_input = requests[4].body_json["input"].as_array().unwrap();
        for text in meta_texts {
            assert!(
                final_input
                    .iter()
                    .any(|item| item["role"] == "user" && item["content"][0]["text"] == text),
                "{text}"
            );
        }
        drop(session);

        let session = KitSession::new(provider.clone(), config.clone()).unwrap();
        assert_eq!(users(session.messages()), snapshot);
        assert!(!session.set_context("env", "active-env"));
        assert!(session.compact().await.unwrap().is_some());
        drop(session);

        let session = KitSession::new(provider, config).unwrap();
        let compacted = users(session.messages());
        assert!(
            compacted
                .iter()
                .any(|(text, meta)| *meta && text.starts_with("<compaction-summary>"))
        );
        assert!(compacted.contains(&(meta_texts[7].into(), true)));
        let raw_entries = session
            .entries()
            .into_iter()
            .filter_map(|entry| match entry.payload {
                SessionEntryPayload::Message { message } => Some(message),
                _ => None,
            })
            .collect();
        assert_eq!(
            users(raw_entries)
                .iter()
                .filter(|(text, meta)| text == "raw-meta" && *meta)
                .count(),
            1
        );
        assert!(
            users(session.visible_messages())
                .iter()
                .all(|(_, meta)| !meta)
        );
    });
}
