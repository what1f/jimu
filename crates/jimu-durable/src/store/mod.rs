mod memory;
mod sqlite;

pub use memory::MemoryDurableStore;
pub use sqlite::SqliteDurableStore;

use crate::error::DurableError;
use crate::types::{
    CommitBatch, CommitReceipt, CompletedToolRecord, ConversationRecord, DocumentRecord,
    DocumentWrite, LeaseClaimResult, SignalDeliveryResult, SignalRecord, SubmissionRecord,
    TaskCheckpoint,
};
use jimu_session::SessionEntry;
use serde_json::Value;

/// Transactional persistence backend for [`crate::DurableEngine`].
///
/// Every [`DurableStore::commit`] executes atomically so process crashes or CAS conflicts
/// never leave partial state behind.
pub trait DurableStore: Send + Sync {
    /// Atomically create a new conversation and its initial transcript entries (e.g. `SystemBaseline`).
    fn create_conversation(
        &self,
        record: ConversationRecord,
        initial_entries: Vec<SessionEntry>,
    ) -> Result<CommitReceipt, DurableError>;

    /// Fetch a conversation metadata record by ID.
    fn get_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Option<ConversationRecord>, DurableError>;

    /// List all persisted conversations ordered by creation time.
    fn list_conversations(&self) -> Result<Vec<ConversationRecord>, DurableError>;

    /// Load all append-only transcript [`SessionEntry`] rows in sequence order.
    fn load_entries(&self, conversation_id: &str) -> Result<Vec<SessionEntry>, DurableError>;

    /// Fetch an idempotent submission record by `(conversation_id, submission_id)`.
    fn get_submission(
        &self,
        conversation_id: &str,
        submission_id: &str,
    ) -> Result<Option<SubmissionRecord>, DurableError>;

    /// Fetch the active execution phase checkpoint for a conversation, if any.
    fn get_checkpoint(&self, conversation_id: &str)
    -> Result<Option<TaskCheckpoint>, DurableError>;

    /// Fetch a single committed tool side-effect record by `(conversation_id, assistant_entry_id, tool_call_id)`.
    fn get_completed_tool(
        &self,
        conversation_id: &str,
        assistant_entry_id: &str,
        tool_call_id: &str,
    ) -> Result<Option<CompletedToolRecord>, DurableError>;

    /// List all committed tool side-effect records for a given assistant turn entry.
    fn list_completed_tools(
        &self,
        conversation_id: &str,
        assistant_entry_id: &str,
    ) -> Result<Vec<CompletedToolRecord>, DurableError>;

    /// Idempotently deliver an external signal keyed by `(conversation_id, signal_id)`.
    ///
    /// If the signal arrives before a tool parks on `signal_key`, it is buffered as unconsumed
    /// so `AwaitSignal` consumes it immediately without race conditions.
    fn deliver_signal(
        &self,
        conversation_id: &str,
        signal_key: &str,
        signal_id: &str,
        payload: Value,
        now_ms: u64,
    ) -> Result<SignalDeliveryResult, DurableError>;

    /// Peek the oldest unconsumed signal matching `(conversation_id, signal_key)`.
    fn peek_unconsumed_signal(
        &self,
        conversation_id: &str,
        signal_key: &str,
    ) -> Result<Option<SignalRecord>, DurableError> {
        self.peek_next_unconsumed_signal(conversation_id, signal_key, &[], None)
    }

    /// Peek the oldest unconsumed signal matching `(conversation_id, signal_key)`, excluding
    /// `exclude_signal_ids` and optionally filtering `received_at_ms <= max_received_at_ms`.
    fn peek_next_unconsumed_signal(
        &self,
        conversation_id: &str,
        signal_key: &str,
        exclude_signal_ids: &[String],
        max_received_at_ms: Option<u64>,
    ) -> Result<Option<SignalRecord>, DurableError>;

    /// Atomically either return an unconsumed matching signal (if one already arrived within
    /// `max_received_at_ms`) OR commit `tx` (typically setting `Parked(AwaitSignal)`).
    fn commit_or_peek_signal(
        &self,
        tx: CommitBatch,
        signal_key: &str,
        exclude_signal_ids: &[String],
        max_received_at_ms: Option<u64>,
    ) -> Result<Result<CommitReceipt, SignalRecord>, DurableError>;

    /// List all signals (buffered and consumed) for a conversation in arrival order.
    fn list_signals(&self, conversation_id: &str) -> Result<Vec<SignalRecord>, DurableError>;

    /// Read a named JSON state document as `(revision, value)`.
    fn get_document(
        &self,
        conversation_id: &str,
        key: &str,
    ) -> Result<Option<(u64, Value)>, DurableError>;

    /// List all named documents for a conversation ordered by key.
    fn list_documents(&self, conversation_id: &str) -> Result<Vec<DocumentRecord>, DurableError>;

    /// Optimistic compare-and-swap update of a named state document.
    ///
    /// Returns the new revision on success or [`DurableError::Conflict`] on stale `expected_revision`.
    fn put_document(
        &self,
        conversation_id: &str,
        key: &str,
        expected_revision: Option<u64>,
        value: Value,
        now_ms: u64,
    ) -> Result<u64, DurableError> {
        let mut batch = CommitBatch::new(conversation_id, now_ms);
        batch.document_writes.push(DocumentWrite {
            key: key.to_string(),
            expected_revision,
            value,
        });
        let receipt = self.commit(batch)?;
        receipt
            .document_revisions
            .into_iter()
            .next()
            .map(|(_, rev)| rev)
            .ok_or_else(|| DurableError::Storage("document write returned no revision".into()))
    }

    /// Atomically claim or renew an exclusive execution lease on a conversation.
    ///
    /// Succeeds when the conversation has no lease, has an expired lease
    /// (`lease_expires_at_ms <= now_ms`), or is already leased by `lease_owner`.
    /// Acquiring a lease from an unheld or different owner increments `checkpoint_version`
    /// so any stale owner's in-flight commits are fenced out.
    fn try_claim_conversation(
        &self,
        conversation_id: &str,
        lease_owner: &str,
        now_ms: u64,
        lease_ttl_ms: u64,
    ) -> Result<LeaseClaimResult, DurableError>;

    /// Atomically release an execution lease if currently held by `lease_owner`.
    fn release_conversation(
        &self,
        conversation_id: &str,
        lease_owner: &str,
        now_ms: u64,
    ) -> Result<bool, DurableError>;

    /// Execute an atomic single-transaction batch commit.
    fn commit(&self, tx: CommitBatch) -> Result<CommitReceipt, DurableError>;
}
