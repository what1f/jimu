use bytes::Bytes;
use futures_util::StreamExt;
use jimu_agent::{AgentTool, AgentToolResult, ToolProgressCallback};
use jimu_ai::{
    BoxFuture, CancelToken, ContentPart, HttpRequest, HttpTransport, ImageContent, SseStream,
    TextContent, ToolDefinition, TransportError,
};
use jimu_schema::Schema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use thiserror::Error;

/// Latest MCP protocol version requested by [`McpClient::initialize`].
pub const MCP_PROTOCOL_VERSION: &str = "2025-03-26";

const MAX_LIST_PAGES: usize = 1_000;

/// Errors returned by [`McpClient`] and [`McpTransport`] implementations.
#[derive(Debug, Error)]
pub enum McpError {
    #[error("MCP transport error: {0}")]
    Transport(#[from] TransportError),

    #[error("MCP JSON-RPC error {code}: {message}")]
    JsonRpc {
        code: i64,
        message: String,
        data: Option<Value>,
    },

    #[error("MCP protocol error: {0}")]
    Protocol(String),

    #[error("MCP JSON serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("MCP request cancelled")]
    Cancelled,
}

/// JSON-RPC 2.0 request/response identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcId {
    Number(u64),
    String(String),
}

impl JsonRpcId {
    fn matches_u64(&self, expected: u64) -> bool {
        match self {
            JsonRpcId::Number(n) => *n == expected,
            JsonRpcId::String(s) => s.parse::<u64>().ok() == Some(expected),
        }
    }
}

/// JSON-RPC 2.0 error object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// JSON-RPC 2.0 response envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<JsonRpcId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcErrorObject>,
}

/// Server identity reported in `initialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerInfo {
    pub name: String,
    pub version: String,
}

/// Result returned by MCP `initialize`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpInitializeResult {
    pub protocol_version: String,
    #[serde(default)]
    pub capabilities: Value,
    pub server_info: McpServerInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// Tool definition returned by MCP `tools/list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default = "default_object_schema")]
    pub input_schema: Schema,
}

fn default_object_schema() -> Schema {
    json!({ "type": "object", "properties": {} })
}

/// Normalized result returned by MCP `tools/call`.
#[derive(Debug, Clone, PartialEq)]
pub struct McpToolCallResult {
    pub content: Vec<ContentPart>,
    pub structured_content: Option<Value>,
    pub is_error: bool,
}

/// Controls whether discovered MCP tools are exposed as resident or deferred tools in [`crate::KitSession`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum McpToolRegistrationMode {
    /// Register into the top-level resident `tools` array before the first turn. Once a session
    /// already has messages, new or schema-changed tools automatically route through the deferred
    /// gateway so the prompt-cache prefix stays byte-identical.
    Resident,
    /// Register into the deferred tool gateway (`tool_search` / `call_tool`), preserving prompt cache bytes.
    #[default]
    Deferred,
    /// Admit tools within the session's schema budget before the first turn, and defer the rest.
    Auto,
}

/// Abstract JSON-RPC 2.0 transport trait for MCP (Streamable HTTP, stdio, in-memory, etc.).
pub trait McpTransport: Send + Sync {
    /// Send a JSON-RPC 2.0 request and await its matching [`JsonRpcResponse`].
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
        id: u64,
        cancel: CancelToken,
    ) -> BoxFuture<'a, Result<JsonRpcResponse, McpError>>;

    /// Send a JSON-RPC 2.0 notification (no response expected).
    fn notify<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
        cancel: CancelToken,
    ) -> BoxFuture<'a, Result<(), McpError>>;

    /// Optional hook to inform the transport of the negotiated MCP protocol version.
    fn set_protocol_version(&self, _version: String) {}

    /// Optional hook to clear transport-level session state before a fresh `initialize` handshake.
    fn reset_session(&self) {}

    /// Close the transport cleanly.
    fn close(&self) -> BoxFuture<'_, Result<(), McpError>> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Default)]
struct StreamableHttpState {
    session_id: Option<String>,
    protocol_version: Option<String>,
}

/// Streamable HTTP MCP transport over [`jimu_ai::HttpTransport`].
///
/// Supports both `application/json` and `text/event-stream` (SSE) responses, tracks
/// `Mcp-Session-Id` and `MCP-Protocol-Version` headers automatically, and requires zero `tokio`
/// or `reqwest` dependencies.
pub struct StreamableHttpMcpTransport {
    http: Arc<dyn HttpTransport>,
    url: String,
    headers: Vec<(String, String)>,
    state: Mutex<StreamableHttpState>,
}

impl StreamableHttpMcpTransport {
    pub fn new(
        http: Arc<dyn HttpTransport>,
        url: impl Into<String>,
        headers: Vec<(String, String)>,
    ) -> Self {
        Self {
            http,
            url: url.into(),
            headers,
            state: Mutex::new(StreamableHttpState::default()),
        }
    }

