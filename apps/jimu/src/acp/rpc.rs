use crate::acp::{AcpConfig, AcpServer, auth::Auth, schema::v1 as acp, server::Connection};
use jimu_ai::CancelToken;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{mpsc, oneshot},
    task::JoinSet,
};
use uuid::Uuid;

pub(crate) const CLIENT_TIMEOUT: i32 = -32001;

pub(crate) type Result<T> = std::result::Result<T, acp::Error>;
pub(crate) fn invalid(message: &str) -> acp::Error {
    acp::Error::new(-32602, message)
}
pub(crate) fn failed(message: &str) -> acp::Error {
    acp::Error::new(-32603, message)
}
pub(crate) fn params<T: DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|_| acp::Error::invalid_params())
}
pub(crate) fn encode(value: impl Serialize) -> Result<Value> {
    serde_json::to_value(value).map_err(|_| acp::Error::internal_error())
}

type Pending = HashMap<Value, oneshot::Sender<Result<Value>>>;
pub(crate) struct Peer {
    writer: mpsc::UnboundedSender<Value>,
    auth: Arc<Auth>,
    pending: Mutex<Pending>,
    pub(crate) closed: CancelToken,
}

impl Peer {
    fn send(&self, value: Value) {
        let _ = self.writer.send(value);
    }

    pub(crate) fn notify(&self, method: &str, payload: impl Serialize) {
        let Ok(mut payload) = encode(payload) else {
            return;
        };
        self.auth.redact(&mut payload);
        self.send(json!({"jsonrpc":"2.0", "method":method, "params":payload}));
    }

    pub(crate) async fn request<T: DeserializeOwned>(
        &self,
        method: &str,
        payload: impl Serialize,
        cancel: &CancelToken,
    ) -> Result<T> {
        self.request_with_timeout(method, payload, cancel, Duration::from_secs(60))
            .await
    }

    pub(crate) async fn request_with_timeout<T: DeserializeOwned>(
        &self,
        method: &str,
        payload: impl Serialize,
        cancel: &CancelToken,
        timeout: Duration,
    ) -> Result<T> {
        if self.closed.is_cancelled() || cancel.is_cancelled() {
            return Err(acp::Error::request_cancelled());
        }
        let mut payload = encode(payload)?;
        if method == "session/request_permission" {
            self.auth.redact(&mut payload);
        }
        let id = json!(format!("jimu-{}", Uuid::new_v4()));
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone(), tx);
        let _pending = PendingRequest {
            peer: self,
            id: id.clone(),
        };
        self.send(json!({"jsonrpc":"2.0", "id":id, "method":method, "params":payload}));
        let response = tokio::select! {
            biased;
            () = self.closed.cancelled() => Err(acp::Error::request_cancelled()),
            () = cancel.cancelled() => Err(acp::Error::request_cancelled()),
            response = rx => response.map_err(|_| acp::Error::request_cancelled())?,
            () = tokio::time::sleep(timeout) => Err(acp::Error::new(CLIENT_TIMEOUT, "The client did not respond in time.")),
        }?;
        params(response)
    }

    pub(crate) fn send_cleanup(&self, method: &str, payload: impl Serialize) {
        if let Ok(payload) = encode(payload) {
            let id = format!("jimu-{}", Uuid::new_v4());
            self.send(json!({"jsonrpc":"2.0", "id":id, "method":method, "params":payload}));
        }
    }

    fn response(&self, value: &Value) {
        let Some(id) = value.get("id") else { return };
        if let Some(tx) = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id)
        {
            let result = match (value.get("result"), value.get("error")) {
                (Some(result), None) => Ok(result.clone()),
                (None, Some(error)) => {
                    let code = error
                        .get("code")
                        .and_then(Value::as_i64)
                        .and_then(|n| i32::try_from(n).ok());
                    Err(acp::Error::new(
                        code.unwrap_or(-32603),
                        "The client could not complete the request.",
                    ))
                }
                _ => Err(acp::Error::invalid_request()),
            };
            let _ = tx.send(result);
        }
    }
}

struct PendingRequest<'a> {
    peer: &'a Peer,
    id: Value,
}
impl Drop for PendingRequest<'_> {
    fn drop(&mut self) {
        if self
            .peer
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id)
            .is_some()
            && !self.peer.closed.is_cancelled()
        {
            self.peer
                .notify("$/cancel_request", json!({"requestId": self.id}));
        }
    }
}

struct Inbound {
    cancel: CancelToken,
    session_cancel: CancelToken,
    prompt_session: Option<String>,
}

type Requests = Arc<Mutex<HashMap<Value, Arc<Inbound>>>>;

