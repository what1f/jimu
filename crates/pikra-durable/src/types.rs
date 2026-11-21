use pikra_ai::{AssistantMessageEvent, Model, StopReason, ToolCall, ToolResultMessage, Usage};
use pikra_session::SessionEntry;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Lifecycle status of a durable conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationStatus {
    Idle,
    Running,
    Parked,
    Failed,
    Cancelled,
}

impl ConversationStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Parked => "parked",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "idle" => Some(Self::Idle),
            "running" => Some(Self::Running),
            "parked" => Some(Self::Parked),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

/// Persisted conversation metadata row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationRecord {
    pub conversation_id: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_entry_id: Option<String>,
    pub status: ConversationStatus,
    pub active_model: Model,
    pub system_prompt: String,
    #[serde(default)]
    pub checkpoint_version: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_expires_at_ms: Option<u64>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// Outcome of an atomic lease claim/renewal attempt on a conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseClaimResult {
    /// Execution lease acquired or renewed by the caller.
    Acquired {
        checkpoint_version: u64,
        lease_expires_at_ms: u64,
    },
    /// Another active engine currently holds a live (unexpired) lease.
    HeldByOther {
        lease_owner: String,
        lease_expires_at_ms: u64,
    },
}

/// Idempotent execution state for a `(conversation_id, submission_id)` pair.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SubmissionState {
    Running,
    Parked,
    Completed {
        assistant_entry_id: String,
        final_text: String,
        stop_reason: StopReason,
        usage: Usage,
    },
    Failed {
        error: String,
    },
    Cancelled,
}

/// Persisted idempotent submission record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubmissionRecord {
    pub conversation_id: String,
    pub submission_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_entry_id: Option<String>,
    pub state: SubmissionState,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// Serializable continuation metadata for a tool call that parked on a timer or signal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParkedToolCall {
    pub assistant_entry_id: String,
    pub tool_call: ToolCall,
    /// Complete ordered list of tool calls issued by the assistant turn so remaining calls
    /// and source-ordered `ToolResultMessage` commits resume intact after waking.
    pub batch_tool_calls: Vec<ToolCall>,
    pub checkpoint: Value,
}

/// Durable suspension condition stored while a conversation holds zero active RAM/tasks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ParkState {
    SleepUntil {
        wake_at_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call: Option<ParkedToolCall>,
    },
    AwaitSignal {
        signal_key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_at_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call: Option<ParkedToolCall>,
    },
}

/// Active execution phase checkpoint for deterministic crash recovery.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum TaskCheckpoint {
    Generating {
        submission_id: String,
        turn_index: u32,
        attempt: u32,
    },
    ExecutingTools {
        submission_id: String,
        turn_index: u32,
        assistant_entry_id: String,
        tool_calls: Vec<ToolCall>,
    },
    Compacting {
        submission_id: String,
        #[serde(default)]
        turn_index: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_leaf_id: Option<String>,
    },
    Parked {
        submission_id: String,
        turn_index: u32,
        park: ParkState,
    },
}

impl TaskCheckpoint {
    pub fn submission_id(&self) -> &str {
        match self {
            Self::Generating { submission_id, .. }
            | Self::ExecutingTools { submission_id, .. }
            | Self::Compacting { submission_id, .. }
            | Self::Parked { submission_id, .. } => submission_id,
        }
    }
}

/// Side-effect ledger entry recording a finalized tool call result before transcript advancement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletedToolRecord {
    pub conversation_id: String,
    pub tool_call_id: String,
    pub assistant_entry_id: String,
    pub tool_name: String,
    pub message: ToolResultMessage,
    #[serde(default)]
    pub details: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    #[serde(default)]
    pub terminate: bool,
    pub completed_at_ms: u64,
}

/// External signal buffered or consumed for a conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignalRecord {
    pub conversation_id: String,
    pub signal_id: String,
    pub signal_key: String,
    pub payload: Value,
    pub received_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumed_at_ms: Option<u64>,
}

/// Outcome of an idempotent `deliver_signal` call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SignalDeliveryResult {
    /// Signal was newly persisted and buffered/ready for wakeup.
    Delivered(SignalRecord),
    /// A signal with the same `(conversation_id, signal_id)` already existed; no duplicate stored.
    Duplicate(SignalRecord),
}

impl SignalDeliveryResult {
    pub fn record(&self) -> &SignalRecord {
        match self {
            Self::Delivered(r) | Self::Duplicate(r) => r,
        }
    }

    pub fn is_duplicate(&self) -> bool {
        matches!(self, Self::Duplicate(_))
    }
}

/// Named JSON document persisted with monotonic CAS revision per conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentRecord {
    pub conversation_id: String,
    pub key: String,
    pub revision: u64,
    pub value: Value,
    pub updated_at_ms: u64,
}