    pub fn session_id(&self) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .session_id
            .clone()
    }

    fn build_headers(&self) -> Vec<(String, String)> {
        let mut out = self.headers.clone();
        let has_header = |list: &[(String, String)], name: &str| {
            list.iter().any(|(k, _)| k.eq_ignore_ascii_case(name))
        };
        if !has_header(&out, "content-type") {
            out.push(("content-type".into(), "application/json".into()));
        }
        if !has_header(&out, "accept") {
            out.push((
                "accept".into(),
                "application/json, text/event-stream".into(),
            ));
        }
        let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(sid) = &guard.session_id
            && !has_header(&out, "mcp-session-id")
        {
            out.push(("mcp-session-id".into(), sid.clone()));
        }
        if let Some(ver) = &guard.protocol_version
            && !has_header(&out, "mcp-protocol-version")
        {
            out.push(("mcp-protocol-version".into(), ver.clone()));
        }
        out
    }

    fn clear_session_id(&self) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .session_id = None;
    }

    fn capture_response_headers(&self, headers: &[(String, String)]) {
        if let Some((_, sid)) = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("mcp-session-id"))
            && !sid.trim().is_empty()
        {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            guard.session_id = Some(sid.trim().to_string());
        }
    }

    async fn respond_to_inbound_sse_requests(&self, value: &Value, cancel: &CancelToken) {
        let mut replies = Vec::new();
        collect_inbound_request_replies(value, &mut replies);
        for reply in replies {
            if cancel.is_cancelled() {
                break;
            }
            let Ok(body_bytes) = serde_json::to_vec(&reply) else {
                continue;
            };
            let req = HttpRequest {
                method: "POST".into(),
                url: self.url.clone(),
                headers: self.build_headers(),
                body: Bytes::from(body_bytes),
                timeout_ms: None,
            };
            if let Ok(resp) = self.http.send(req, cancel.clone()).await {
                let _ = read_all_bytes(resp.body, cancel).await;
            }
        }
    }
}

fn collect_inbound_request_replies(value: &Value, out: &mut Vec<Value>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_inbound_request_replies(item, out);
            }
        }
        Value::Object(map) => {
            if let Some(method) = map.get("method").and_then(Value::as_str)
                && let Some(req_id) = map.get("id").filter(|v| !v.is_null())
            {
                let reply = if method == "ping" {
                    json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "result": {},
                    })
                } else {
                    json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "error": {
                            "code": -32601,
                            "message": format!("Method not found: {method}"),
                        },
                    })
                };
                out.push(reply);
            }
        }
        _ => {}
    }
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

async fn read_all_bytes(
    mut stream: jimu_ai::ByteStream,
    cancel: &CancelToken,
) -> Result<Vec<u8>, McpError> {
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        if cancel.is_cancelled() {
            return Err(McpError::Cancelled);
        }
        let bytes = chunk?;
        buf.extend_from_slice(&bytes);
    }
    Ok(buf)
}

fn extract_matching_response(
    value: Value,
    expected_id: u64,
) -> Result<Option<JsonRpcResponse>, McpError> {
    match value {
        Value::Array(items) => {
            for item in items {
                if let Ok(resp) = serde_json::from_value::<JsonRpcResponse>(item)
                    && resp
                        .id
                        .as_ref()
                        .is_some_and(|id| id.matches_u64(expected_id))
                    && (resp.result.is_some() || resp.error.is_some())
                {
                    return Ok(Some(resp));
                }
            }
            Ok(None)
        }
        Value::Object(_) => {
            let resp: JsonRpcResponse = serde_json::from_value(value)?;
            if resp
                .id
                .as_ref()
                .is_some_and(|id| id.matches_u64(expected_id))
                && (resp.result.is_some() || resp.error.is_some())
            {
                Ok(Some(resp))
            } else {
                Ok(None)
            }
        }
        _ => Err(McpError::Protocol(
            "JSON-RPC response payload must be a JSON object or array".into(),
        )),
    }
}

