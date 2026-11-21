use serde_json::{json, Value};
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
}

impl RpcClientProc {
    fn spawn(args: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_pikra"))
            .args(args)
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
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
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
                    let chunk1 = sse_chunk(json!({ "role": "assistant", "content": "working..." }), None);
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
                    let c1 = sse_chunk(json!({ "role": "assistant", "content": "extra" }), Some("stop"));
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

    let mut proc = RpcClientProc::spawn(&[
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
    ]);

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
    assert!(tools.contains(&"read") && tools.contains(&"write"));
    assert_eq!(state_resp["data"]["extensions"].as_array().unwrap().len(), 1);

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
    loop {
        let ev = proc.next_record().await;
        if ev["type"] == "context_appended" && ev["name"] == "workspace_env" {
            saw_context_event = true;
        }
        if ev["type"] == "message_update"
            && ev["assistant_message_event"]["type"] == "text_delta"
            && ev["assistant_message_event"]["delta"] == "working..."
        {
            break;
        }
    }
    assert!(saw_context_event, "expected context_appended event");

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
    assert_eq!(proc.read_response_for("ac1").await.0["data"]["enabled"], false);

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

    let mut proc = RpcClientProc::spawn(&[
        "--mode",
        "rpc",
        "--no-session",
        "--no-tools",
        "--provider",
        "deepseek",
        "--model",
        "deepseek-chat",
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
        if ev["type"] == "message_update"
            && ev["assistant_message_event"]["type"] == "text_delta"
        {
            saw_delta = true;
        }
        if ev["type"] == "agent_settled" {
            assert_eq!(ev["aborted"], false);
            break;
        }
    }
    assert!(saw_delta, "expected streaming text_delta from live DeepSeek");

    proc.send_json(&json!({ "id": "live-last", "type": "get_last_assistant_text" }))
        .await;
    let (last, _) = proc.read_response_for("live-last").await;
    let reply = last["data"]["text"].as_str().unwrap_or("");
    assert!(!reply.trim().is_empty(), "expected non-empty live reply");

    let status = proc.close_stdin_and_wait().await;
    assert!(status.success(), "expected clean exit 0, got {status}");
}
