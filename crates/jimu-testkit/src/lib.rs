//! Deterministic Faux Streaming Transport/Provider and prompt-cache prefix inspector for E2E tests.

use bytes::Bytes;
use futures_channel::mpsc::{self, UnboundedSender};
use futures_core::Stream;
use futures_util::StreamExt;
use jimu_ai::{
    AssistantEventStream, AssistantMessageEvent, BoxFuture, ByteStream, CancelToken, HttpRequest,
    HttpStreamResponse, HttpTransport, Model, SimpleStreamOptions, StreamProvider,
    TranscriptContext, TransportError,
};
use serde_json::Value;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

/// Recorded HTTP request captured by `FauxTransport`.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body_bytes: Bytes,
    pub body_json: Value,
}

impl RecordedRequest {
    /// Case-insensitive header lookup.
    pub fn header(&self, name: &str) -> Option<&str> {
        let lower = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k.to_ascii_lowercase() == lower)
            .map(|(_, v)| v.as_str())
    }
}

/// Handle to feed SSE chunks or errors into a gated `ScriptedHttpResponse` deterministically (no sleeps).
#[derive(Clone)]
pub struct SseScriptSender {
    tx: UnboundedSender<Result<Bytes, TransportError>>,
}

impl SseScriptSender {
    pub fn push_str(&self, chunk: impl Into<String>) -> bool {
        self.tx
            .unbounded_send(Ok(Bytes::from(chunk.into())))
            .is_ok()
    }

    pub fn push_bytes(&self, chunk: impl Into<Bytes>) -> bool {
        self.tx.unbounded_send(Ok(chunk.into())).is_ok()
    }

    pub fn push_error(&self, err: TransportError) -> bool {
        self.tx.unbounded_send(Err(err)).is_ok()
    }

    pub fn close(self) {
        self.tx.close_channel();
    }
}

/// Scripted HTTP outcome served by `FauxTransport`.
pub enum ScriptedHttpResponse {
    Stream {
        status: u16,
        headers: Vec<(String, String)>,
        body: ByteStream,
    },
    Error(TransportError),
    PendingUntilCancel,
}

impl ScriptedHttpResponse {
    /// Serve a 200 OK `text/event-stream` response from pre-chunked byte slices.
    pub fn sse(status: u16, chunks: Vec<Bytes>) -> Self {
        let items: Vec<Result<Bytes, TransportError>> = chunks.into_iter().map(Ok).collect();
        let body: ByteStream = Box::pin(futures_util::stream::iter(items));
        Self::Stream {
            status,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body,
        }
    }

    /// Convenience constructor for HTTP 200 SSE responses split across arbitrary string chunks.
    pub fn sse_strings(chunks: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let bytes = chunks.into_iter().map(|s| Bytes::from(s.into())).collect();
        Self::sse(200, bytes)
    }

    /// Create a channel-gated HTTP 200 SSE response for deterministic mid-stream cancel/hang tests.
    pub fn gated_sse() -> (Self, SseScriptSender) {
        let (tx, rx) = mpsc::unbounded();
        let body: ByteStream = Box::pin(rx);
        let resp = Self::Stream {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body,
        };
        (resp, SseScriptSender { tx })
    }

    /// Serve a custom `ByteStream` with explicit HTTP status and headers.
    pub fn stream(status: u16, headers: Vec<(String, String)>, body: ByteStream) -> Self {
        Self::Stream {
            status,
            headers,
            body,
        }
    }

    /// Serve an HTTP non-2xx status error.
    pub fn http_error(status: u16, body: impl Into<String>) -> Self {
        Self::Error(TransportError::HttpStatus {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.into(),
        })
    }

    /// Serve an arbitrary `TransportError`.
    pub fn transport_error(err: TransportError) -> Self {
        Self::Error(err)
    }

    /// Hang on `send()` until `CancelToken::cancel()` fires, then return `TransportError::Aborted`.
    pub fn pending_until_cancel() -> Self {
        Self::PendingUntilCancel
    }
}

#[derive(Default)]
struct FauxTransportState {
    responses: VecDeque<ScriptedHttpResponse>,
    requests: Vec<RecordedRequest>,
}

/// Deterministic in-memory `HttpTransport` recording every request and replaying scripted responses.
#[derive(Clone, Default)]
pub struct FauxTransport {
    state: Arc<Mutex<FauxTransportState>>,
}