impl McpTransport for StreamableHttpMcpTransport {
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
        id: u64,
        cancel: CancelToken,
    ) -> BoxFuture<'a, Result<JsonRpcResponse, McpError>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(McpError::Cancelled);
            }

            if method == "initialize" {
                self.clear_session_id();
            }

            let mut body_obj = serde_json::Map::new();
            body_obj.insert("jsonrpc".into(), json!("2.0"));
            body_obj.insert("id".into(), json!(id));
            body_obj.insert("method".into(), json!(method));
            if let Some(p) = params {
                body_obj.insert("params".into(), p);
            }
            let body_bytes = Bytes::from(serde_json::to_vec(&Value::Object(body_obj))?);

            let headers = self.build_headers();
            let sent_session_id = headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("mcp-session-id"));

            let req = HttpRequest {
                method: "POST".into(),
                url: self.url.clone(),
                headers,
                body: body_bytes,
                timeout_ms: None,
            };

            let resp = self.http.send(req, cancel.clone()).await?;

            if resp.status >= 400 {
                if resp.status == 404 && sent_session_id {
                    self.clear_session_id();
                }
                let raw = read_all_bytes(resp.body, &cancel).await.unwrap_or_default();
                let body_text = String::from_utf8_lossy(&raw).into_owned();
                return Err(McpError::Transport(TransportError::HttpStatus {
                    status: resp.status,
                    headers: resp.headers,
                    body: body_text,
                }));
            }
            self.capture_response_headers(&resp.headers);

            let content_type = header_value(&resp.headers, "content-type")
                .and_then(|ct| ct.split(';').next())
                .map(|ct| ct.trim().to_ascii_lowercase())
                .unwrap_or_else(|| "application/json".into());

            if content_type == "text/event-stream" {
                let mut sse = SseStream::new(resp.body, cancel.clone());
                while let Some(ev_res) = sse.next().await {
                    let ev = ev_res?;
                    let data = ev.data.trim();
                    if data.is_empty() || ev.is_done() {
                        continue;
                    }
                    let parsed: Value = match serde_json::from_str(data) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    self.respond_to_inbound_sse_requests(&parsed, &cancel).await;
                    if let Some(matched) = extract_matching_response(parsed, id)? {
                        return Ok(matched);
                    }
                }
                Err(McpError::Protocol(format!(
                    "SSE stream ended without JSON-RPC response for id {id}"
                )))
            } else {
                let raw = read_all_bytes(resp.body, &cancel).await?;
                let parsed: Value = serde_json::from_slice(&raw)?;
                extract_matching_response(parsed, id)?.ok_or_else(|| {
                    McpError::Protocol(format!(
                        "JSON response did not contain matching JSON-RPC response for id {id}"
                    ))
                })
            }
        })
    }

    fn notify<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
        cancel: CancelToken,
    ) -> BoxFuture<'a, Result<(), McpError>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(McpError::Cancelled);
            }

            let mut body_obj = serde_json::Map::new();
            body_obj.insert("jsonrpc".into(), json!("2.0"));
            body_obj.insert("method".into(), json!(method));
            if let Some(p) = params {
                body_obj.insert("params".into(), p);
            }
            let body_bytes = Bytes::from(serde_json::to_vec(&Value::Object(body_obj))?);

            let headers = self.build_headers();
            let sent_session_id = headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("mcp-session-id"));

            let req = HttpRequest {
                method: "POST".into(),
                url: self.url.clone(),
                headers,
                body: body_bytes,
                timeout_ms: None,
            };

            let resp = self.http.send(req, cancel.clone()).await?;

            if resp.status >= 400 {
                if resp.status == 404 && sent_session_id {
                    self.clear_session_id();
                }
                let raw = read_all_bytes(resp.body, &cancel).await.unwrap_or_default();
                let body_text = String::from_utf8_lossy(&raw).into_owned();
                return Err(McpError::Transport(TransportError::HttpStatus {
                    status: resp.status,
                    headers: resp.headers,
                    body: body_text,
                }));
            }
            self.capture_response_headers(&resp.headers);
            Ok(())
        })
    }

    fn set_protocol_version(&self, version: String) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .protocol_version = Some(version);
    }

    fn reset_session(&self) {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard.session_id = None;
        guard.protocol_version = None;
    }

    fn close(&self) -> BoxFuture<'_, Result<(), McpError>> {
        Box::pin(async move {
            let sid = self.session_id();
            if sid.is_some() {
                let req = HttpRequest {
                    method: "DELETE".into(),
                    url: self.url.clone(),
                    headers: self.build_headers(),
                    body: Bytes::new(),
                    timeout_ms: None,
                };
                // Servers may respond with 405 Method Not Allowed on DELETE; ignore non-fatal errors.
                let _ = self.http.send(req, CancelToken::new()).await;
            }
            Ok(())
        })
    }
}

#[cfg(all(feature = "stdio", not(target_arch = "wasm32")))]
mod stdio {
    use super::{JsonRpcId, JsonRpcResponse, McpError, McpTransport};
    use futures_util::future::{self, Either};
    use jimu_ai::{BoxFuture, CancelToken};
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::process::Stdio;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::process::{Child, ChildStdin, Command};
    use tokio::sync::{Mutex as AsyncMutex, oneshot};
    use tokio::task::JoinHandle;