fn result_message(id: Value, result: Result<Value>) -> Value {
    match result {
        Ok(result) => json!({"jsonrpc":"2.0", "id":id, "result":result}),
        Err(error) => json!({"jsonrpc":"2.0", "id":id, "error":error}),
    }
}

enum Job {
    Reply(Value),
    Request {
        id: Value,
        method: String,
        params: Value,
        request: Arc<Inbound>,
        steering: Option<crate::acp::session::SteeringOrder>,
    },
}

fn classify(conn: &Connection, requests: &Requests, value: Value) -> Option<Job> {
    let error = || {
        Some(Job::Reply(result_message(
            Value::Null,
            Err(acp::Error::invalid_request()),
        )))
    };
    if value.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return error();
    }
    let id = value.get("id");
    let valid_id =
        id.is_some_and(|id| id.is_string() || id.is_i64() || id.is_u64() || id.is_null());
    let Some(method) = value.get("method").and_then(Value::as_str) else {
        if value.get("method").is_none()
            && valid_id
            && (value.get("result").is_some() || value.get("error").is_some())
        {
            conn.peer.response(&value);
            return None;
        }
        return error();
    };
    if (id.is_some() && !valid_id) || value.get("result").is_some() || value.get("error").is_some()
    {
        return error();
    }
    let payload = value.get("params").cloned().unwrap_or_else(|| json!({}));
    let Some(id) = id else {
        match method {
            "$/cancel_request" => {
                if let Some(target) = payload.get("requestId").or_else(|| payload.get("id"))
                    && let Some(request) = requests
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(target)
                {
                    request.cancel.cancel();
                }
            }
            "session/cancel" => {
                if let Some(session) = payload.get("sessionId").and_then(Value::as_str) {
                    for request in requests.lock().unwrap_or_else(|e| e.into_inner()).values() {
                        if request.prompt_session.as_deref() == Some(session) {
                            request.session_cancel.cancel();
                        }
                    }
                    conn.cancel_turn(session);
                }
            }
            _ => {}
        }
        return None;
    };
    let request = Arc::new(Inbound {
        cancel: CancelToken::new(),
        session_cancel: CancelToken::new(),
        prompt_session: (method == "session/prompt")
            .then(|| payload.get("sessionId")?.as_str().map(str::to_owned))
            .flatten(),
    });
    {
        let mut requests = requests.lock().unwrap_or_else(|e| e.into_inner());
        if requests.contains_key(id) {
            return error();
        }
        requests.insert(id.clone(), request.clone());
    }
    let steering = if method == "_session/steering" {
        payload
            .get("sessionId")
            .and_then(Value::as_str)
            .and_then(|id| conn.order_steering(id))
    } else {
        None
    };
    Some(Job::Request {
        id: id.clone(),
        method: method.into(),
        params: payload,
        request,
        steering,
    })
}

async fn execute(
    conn: Arc<Connection>,
    requests: Requests,
    job: Job,
) -> (Value, Option<acp::SessionNotification>) {
    let Job::Request {
        id,
        method,
        params,
        request,
        steering,
    } = job
    else {
        let Job::Reply(reply) = job else {
            unreachable!()
        };
        return (reply, None);
    };
    let _steering_turn = if let Some((previous, turn)) = steering {
        if let Some(previous) = previous {
            let _ = previous.await;
        }
        Some(turn)
    } else {
        None
    };
    let source_session = params.get("sessionId").cloned();
    let result = if request.cancel.is_cancelled() {
        Err(acp::Error::request_cancelled())
    } else {
        conn.dispatch(&method, params, &request.cancel, &request.session_cancel)
            .await
    };
    let mut result = {
        let mut requests = requests.lock().unwrap_or_else(|e| e.into_inner());
        requests.remove(&id);
        match result {
            // Never overwrite an already-completed Ok response (e.g. session/new or session/fork)
            // with request_cancelled, or the client would never receive the sessionId while the
            // server leaks the live session and MCP child processes.
            Ok(value) => Ok(value),
            Err(_) if request.cancel.is_cancelled() => Err(acp::Error::request_cancelled()),
            Err(error) => Err(error),
        }
    };
    let after = result
        .as_ref()
        .ok()
        .and_then(|result| conn.commands_after(&method, source_session.as_ref(), result));
    if let Ok(result) = &mut result {
        conn.server.auth.redact(result);
    }
    (result_message(id, result), after)
}

#[derive(Default)]
struct Frame {
    bytes: Vec<u8>,
    too_long: bool,
}

enum Record {
    Empty,
    Json(Option<Value>),
}

