//! Harness-agnostic contract for connecting any agent runtime to `pikra-tui`.
//!
//! External harnesses only need to implement [`TuiHarness`] (5 core methods) to get the full
//! terminal UI, streaming Markdown, theme engine, diff/VTE tool cards, selection,
//! and interactive modals. Optional features (model picker, permission mode, session picker,
//! context meter, queue inspection, background processes, skills, MCP, tree rewind) are opt-in via
//! [`HarnessCapabilities`].

pub mod capabilities;
pub mod chrome;
pub mod tools;

pub use capabilities::{
    BackgroundTask, CompactionCap, ContextMeterCap, ContextMeterSnapshot, HarnessCapabilities,
    McpCap, McpServerSummary, ModelOption, ModelPickerCap, PermissionModeCap, PermissionModeOption,
    QueueCap, QueuedItem, RewindCap, RewindPoint, SessionOption, SessionPickerCap, SkillSummary,
    SkillsCap, TasksCap,
};
pub use chrome::{
    AutocompleteProvider, ChromeConfig, ChromeSlots, ChromeState, CombinedAutocompleteProvider,
    SlashCommand, SlashContext, TuiController, UiText,
};
pub use tools::{ToolOutput, ToolRenderer, ToolRendererRegistry, ToolView};

use futures_channel::oneshot;
use futures_core::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

/// Heap-allocated `Send` future alias (avoids depending on `pikra-ai`).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Heap-allocated `Send` event stream alias.
pub type BoxEventStream = Pin<Box<dyn Stream<Item = TuiEvent> + Send>>;

/// Image payload attached to a user prompt or tool output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputImage {
    pub mime_type: String,
    pub data: Vec<u8>,
    pub label: Option<String>,
}

/// User-submitted message (plain text plus optional multimodal attachments).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserInput {
    pub text: String,
    pub images: Vec<InputImage>,
}

impl UserInput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
        }
    }
}

/// Cumulative or per-request token/cost usage reported by a harness.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageStats {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: Option<u64>,
    pub total_tokens: u64,
    pub cost_usd: f64,
}

/// Severity for a system/session notification block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Success,
    Warning,
    Error,
}

/// Lifecycle state for an optional subagent block (used by multi-agent hosts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentStatus {
    Running,
    Completed { summary: Option<String> },
    Failed { error: String },
    Cancelled,
}

/// Subagent progress event emitted by multi-agent harnesses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentUpdate {
    pub id: String,
    pub label: String,
    pub task: String,
    pub status: SubagentStatus,
    pub detail: Option<String>,
}

/// Decision returned from a tool approval modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    Deny,
    AllowOnce,
    AllowSession,
}

/// Single option in a structured question prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionOption {
    pub label: String,
    pub description: Option<String>,
}

/// Structured question modal payload (single-select, multi-select, or free-text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionPrompt {
    pub title: String,
    pub question: String,
    pub options: Vec<QuestionOption>,
    pub multi_select: bool,
    pub allow_freeform: bool,
}

/// Answer returned from [`QuestionPrompt`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionAnswer {
    pub selected_indices: Vec<usize>,
    pub freeform: Option<String>,
}

/// Decision returned from a plan approval modal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanDecision {
    Approve,
    Reject { feedback: Option<String> },
}

/// Interactive modal request sent by a harness or tool to the TUI event loop.
pub enum InteractionRequest {
    /// Ask the user to approve or deny a tool execution.
    ApproveTool {
        tool_name: String,
        summary: String,
        detail: Option<String>,
        reply: oneshot::Sender<ApprovalDecision>,
    },
    /// Present a structured question card (e.g. `ask_user`).
    AskQuestion {
        prompt: QuestionPrompt,
        reply: oneshot::Sender<Option<QuestionAnswer>>,
    },
    /// Present a Markdown plan for user review/approval.
    ApprovePlan {
        title: String,
        markdown: String,
        reply: oneshot::Sender<PlanDecision>,
    },
    /// Present a generic selection modal.
    Select {
        title: String,
        options: Vec<String>,
        reply: oneshot::Sender<Option<usize>>,
    },
}