    /// Configuration for spawning a local stdio MCP server subprocess.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct StdioMcpConfig {
        pub command: String,
        pub args: Vec<String>,
        pub env: Vec<(String, String)>,
        pub cwd: Option<PathBuf>,
    }

    impl StdioMcpConfig {
        pub fn new(command: impl Into<String>) -> Self {
            Self {
                command: command.into(),
                args: Vec::new(),
                env: Vec::new(),
                cwd: None,
            }
        }
    }

    const MAX_STDIO_FRAME_BYTES: usize = 16 * 1024 * 1024;

    #[cfg(unix)]
    fn kill_process_group(pid: u32) {
        if pid == 0 || pid > i32::MAX as u32 {
            return;
        }
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        const SIGKILL: i32 = 9;
        // SAFETY: `pid` is the process group leader created via `Command::process_group(0)`
        // and we signal `-pid` BEFORE reaping `child`, so the OS cannot have recycled `pid`.
        unsafe {
            let _ = kill(-(pid as i32), SIGKILL);
        }
    }

    #[cfg(not(unix))]
    fn kill_process_group(_pid: u32) {}

    struct PendingState {
        closed: bool,
        closed_reason: String,
        waiters: HashMap<u64, oneshot::Sender<Result<JsonRpcResponse, McpError>>>,
    }

    impl PendingState {
        fn new() -> Self {
            Self {
                closed: false,
                closed_reason: String::new(),
                waiters: HashMap::new(),
            }
        }
    }

    fn close_and_fail_all_pending(pending: &Mutex<PendingState>, message: &str) {
        let waiters: Vec<_> = {
            let mut state = pending.lock().unwrap_or_else(|e| e.into_inner());
            state.closed = true;
            if state.closed_reason.is_empty() {
                state.closed_reason = message.to_string();
            }
            state.waiters.drain().map(|(_, tx)| tx).collect()
        };
        for tx in waiters {
            let _ = tx.send(Err(McpError::Protocol(message.to_string())));
        }
    }

    fn id_as_u64(id: &JsonRpcId) -> Option<u64> {
        match id {
            JsonRpcId::Number(n) => Some(*n),
            JsonRpcId::String(s) => s.parse::<u64>().ok(),
        }
    }

    async fn write_raw_frame(
        stdin: &AsyncMutex<Option<ChildStdin>>,
        frame: &[u8],
    ) -> Result<(), McpError> {
        let mut guard = stdin.lock().await;
        let pipe = guard
            .as_mut()
            .ok_or_else(|| McpError::Protocol("stdio MCP transport is closed".into()))?;
        pipe.write_all(frame)
            .await
            .map_err(|err| McpError::Protocol(format!("stdio MCP write error: {err}")))?;
        pipe.flush()
            .await
            .map_err(|err| McpError::Protocol(format!("stdio MCP flush error: {err}")))?;
        Ok(())
    }

    async fn dispatch_parsed_frame(
        value: Value,
        pending: &Mutex<PendingState>,
        stdin: &Arc<AsyncMutex<Option<ChildStdin>>>,
    ) {
        match value {
            Value::Array(items) => {
                for item in items {
                    Box::pin(dispatch_parsed_frame(item, pending, stdin)).await;
                }
            }
            Value::Object(ref map) => {
                // Handle inbound server-to-client requests (such as `ping`) that carry both `method` and `id`.
                if let Some(method) = map.get("method").and_then(Value::as_str) {
                    if let Some(req_id) = map.get("id").filter(|v| !v.is_null()) {
                        let reply = if method == "ping" {
                            json!({
                                "jsonrpc": "2.0",
                                "id": req_id,
                                "result": {},
                            })
                        } else {
                            json!({
                                "jsonrpc": "2.0",
                                "id": req_id,
                                "error": {
                                    "code": -32601,
                                    "message": format!("Method not found: {method}"),
                                },
                            })
                        };
                        if let Ok(mut frame) = serde_json::to_vec(&reply) {
                            frame.push(b'\n');
                            let stdin_clone = Arc::clone(stdin);
                            tokio::spawn(async move {
                                let _ = write_raw_frame(&stdin_clone, &frame).await;
                            });
                        }
                    }
                    return;
                }

                if let Ok(resp) = serde_json::from_value::<JsonRpcResponse>(value)
                    && (resp.result.is_some() || resp.error.is_some())
                    && let Some(id) = resp.id.as_ref().and_then(id_as_u64)
                {
                    let waiter = pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .waiters
                        .remove(&id);
                    if let Some(tx) = waiter {
                        let _ = tx.send(Ok(resp));
                    }
                }
            }
            _ => {}
        }
    }

    enum ReadFrameOutcome {
        Line(Vec<u8>),
        Eof,
        TooLarge,
        IoError(std::io::Error),
    }

    async fn read_bounded_line<R: AsyncBufReadExt + Unpin>(
        reader: &mut R,
        buf: &mut Vec<u8>,
        max_bytes: usize,
    ) -> ReadFrameOutcome {
        buf.clear();
        let mut over_limit = false;
        loop {
            let available = match reader.fill_buf().await {
                Ok([]) => {
                    if buf.is_empty() && !over_limit {
                        return ReadFrameOutcome::Eof;
                    }
                    return if over_limit {
                        ReadFrameOutcome::TooLarge
                    } else {
                        ReadFrameOutcome::Line(std::mem::take(buf))
                    };
                }
                Ok(slice) => slice,
                Err(err) => return ReadFrameOutcome::IoError(err),
            };

            if let Some(nl_pos) = available.iter().position(|&b| b == b'\n') {
                let take_len = nl_pos + 1;
                if !over_limit && buf.len().saturating_add(take_len) <= max_bytes {
                    buf.extend_from_slice(&available[..take_len]);
                    reader.consume(take_len);
                    return ReadFrameOutcome::Line(std::mem::take(buf));
                }
                reader.consume(take_len);
                return ReadFrameOutcome::TooLarge;
            }

            let chunk_len = available.len();
            if !over_limit {
                if buf.len().saturating_add(chunk_len) <= max_bytes {
                    buf.extend_from_slice(available);
                } else {
                    buf.clear();
                    over_limit = true;
                }
            }
            reader.consume(chunk_len);
        }
    }

    /// Newline-delimited JSON-RPC 2.0 MCP transport over a child process's `stdin`/`stdout`.
    pub struct StdioMcpTransport {
        stdin: Arc<AsyncMutex<Option<ChildStdin>>>,
        child: Mutex<Option<Child>>,
        child_pid: AtomicU32,
        pending: Arc<Mutex<PendingState>>,
        reader_task: Mutex<Option<JoinHandle<()>>>,
        closed: AtomicBool,
    }

    impl StdioMcpTransport {
        /// Spawn the configured MCP subprocess and start its stdout JSON-RPC frame reader.
        pub fn spawn(config: StdioMcpConfig) -> Result<Self, McpError> {
            let handle = tokio::runtime::Handle::try_current().map_err(|_| {
                McpError::Protocol(
                    "StdioMcpTransport::spawn requires an active Tokio runtime".into(),
                )
            })?;

            let mut cmd = Command::new(&config.command);
            cmd.args(&config.args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            for (k, v) in &config.env {
                cmd.env(k, v);
            }
            if let Some(cwd) = &config.cwd {
                cmd.current_dir(cwd);
            }
            #[cfg(unix)]
            cmd.process_group(0);

            let mut child = cmd.spawn().map_err(|err| {
                McpError::Protocol(format!(
                    "failed to spawn stdio MCP server `{}`: {err}",
                    config.command
                ))
            })?;

            let pid = child.id().unwrap_or(0);
            let stdin = child.stdin.take().ok_or_else(|| {
                McpError::Protocol("failed to capture stdio MCP child stdin".into())
            })?;
            let stdout = child.stdout.take().ok_or_else(|| {
                McpError::Protocol("failed to capture stdio MCP child stdout".into())
            })?;

            let stdin_arc = Arc::new(AsyncMutex::new(Some(stdin)));
            let pending = Arc::new(Mutex::new(PendingState::new()));
            let reader_pending = Arc::clone(&pending);
            let reader_stdin = Arc::clone(&stdin_arc);
            let reader_task = handle.spawn(async move {
                let mut reader = BufReader::new(stdout);
                let mut raw_buf = Vec::new();
                loop {
                    match read_bounded_line(&mut reader, &mut raw_buf, MAX_STDIO_FRAME_BYTES).await
                    {
                        ReadFrameOutcome::Eof => {
                            close_and_fail_all_pending(
                                &reader_pending,
                                "stdio MCP server closed stdout before responding",
                            );
                            break;
                        }
                        ReadFrameOutcome::TooLarge => {
                            close_and_fail_all_pending(
                                &reader_pending,
                                "stdio MCP stdout frame exceeded 16 MiB limit",
                            );
                            break;
                        }
                        ReadFrameOutcome::Line(bytes) => {
                            let Ok(text) = std::str::from_utf8(&bytes) else {
                                continue;
                            };
                            let trimmed = text.trim();
                            if trimmed.is_empty() {
                                continue;
                            }
                            if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
                                dispatch_parsed_frame(val, &reader_pending, &reader_stdin).await;
                            }
                        }
                        ReadFrameOutcome::IoError(err) => {
                            close_and_fail_all_pending(
                                &reader_pending,
                                &format!("stdio MCP stdout read error: {err}"),
                            );
                            break;
                        }
                    }
                }
            });

            Ok(Self {
                stdin: stdin_arc,
                child: Mutex::new(Some(child)),
                child_pid: AtomicU32::new(pid),
                pending,
                reader_task: Mutex::new(Some(reader_task)),
                closed: AtomicBool::new(false),
            })
        }

        fn terminate_process_group_sync(&self) -> Option<Child> {
            self.closed.store(true, Ordering::SeqCst);
            if let Some(handle) = self
                .reader_task
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                handle.abort();
            }
            close_and_fail_all_pending(&self.pending, "stdio MCP transport closed");

            let pid = self.child_pid.swap(0, Ordering::SeqCst);
            let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner()).take();
            if pid > 0 {
                // Signal the entire process group BEFORE reaping `child` so worker descendants
                // are terminated even if the group leader already exited on its own.
                kill_process_group(pid);
            }
            if let Some(ref mut c) = child {
                let _ = c.start_kill();
            }
            child
        }

        /// Close stdin, terminate the child process group cleanly, and fail any pending waiters.
        pub async fn close(&self) {
            // Terminate child process group BEFORE acquiring `self.stdin` so any in-flight
            // `write_frame` blocked on a full pipe unblocks with EPIPE immediately.
            let child = self.terminate_process_group_sync();
            {
                let mut stdin_guard = self.stdin.lock().await;
                let _ = stdin_guard.take();
            }
            if let Some(mut child) = child {
                let _ = child.wait().await;
            }
        }

        async fn write_frame(&self, frame: &[u8]) -> Result<(), McpError> {
            write_raw_frame(&self.stdin, frame).await
        }
    }

    impl Drop for StdioMcpTransport {
        fn drop(&mut self) {
            let _ = self.terminate_process_group_sync();
        }
    }

    impl McpTransport for StdioMcpTransport {
        fn request<'a>(
            &'a self,
            method: &'a str,
            params: Option<Value>,
            id: u64,
            cancel: CancelToken,
        ) -> BoxFuture<'a, Result<JsonRpcResponse, McpError>> {
            Box::pin(async move {
                if cancel.is_cancelled() {
                    return Err(McpError::Cancelled);
                }
                if self.closed.load(Ordering::SeqCst) {
                    return Err(McpError::Protocol("stdio MCP transport is closed".into()));
                }

                let mut body_obj = serde_json::Map::new();
                body_obj.insert("jsonrpc".into(), json!("2.0"));
                body_obj.insert("id".into(), json!(id));
                body_obj.insert("method".into(), json!(method));
                if let Some(p) = params {
                    body_obj.insert("params".into(), p);
                }
                let mut frame = serde_json::to_vec(&Value::Object(body_obj))?;
                frame.push(b'\n');

                let (tx, rx) = oneshot::channel();
                {
                    let mut state = self.pending.lock().unwrap_or_else(|e| e.into_inner());
                    if state.closed || self.closed.load(Ordering::SeqCst) {
                        let msg = if state.closed_reason.is_empty() {
                            "stdio MCP transport is closed".to_string()
                        } else {
                            state.closed_reason.clone()
                        };
                        return Err(McpError::Protocol(msg));
                    }
                    state.waiters.insert(id, tx);
                }
                struct PendingWaiterGuard<'g> {
                    pending: &'g Mutex<PendingState>,
                    id: u64,
                    armed: bool,
                }
                impl Drop for PendingWaiterGuard<'_> {
                    fn drop(&mut self) {
                        if self.armed {
                            self.pending
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .waiters
                                .remove(&self.id);
                        }
                    }
                }
                let mut waiter_guard = PendingWaiterGuard {
                    pending: &self.pending,
                    id,
                    armed: true,
                };

                let write_res = {
                    let write_fut = self.write_frame(&frame);
                    let cancel_write = cancel.cancelled();
                    futures_util::pin_mut!(write_fut, cancel_write);
                    match future::select(write_fut, cancel_write).await {
                        Either::Left((res, _)) => res,
                        Either::Right(((), _)) => {
                            waiter_guard.armed = false;
                            self.pending
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .waiters
                                .remove(&id);
                            // Cancelling mid-write can leave a partial JSON frame on the pipe; tear down cleanly.
                            self.close().await;
                            return Err(McpError::Cancelled);
                        }
                    }
                };

                if let Err(err) = write_res {
                    waiter_guard.armed = false;
                    self.pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .waiters
                        .remove(&id);
                    return Err(err);
                }

                let cancel_fut = cancel.cancelled();
                futures_util::pin_mut!(rx, cancel_fut);
                let out = match future::select(rx, cancel_fut).await {
                    Either::Left((Ok(res), _)) => res,
                    Either::Left((Err(_), _)) => Err(McpError::Protocol(
                        "stdio MCP response channel closed".into(),
                    )),
                    Either::Right(((), _)) => {
                        let removed = self
                            .pending
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .waiters
                            .remove(&id)
                            .is_some();
                        if removed
                            && method != "initialize"
                            && !self.closed.load(Ordering::SeqCst)
                            && let Ok(mut cancel_frame) = serde_json::to_vec(&json!({
                                "jsonrpc": "2.0",
                                "method": "notifications/cancelled",
                                "params": {
                                    "requestId": id,
                                    "reason": "Request cancelled",
                                },
                            }))
                        {
                            cancel_frame.push(b'\n');
                            let _ = self.write_frame(&cancel_frame).await;
                        }
                        Err(McpError::Cancelled)
                    }
                };
                waiter_guard.armed = false;
                out
            })
        }

        fn notify<'a>(
            &'a self,
            method: &'a str,
            params: Option<Value>,
            cancel: CancelToken,
        ) -> BoxFuture<'a, Result<(), McpError>> {
            Box::pin(async move {
                if cancel.is_cancelled() {
                    return Err(McpError::Cancelled);
                }
                if self.closed.load(Ordering::SeqCst) {
                    return Err(McpError::Protocol("stdio MCP transport is closed".into()));
                }

                let mut body_obj = serde_json::Map::new();
                body_obj.insert("jsonrpc".into(), json!("2.0"));
                body_obj.insert("method".into(), json!(method));
                if let Some(p) = params {
                    body_obj.insert("params".into(), p);
                }
                let mut frame = serde_json::to_vec(&Value::Object(body_obj))?;
                frame.push(b'\n');
                self.write_frame(&frame).await
            })
        }

        fn close(&self) -> BoxFuture<'_, Result<(), McpError>> {
            Box::pin(async move {
                StdioMcpTransport::close(self).await;
                Ok(())
            })
        }
    }
}

