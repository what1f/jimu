//! Stateful in-memory Agent Turn Loop, concurrent tool execution, steering, and middleware hooks.

use pikra_ai::{
    AssistantMessage, AssistantMessageEvent, BoxFuture, CancelToken, ContentPart, Message, Model,
    ThinkingLevel, ToolCall, ToolDefinition, ToolResultMessage, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ToolExecutionMode {
    #[default]
    Parallel,
    Sequential,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum QueueMode {
    #[default]
    OneAtATime,
    All,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentMessage {
    Llm(Message),
    Custom {
        custom_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        content: Vec<ContentPart>,
        #[serde(default)]
        details: Value,
        #[serde(default)]
        include_in_llm: bool,
        #[serde(default)]
        display: bool,
        timestamp: u64,
    },
}

impl From<Message> for AgentMessage {
    fn from(m: Message) -> Self {
        Self::Llm(m)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentToolResult {
    pub content: Vec<ContentPart>,
    #[serde(default)]
    pub details: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default)]
    pub terminate: bool,
}

impl AgentToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentPart::Text(pikra_ai::TextContent::plain(text))],
            details: Value::Null,
            structured_content: None,
            is_error: false,
            terminate: false,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        let mut r = Self::text(text);
        r.is_error = true;
        r
    }
}

pub type ToolProgressCallback = Arc<dyn Fn(AgentToolResult) + Send + Sync>;

pub trait AgentTool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }
    fn execute(
        &self,
        tool_call_id: String,
        args: Value,
        cancel: CancelToken,
        on_update: ToolProgressCallback,
    ) -> BoxFuture<'_, Result<AgentToolResult, String>>;
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BeforeToolCallResult {
    pub block: bool,
    pub reason: Option<String>,
    pub terminate: bool,
    pub args_override: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AfterToolCallResult {
    pub content: Option<Vec<ContentPart>>,
    pub details: Option<Value>,
    pub structured_content: Option<Value>,
    pub is_error: Option<bool>,
    pub terminate: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnDecision {
    Continue,
    End,
}

#[derive(Debug, Clone)]
pub struct AgentContextSnapshot {
    pub system_prompt: String,
    pub tools: Vec<ToolDefinition>,
    pub messages: Vec<AgentMessage>,
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub fast: bool,
}

#[derive(Debug, Clone, Default)]
pub struct RequestOverride {
    pub system_prompt: Option<String>,
    pub tools: Option<Vec<ToolDefinition>>,
    pub messages: Option<Vec<AgentMessage>>,
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
    pub fast: Option<bool>,
}

pub struct BeforeToolCallCtx<'a> {
    pub assistant_message: &'a AssistantMessage,
    pub tool_call: &'a ToolCall,
    pub args: &'a Value,
    pub snapshot: &'a AgentContextSnapshot,
}

pub struct AfterToolCallCtx<'a> {
    pub assistant_message: &'a AssistantMessage,
    pub tool_call: &'a ToolCall,
    pub args: &'a Value,
    pub result: &'a AgentToolResult,
    pub snapshot: &'a AgentContextSnapshot,
}

pub struct FinishTurnCtx<'a> {
    pub assistant_message: &'a AssistantMessage,
    pub tool_results: &'a [ToolResultMessage],
    pub snapshot: &'a AgentContextSnapshot,
}

pub trait AgentMiddleware: Send + Sync {
    fn prepare_request<'a>(
        &'a self,
        _snapshot: &'a AgentContextSnapshot,
        _cancel: CancelToken,
    ) -> BoxFuture<'a, Option<RequestOverride>> {
        Box::pin(async { None })
    }

    fn transform_context<'a>(
        &'a self,
        messages: Vec<AgentMessage>,
        _cancel: CancelToken,
    ) -> BoxFuture<'a, Vec<AgentMessage>> {
        Box::pin(async move { messages })
    }

    fn before_tool_call<'a>(
        &'a self,
        _ctx: BeforeToolCallCtx<'a>,
        _cancel: CancelToken,
    ) -> BoxFuture<'a, Option<BeforeToolCallResult>> {
        Box::pin(async { None })
    }

    fn after_tool_call<'a>(
        &'a self,
        _ctx: AfterToolCallCtx<'a>,
        _cancel: CancelToken,
    ) -> BoxFuture<'a, Option<AfterToolCallResult>> {
        Box::pin(async { None })
    }

    fn finish_turn<'a>(
        &'a self,
        _ctx: FinishTurnCtx<'a>,
        _cancel: CancelToken,
    ) -> BoxFuture<'a, Option<TurnDecision>> {
        Box::pin(async { None })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    AgentStart,
    TurnStart,
    MessageStart {
        message: AgentMessage,
    },
    MessageUpdate {
        message: AssistantMessage,
        assistant_message_event: AssistantMessageEvent,
    },
    MessageEnd {
        message: AgentMessage,
    },
    RequestUsage {
        model: String,
        provider: String,
        usage: Usage,
    },
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: Value,
    },
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        args: Value,
        partial_result: AgentToolResult,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        result: AgentToolResult,
        is_error: bool,
    },
    Steered {
        id: String,
        text: String,
    },
    TurnEnd {
        message: AssistantMessage,
        tool_results: Vec<ToolResultMessage>,
    },
    AgentEnd {
        messages: Vec<AgentMessage>,
        user_message_kept: bool,
    },
}
