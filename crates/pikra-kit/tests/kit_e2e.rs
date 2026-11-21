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
    KitEvent, KitSession, KitSessionConfig, McpClient, McpToolRegistrationMode, StdioMcpConfig,
    StdioMcpTransport, CALL_TOOL_NAME, CODEMODE_EXEC_TOOL_NAME, TOOL_SEARCH_NAME,
};
use pikra_schema::{int_schema, obj_schema, str_schema};
use pikra_testkit::{assert_prompt_prefix_identical, FauxTransport, ScriptedHttpResponse};
use serde_json::{json, Value};
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

fn json_http_response(status: u16, headers: Vec<(String, String)>, body: Value) -> ScriptedHttpResponse {
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
        assert!(session
            .resident_tool_definitions()
            .iter()
            .any(|t| t.name == CODEMODE_EXEC_TOOL_NAME));

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
        assert_eq!(parsed["hasExec"], false, "exec must be excluded to prevent recursion");
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

        let attached = session
            .attach_mcp_client(
                Arc::clone(&mcp_client),
                McpToolRegistrationMode::Deferred,
                Some("mcp"),
                CancelToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(attached, vec!["mcp_lookup_weather".to_string()]);

        llm_http.enqueue_sse(openai_sse_tool_call(
            "m1",
            "call_mcp_weather",
            CALL_TOOL_NAME,
            &json!({ "name": "mcp_lookup_weather", "arguments": { "city": "Tokyo" } }),
        ));
        llm_http.enqueue_sse(openai_sse_text("m2", "It is 22C and sunny in Tokyo."));

        let msgs = session.prompt("What is the weather in Tokyo?").await.unwrap();
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

        // Verify MCP HTTP protocol headers (Mcp-Session-Id & MCP-Protocol-Version propagated!).
        let mcp_reqs = mcp_http.requests();
        assert_eq!(mcp_reqs.len(), 4);
        assert_eq!(mcp_reqs[0].body_json["method"], "initialize");
        assert_eq!(mcp_reqs[1].body_json["method"], "notifications/initialized");
        assert_eq!(mcp_reqs[1].header("mcp-session-id"), Some("sess_abc123"));
        assert_eq!(mcp_reqs[1].header("mcp-protocol-version"), Some("2025-03-26"));
        assert_eq!(mcp_reqs[2].body_json["method"], "tools/list");
        assert_eq!(mcp_reqs[3].body_json["method"], "tools/call");
        assert_eq!(mcp_reqs[3].body_json["params"]["name"], "lookup_weather");
        assert_eq!(mcp_reqs[3].body_json["params"]["arguments"]["city"], "Tokyo");
    });
}

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