#[cfg(all(feature = "stdio", not(target_arch = "wasm32")))]
pub use stdio::{StdioMcpConfig, StdioMcpTransport};

/// JSON-RPC 2.0 MCP client backed by any [`McpTransport`].
pub struct McpClient {
    transport: Arc<dyn McpTransport>,
    next_id: AtomicU64,
    initialized: Mutex<Option<McpInitializeResult>>,
}

impl McpClient {
    pub fn new(transport: Arc<dyn McpTransport>) -> Self {
        Self {
            transport,
            next_id: AtomicU64::new(1),
            initialized: Mutex::new(None),
        }
    }

    /// Convenience constructor backed by [`StreamableHttpMcpTransport`].
    pub fn with_http(
        http: Arc<dyn HttpTransport>,
        url: impl Into<String>,
        headers: Vec<(String, String)>,
    ) -> Self {
        Self::new(Arc::new(StreamableHttpMcpTransport::new(
            http, url, headers,
        )))
    }

    pub fn initialize_result(&self) -> Option<McpInitializeResult> {
        self.initialized
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    async fn rpc_call(
        &self,
        method: &str,
        params: Option<Value>,
        cancel: CancelToken,
    ) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let resp = self.transport.request(method, params, id, cancel).await?;
        if let Some(err) = resp.error {
            return Err(McpError::JsonRpc {
                code: err.code,
                message: err.message,
                data: err.data,
            });
        }
        resp.result.ok_or_else(|| {
            McpError::Protocol(format!(
                "JSON-RPC response for `{method}` contained neither `result` nor `error`"
            ))
        })
    }