impl FauxTransport {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn enqueue(&self, response: ScriptedHttpResponse) {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard.responses.push_back(response);
    }

    pub fn enqueue_sse(&self, chunks: impl IntoIterator<Item = impl Into<String>>) {
        self.enqueue(ScriptedHttpResponse::sse_strings(chunks));
    }

    pub fn enqueue_gated_sse(&self) -> SseScriptSender {
        let (resp, tx) = ScriptedHttpResponse::gated_sse();
        self.enqueue(resp);
        tx
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .requests
            .clone()
    }

    pub fn take_requests(&self) -> Vec<RecordedRequest> {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut guard.requests)
    }

    pub fn request_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .requests
            .len()
    }
}

impl HttpTransport for FauxTransport {
    fn send(
        &self,
        request: HttpRequest,
        cancel: CancelToken,
    ) -> BoxFuture<'_, Result<HttpStreamResponse, TransportError>> {
        let body_json = serde_json::from_slice::<Value>(&request.body).unwrap_or(Value::Null);
        let recorded = RecordedRequest {
            method: request.method,
            url: request.url,
            headers: request.headers,
            body_bytes: request.body,
            body_json,
        };

        let next = {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            guard.requests.push(recorded);
            guard.responses.pop_front()
        };

        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(TransportError::Aborted);
            }
            match next {
                Some(ScriptedHttpResponse::Stream {
                    status,
                    headers,
                    body,
                }) => Ok(HttpStreamResponse {
                    status,
                    headers,
                    body,
                }),
                Some(ScriptedHttpResponse::Error(err)) => Err(err),
                Some(ScriptedHttpResponse::PendingUntilCancel) => {
                    cancel.cancelled().await;
                    Err(TransportError::Aborted)
                }
                None => Err(TransportError::Network(
                    "FauxTransport: no scripted response enqueued".into(),
                )),
            }
        })
    }
}

/// Recorded invocation made against `FauxProvider`.
#[derive(Debug, Clone)]
pub struct RecordedProviderCall {
    pub model: Model,
    pub context: TranscriptContext,
    pub options: SimpleStreamOptions,
}

/// Channel sender for feeding `AssistantMessageEvent` items into a gated `FauxProvider` stream.
#[derive(Clone)]
pub struct FauxEventSender {
    tx: UnboundedSender<AssistantMessageEvent>,
}

impl FauxEventSender {
    pub fn push(&self, event: AssistantMessageEvent) -> bool {
        self.tx.unbounded_send(event).is_ok()
    }

    pub fn close(self) {
        self.tx.close_channel();
    }
}

struct GatedEventStream {
    rx: mpsc::UnboundedReceiver<AssistantMessageEvent>,
}

impl Stream for GatedEventStream {
    type Item = AssistantMessageEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_next_unpin(cx)
    }
}

#[derive(Default)]
struct FauxProviderState {
    streams: VecDeque<AssistantEventStream>,
    calls: Vec<RecordedProviderCall>,
}

/// Deterministic in-memory `StreamProvider` for higher-level agent/session E2E tests.
#[derive(Clone, Default)]
pub struct FauxProvider {
    state: Arc<Mutex<FauxProviderState>>,
}

impl FauxProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn enqueue_events(&self, events: Vec<AssistantMessageEvent>) {
        self.enqueue_stream(Box::pin(futures_util::stream::iter(events)));
    }

    pub fn enqueue_stream(&self, stream: AssistantEventStream) {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard.streams.push_back(stream);
    }

    pub fn enqueue_gated(&self) -> FauxEventSender {
        let (tx, rx) = mpsc::unbounded();
        self.enqueue_stream(Box::pin(GatedEventStream { rx }));
        FauxEventSender { tx }
    }

    pub fn recorded_calls(&self) -> Vec<RecordedProviderCall> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .calls
            .clone()
    }
}

impl StreamProvider for FauxProvider {
    fn stream(
        &self,
        model: &Model,
        context: &TranscriptContext,
        options: SimpleStreamOptions,
    ) -> AssistantEventStream {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard.calls.push(RecordedProviderCall {
            model: model.clone(),
            context: context.clone(),
            options,
        });
        guard
            .streams
            .pop_front()
            .unwrap_or_else(|| Box::pin(futures_util::stream::empty()))
    }
}

