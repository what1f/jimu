use serde_json::{Value, json};
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Notify;

struct RpcClientProc {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    buf: Vec<u8>,
    _fixture_home: tempfile::TempDir,
}

impl RpcClientProc {
    fn spawn(args: &[&str]) -> Self {
        Self::spawn_with_home(args, tempfile::tempdir().expect("fixture home"))
    }

    fn spawn_with_home(args: &[&str], home: tempfile::TempDir) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_pikra"))
            .args(args)
            .env("HOME", home.path())
            .env("PIKRA_HOME", home.path().join(".pikra"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn pikra binary");
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        Self {
            child,
            stdin: Some(stdin),
            stdout,
            buf: Vec::with_capacity(4096),
            _fixture_home: home,
        }
    }

    async fn send_raw_bytes(&mut self, bytes: &[u8]) {
        let stdin = self.stdin.as_mut().expect("stdin still open");
        stdin.write_all(bytes).await.expect("write stdin");
        stdin.flush().await.expect("flush stdin");
    }

    async fn send_json(&mut self, val: &Value) {
        let mut line = serde_json::to_string(val).expect("serialize cmd");
        line.push('\n');
        self.send_raw_bytes(line.as_bytes()).await;
    }

    /// Strict LF byte reader matching RPC framing (never splits on U+2028/U+2029).
    async fn next_record(&mut self) -> Value {
        self.buf.clear();
        let n = self
            .stdout
            .read_until(b'\n', &mut self.buf)
            .await
            .expect("read stdout");
        assert!(n > 0, "unexpected EOF on pikra stdout");
        if self.buf.last() == Some(&b'\n') {
            self.buf.pop();
        }
        if self.buf.last() == Some(&b'\r') {
            self.buf.pop();
        }
        let text = String::from_utf8(self.buf.clone()).expect("valid utf8 record");
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("invalid JSON record `{text}`: {e}"))
    }

    async fn read_response_for(&mut self, id: &str) -> (Value, Vec<Value>) {
        let mut events = Vec::new();
        loop {
            let rec = self.next_record().await;
            if rec.get("type").and_then(Value::as_str) == Some("response")
                && rec.get("id").and_then(Value::as_str) == Some(id)
            {
                return (rec, events);
            }
            events.push(rec);
        }
    }

    async fn close_stdin_and_wait(mut self) -> std::process::ExitStatus {
        drop(self.stdin.take());
        self.child.wait().await.expect("wait child")
    }
}

async fn read_http_request_body(stream: &mut TcpStream) -> String {
    let mut raw = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end;
    loop {
        let n = stream.read(&mut tmp).await.expect("read http req");
        assert!(n > 0, "client closed before full headers");
        raw.extend_from_slice(&tmp[..n]);
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }
    }
    let headers_str = String::from_utf8_lossy(&raw[..header_end]);
    let content_len: usize = headers_str
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            if k.eq_ignore_ascii_case("content-length") {
                v.trim().parse().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);

    while raw.len() < header_end + content_len {
        let n = stream.read(&mut tmp).await.expect("read http body");
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&tmp[..n]);
    }
    String::from_utf8_lossy(&raw[header_end..header_end + content_len]).to_string()
}

fn sse_chunk(delta: Value, finish_reason: Option<&str>) -> String {
    let payload = json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion.chunk",
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish_reason,
        }]
    });
    format!("data: {payload}\n\n")
}

async fn write_sse_headers(stream: &mut TcpStream) {
    let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
    stream
        .write_all(headers.as_bytes())
        .await
        .expect("write sse headers");
    stream.flush().await.expect("flush headers");
}