impl Frame {
    async fn read(
        &mut self,
        reader: &mut (impl AsyncBufRead + Unpin),
    ) -> std::io::Result<Option<Record>> {
        const MAX_BYTES: usize = 64 * 1024 * 1024;
        loop {
            let bytes = reader.fill_buf().await?;
            if bytes.is_empty() {
                let incomplete = self.too_long || !self.bytes.iter().all(u8::is_ascii_whitespace);
                self.bytes.clear();
                self.too_long = false;
                return Ok(incomplete.then_some(Record::Json(None)));
            }
            let end = bytes.iter().position(|byte| *byte == b'\n');
            let count = end.map_or(bytes.len(), |index| index + 1);
            if count > MAX_BYTES - self.bytes.len() {
                self.too_long = true;
                self.bytes.clear();
            }
            if !self.too_long {
                self.bytes.extend_from_slice(&bytes[..count]);
            }
            reader.consume(count);
            if end.is_some() {
                let record = if self.too_long {
                    Record::Json(None)
                } else if self.bytes.iter().all(u8::is_ascii_whitespace) {
                    Record::Empty
                } else {
                    Record::Json(serde_json::from_slice(&self.bytes).ok())
                };
                self.bytes.clear();
                self.too_long = false;
                return Ok(Some(record));
            }
        }
    }
}

/// Serve one ACP connection. Both directions use UTF-8 JSON, one message per line.
pub async fn serve_io<R, W>(
    server: Arc<AcpServer>,
    mut reader: R,
    mut writer: W,
) -> std::result::Result<(), String>
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (tx, mut rx) = mpsc::unbounded_channel();
    let peer = Arc::new(Peer {
        writer: tx,
        auth: server.auth.clone(),
        pending: Mutex::default(),
        closed: CancelToken::new(),
    });
    let conn = Arc::new(Connection::new(server, peer.clone()));
    let mut output = tokio::spawn(async move {
        while let Some(value) = rx.recv().await {
            let mut frame = serde_json::to_vec(&value).map_err(std::io::Error::other)?;
            frame.push(b'\n');
            writer.write_all(&frame).await?;
            writer.flush().await?;
        }
        Ok::<(), std::io::Error>(())
    });
    let requests = Requests::default();
    let mut tasks = JoinSet::new();
    let mut line = Frame::default();
    let mut writer_stopped = false;
    let input = loop {
        tokio::select! {
            result = &mut output => {
                writer_stopped = true;
                break result.map_err(|_| "The ACP writer stopped.".to_owned()).and_then(|value| value.map_err(|err| err.to_string()));
            }
            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                if result.is_err() { break Err("An ACP request handler stopped.".into()); }
            }
            result = line.read(&mut reader) => {
                let parsed = match result {
                    Ok(None) => break Ok(()),
                    Ok(Some(Record::Empty)) => continue,
                    Ok(Some(Record::Json(value))) => value,
                    Err(error) => break Err(error.to_string()),
                };
                let (jobs, batch) = match parsed {
                    Some(Value::Array(values)) if values.is_empty() => (vec![Job::Reply(result_message(Value::Null, Err(acp::Error::invalid_request())))], false),
                    Some(Value::Array(values)) => (values.into_iter().filter_map(|v| classify(&conn, &requests, v)).collect(), true),
                    Some(value) => (classify(&conn, &requests, value).into_iter().collect(), false),
                    None => (vec![Job::Reply(result_message(Value::Null, Err(acp::Error::parse_error())))], false),
                };
                if jobs.is_empty() { continue; }
                let conn = conn.clone();
                let requests = requests.clone();
                tasks.spawn(async move {
                    let (mut replies, after): (Vec<_>, Vec<_>) = futures_util::future::join_all(jobs.into_iter().map(|job| execute(conn.clone(), requests.clone(), job))).await.into_iter().unzip();
                    if batch { conn.peer.send(Value::Array(replies)); }
                    else if let Some(reply) = replies.pop() { conn.peer.send(reply); }
                    for notification in after.into_iter().flatten() { conn.peer.notify("session/update", notification); }
                });
            }
        }
    };
    peer.closed.cancel();
    conn.cancel_all();
    {
        for request in requests.lock().unwrap().values() {
            // Already received short requests can still reply to a half-closed input.
            if request.prompt_session.is_some() {
                request.session_cancel.cancel();
            }
        }
    }
    while tasks.join_next().await.is_some() {}
    conn.shutdown().await;
    drop(conn);
    drop(peer);
    if !writer_stopped {
        output
            .await
            .map_err(|_| "The ACP writer stopped.".to_owned())?
            .map_err(|err| err.to_string())?;
    }
    input
}

/// Serve ACP on stdin and stdout until the client disconnects.
pub async fn serve_stdio(config: AcpConfig) -> std::result::Result<(), String> {
    serve_io(
        Arc::new(AcpServer::new(config)?),
        BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
    )
    .await
}