fn strip_tail_cache_control(msg: &Value) -> Value {
    let mut cloned = msg.clone();
    if let Value::Object(obj) = &mut cloned
        && let Some(Value::Array(blocks)) = obj.get_mut("content")
        && let Some(Value::Object(last)) = blocks.last_mut()
    {
        last.remove("cache_control");
    }
    cloned
}

fn to_canonical_bytes(val: &Value) -> Vec<u8> {
    serde_json::to_vec(val).expect("JSON value must serialize")
}

/// Check that system prompt, tools array bytes, and prior message prefix bytes stay 100% identical
/// across two consecutive turns `req1` -> `req2` (supporting OpenAI Responses, OpenAI Completions,
/// Anthropic Messages, and Google Generative AI payloads).
pub fn check_prompt_prefix_identical(
    req1: &RecordedRequest,
    req2: &RecordedRequest,
) -> Result<(), String> {
    let b1 = &req1.body_json;
    let b2 = &req2.body_json;

    // 1. Verify top-level system prompt bytes (if present on either request).
    for sys_key in ["instructions", "system", "systemInstruction"] {
        if b1.get(sys_key).is_some() || b2.get(sys_key).is_some() {
            let s1 = b1.get(sys_key).map(to_canonical_bytes);
            let s2 = b2.get(sys_key).map(to_canonical_bytes);
            if s1 != s2 {
                return Err(format!(
                    "system prompt `{sys_key}` drifted across turns:\n  turn1: {}\n  turn2: {}",
                    b1.get(sys_key).unwrap_or(&Value::Null),
                    b2.get(sys_key).unwrap_or(&Value::Null),
                ));
            }
        }
    }

    // 2. Verify tools array bytes (including exact tool and JSON Schema property order).
    if b1.get("tools").is_some() || b2.get("tools").is_some() {
        let t1 = b1.get("tools").map(to_canonical_bytes);
        let t2 = b2.get("tools").map(to_canonical_bytes);
        if t1 != t2 {
            return Err(format!(
                "tools array bytes drifted across turns:\n  turn1: {}\n  turn2: {}",
                b1.get("tools").unwrap_or(&Value::Null),
                b2.get("tools").unwrap_or(&Value::Null),
            ));
        }
    }

    // 3. Verify prior message prefix bytes.
    let msg_key = ["input", "messages", "contents"]
        .into_iter()
        .find(|k| b1.get(*k).is_some() || b2.get(*k).is_some())
        .ok_or_else(|| {
            "could not locate input/messages/contents array in request JSON".to_string()
        })?;

    let arr1 = b1
        .get(msg_key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("turn1 `{msg_key}` is not a JSON array"))?;
    let arr2 = b2
        .get(msg_key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("turn2 `{msg_key}` is not a JSON array"))?;

    if arr2.len() < arr1.len() {
        return Err(format!(
            "turn2 `{msg_key}` length ({}) is shorter than turn1 ({})",
            arr2.len(),
            arr1.len()
        ));
    }

    let is_anthropic = b1.get("system").is_some() || req1.url.contains("/messages");

    for idx in 0..arr1.len() {
        let is_moving_anthropic_tail =
            is_anthropic && idx + 1 == arr1.len() && arr2.len() > arr1.len();

        let (m1_bytes, m2_bytes) = if is_moving_anthropic_tail {
            (
                to_canonical_bytes(&strip_tail_cache_control(&arr1[idx])),
                to_canonical_bytes(&strip_tail_cache_control(&arr2[idx])),
            )
        } else {
            (
                to_canonical_bytes(&arr1[idx]),
                to_canonical_bytes(&arr2[idx]),
            )
        };

        if m1_bytes != m2_bytes {
            return Err(format!(
                "message prefix `{msg_key}[{idx}]` drifted across turns:\n  turn1: {}\n  turn2: {}",
                arr1[idx], arr2[idx],
            ));
        }
    }

    Ok(())
}

/// Panic with a detailed diff if system prompt, tools bytes, or prior message prefix drifted across turns.
pub fn assert_prompt_prefix_identical(req1: &RecordedRequest, req2: &RecordedRequest) {
    if let Err(err) = check_prompt_prefix_identical(req1, req2) {
        panic!("prompt cache prefix mismatch: {err}");
    }
}