    /// Run MCP handshake (`initialize` followed by `notifications/initialized`).
    pub async fn initialize(
        &self,
        client_name: &str,
        client_version: &str,
        cancel: CancelToken,
    ) -> Result<McpInitializeResult, McpError> {
        self.transport.reset_session();
        let params = json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": client_name,
                "version": client_version,
            }
        });

        let raw = self
            .rpc_call("initialize", Some(params), cancel.clone())
            .await?;
        let init: McpInitializeResult = serde_json::from_value(raw)?;
        self.transport
            .set_protocol_version(init.protocol_version.clone());
        *self.initialized.lock().unwrap_or_else(|e| e.into_inner()) = Some(init.clone());

        self.transport
            .notify("notifications/initialized", None, cancel)
            .await?;

        Ok(init)
    }

    /// Discover all tools via paginated `tools/list`.
    pub async fn list_tools(&self, cancel: CancelToken) -> Result<Vec<McpToolInfo>, McpError> {
        let mut all_tools = Vec::new();
        let mut cursor: Option<String> = None;

        for _ in 0..MAX_LIST_PAGES {
            let params = cursor.as_ref().map(|c| json!({ "cursor": c }));
            let raw = self.rpc_call("tools/list", params, cancel.clone()).await?;
            let tools_val = raw.get("tools").cloned().ok_or_else(|| {
                McpError::Protocol("MCP `tools/list` response missing `tools` array".into())
            })?;
            let page: Vec<McpToolInfo> = serde_json::from_value(tools_val)?;
            all_tools.extend(page);

            let next_cursor = raw
                .get("nextCursor")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string);
            match next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }

        Ok(all_tools)
    }

    /// Invoke an MCP tool via `tools/call`.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        cancel: CancelToken,
    ) -> Result<McpToolCallResult, McpError> {
        let params = json!({
            "name": name,
            "arguments": arguments,
        });
        let raw = self.rpc_call("tools/call", Some(params), cancel).await?;
        let is_error = raw.get("isError").and_then(Value::as_bool).unwrap_or(false);
        let structured_content = raw.get("structuredContent").cloned();

        let mut content = Vec::new();
        if let Some(items) = raw.get("content").and_then(Value::as_array) {
            for item in items {
                let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
                match kind {
                    "text" => {
                        if let Some(text) = item.get("text").and_then(Value::as_str) {
                            content.push(ContentPart::Text(TextContent::plain(text)));
                        }
                    }
                    "image" => {
                        content.push(checked_image(
                            item.get("data").and_then(Value::as_str).unwrap_or_default(),
                            item.get("mimeType")
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                        ));
                    }
                    "resource" => {
                        if let Some(text) = item
                            .get("resource")
                            .and_then(|r| r.get("text"))
                            .and_then(Value::as_str)
                        {
                            content.push(ContentPart::Text(TextContent::plain(text)));
                        }
                    }
                    _ => {
                        content.push(ContentPart::Text(TextContent::plain(item.to_string())));
                    }
                }
            }
        }

        if content.is_empty()
            && let Some(sc) = &structured_content
        {
            content.push(ContentPart::Text(TextContent::plain(
                serde_json::to_string(sc).unwrap_or_else(|_| sc.to_string()),
            )));
        }

        Ok(McpToolCallResult {
            content,
            structured_content,
            is_error,
        })
    }

    /// Wrap discovered MCP tools as [`AgentTool`]s ready for registration in [`crate::KitSession`].
    pub fn to_agent_tools(
        self: &Arc<Self>,
        tools: &[McpToolInfo],
        name_prefix: Option<&str>,
    ) -> Vec<Arc<dyn AgentTool>> {
        tools
            .iter()
            .map(|info| {
                let exposed_name = match name_prefix.filter(|p| !p.is_empty()) {
                    Some(prefix) => format!("{prefix}_{}", info.name),
                    None => info.name.clone(),
                };
                Arc::new(McpAgentTool {
                    client: Arc::clone(self),
                    remote_name: info.name.clone(),
                    definition: ToolDefinition {
                        name: exposed_name,
                        description: info.description.clone().unwrap_or_default(),
                        parameters: info.input_schema.clone(),
                    },
                }) as Arc<dyn AgentTool>
            })
            .collect()
    }

    pub async fn close(&self) -> Result<(), McpError> {
        self.transport.close().await
    }
}

