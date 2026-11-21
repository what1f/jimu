use pikra_agent::AgentMessage;
use pikra_ai::{
    Message, SystemMessage, ThinkingLevel, ToolDefinition, TranscriptContext, Usage, UserMessage,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// Current JSONL session format version.
pub const CURRENT_SESSION_VERSION: u32 = 1;

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// First line in a JSONL session file describing session metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHeader {
    pub version: u32,
    pub id: String,
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
}

/// Append-only node in the session entry tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEntry {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub timestamp: u64,
    #[serde(flatten)]
    pub payload: SessionEntryPayload,
}

/// Typed payload stored inside each [`SessionEntry`].
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEntryPayload {
    /// Locks initial base system instructions and resident tool definition order at index 0.
    SystemBaseline {
        system_prompt: String,
        #[serde(default)]
        tools: Vec<ToolDefinition>,
    },
    /// Conversation message (`User`, `Assistant`, `ToolResult`, or `AgentMessage::Custom`).
    Message { message: AgentMessage },
    /// Named context block rendered into the LLM transcript as `<system-reminder name="...">`.
    ContextBlock {
        name: String,
        text: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        ephemeral: bool,
    },
    /// Model switch recorded on the active branch.
    ModelChange { provider: String, model_id: String },
    /// Reasoning / fast-mode change recorded on the active branch.
    ThinkingChange { level: ThinkingLevel, fast: bool },
    /// Context compaction checkpoint replacing earlier turns with a bounded summary.
    Compaction {
        summary: String,
        first_kept_entry_id: String,
        tokens_before: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    /// Arbitrary host/extension metadata (not sent directly to the LLM).
    Custom { custom_type: String, data: Value },
}

/// Tree representation of all branches in a session (returned by [`crate::SessionManager::tree`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionTreeNode {
    pub entry: SessionEntry,
    pub children: Vec<SessionTreeNode>,
}

/// Metadata for the latest compaction checkpoint active on a reconstructed branch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionCheckpoint {
    pub entry_id: String,
    pub summary: String,
    pub first_kept_entry_id: String,
    pub tokens_before: u64,
    pub usage: Option<Usage>,
}

/// Result of walking root -> active leaf and replaying compaction + baseline invariants.
#[derive(Debug, Clone, PartialEq)]
pub struct ReconstructedContext {
    pub system_prompt: String,
    pub tools: Vec<ToolDefinition>,
    pub messages: Vec<AgentMessage>,
    pub model: Option<(String, String)>,
    pub thinking: Option<(ThinkingLevel, bool)>,
    pub latest_compaction: Option<CompactionCheckpoint>,
    /// Named context blocks currently visible in the reconstructed transcript (after latest compaction).
    pub visible_context_blocks: BTreeMap<String, String>,
    /// All named context blocks ever set along the branch (including those predating compaction).
    pub all_branch_context_blocks: BTreeMap<String, String>,
}

impl ReconstructedContext {
    /// Convert this reconstructed session state into a wire-ready [`TranscriptContext`].
    pub fn to_transcript_context(&self) -> TranscriptContext {
        pikra_agent::default_convert_to_llm(&self.system_prompt, &self.tools, &self.messages)
    }
}

/// Events emitted by [`crate::SessionManager`].
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    EntryAppended {
        entry: SessionEntry,
    },
    ContextAppended {
        name: String,
        text: String,
    },
    CompactionStart {
        tokens_before: u64,
    },
    CompactionEnd {
        summary: String,
        tokens_before: u64,
        first_kept_entry_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    Forked {
        from_leaf_id: Option<String>,
        to_leaf_id: String,
    },
}

/// Format a named context block into the canonical `<system-reminder>` tag.
pub fn format_context_block(name: &str, text: &str) -> String {
    format!("<system-reminder name=\"{name}\">\n{text}\n</system-reminder>")
}

/// Render a `ContextBlock` entry as a user message for the LLM transcript.
pub fn context_block_to_agent_message(name: &str, text: &str, timestamp: u64) -> AgentMessage {
    AgentMessage::Llm(Message::User(UserMessage::meta_text(
        format_context_block(name, text),
        timestamp,
    )))
}

/// Render a `Compaction` summary checkpoint as a user message for the LLM transcript.
pub fn compaction_summary_to_agent_message(summary: &str, timestamp: u64) -> AgentMessage {
    AgentMessage::Llm(Message::User(UserMessage::meta_text(
        format!(
            "<compaction-summary>\nThe following is a checkpoint summary of earlier turns in this conversation:\n\n{summary}\n</compaction-summary>"
        ),
        timestamp,
    )))
}

/// Build the leading `Message::System` from a `SystemBaseline`.
pub fn baseline_to_system_message(
    system_prompt: &str,
    tools: &[ToolDefinition],
    timestamp: u64,
) -> AgentMessage {
    AgentMessage::Llm(Message::System(SystemMessage {
        content: system_prompt.to_string(),
        sections: Vec::new(),
        tools_added: tools.to_vec(),
        tools_removed: Vec::new(),
        timestamp,
    }))
}

/// Convert entries returned by `SessionManager::flush_pending_context` to prompt messages and events.
pub fn prepare_flushed_prompt(
    flushed: Vec<SessionEntry>,
    user: AgentMessage,
) -> (Vec<AgentMessage>, usize, Vec<SessionEvent>) {
    let mut messages = Vec::with_capacity(flushed.len() + 1);
    let mut events = Vec::new();
    for entry in flushed {
        match entry.payload {
            SessionEntryPayload::ContextBlock { name, text, .. } => {
                messages.push(context_block_to_agent_message(
                    &name,
                    &text,
                    entry.timestamp,
                ));
                events.push(SessionEvent::ContextAppended { name, text });
            }
            SessionEntryPayload::Message { message } => messages.push(message),
            _ => {}
        }
    }
    let count = messages.len();
    messages.push(user);
    (messages, count, events)
}