/// Optimistic compare-and-swap document write operation included in a [`CommitBatch`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentWrite {
    pub key: String,
    /// `None` requires the document to not exist yet (creates revision `1`).
    /// `Some(rev)` requires the current revision to equal `rev` (advances to `rev + 1`).
    pub expected_revision: Option<u64>,
    pub value: Value,
}

/// Checkpoint update instruction within a [`CommitBatch`].
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Default)]
pub enum CheckpointMutation {
    #[default]
    Unchanged,
    Set(TaskCheckpoint),
    Clear,
}

/// Atomic single-transaction batch commit targeting one conversation.
#[derive(Debug, Clone)]
pub struct CommitBatch {
    pub conversation_id: String,
    pub now_ms: u64,
    pub expected_head_entry_id: Option<Option<String>>,
    pub expected_lease_owner: Option<String>,
    pub expected_checkpoint_version: Option<u64>,
    pub renew_lease_expires_at_ms: Option<u64>,
    pub release_lease: bool,
    pub require_no_checkpoint: bool,
    pub upsert_conversation: Option<ConversationRecord>,
    pub update_status: Option<ConversationStatus>,
    pub update_model: Option<Model>,
    pub append_entries: Vec<SessionEntry>,
    pub upsert_submission: Option<SubmissionRecord>,
    pub checkpoint: CheckpointMutation,
    pub record_completed_tools: Vec<CompletedToolRecord>,
    pub consume_signal_ids: Vec<String>,
    pub document_writes: Vec<DocumentWrite>,
}

impl CommitBatch {
    pub fn new(conversation_id: impl Into<String>, now_ms: u64) -> Self {
        Self {
            conversation_id: conversation_id.into(),
            now_ms,
            expected_head_entry_id: None,
            expected_lease_owner: None,
            expected_checkpoint_version: None,
            renew_lease_expires_at_ms: None,
            release_lease: false,
            require_no_checkpoint: false,
            upsert_conversation: None,
            update_status: None,
            update_model: None,
            append_entries: Vec::new(),
            upsert_submission: None,
            checkpoint: CheckpointMutation::Unchanged,
            record_completed_tools: Vec::new(),
            consume_signal_ids: Vec::new(),
            document_writes: Vec::new(),
        }
    }

    pub(crate) fn advances_checkpoint_version(&self) -> bool {
        !matches!(self.checkpoint, CheckpointMutation::Unchanged)
            || !self.append_entries.is_empty()
            || self.upsert_submission.is_some()
            || self.update_status.is_some()
            || !self.record_completed_tools.is_empty()
            || self.expected_checkpoint_version.is_some()
    }
}

/// Receipt returned after an atomic [`CommitBatch`] succeeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitReceipt {
    pub conversation_id: String,
    pub head_entry_id: Option<String>,
    pub last_seq: u64,
    pub checkpoint_version: u64,
    pub document_revisions: Vec<(String, u64)>,
}

/// Settled outcome of [`crate::DurableEngine::submit`] or [`crate::DurableEngine::recover`].
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RunOutcome {
    Completed {
        submission_id: String,
        assistant_entry_id: String,
        final_text: String,
        stop_reason: StopReason,
        usage: Usage,
        deduplicated: bool,
    },
    Parked {
        submission_id: String,
        park: ParkState,
    },
    Failed {
        submission_id: String,
        error: String,
    },
    Cancelled {
        submission_id: String,
    },
    Idle,
}

/// Streaming observability events emitted by [`crate::DurableEngine`].
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DurableEvent {
    SubmissionStarted {
        conversation_id: String,
        submission_id: String,
    },
    AssistantEvent {
        conversation_id: String,
        submission_id: String,
        turn_index: u32,
        event: AssistantMessageEvent,
    },
    ToolStarted {
        conversation_id: String,
        submission_id: String,
        tool_call_id: String,
        tool_name: String,
        args: Value,
    },
    ToolParked {
        conversation_id: String,
        submission_id: String,
        tool_call_id: String,
        tool_name: String,
        park: ParkState,
    },
    ToolCompleted {
        conversation_id: String,
        submission_id: String,
        tool_call_id: String,
        tool_name: String,
        is_error: bool,
        replayed: bool,
    },
    Compacted {
        conversation_id: String,
        entry_id: String,
        tokens_before: u64,
        first_kept_entry_id: String,
    },
    ConversationParked {
        conversation_id: String,
        submission_id: String,
        park: ParkState,
    },
    SubmissionCompleted {
        conversation_id: String,
        submission_id: String,
        assistant_entry_id: String,
        final_text: String,
        stop_reason: StopReason,
        usage: Usage,
    },
    SubmissionFailed {
        conversation_id: String,
        submission_id: String,
        error: String,
    },
}