struct McpAgentTool {
    client: Arc<McpClient>,
    remote_name: String,
    definition: ToolDefinition,
}

impl AgentTool for McpAgentTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    fn execute(
        &self,
        _tool_call_id: String,
        args: Value,
        cancel: CancelToken,
        _on_update: ToolProgressCallback,
    ) -> BoxFuture<'_, Result<AgentToolResult, String>> {
        Box::pin(async move {
            let res = self
                .client
                .call_tool(&self.remote_name, args, cancel)
                .await
                .map_err(|err| err.to_string())?;
            Ok(AgentToolResult {
                content: res.content,
                details: Value::Null,
                structured_content: res.structured_content,
                is_error: res.is_error,
                terminate: false,
            })
        })
    }
}

/// MCP and codemode supply already-encoded images. Check the envelope before adding it to history.
pub(crate) fn checked_image(data: &str, mime: &str) -> ContentPart {
    let omitted =
        |reason| ContentPart::Text(TextContent::plain(format!("[Image omitted: {reason}]")));
    if data.len() > 5 * 1024 * 1024 {
        return omitted("larger than 5 MiB");
    }
    let Some(bytes) = decode_image_base64(data) else {
        return omitted("invalid base64");
    };
    let complete = match mime {
        "image/png" => complete_png(&bytes),
        "image/jpeg" => bytes.starts_with(&[0xff, 0xd8, 0xff]) && jpeg_has_eoi(&bytes),
        "image/webp" => {
            bytes.starts_with(b"RIFF")
                && bytes.get(8..12) == Some(b"WEBP")
                && matches!(bytes.get(12..16), Some(b"VP8 " | b"VP8L" | b"VP8X"))
                && (12..=bytes.len() as u64 - 8).contains(&u64::from(u32::from_le_bytes(
                    bytes[4..8].try_into().unwrap(),
                )))
        }
        "image/gif" => {
            bytes.len() >= 14
                && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"))
                && bytes.last() == Some(&0x3b)
        }
        _ => return omitted("unsupported image type"),
    };
    if !complete {
        return omitted("incomplete image or incorrect image type");
    }
    ContentPart::Image(ImageContent {
        data: data.to_owned(),
        mime_type: mime.to_owned(),
    })
}

