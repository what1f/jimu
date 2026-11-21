//! Unified multi-provider streaming LLM API, transcript model, and transport traits.

pub mod models;
pub mod oauth;
pub mod providers;
pub mod sse;
pub mod transcript;
pub mod transform;
pub mod usage;

pub use models::{Client, Models, ProviderConfig, complete};
pub use sse::{SseEvent, SseParser, SseStream};
pub use transcript::{ResolvedTranscript, render_system_message_text};
pub use transform::{
    ERROR_TOOL_IMAGE_PLACEHOLDER, NON_VISION_TOOL_IMAGE_PLACEHOLDER,
    NON_VISION_USER_IMAGE_PLACEHOLDER, SplitErrorToolResult, normalize_tool_call_id_anthropic,
    normalize_tool_call_id_openai_completions, normalize_tool_call_id_openai_responses, short_hash,
    split_error_tool_result_images, transform_messages,
};
pub use usage::{calculate_cost, clamp_prompt_cache_key, parse_tool_call_arguments};

use bytes::Bytes;
use futures_core::Stream;
use pikra_schema::Schema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use thiserror::Error;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, TransportError>> + Send>>;
pub type AssistantEventStream = Pin<Box<dyn Stream<Item = AssistantMessageEvent> + Send>>;

/// Runtime-agnostic cooperative cancellation token.
#[derive(Clone, Default)]
pub struct CancelToken {
    inner: Arc<CancelInner>,
}

#[derive(Default)]
struct CancelInner {
    cancelled: AtomicBool,
    wakers: Mutex<Vec<Waker>>,
}

impl std::fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancelToken")
            .field("is_cancelled", &self.is_cancelled())
            .finish()
    }
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        if !self.inner.cancelled.swap(true, Ordering::SeqCst) {
            let wakers =
                std::mem::take(&mut *self.inner.wakers.lock().unwrap_or_else(|e| e.into_inner()));
            for waker in wakers {
                waker.wake();
            }
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    pub fn cancelled(&self) -> CancelledFuture {
        CancelledFuture {
            token: self.clone(),
        }
    }
}

pub struct CancelledFuture {
    token: CancelToken,
}