#[tokio::test]
async fn rpc_full_lifecycle_over_stdio_pipes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = tmp.path();
    std::fs::write(ws.join("input.txt"), "alpha-file-content").expect("write input.txt");
    std::fs::create_dir(ws.join(".git")).expect("set project root");
    let home = tempfile::tempdir().expect("fixture home");
    let data_dir = home.path().join("data");
    for (root, name) in [
        (ws.join(".agents/skills"), "rpc-agents-skill"),
        (ws.join(".pikra/skills"), "rpc-project-skill"),
        (ws.join("extra-skills"), "rpc-extra-skill"),
        (home.path().join(".agents/skills"), "rpc-user-agents-skill"),
        (data_dir.join("skills"), "rpc-user-pikra-skill"),
    ] {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).expect("create skill folder");
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: RPC fixture\n---\n"),
        )
        .expect("write skill");
    }
    let extra_instructions = ws.join("extra.md");
    let last_instructions = ws.join("last.md");
    let instruction_sources = [
        (
            home.path().join(".agents/AGENTS.md"),
            "neutral user instructions",
        ),
        (data_dir.join("AGENTS.md"), "data directory instructions"),
        (ws.join("AGENTS.override.md"), "workspace instructions"),
        (extra_instructions.clone(), "explicit instructions one"),
        (last_instructions.clone(), "explicit instructions two"),
    ];
    for (file, text) in &instruction_sources {
        std::fs::write(file, text).unwrap();
    }
    let extra_skills = ws.join("extra-skills");

    let ext_path = ws.join("hook.js");
    std::fs::write(
        &ext_path,
        r#"
export default function(pikra) {
  pikra.on("after_tool_call", (ctx) => {
    if (ctx.toolCall && ctx.toolCall.name === "read") {
      const existing = (ctx.result.content || []).map(p => p.text || "").join("");
      return { content: existing + " [js-hook-ok]" };
    }
  });
}
"#,
    )
    .expect("write hook.js");

    let session_path = ws.join("session.jsonl");

    // Local loopback mock OpenAI Chat Completions SSE server.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let captured_bodies: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let release_req1 = Arc::new(Notify::new());

    let srv_bodies = Arc::clone(&captured_bodies);
    let srv_release_req1 = Arc::clone(&release_req1);

    let server_handle = tokio::spawn(async move {
        let mut req_idx = 0usize;
        while let Ok((mut socket, _)) = listener.accept().await {
            let body_str = read_http_request_body(&mut socket).await;
            let body_json: Value = serde_json::from_str(&body_str).expect("valid req json");
            srv_bodies.lock().unwrap().push(body_json);
            req_idx += 1;

            write_sse_headers(&mut socket).await;

            match req_idx {
                1 => {
                    // Emit initial delta, wait for test to inspect streaming state & enqueue steer,
                    // then emit write + read tool calls.
                    let chunk1 = sse_chunk(
                        json!({ "role": "assistant", "content": "working..." }),
                        None,
                    );
                    socket.write_all(chunk1.as_bytes()).await.unwrap();
                    socket.flush().await.unwrap();

                    srv_release_req1.notified().await;

                    let tc_write = sse_chunk(
                        json!({
                            "tool_calls": [{
                                "index": 0,
                                "id": "call_write_1",
                                "type": "function",
                                "function": {
                                    "name": "write",
                                    "arguments": "{\"path\":\"created.txt\",\"content\":\"hello-pikra\"}"
                                }
                            }]
                        }),
                        None,
                    );
                    let tc_read = sse_chunk(
                        json!({
                            "tool_calls": [{
                                "index": 1,
                                "id": "call_read_1",
                                "type": "function",
                                "function": {
                                    "name": "read",
                                    "arguments": "{\"path\":\"input.txt\"}"
                                }
                            }]
                        }),
                        Some("tool_calls"),
                    );
                    socket.write_all(tc_write.as_bytes()).await.unwrap();
                    socket.write_all(tc_read.as_bytes()).await.unwrap();
                    socket.write_all(b"data: [DONE]\n\n").await.unwrap();
                    socket.flush().await.unwrap();
                }
                2 => {
                    let c1 = sse_chunk(json!({ "role": "assistant", "content": "Done " }), None);
                    let c2 = sse_chunk(json!({ "content": "after steer!" }), Some("stop"));
                    socket.write_all(c1.as_bytes()).await.unwrap();
                    socket.write_all(c2.as_bytes()).await.unwrap();
                    socket.write_all(b"data: [DONE]\n\n").await.unwrap();
                    socket.flush().await.unwrap();
                }
                3 => {
                    // Emit one delta then hold open until client aborts the connection.
                    let c1 = sse_chunk(
                        json!({ "role": "assistant", "content": "partial-before-abort" }),
                        None,
                    );
                    let _ = socket.write_all(c1.as_bytes()).await;
                    let _ = socket.flush().await;
                    // Wait for client disconnect (abort).
                    let mut drain = [0u8; 64];
                    let _ = socket.read(&mut drain).await;
                }
                _ => {
                    let c1 = sse_chunk(
                        json!({ "role": "assistant", "content": "extra" }),
                        Some("stop"),
                    );
                    let _ = socket.write_all(c1.as_bytes()).await;
                    let _ = socket.write_all(b"data: [DONE]\n\n").await;
                    let _ = socket.flush().await;
                }
            }
        }
    });

    let base_url = format!("http://{addr}/v1");
    let cwd_str = ws.display().to_string();
    let session_str = session_path.display().to_string();
    let ext_str = ext_path.display().to_string();

    let mut proc = RpcClientProc::spawn_with_home(
        &[
            "--mode",
            "rpc",
            "--cwd",
            &cwd_str,
            "--provider",
            "mock",
            "--model",
            "mock-1",
            "--api",
            "openai-completions",
            "--base-url",
            &base_url,
            "--session",
            &session_str,
            "--extension",
            &ext_str,
            "--skill-dir",
            extra_skills.to_str().unwrap(),
            "--pikra-home",
            data_dir.to_str().unwrap(),
            "--walk-boundary=cwd-only",
            "--instruction-max-bytes=unlimited",
            "--instruction-file",
            extra_instructions.to_str().unwrap(),
            "--instruction-file",
            last_instructions.to_str().unwrap(),
        ],
        home,
    );

    // 1. Malformed JSON emits parse error without crashing.
    proc.send_raw_bytes(b"{not-valid-json\n").await;
    let parse_err = proc.next_record().await;
    assert_eq!(parse_err["type"], "response");
    assert_eq!(parse_err["command"], "parse");
    assert_eq!(parse_err["success"], false);

    // 2. get_state reflects model, tools, and loaded JS extension.
    proc.send_json(&json!({ "id": "s1", "type": "get_state" }))
        .await;
    let (state_resp, _) = proc.read_response_for("s1").await;
    assert_eq!(state_resp["success"], true);
    assert_eq!(state_resp["data"]["model"]["id"], "mock-1");
    assert_eq!(state_resp["data"]["is_streaming"], false);
    let tools: Vec<&str> = state_resp["data"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    for name in [
        "read",
        "write",
        "edit",
        "apply_patch",
        "find",
        "grep",
        "bash",
        "sleep",
        "web_fetch",
    ] {
        assert!(tools.contains(&name), "missing default tool {name}");
    }
    #[cfg(unix)]
    for name in ["exec_command", "write_stdin", "monitor"] {
        assert!(tools.contains(&name), "missing default tool {name}");
    }
    assert_eq!(
        state_resp["data"]["extensions"].as_array().unwrap().len(),
        1
    );

    // 3. set_context queues named context block for the next turn.
    proc.send_json(&json!({
        "id": "ctx1",
        "type": "set_context",
        "name": "workspace_env",
        "text": "mode=test"
    }))
    .await;
    let (r_ctx1, _) = proc.read_response_for("ctx1").await;
    assert_eq!(r_ctx1["data"]["changed"], true);

    // 4. Send prompt containing raw UTF-8 U+2028 inside JSON string (must not split record!).
    let raw_prompt = "{\"id\":\"p1\",\"type\":\"prompt\",\"message\":\"hello\u{2028}world\"}\r\n";
    proc.send_raw_bytes(raw_prompt.as_bytes()).await;
    let (p1_resp, _) = proc.read_response_for("p1").await;
    assert_eq!(p1_resp["success"], true);
    assert_eq!(p1_resp["data"]["disposition"], "started");

    // Wait until streaming delta arrives so we know Request 1 is active mid-stream.
    let mut saw_context_event = false;
    let mut saw_skill_catalog = false;
    loop {
        let ev = proc.next_record().await;
        if ev["type"] == "context_appended" && ev["name"] == "workspace_env" {
            saw_context_event = true;
        }
        if ev["type"] == "context_appended" && ev["name"] == "skills" {
            let text = ev["text"].as_str().unwrap();
            assert!(
                text.contains("rpc-agents-skill")
                    && text.contains("rpc-project-skill")
                    && text.contains("rpc-extra-skill")
                    && text.contains("rpc-user-agents-skill")
                    && text.contains("rpc-user-pikra-skill")
            );
            saw_skill_catalog = true;
        }
        if ev["type"] == "message_update"
            && ev["assistant_message_event"]["type"] == "text_delta"
            && ev["assistant_message_event"]["delta"] == "working..."
        {
            break;
        }
    }
    assert!(saw_context_event, "expected context_appended event");
    assert!(saw_skill_catalog, "expected discovered skills in context");

    // Concurrently inspect state & enqueue mid-stream steer while prompt is running.
    proc.send_json(&json!({ "id": "s2", "type": "get_state" }))
        .await;
    let (s2_resp, _) = proc.read_response_for("s2").await;
    assert_eq!(s2_resp["data"]["is_streaming"], true);

    proc.send_json(&json!({
        "id": "st1",
        "type": "steer",
        "message": "switch to beta"
    }))
    .await;
    let (st1_resp, _) = proc.read_response_for("st1").await;
    assert_eq!(st1_resp["success"], true);
    assert_eq!(st1_resp["data"]["disposition"], "queued");

    // Release Request 1 so tool calls run and queued steer is delivered to Request 2.
    release_req1.notify_one();

    let mut saw_steered = false;
    let mut saw_read_hooked = false;
    let mut streamed_text = String::new();
    loop {
        let ev = proc.next_record().await;
        match ev["type"].as_str() {
            Some("steered") => {
                if ev["text"] == "switch to beta" {
                    saw_steered = true;
                }
            }
            Some("tool_execution_end") => {
                if ev["tool_name"] == "read" {
                    let out = ev["result"]["content"][0]["text"].as_str().unwrap_or("");
                    if out.contains("alpha-file-content") && out.contains("[js-hook-ok]") {
                        saw_read_hooked = true;
                    }
                }
            }
            Some("message_update") => {
                if ev["assistant_message_event"]["type"] == "text_delta" {
                    streamed_text.push_str(
                        ev["assistant_message_event"]["delta"]
                            .as_str()
                            .unwrap_or(""),
                    );
                }
            }
            Some("agent_settled") => {
                assert_eq!(ev["aborted"], false);
                break;
            }
            _ => {}
        }
    }

    assert!(saw_steered, "expected steered event");
    assert!(saw_read_hooked, "expected JS after_tool_call hook output");
    assert_eq!(streamed_text, "Done after steer!");
    assert_eq!(
        std::fs::read_to_string(ws.join("created.txt")).expect("created.txt"),
        "hello-pikra"
    );

    // Verify wire requests captured U+2028, <system-reminder>, JS hook output, and steer message.
    {
        let bodies = captured_bodies.lock().unwrap();
        assert!(bodies.len() >= 2);
        let req1_msgs: Vec<&str> = bodies[0]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["content"].as_str())
            .collect();
        assert!(
            req1_msgs.iter().any(|c| c.contains("hello\u{2028}world")),
            "expected U+2028 preserved in request 1 messages: {req1_msgs:?}"
        );
        assert!(
            req1_msgs
                .iter()
                .any(|c| c.contains("<system-reminder name=\"workspace_env\">")),
            "expected <system-reminder> in request 1 messages: {req1_msgs:?}"
        );
        let instructions = req1_msgs
            .iter()
            .find(|content| content.contains("<system-reminder name=\"instructions\">"))
            .unwrap();
        let positions: Vec<_> = instruction_sources
            .iter()
            .map(|(_, text)| instructions.find(text).unwrap())
            .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        let req2_str = serde_json::to_string(&bodies[1]).unwrap();
        assert!(req2_str.contains("[js-hook-ok]"));
        assert!(req2_str.contains("switch to beta"));
    }

    // Check deduplication after turn flushed the context block onto the branch.
    proc.send_json(&json!({
        "id": "ctx2",
        "type": "set_context",
        "name": "workspace_env",
        "text": "mode=test"
    }))
    .await;
    let (r_ctx2, _) = proc.read_response_for("ctx2").await;
    assert_eq!(r_ctx2["data"]["changed"], false);

    // Check get_last_assistant_text.
    proc.send_json(&json!({ "id": "last1", "type": "get_last_assistant_text" }))
        .await;
    let (last_resp, _) = proc.read_response_for("last1").await;
    assert_eq!(last_resp["data"]["text"], "Done after steer!");

    // 5. Mid-stream abort.
    proc.send_json(&json!({
        "id": "p2",
        "type": "prompt",
        "message": "long running request"
    }))
    .await;
    let (p2_resp, _) = proc.read_response_for("p2").await;
    assert_eq!(p2_resp["data"]["disposition"], "started");

    loop {
        let ev = proc.next_record().await;
        if ev["type"] == "message_update"
            && ev["assistant_message_event"]["type"] == "text_delta"
            && ev["assistant_message_event"]["delta"] == "partial-before-abort"
        {
            break;
        }
    }

    proc.send_json(&json!({ "id": "ab1", "type": "abort" }))
        .await;
    let (ab1_resp, ab_events) = proc.read_response_for("ab1").await;
    assert_eq!(ab1_resp["success"], true);
    assert!(
        ab_events
            .iter()
            .any(|e| e["type"] == "agent_settled" && e["aborted"] == true),
        "expected agent_settled(aborted=true) before abort response"
    );

    // 6. get_entries, get_tree, fork, and queue/config commands.
    proc.send_json(&json!({ "id": "ent1", "type": "get_entries" }))
        .await;
    let (ent_resp, _) = proc.read_response_for("ent1").await;
    let entries = ent_resp["data"]["entries"].as_array().unwrap();
    assert!(entries.len() >= 4);
    let fork_target_id = entries[1]["id"].as_str().unwrap().to_string();

    proc.send_json(&json!({ "id": "fk1", "type": "fork", "entry_id": fork_target_id }))
        .await;
    let (fk_resp, fk_events) = proc.read_response_for("fk1").await;
    assert_eq!(fk_resp["success"], true);
    assert_eq!(fk_resp["data"]["leaf_id"], fork_target_id);
    assert!(fk_events.iter().any(|e| e["type"] == "forked"));

    proc.send_json(&json!({ "id": "tr1", "type": "get_tree" }))
        .await;
    let (tr_resp, _) = proc.read_response_for("tr1").await;
    assert_eq!(tr_resp["data"]["leaf_id"], fork_target_id);
    assert!(!tr_resp["data"]["tree"].as_array().unwrap().is_empty());

    // Queue + clear_queue
    proc.send_json(&json!({ "id": "q1", "type": "steer", "message": "queued-s" }))
        .await;
    let _ = proc.read_response_for("q1").await;
    proc.send_json(&json!({ "id": "q2", "type": "clear_queue" }))
        .await;
    let (clr_resp, _) = proc.read_response_for("q2").await;
    assert_eq!(clr_resp["data"]["steering"], json!(["queued-s"]));

    // Model + thinking + fast + auto_compaction
    proc.send_json(&json!({
        "id": "m1",
        "type": "set_model",
        "provider": "mock",
        "model_id": "mock-2"
    }))
    .await;
    let (m1_resp, _) = proc.read_response_for("m1").await;
    assert_eq!(m1_resp["success"], true);
    assert_eq!(m1_resp["data"]["id"], "mock-2");

    proc.send_json(&json!({ "id": "m2", "type": "get_available_models" }))
        .await;
    let (m2_resp, _) = proc.read_response_for("m2").await;
    assert_eq!(m2_resp["data"]["models"].as_array().unwrap().len(), 2);

    proc.send_json(&json!({ "id": "th1", "type": "set_thinking_level", "level": "medium" }))
        .await;
    assert_eq!(proc.read_response_for("th1").await.0["success"], true);

    proc.send_json(&json!({ "id": "f1", "type": "set_fast", "fast": true }))
        .await;
    assert_eq!(proc.read_response_for("f1").await.0["success"], true);

    proc.send_json(&json!({ "id": "ac1", "type": "set_auto_compaction", "enabled": false }))
        .await;
    assert_eq!(
        proc.read_response_for("ac1").await.0["data"]["enabled"],
        false
    );

    // Verify session JSONL persisted on disk, then test new_session + switch_session.
    assert!(Path::new(&session_path).exists());
    proc.send_json(&json!({ "id": "ns1", "type": "new_session" }))
        .await;
    let (ns_resp, _) = proc.read_response_for("ns1").await;
    assert_eq!(ns_resp["success"], true);
    let new_sid = ns_resp["data"]["session_id"].as_str().unwrap().to_string();

    proc.send_json(&json!({
        "id": "sw1",
        "type": "switch_session",
        "session_path": session_str
    }))
    .await;
    let (sw_resp, _) = proc.read_response_for("sw1").await;
    assert_eq!(sw_resp["success"], true);
    assert_ne!(sw_resp["data"]["session_id"].as_str().unwrap(), new_sid);

    // 7. Clean exit on stdin EOF.
    let status = proc.close_stdin_and_wait().await;
    assert!(status.success(), "expected clean exit 0, got {status}");

    server_handle.abort();
}

