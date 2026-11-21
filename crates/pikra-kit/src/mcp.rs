use bytes::Bytes;
use futures_util::StreamExt;
use pikra_agent::{AgentTool, AgentToolResult, ToolProgressCallback};
use pikra_ai::{
    BoxFuture, CancelToken, ContentPart, HttpRequest, HttpTransport, ImageContent, SseStream,
    TextContent, ToolDefinition, TransportError,
};
use pikra_schema::Schema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
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
    /// Register into the top-level resident `tools` array (only recommended before the first turn).
    Resident,
    /// Register into the deferred tool gateway (`tool_search` / `call_tool`), preserving prompt cache bytes.
    #[default]
    Deferred,
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

/// Streamable HTTP MCP transport over [`pikra_ai::HttpTransport`].
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
            out.push(("accept".into(), "application/json, text/event-stream".into()));
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
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

async fn read_all_bytes(
    mut stream: pikra_ai::ByteStream,
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

fn extract_matching_response(value: Value, expected_id: u64) -> Result<Option<JsonRpcResponse>, McpError> {
    match value {
        Value::Array(items) => {
            for item in items {
                if let Ok(resp) = serde_json::from_value::<JsonRpcResponse>(item)
                    && resp.id.as_ref().is_some_and(|id| id.matches_u64(expected_id))
                    && (resp.result.is_some() || resp.error.is_some())
                {
                    return Ok(Some(resp));
                }
            }
            Ok(None)
        }
        Value::Object(_) => {
            let resp: JsonRpcResponse = serde_json::from_value(value)?;
            if resp.id.as_ref().is_some_and(|id| id.matches_u64(expected_id))
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

            let mut body_obj = serde_json::Map::new();
            body_obj.insert("jsonrpc".into(), json!("2.0"));
            body_obj.insert("id".into(), json!(id));
            body_obj.insert("method".into(), json!(method));
            if let Some(p) = params {
                body_obj.insert("params".into(), p);
            }
            let body_bytes = Bytes::from(serde_json::to_vec(&Value::Object(body_obj))?);

            let req = HttpRequest {
                method: "POST".into(),
                url: self.url.clone(),
                headers: self.build_headers(),
                body: body_bytes,
                timeout_ms: None,
            };

            let resp = self.http.send(req, cancel.clone()).await?;
            self.capture_response_headers(&resp.headers);

            if resp.status >= 400 {
                let raw = read_all_bytes(resp.body, &cancel).await.unwrap_or_default();
                let body_text = String::from_utf8_lossy(&raw).into_owned();
                return Err(McpError::Transport(TransportError::HttpStatus {
                    status: resp.status,
                    headers: resp.headers,
                    body: body_text,
                }));
            }

            let content_type = header_value(&resp.headers, "content-type")
                .and_then(|ct| ct.split(';').next())
                .map(|ct| ct.trim().to_ascii_lowercase())
                .unwrap_or_else(|| "application/json".into());

            if content_type == "text/event-stream" {
                let mut sse = SseStream::new(resp.body, cancel);
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

            let req = HttpRequest {
                method: "POST".into(),
                url: self.url.clone(),
                headers: self.build_headers(),
                body: body_bytes,
                timeout_ms: None,
            };

            let resp = self.http.send(req, cancel.clone()).await?;
            self.capture_response_headers(&resp.headers);

            if resp.status >= 400 {
                let raw = read_all_bytes(resp.body, &cancel).await.unwrap_or_default();
                let body_text = String::from_utf8_lossy(&raw).into_owned();
                return Err(McpError::Transport(TransportError::HttpStatus {
                    status: resp.status,
                    headers: resp.headers,
                    body: body_text,
                }));
            }
            Ok(())
        })
    }

    fn set_protocol_version(&self, version: String) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .protocol_version = Some(version);
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
    use pikra_ai::{BoxFuture, CancelToken};
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
        // and we only signal it while the child has not yet been reaped.
        unsafe {
            let _ = kill(-(pid as i32), SIGKILL);
        }
    }

    #[cfg(not(unix))]
    fn kill_process_group(_pid: u32) {}

    type PendingMap = HashMap<u64, oneshot::Sender<Result<JsonRpcResponse, McpError>>>;

    fn fail_all_pending(pending: &Mutex<PendingMap>, message: &str) {
        let waiters: Vec<_> = {
            let mut map = pending.lock().unwrap_or_else(|e| e.into_inner());
            map.drain().map(|(_, tx)| tx).collect()
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

    fn dispatch_parsed_frame(value: Value, pending: &Mutex<PendingMap>) {
        match value {
            Value::Array(items) => {
                for item in items {
                    dispatch_parsed_frame(item, pending);
                }
            }
            Value::Object(_) => {
                if let Ok(resp) = serde_json::from_value::<JsonRpcResponse>(value)
                    && (resp.result.is_some() || resp.error.is_some())
                    && let Some(id) = resp.id.as_ref().and_then(id_as_u64)
                {
                    let waiter = pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&id);
                    if let Some(tx) = waiter {
                        let _ = tx.send(Ok(resp));
                    }
                }
            }
            _ => {}
        }
    }

    /// Newline-delimited JSON-RPC 2.0 MCP transport over a child process's `stdin`/`stdout`.
    pub struct StdioMcpTransport {
        stdin: AsyncMutex<Option<ChildStdin>>,
        child: Mutex<Option<Child>>,
        child_pid: AtomicU32,
        pending: Arc<Mutex<PendingMap>>,
        reader_task: Mutex<Option<JoinHandle<()>>>,
        closed: AtomicBool,
    }

    impl StdioMcpTransport {
        /// Spawn the configured MCP subprocess and start its stdout JSON-RPC frame reader.
        pub fn spawn(config: StdioMcpConfig) -> Result<Self, McpError> {
            let handle = tokio::runtime::Handle::try_current().map_err(|_| {
                McpError::Protocol("StdioMcpTransport::spawn requires an active Tokio runtime".into())
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

            let pending: Arc<Mutex<PendingMap>> = Arc::new(Mutex::new(HashMap::new()));
            let reader_pending = Arc::clone(&pending);
            let reader_task = handle.spawn(async move {
                let mut reader = BufReader::new(stdout);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) => {
                            fail_all_pending(
                                &reader_pending,
                                "stdio MCP server closed stdout before responding",
                            );
                            break;
                        }
                        Ok(_) => {
                            let trimmed = line.trim();
                            if trimmed.is_empty() {
                                continue;
                            }
                            if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
                                dispatch_parsed_frame(val, &reader_pending);
                            }
                        }
                        Err(err) => {
                            fail_all_pending(
                                &reader_pending,
                                &format!("stdio MCP stdout read error: {err}"),
                            );
                            break;
                        }
                    }
                }
            });

            Ok(Self {
                stdin: AsyncMutex::new(Some(stdin)),
                child: Mutex::new(Some(child)),
                child_pid: AtomicU32::new(pid),
                pending,
                reader_task: Mutex::new(Some(reader_task)),
                closed: AtomicBool::new(false),
            })
        }

        /// Close stdin, terminate the child process group cleanly, and fail any pending waiters.
        pub async fn close(&self) {
            self.closed.store(true, Ordering::SeqCst);
            {
                let mut stdin_guard = self.stdin.lock().await;
                let _ = stdin_guard.take();
            }
            if let Some(handle) = self
                .reader_task
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                handle.abort();
            }
            fail_all_pending(&self.pending, "stdio MCP transport closed");

            let pid = self.child_pid.swap(0, Ordering::SeqCst);
            let child = self.child.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(mut child) = child {
                let still_running = matches!(child.try_wait(), Ok(None));
                if still_running {
                    kill_process_group(pid);
                    let _ = child.kill().await;
                }
            }
        }

        async fn write_frame(&self, frame: &[u8]) -> Result<(), McpError> {
            let mut guard = self.stdin.lock().await;
            let stdin = guard.as_mut().ok_or_else(|| {
                McpError::Protocol("stdio MCP transport is closed".into())
            })?;
            stdin
                .write_all(frame)
                .await
                .map_err(|err| McpError::Protocol(format!("stdio MCP write error: {err}")))?;
            stdin
                .flush()
                .await
                .map_err(|err| McpError::Protocol(format!("stdio MCP flush error: {err}")))?;
            Ok(())
        }
    }

    impl Drop for StdioMcpTransport {
        fn drop(&mut self) {
            self.closed.store(true, Ordering::SeqCst);
            if let Some(handle) = self
                .reader_task
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                handle.abort();
            }
            let pid = self.child_pid.swap(0, Ordering::SeqCst);
            if let Some(mut child) = self.child.lock().unwrap_or_else(|e| e.into_inner()).take()
                && matches!(child.try_wait(), Ok(None))
            {
                kill_process_group(pid);
                let _ = child.start_kill();
            }
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
                self.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id, tx);

                if let Err(err) = self.write_frame(&frame).await {
                    self.pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&id);
                    return Err(err);
                }

                let cancel_fut = cancel.cancelled();
                futures_util::pin_mut!(rx, cancel_fut);
                match future::select(rx, cancel_fut).await {
                    Either::Left((Ok(res), _)) => res,
                    Either::Left((Err(_), _)) => Err(McpError::Protocol(
                        "stdio MCP response channel closed".into(),
                    )),
                    Either::Right(((), _)) => {
                        self.pending
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&id);
                        Err(McpError::Cancelled)
                    }
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
        Self::new(Arc::new(StreamableHttpMcpTransport::new(http, url, headers)))
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
        let params = json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": client_name,
                "version": client_version,
            }
        });

        let raw = self.rpc_call("initialize", Some(params), cancel.clone()).await?;
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
        let is_error = raw
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
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
                        if let (Some(data), Some(mime)) = (
                            item.get("data").and_then(Value::as_str),
                            item.get("mimeType").and_then(Value::as_str),
                        ) {
                            content.push(ContentPart::Image(ImageContent {
                                data: data.to_string(),
                                mime_type: mime.to_string(),
                            }));
                        }
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