// Standard RFC 4648 alphabet and canonical padding. No codecs are needed in the kit.
fn decode_image_base64(data: &str) -> Option<Vec<u8>> {
    if !data.len().is_multiple_of(4) {
        return None;
    }
    let text = data.trim_end_matches('=');
    let padding = data.len() - text.len();
    if padding > 2 {
        return None;
    }
    let mut bytes = Vec::with_capacity(data.len() / 4 * 3);
    let mut bits = 0;
    let mut acc = 0u16;
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u16::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push((acc >> bits) as u8);
        }
    }
    (acc & ((1 << bits) - 1) == 0).then_some(bytes)
}

fn complete_png(bytes: &[u8]) -> bool {
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR") {
        return false;
    }
    let mut chunks = &bytes[8..];
    let mut has_data = false;
    while let [a, b, c, d, kind @ ..] = chunks {
        let size = u32::from_be_bytes([*a, *b, *c, *d]) as usize;
        if kind.starts_with(b"IEND") {
            return has_data && size == 0 && kind.starts_with(b"IEND\xaeB\x60\x82");
        }
        has_data |= kind.starts_with(b"IDAT");
        let Some(rest) = size.checked_add(12).and_then(|end| chunks.get(end..)) else {
            break;
        };
        chunks = rest;
    }
    false
}

// Skip length-prefixed segments so EOI inside an embedded preview cannot end the main image.
fn jpeg_has_eoi(mut bytes: &[u8]) -> bool {
    let mut saw_scan = false;
    bytes = &bytes[2..];
    while let Some(marker) = bytes.iter().position(|&b| b == 0xff) {
        bytes = &bytes[marker..];
        let Some(code) = bytes.iter().position(|&b| b != 0xff) else {
            break;
        };
        let marker = bytes[code];
        bytes = &bytes[code + 1..];
        match marker {
            0xd9 => return saw_scan,
            0 | 1 | 0xd0..=0xd7 => continue, // stuffed byte, TEM, or restart
            _ => {
                let [hi, lo, ..] = *bytes else { break };
                let len = u16::from_be_bytes([hi, lo]) as usize;
                let Some(rest) = bytes.get(len..).filter(|_| len >= 2) else {
                    break;
                };
                saw_scan |= marker == 0xda;
                bytes = rest;
            }
        }
    }
    false
}