#[tokio::test]
async fn rpc_live_deepseek_smoke_if_key_present() {
    if std::env::var("DORA_TEST_DEEPSEEK_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .is_none()
    {
        return;
    }

    let workspace = tempfile::tempdir().expect("live fixture workspace");
    let mut proc = RpcClientProc::spawn(&[
        "--mode",
        "rpc",
        "--cwd",
        workspace.path().to_str().unwrap(),
        "--no-session",
        "--no-tools",
        "--provider",
        "deepseek",
        "--model",
        "deepseek-flash",
        "--api",
        "openai-completions",
        "--base-url",
        "https://api.deepseek.com/v1",
        "--api-key-env",
        "DORA_TEST_DEEPSEEK_KEY",
    ]);

    proc.send_json(&json!({
        "id": "live-1",
        "type": "prompt",
        "message": "Reply with exactly the single word PONG."
    }))
    .await;
    let (resp, _) = proc.read_response_for("live-1").await;
    assert_eq!(resp["success"], true);
    assert_eq!(resp["data"]["disposition"], "started");

    let mut saw_delta = false;
    loop {
        let ev = proc.next_record().await;
        if ev["type"] == "message_update" && ev["assistant_message_event"]["type"] == "text_delta" {
            saw_delta = true;
        }
        if ev["type"] == "agent_settled" {
            assert_eq!(ev["aborted"], false);
            break;
        }
    }
    assert!(
        saw_delta,
        "expected streaming text_delta from live DeepSeek"
    );

    proc.send_json(&json!({ "id": "live-last", "type": "get_last_assistant_text" }))
        .await;
    let (last, _) = proc.read_response_for("live-last").await;
    let reply = last["data"]["text"].as_str().unwrap_or("");
    assert!(!reply.trim().is_empty(), "expected non-empty live reply");

    let status = proc.close_stdin_and_wait().await;
    assert!(status.success(), "expected clean exit 0, got {status}");
}

#[tokio::test]
async fn rpc_review_regressions_defaults_init_oneshot_model_restore_and_async_compact() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = tmp.path();
    let cwd_str = ws.display().to_string();

    // #13: Initialization failure (--session pointing to a directory) must exit non-zero immediately without hanging.
    let bad_init = Command::new(env!("CARGO_BIN_EXE_pikra"))
        .args(["--mode", "rpc", "--cwd", &cwd_str, "--session", &cwd_str])
        .stdin(Stdio::null())
        .output()
        .await
        .expect("run pikra with dir session");
    assert!(
        !bad_init.status.success(),
        "expected non-zero exit when --session is a directory"
    );
    let bad_stderr = String::from_utf8_lossy(&bad_init.stderr);
    assert!(
        bad_stderr.contains("error:"),
        "expected error on stderr: {bad_stderr}"
    );

    // #9: Default Anthropic and DeepSeek models use non-retired IDs.
    for (provider, expected_model) in [
        ("anthropic", "claude-sonnet-5-5"),
        ("deepseek", "deepseek-flash"),
    ] {
        let mut p = RpcClientProc::spawn(&[
            "--mode",
            "rpc",
            "--no-session",
            "--no-tools",
            "--provider",
            provider,
        ]);
        p.send_json(&json!({ "id": "st", "type": "get_state" }))
            .await;
        let (resp, _) = p.read_response_for("st").await;
        assert_eq!(resp["data"]["model"]["id"], expected_model);
        assert!(p.close_stdin_and_wait().await.success());
    }

    // #14: One-shot text & json modes exit non-zero on provider request failure (closed loopback port).
    let dead_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let dead_addr = dead_listener.local_addr().expect("addr");
    drop(dead_listener);
    let dead_url = format!("http://{dead_addr}/v1");

    let oneshot_text = Command::new(env!("CARGO_BIN_EXE_pikra"))
        .args([
            "ask",
            "--no-user-instructions",
            "--no-session",
            "--no-tools",
            "--provider",
            "mock",
            "--model",
            "mock-1",
            "--api",
            "openai-completions",
            "--base-url",
            &dead_url,
            "ping",
        ])
        .output()
        .await
        .expect("run oneshot text");
    assert!(
        !oneshot_text.status.success(),
        "expected non-zero exit in text mode"
    );
    assert!(
        !oneshot_text.stderr.is_empty(),
        "expected error message on stderr in text mode"
    );

    let oneshot_json = Command::new(env!("CARGO_BIN_EXE_pikra"))
        .args([
            "--mode",
            "json",
            "--no-instructions",
            "--no-session",
            "--no-tools",
            "--provider",
            "mock",
            "--model",
            "mock-1",
            "--api",
            "openai-completions",
            "--base-url",
            &dead_url,
            "ping",
        ])
        .output()
        .await
        .expect("run oneshot json");
    assert!(
        !oneshot_json.status.success(),
        "expected non-zero exit in json mode"
    );

    // #15 & #16: Model persistence across restart + non-blocking manual compact with get_state & abort.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let addr = listener.local_addr().expect("local addr");
    let compact_req_seen = Arc::new(Notify::new());
    let srv_compact_seen = Arc::clone(&compact_req_seen);

    let server = tokio::spawn(async move {
        let mut idx = 0usize;
        while let Ok((mut sock, _)) = listener.accept().await {
            let _ = read_http_request_body(&mut sock).await;
            idx += 1;
            write_sse_headers(&mut sock).await;
            if idx == 1 {
                let c = sse_chunk(
                    json!({ "role": "assistant", "content": "turn1 ok" }),
                    Some("stop"),
                );
                let _ = sock.write_all(c.as_bytes()).await;
                let _ = sock.write_all(b"data: [DONE]\n\n").await;
                let _ = sock.flush().await;
            } else {
                // Stall compaction summarization stream until aborted by client.
                srv_compact_seen.notify_one();
                let mut buf = [0u8; 64];
                let _ = sock.read(&mut buf).await;
            }
        }
    });

    let base_url = format!("http://{addr}/v1");
    let session_path = ws.join("persist-model.jsonl");
    let session_str = session_path.display().to_string();

    // First process: set_model to custom local/chosen + run 1 turn so compaction has messages.
    {
        let mut p1 = RpcClientProc::spawn(&[
            "--mode",
            "rpc",
            "--cwd",
            &cwd_str,
            "--no-tools",
            "--session",
            &session_str,
        ]);
        p1.send_json(&json!({
            "id": "sm1",
            "type": "set_model",
            "provider": "local",
            "model_id": "chosen",
            "api": "openai-completions",
            "base_url": base_url,
            "context_window": 32000,
            "max_output_tokens": 4096,
            "supports_images": false,
            "supports_reasoning": false
        }))
        .await;
        let (sm_resp, _) = p1.read_response_for("sm1").await;
        assert_eq!(sm_resp["success"], true);

        p1.send_json(&json!({ "id": "pr1", "type": "prompt", "message": "hello" }))
            .await;
        let (pr_resp, _) = p1.read_response_for("pr1").await;
        assert_eq!(pr_resp["success"], true);
        loop {
            let ev = p1.next_record().await;
            if ev["type"] == "agent_settled" {
                assert_eq!(ev["aborted"], false);
                break;
            }
        }
        assert!(p1.close_stdin_and_wait().await.success());
    }

    // Second process: reopen same --session WITHOUT --provider/--model -> restores local/chosen & custom base_url!
    {
        let mut p2 = RpcClientProc::spawn(&[
            "--mode",
            "rpc",
            "--cwd",
            &cwd_str,
            "--no-tools",
            "--session",
            &session_str,
        ]);
        p2.send_json(&json!({ "id": "gs1", "type": "get_state" }))
            .await;
        let (gs_resp, _) = p2.read_response_for("gs1").await;
        assert_eq!(gs_resp["data"]["model"]["provider"], "local");
        assert_eq!(gs_resp["data"]["model"]["id"], "chosen");
        assert_eq!(gs_resp["data"]["model"]["base_url"], base_url);
        assert_eq!(gs_resp["data"]["model"]["context_window"], 32000);

        // #16: Trigger manual compact (stalls on mock server), then verify get_state & busy prompt & abort do NOT block!
        p2.send_json(&json!({ "id": "cmp1", "type": "compact" }))
            .await;
        compact_req_seen.notified().await;

        p2.send_json(&json!({ "id": "gs2", "type": "get_state" }))
            .await;
        let (gs2_resp, _) = p2.read_response_for("gs2").await;
        assert_eq!(gs2_resp["success"], true);
        assert_eq!(gs2_resp["data"]["is_compacting"], true);

        p2.send_json(&json!({ "id": "busy1", "type": "prompt", "message": "blocked" }))
            .await;
        let (busy_resp, _) = p2.read_response_for("busy1").await;
        assert_eq!(busy_resp["success"], false);

        p2.send_json(&json!({ "id": "ab_cmp", "type": "abort" }))
            .await;
        let (ab_resp, between) = p2.read_response_for("ab_cmp").await;
        assert_eq!(ab_resp["success"], true);
        assert!(
            between
                .iter()
                .any(|r| r["type"] == "response" && r["id"] == "cmp1" && r["success"] == false),
            "expected aborted compact error response before abort settled: {between:?}"
        );

        assert!(p2.close_stdin_and_wait().await.success());
    }

    server.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn rpc_reports_persistence_error_when_session_file_unwritable() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().expect("tempdir");
    let ws = tmp.path();
    let session_path = ws.join("readonly.jsonl");
    let cwd_str = ws.display().to_string();
    let session_str = session_path.display().to_string();

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let _ = read_http_request_body(&mut sock).await;
            write_sse_headers(&mut sock).await;
            let c = sse_chunk(
                json!({ "role": "assistant", "content": "ok" }),
                Some("stop"),
            );
            let _ = sock.write_all(c.as_bytes()).await;
            let _ = sock.write_all(b"data: [DONE]\n\n").await;
            let _ = sock.flush().await;
        }
    });

    let base_url = format!("http://{addr}/v1");
    let mut proc = RpcClientProc::spawn(&[
        "--mode",
        "rpc",
        "--cwd",
        &cwd_str,
        "--no-tools",
        "--provider",
        "mock",
        "--model",
        "mock-1",
        "--api",
        "openai-completions",
        "--base-url",
        &base_url,
        "--session",
        &session_str,
    ]);

    // Ensure session header/baseline is written, then make file read-only so append_message fails.
    proc.send_json(&json!({ "id": "s0", "type": "get_state" }))
        .await;
    let _ = proc.read_response_for("s0").await;
    std::fs::set_permissions(&session_path, std::fs::Permissions::from_mode(0o444))
        .expect("chmod 0444");

    proc.send_json(&json!({ "id": "p_err", "type": "prompt", "message": "cannot persist" }))
        .await;
    let (p_resp, _) = proc.read_response_for("p_err").await;
    assert_eq!(p_resp["success"], true);

    let settled_error = loop {
        let ev = proc.next_record().await;
        if ev["type"] == "agent_settled" {
            break ev["error"].as_str().map(ToString::to_string);
        }
    };

    // Restore permissions so tempdir cleanup succeeds.
    let _ = std::fs::set_permissions(&session_path, std::fs::Permissions::from_mode(0o644));
    assert!(
        settled_error
            .as_deref()
            .is_some_and(|e| e.contains("failed to persist session message")),
        "expected persistence failure on agent_settled, got: {settled_error:?}"
    );

    assert!(proc.close_stdin_and_wait().await.success());
    server.abort();
}