impl Future for CancelledFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.token.is_cancelled() {
            return Poll::Ready(());
        }
        let mut wakers = self
            .token
            .inner
            .wakers
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if self.token.is_cancelled() {
            return Poll::Ready(());
        }
        if !wakers.iter().any(|w| w.will_wake(cx.waker())) {
            wakers.push(cx.waker().clone());
        }
        Poll::Pending
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApiKind {
    OpenaiResponses,
    OpenaiCodexResponses,
    OpenaiCompletions,
    AnthropicMessages,
    GoogleGenerativeAi,
    #[serde(untagged)]
    Custom(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Off,
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl std::str::FromStr for ThinkingLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Ok(Self::Off),
            "none" | "disable" => Ok(Self::None),
            "minimal" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::Xhigh),
            "max" => Ok(Self::Max),
            other => Err(format!(
                "unsupported thinking level `{other}` (expected off, none, minimal, low, medium, high, xhigh, or max)"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensField {
    MaxTokens,
    MaxCompletionTokens,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ModelCompat {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens_field: Option<MaxTokensField>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CacheRetention {
    None,
    #[default]
    Short,
    Long,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ModelCost {
    /// USD per 1M tokens.
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub api: ApiKind,
    pub base_url: String,
    pub context_window: u32,
    pub max_output_tokens: u32,
    pub supports_images: bool,
    pub supports_reasoning: bool,
    #[serde(default)]
    pub cost: ModelCost,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compat: Option<ModelCompat>,
}

impl Model {
    pub fn new(
        id: impl Into<String>,
        provider: impl Into<String>,
        api: ApiKind,
        base_url: impl Into<String>,
    ) -> Self {
        let id = id.into();
        Self {
            name: id.clone(),
            id,
            provider: provider.into(),
            api,
            base_url: base_url.into(),
            context_window: 128_000,
            max_output_tokens: 8_192,
            supports_images: false,
            supports_reasoning: false,
            cost: ModelCost::default(),
            headers: Vec::new(),
            compat: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Schema,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextContent {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_signature: Option<String>,
}

impl TextContent {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            text_signature: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageContent {
    /// Base64-encoded raw bytes (without `data:` prefix).
    pub data: String,
    pub mime_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingContent {
    pub thinking: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_signature: Option<String>,
    #[serde(default)]
    pub redacted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ContentPart {
    Text(TextContent),
    Image(ImageContent),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AssistantBlock {
    Text(TextContent),
    Thinking(ThinkingContent),
    ToolCall(ToolCall),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
    pub total_tokens: u64,
    #[serde(default)]
    pub cost_total: f64,
    #[serde(default)]
    pub cost_from_provider: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    #[default]
    Pending,
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantDiagnostic {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemMessage {
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<(String, Option<String>)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools_added: Vec<ToolDefinition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools_removed: Vec<String>,
    pub timestamp: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub content: Vec<ContentPart>,
    pub timestamp: u64,
}

impl UserMessage {
    pub fn text(text: impl Into<String>, timestamp: u64) -> Self {
        Self {
            id: None,
            content: vec![ContentPart::Text(TextContent::plain(text))],
            timestamp,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub content: Vec<AssistantBlock>,
    pub api: ApiKind,
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<ThinkingLevel>,
    #[serde(default)]
    pub usage: Usage,
    pub stop_reason: StopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<AssistantDiagnostic>,
    pub timestamp: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

impl AssistantMessage {
    pub fn text_output(&self) -> String {
        let mut out = String::new();
        for block in &self.content {
            if let AssistantBlock::Text(t) = block {
                out.push_str(&t.text);
            }
        }
        out
    }

    pub fn tool_calls(&self) -> Vec<&ToolCall> {
        self.content
            .iter()
            .filter_map(|b| match b {
                AssistantBlock::ToolCall(tc) => Some(tc),
                _ => None,
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<ContentPart>,
    #[serde(default)]
    pub is_error: bool,
    pub timestamp: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    System(SystemMessage),
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TranscriptContext {
    pub messages: Vec<Message>,
}

impl TranscriptContext {
    pub fn new(
        system_prompt: impl Into<String>,
        tools: Vec<ToolDefinition>,
        messages: Vec<Message>,
    ) -> Self {
        let mut all = Vec::with_capacity(messages.len() + 1);
        all.push(Message::System(SystemMessage {
            content: system_prompt.into(),
            sections: Vec::new(),
            tools_added: tools,
            tools_removed: Vec::new(),
            timestamp: 0,
        }));
        all.extend(messages);
        Self { messages: all }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantMessageEvent {
    Start {
        partial: AssistantMessage,
    },
    TextStart {
        content_index: usize,
    },
    TextDelta {
        content_index: usize,
        delta: String,
    },
    TextEnd {
        content_index: usize,
        text: String,
    },
    ThinkingStart {
        content_index: usize,
    },
    ThinkingDelta {
        content_index: usize,
        delta: String,
    },
    ThinkingEnd {
        content_index: usize,
        thinking: String,
    },
    ToolCallStart {
        content_index: usize,
        id: String,
        name: String,
    },
    ToolCallDelta {
        content_index: usize,
        delta: String,
    },
    ToolCallEnd {
        content_index: usize,
        tool_call: ToolCall,
    },
    Done {
        reason: StopReason,
        message: AssistantMessage,
    },
    Error {
        reason: StopReason,
        error: AssistantMessage,
    },
}

#[derive(Debug, Clone, Error, Serialize, Deserialize)]
pub enum TransportError {
    #[error("aborted")]
    Aborted,
    #[error("timeout")]
    Timeout,
    #[error("network error: {0}")]
    Network(String),
    #[error("http status {status}: {body}")]
    HttpStatus {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
    },
}

#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
    pub timeout_ms: Option<u64>,
}

pub struct HttpStreamResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: ByteStream,
}

pub trait HttpTransport: Send + Sync {
    fn send(
        &self,
        request: HttpRequest,
        cancel: CancelToken,
    ) -> BoxFuture<'_, Result<HttpStreamResponse, TransportError>>;
}

#[derive(Debug, Clone, Default)]
pub struct SimpleStreamOptions {
    pub api_key: Option<String>,
    pub reasoning: Option<ThinkingLevel>,
    pub fast: Option<bool>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    pub cache_retention: CacheRetention,
    pub session_id: Option<String>,
    pub headers: Vec<(String, Option<String>)>,
    pub sampling_params: Map<String, Value>,
    pub cancel: CancelToken,
}

pub trait StreamProvider: Send + Sync {
    fn stream(
        &self,
        model: &Model,
        context: &TranscriptContext,
        options: SimpleStreamOptions,
    ) -> AssistantEventStream;
}
