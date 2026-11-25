use jimu_agent::AgentMessage;
use jimu_ai::{
    ApiKind, Message, Model, ModelCost, SystemMessage, ThinkingLevel, ToolDefinition,
    TranscriptContext, Usage, UserMessage,
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

fn is_safe_persisted_base_url(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed
            .chars()
            .any(|c| matches!(c, '?' | '#') || c.is_whitespace() || c.is_control())
    {
        return false;
    }
    let Some((scheme, rest)) = trimmed.split_once("://") else {
        return false;
    };
    if !(scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")) {
        return false;
    }
    let (authority, _) = rest.split_once('/').unwrap_or((rest, ""));
    !authority.is_empty()
        && !authority.starts_with(':')
        && !authority.contains('@')
        && !authority.contains('\\')
}

/// Non-secret model routing metadata persisted in [`SessionHeader`] and
/// [`SessionEntryPayload::ModelChange`] so cross-mode restores and branch forks always recover the
/// exact model endpoint without ever storing API keys, auth headers, or URL credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionModelMetadata {
    pub provider: String,
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<ApiKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_images: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_reasoning: Option<bool>,
}

impl SessionModelMetadata {
    /// Construct non-secret metadata from a [`Model`]. Unsafe URLs (credentials, query strings,
    /// fragments, or non-HTTP schemes) are omitted so secrets are never persisted.
    pub fn from_model(model: &Model) -> Self {
        let base_url = (!model.base_url.is_empty() && is_safe_persisted_base_url(&model.base_url))
            .then(|| model.base_url.clone());
        Self {
            provider: model.provider.clone(),
            model_id: model.id.clone(),
            api: Some(model.api.clone()),
            base_url,
            context_window: Some(model.context_window),
            max_output_tokens: Some(model.max_output_tokens),
            supports_images: Some(model.supports_images),
            supports_reasoning: Some(model.supports_reasoning),
        }
    }

    /// Reconstruct a full [`Model`] when complete non-secret routing (`api` and `base_url`) is
    /// present; returns `None` for legacy bare `(provider, model_id)` records.
    pub fn to_full_model(&self) -> Option<Model> {
        let base_url = self
            .base_url
            .clone()
            .filter(|url| is_safe_persisted_base_url(url))?;
        self.to_model_with_base_url(base_url)
    }

    /// Reconstruct a [`Model`] when `api` is recorded, using `fallback_base_url` only if no
    /// `base_url` was stored (never bypassing validation when an unsafe `base_url` is present).
    pub fn to_model_with_fallback_base_url(&self, fallback_base_url: String) -> Option<Model> {
        let base_url = match self.base_url.as_deref() {
            Some(url) if is_safe_persisted_base_url(url) => url.to_owned(),
            Some(_) => return None,
            None => fallback_base_url,
        };
        self.to_model_with_base_url(base_url)
    }

    fn to_model_with_base_url(&self, base_url: String) -> Option<Model> {
        let api = self.api.clone()?;
        Some(Model {
            id: self.model_id.clone(),
            name: self.model_id.clone(),
            api,
            provider: self.provider.clone(),
            base_url,
            supports_reasoning: self.supports_reasoning.unwrap_or(false),
            supports_images: self.supports_images.unwrap_or(false),
            context_window: self.context_window.unwrap_or(128_000),
            max_output_tokens: self.max_output_tokens.unwrap_or(8_192),
            cost: ModelCost::default(),
            headers: Vec::new(),
            compat: None,
        })
    }
}

/// First line in a JSONL session file describing session metadata shared across all branches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHeader {
    pub version: u32,
    pub id: String,
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_model: Option<SessionModelMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_thinking: Option<ThinkingLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_fast: Option<bool>,
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
    /// Model switch recorded on the active branch (optionally carrying complete non-secret routing).
    ModelChange {
        provider: String,
        model_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api: Option<ApiKind>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_window: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_output_tokens: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        supports_images: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        supports_reasoning: Option<bool>,
    },
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
    pub model_metadata: Option<SessionModelMetadata>,
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
        jimu_agent::default_convert_to_llm(&self.system_prompt, &self.tools, &self.messages)
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