/// Pre-existing visible message restored when opening the TUI on an existing session.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum HistoryMessage {
    User {
        text: String,
        steered: bool,
    },
    Assistant {
        thinking: Option<String>,
        text: String,
        error: Option<String>,
    },
    Tool {
        call_id: String,
        name: String,
        effective_name: Option<String>,
        args: Value,
        output: ToolOutput,
    },
    Notice {
        level: NoticeLevel,
        message: String,
    },
}

/// Framework-agnostic event stream consumed by the TUI scrollback and status bar.
#[allow(clippy::large_enum_variant)]
pub enum TuiEvent {
    /// A foreground or background agent turn started.
    TurnStarted,
    /// The active turn finished.
    TurnEnded,
    /// A user message was accepted into the transcript.
    UserMessage {
        id: Option<String>,
        text: String,
        steered: bool,
    },
    /// Incremental assistant Markdown text delta for `block_index`.
    TextDelta { block_index: usize, delta: String },
    /// Incremental assistant reasoning/thinking delta for `block_index`.
    ThinkingDelta { block_index: usize, delta: String },
    /// Marks a redacted reasoning block.
    ThinkingRedacted { block_index: usize },
    /// Finalizes the current assistant message.
    AssistantMessageEnd { error: Option<String> },
    /// A tool execution started.
    ToolStart {
        call_id: String,
        name: String,
        effective_name: Option<String>,
        args: Value,
    },
    /// Incremental progress output for a running tool.
    ToolUpdate { call_id: String, output: ToolOutput },
    /// Final result for a tool execution.
    ToolEnd {
        call_id: String,
        name: String,
        effective_name: Option<String>,
        output: ToolOutput,
    },
    /// Incremental or per-request usage update.
    UsageUpdate(UsageStats),
    /// Updated context-window utilization snapshot.
    ContextUsageUpdate(ContextMeterSnapshot),
    /// Updated list of queued steering / follow-up inputs.
    QueueChanged(Vec<QueuedItem>),
    /// Optional subagent progress update.
    SubagentEvent(SubagentUpdate),
    /// Session or system notice (compaction, branch switch, etc.).
    SessionNotice { level: NoticeLevel, message: String },
    /// Interactive modal request (approval, question, plan review, selection).
    Interaction(InteractionRequest),
    /// Non-fatal runtime error displayed in the transcript.
    Error(String),
}

/// Core harness contract implemented by Pikra or any third-party agent runtime.
pub trait TuiHarness: Send + Sync {
    /// Submit a new foreground user prompt.
    fn submit(&self, input: UserInput) -> BoxFuture<'_, Result<(), String>>;

    /// Subscribe to the live stream of [`TuiEvent`] updates.
    fn subscribe(&self) -> BoxEventStream;

    /// Abort the currently running turn and await complete teardown so immediate subsequent
    /// [`Self::submit`] calls never spuriously fail as busy.
    fn abort(&self) -> BoxFuture<'_, ()>;

    /// Inject a steering message mid-turn (optional; default returns unsupported).
    fn steer(&self, _input: UserInput) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async { Err("steering is not supported by this harness".into()) })
    }

    /// Enqueue a follow-up message after the current turn (optional; default returns unsupported).
    fn follow_up(&self, _input: UserInput) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async { Err("follow-up queueing is not supported by this harness".into()) })
    }

    /// Return existing visible messages to seed the transcript on startup.
    fn initial_history(&self) -> Vec<HistoryMessage> {
        Vec::new()
    }

    /// Return initial cumulative usage (e.g. when restoring an existing session).
    fn initial_usage(&self) -> UsageStats {
        UsageStats::default()
    }

    /// Expose optional capabilities supported by this harness. Unimplemented capabilities
    /// automatically stay hidden in the UI.
    fn capabilities(&self) -> HarnessCapabilities<'_> {
        HarnessCapabilities::default()
    }
}
