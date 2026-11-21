//! Streaming wire adapters for OpenAI Responses, OpenAI Codex Responses, OpenAI Completions, Anthropic Messages, and Google Generative AI.

pub mod anthropic_messages;
pub mod google_generative_ai;
pub mod openai_codex_responses;
pub mod openai_completions;
pub mod openai_responses;

use crate::{
    AssistantDiagnostic, AssistantEventStream, AssistantMessage, AssistantMessageEvent,
    CancelToken, HttpRequest, HttpStreamResponse, HttpTransport, Model, StopReason, ThinkingLevel,
    TransportError, Usage,
};
use futures_channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use futures_core::Stream;
use futures_util::future::{self, Either};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{SystemTime, UNIX_EPOCH};

/// In-memory non-blocking event sink backed by a single-producer stream driver (zero runtime required).
pub(crate) struct EventSink(UnboundedSender<AssistantMessageEvent>);

impl EventSink {
    pub(crate) fn send(&self, event: AssistantMessageEvent) {
        let _ = self.0.unbounded_send(event);
    }
}

struct ProducerStream {
    future: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
    rx: UnboundedReceiver<AssistantMessageEvent>,
}

impl Stream for ProducerStream {
    type Item = AssistantMessageEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Poll::Ready(Some(event)) = Pin::new(&mut self.rx).poll_next(cx) {
            return Poll::Ready(Some(event));
        }

        if let Some(fut) = self.future.as_mut()
            && fut.as_mut().poll(cx).is_ready()
        {
            self.future = None;
        }

        match Pin::new(&mut self.rx).poll_next(cx) {
            Poll::Ready(Some(event)) => Poll::Ready(Some(event)),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => {
                if self.future.is_none() {
                    Poll::Ready(None)
                } else {
                    Poll::Pending
                }
            }
        }
    }
}

/// Drive an async protocol function lazily as an `AssistantEventStream` without spawning tasks.
pub(crate) fn drive_event_stream<F, Fut>(producer: F) -> AssistantEventStream
where
    F: FnOnce(EventSink) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = mpsc::unbounded();
    let fut = Box::pin(producer(EventSink(tx)));
    Box::pin(ProducerStream {
        future: Some(fut),
        rx,
    })
}

pub(crate) fn now_ms() -> u64 {
    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    {
        0
    }
    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64)
    }
}

pub(crate) fn new_assistant_message(
    model: &Model,
    thinking_level: Option<ThinkingLevel>,
) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_id: None,
        thinking_level,
        usage: Usage::default(),
        stop_reason: StopReason::Pending,
        error_message: None,
        diagnostics: Vec::new(),
        timestamp: now_ms(),
        duration_ms: None,
    }
}

pub(crate) async fn send_cancellable(
    transport: &dyn HttpTransport,
    request: HttpRequest,
    cancel: CancelToken,
) -> Result<HttpStreamResponse, TransportError> {
    if cancel.is_cancelled() {
        return Err(TransportError::Aborted);
    }
    let send_fut = transport.send(request, cancel.clone());
    let cancel_fut = cancel.cancelled();
    futures_util::pin_mut!(send_fut, cancel_fut);

    let response = match future::select(send_fut, cancel_fut).await {
        Either::Left((res, _)) => res?,
        Either::Right(((), _)) => return Err(TransportError::Aborted),
    };

    if !(200..300).contains(&response.status) {
        let mut body_bytes = Vec::new();
        let mut body = response.body;
        use futures_util::StreamExt;
        while let Some(chunk) = body.next().await {
            if let Ok(bytes) = chunk {
                body_bytes.extend_from_slice(&bytes);
                if body_bytes.len() >= 64 * 1024 {
                    break;
                }
            }
        }
        let body_text = String::from_utf8_lossy(&body_bytes).into_owned();
        return Err(TransportError::HttpStatus {
            status: response.status,
            headers: response.headers,
            body: body_text,
        });
    }

    Ok(response)
}

pub(crate) fn emit_transport_error(
    sink: &EventSink,
    mut output: AssistantMessage,
    err: TransportError,
    cancel: &CancelToken,
    started_at: u64,
) {
    let is_aborted = cancel.is_cancelled() || matches!(err, TransportError::Aborted);
    output.stop_reason = if is_aborted {
        StopReason::Aborted
    } else {
        StopReason::Error
    };
    let (code, retryable) = match &err {
        TransportError::Aborted => ("aborted".to_string(), false),
        TransportError::Timeout => ("timeout".to_string(), true),
        TransportError::Network(_) => ("network".to_string(), true),
        TransportError::HttpStatus { status, .. } => {
            let retryable = matches!(*status, 408 | 409 | 429 | 500..=599);
            (format!("http_{status}"), retryable)
        }
    };
    let msg = err.to_string();
    output.error_message = Some(msg.clone());
    output.diagnostics.push(AssistantDiagnostic {
        code,
        message: msg,
        retryable,
    });
    output.duration_ms = Some(now_ms().saturating_sub(started_at));
    sink.send(AssistantMessageEvent::Error {
        reason: output.stop_reason,
        error: output,
    });
}

pub(crate) fn emit_protocol_error(
    sink: &EventSink,
    mut output: AssistantMessage,
    code: impl Into<String>,
    message: impl Into<String>,
    cancel: &CancelToken,
    started_at: u64,
) {
    let message = message.into();
    output.stop_reason = if cancel.is_cancelled() {
        StopReason::Aborted
    } else {
        StopReason::Error
    };
    output.error_message = Some(message.clone());
    output.diagnostics.push(AssistantDiagnostic {
        code: code.into(),
        message,
        retryable: false,
    });
    output.duration_ms = Some(now_ms().saturating_sub(started_at));
    sink.send(AssistantMessageEvent::Error {
        reason: output.stop_reason,
        error: output,
    });
}

pub(crate) fn thinking_level_effort_str(level: ThinkingLevel) -> Option<&'static str> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::None => Some("none"),
        ThinkingLevel::Minimal => Some("minimal"),
        ThinkingLevel::Low => Some("low"),
        ThinkingLevel::Medium => Some("medium"),
        ThinkingLevel::High => Some("high"),
        ThinkingLevel::Xhigh => Some("xhigh"),
        ThinkingLevel::Max => Some("max"),
    }
}