#[tokio::test]
async fn acp_modes_initialize_over_stdio() {
    for flags in [&["--mode", "acp"][..], &["--acp"][..]] {
        let mut peer = RpcClientProc::spawn(flags);
        peer.send_json(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 1}
        }))
        .await;
        let response = peer.next_record().await;
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], 1);
        assert_eq!(response["result"]["protocolVersion"], 1);
        assert_eq!(response["result"]["agentInfo"]["name"], "pikra-acp");
        if flags.len() == 2 {
            peer.send_raw_bytes(b" \t\n{broken}\n").await;
            let malformed = peer.next_record().await;
            assert_eq!(malformed["error"]["code"], -32700);
            peer.send_json(&json!([])).await;
            assert_eq!(peer.next_record().await["error"]["code"], -32600);
            peer.send_json(&json!([
                {"jsonrpc":"2.0","method":"_ignored"},
                {"jsonrpc":"2.0","id":2,"method":"_unknown"},
                {"jsonrpc":"2.0","id":3,"method":"session/new","params":{"cwd":"relative"}},
                {"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"sessionId":"unused","prompt":[]}},
                {"jsonrpc":"2.0","method":"$/cancel_request","params":{"requestId":4}}
            ])).await;
            let batch = peer.next_record().await;
            let codes: Vec<_> = batch
                .as_array()
                .unwrap()
                .iter()
                .map(|reply| reply["error"]["code"].as_i64().unwrap())
                .collect();
            assert_eq!(codes, [-32601, -32602, -32800]);
        }
        assert!(peer.close_stdin_and_wait().await.success());
    }
}
