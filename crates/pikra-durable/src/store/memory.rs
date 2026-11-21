use crate::error::DurableError;
use crate::store::DurableStore;
use crate::types::{
    CheckpointMutation, CommitBatch, CommitReceipt, CompletedToolRecord, ConversationRecord,
    DocumentRecord, LeaseClaimResult, SignalDeliveryResult, SignalRecord, SubmissionRecord,
    TaskCheckpoint,
};
use pikra_session::SessionEntry;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

#[derive(Default)]
struct ConversationBucket {
    record: Option<ConversationRecord>,
    entries: Vec<(u64, SessionEntry)>,
    submissions: HashMap<String, SubmissionRecord>,
    checkpoint: Option<TaskCheckpoint>,
    completed_tools: HashMap<(String, String), CompletedToolRecord>,
    signals: Vec<SignalRecord>,
    documents: BTreeMap<String, DocumentRecord>,
}

/// Thread-safe in-memory [`DurableStore`] with atomic all-or-nothing [`CommitBatch`] validation.
#[derive(Default)]
pub struct MemoryDurableStore {
    buckets: Mutex<HashMap<String, ConversationBucket>>,
}

impl MemoryDurableStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl DurableStore for MemoryDurableStore {
    fn create_conversation(
        &self,
        mut record: ConversationRecord,
        initial_entries: Vec<SessionEntry>,
    ) -> Result<CommitReceipt, DurableError> {
        let mut guard = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        if guard.contains_key(&record.conversation_id) {
            return Err(DurableError::ConversationAlreadyExists(
                record.conversation_id,
            ));
        }
        let conv_id = record.conversation_id.clone();
        let mut entries = Vec::with_capacity(initial_entries.len());
        let mut seq = 0u64;
        for entry in initial_entries {
            seq += 1;
            record.head_entry_id = Some(entry.id.clone());
            entries.push((seq, entry));
        }
        let head_entry_id = record.head_entry_id.clone();
        let checkpoint_version = record.checkpoint_version;
        let bucket = ConversationBucket {
            record: Some(record),
            entries,
            ..Default::default()
        };
        guard.insert(conv_id.clone(), bucket);
        Ok(CommitReceipt {
            conversation_id: conv_id,
            head_entry_id,
            last_seq: seq,
            checkpoint_version,
            document_revisions: Vec::new(),
        })
    }

    fn get_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Option<ConversationRecord>, DurableError> {
        let guard = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        Ok(guard.get(conversation_id).and_then(|b| b.record.clone()))
    }

    fn list_conversations(&self) -> Result<Vec<ConversationRecord>, DurableError> {
        let guard = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let mut out: Vec<ConversationRecord> =
            guard.values().filter_map(|b| b.record.clone()).collect();
        out.sort_by(|a, b| {
            a.created_at_ms
                .cmp(&b.created_at_ms)
                .then_with(|| a.conversation_id.cmp(&b.conversation_id))
        });
        Ok(out)
    }

    fn load_entries(&self, conversation_id: &str) -> Result<Vec<SessionEntry>, DurableError> {
        let guard = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let Some(bucket) = guard.get(conversation_id) else {
            return Err(DurableError::ConversationNotFound(
                conversation_id.to_string(),
            ));
        };
        Ok(bucket.entries.iter().map(|(_, e)| e.clone()).collect())
    }

    fn get_submission(
        &self,
        conversation_id: &str,
        submission_id: &str,
    ) -> Result<Option<SubmissionRecord>, DurableError> {
        let guard = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        Ok(guard
            .get(conversation_id)
            .and_then(|b| b.submissions.get(submission_id).cloned()))
    }

    fn get_checkpoint(
        &self,
        conversation_id: &str,
    ) -> Result<Option<TaskCheckpoint>, DurableError> {
        let guard = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        Ok(guard
            .get(conversation_id)
            .and_then(|b| b.checkpoint.clone()))
    }

    fn get_completed_tool(
        &self,
        conversation_id: &str,
        assistant_entry_id: &str,
        tool_call_id: &str,
    ) -> Result<Option<CompletedToolRecord>, DurableError> {
        let guard = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        Ok(guard.get(conversation_id).and_then(|b| {
            b.completed_tools
                .get(&(assistant_entry_id.to_string(), tool_call_id.to_string()))
                .cloned()
        }))
    }

    fn list_completed_tools(
        &self,
        conversation_id: &str,
        assistant_entry_id: &str,
    ) -> Result<Vec<CompletedToolRecord>, DurableError> {
        let guard = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let Some(bucket) = guard.get(conversation_id) else {
            return Ok(Vec::new());
        };
        let mut out: Vec<CompletedToolRecord> = bucket
            .completed_tools
            .values()
            .filter(|r| r.assistant_entry_id == assistant_entry_id)
            .cloned()
            .collect();
        out.sort_by(|a, b| {
            a.completed_at_ms
                .cmp(&b.completed_at_ms)
                .then_with(|| a.tool_call_id.cmp(&b.tool_call_id))
        });
        Ok(out)
    }

    fn deliver_signal(
        &self,
        conversation_id: &str,
        signal_key: &str,
        signal_id: &str,
        payload: Value,
        now_ms: u64,
    ) -> Result<SignalDeliveryResult, DurableError> {
        let mut guard = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let Some(bucket) = guard.get_mut(conversation_id) else {
            return Err(DurableError::ConversationNotFound(
                conversation_id.to_string(),
            ));
        };
        if let Some(existing) = bucket.signals.iter().find(|s| s.signal_id == signal_id) {
            return Ok(SignalDeliveryResult::Duplicate(existing.clone()));
        }
        let record = SignalRecord {
            conversation_id: conversation_id.to_string(),
            signal_id: signal_id.to_string(),
            signal_key: signal_key.to_string(),
            payload,
            received_at_ms: now_ms,
            consumed_at_ms: None,
        };
        bucket.signals.push(record.clone());
        Ok(SignalDeliveryResult::Delivered(record))
    }

    fn peek_next_unconsumed_signal(
        &self,
        conversation_id: &str,
        signal_key: &str,
        exclude_signal_ids: &[String],
        max_received_at_ms: Option<u64>,
    ) -> Result<Option<SignalRecord>, DurableError> {
        let guard = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        Ok(guard.get(conversation_id).and_then(|b| {
            find_matching_signal(b, signal_key, exclude_signal_ids, max_received_at_ms)
        }))
    }

    fn commit_or_peek_signal(
        &self,
        tx: CommitBatch,
        signal_key: &str,
        exclude_signal_ids: &[String],
        max_received_at_ms: Option<u64>,
    ) -> Result<Result<CommitReceipt, SignalRecord>, DurableError> {
        let mut guard = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(bucket) = guard.get(&tx.conversation_id) {
            validate_lease_and_version(&tx, bucket.record.as_ref())?;
            if let Some(sig) =
                find_matching_signal(bucket, signal_key, exclude_signal_ids, max_received_at_ms)
            {
                return Ok(Err(sig));
            }
        }
        commit_locked(&mut guard, tx).map(Ok)
    }

    fn list_signals(&self, conversation_id: &str) -> Result<Vec<SignalRecord>, DurableError> {
        let guard = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        Ok(guard
            .get(conversation_id)
            .map(|b| b.signals.clone())
            .unwrap_or_default())
    }

    fn get_document(
        &self,
        conversation_id: &str,
        key: &str,
    ) -> Result<Option<(u64, Value)>, DurableError> {
        let guard = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        Ok(guard
            .get(conversation_id)
            .and_then(|b| b.documents.get(key))
            .map(|d| (d.revision, d.value.clone())))
    }

    fn list_documents(&self, conversation_id: &str) -> Result<Vec<DocumentRecord>, DurableError> {
        let guard = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        Ok(guard
            .get(conversation_id)
            .map(|b| b.documents.values().cloned().collect())
            .unwrap_or_default())
    }

    fn try_claim_conversation(
        &self,
        conversation_id: &str,
        lease_owner: &str,
        now_ms: u64,
        lease_ttl_ms: u64,
    ) -> Result<LeaseClaimResult, DurableError> {
        let mut guard = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let Some(record) = guard
            .get_mut(conversation_id)
            .and_then(|b| b.record.as_mut())
        else {
            return Err(DurableError::ConversationNotFound(
                conversation_id.to_string(),
            ));
        };

        if let (Some(cur_owner), Some(cur_expires)) =
            (&record.lease_owner, record.lease_expires_at_ms)
            && cur_owner != lease_owner
            && cur_expires > now_ms
        {
            return Ok(LeaseClaimResult::HeldByOther {
                lease_owner: cur_owner.clone(),
                lease_expires_at_ms: cur_expires,
            });
        }

        if record.lease_owner.as_deref() != Some(lease_owner) {
            record.checkpoint_version = record.checkpoint_version.saturating_add(1);
        }
        let lease_expires_at_ms = now_ms.saturating_add(lease_ttl_ms);
        record.lease_owner = Some(lease_owner.to_string());
        record.lease_expires_at_ms = Some(lease_expires_at_ms);
        record.updated_at_ms = now_ms;

        Ok(LeaseClaimResult::Acquired {
            checkpoint_version: record.checkpoint_version,
            lease_expires_at_ms,
        })
    }

    fn release_conversation(
        &self,
        conversation_id: &str,
        lease_owner: &str,
        now_ms: u64,
    ) -> Result<bool, DurableError> {
        let mut guard = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let Some(record) = guard
            .get_mut(conversation_id)
            .and_then(|b| b.record.as_mut())
        else {
            return Ok(false);
        };
        if record.lease_owner.as_deref() == Some(lease_owner) {
            record.lease_owner = None;
            record.lease_expires_at_ms = None;
            record.updated_at_ms = now_ms;
            return Ok(true);
        }
        Ok(false)
    }

    fn commit(&self, tx: CommitBatch) -> Result<CommitReceipt, DurableError> {
        let mut guard = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        commit_locked(&mut guard, tx)
    }
}

fn validate_lease_and_version(
    tx: &CommitBatch,
    record: Option<&ConversationRecord>,
) -> Result<(), DurableError> {
    if let Some(expected_owner) = &tx.expected_lease_owner {
        let actual_owner = record.and_then(|r| r.lease_owner.as_deref());
        if actual_owner != Some(expected_owner.as_str()) {
            return Err(DurableError::Conflict {
                conversation_id: tx.conversation_id.clone(),
                key: "__lease_owner__".into(),
                expected: None,
                actual: None,
            });
        }
    }
    if let Some(expected_ver) = tx.expected_checkpoint_version {
        let actual_ver = record.map(|r| r.checkpoint_version).unwrap_or(0);
        if actual_ver != expected_ver {
            return Err(DurableError::Conflict {
                conversation_id: tx.conversation_id.clone(),
                key: "__checkpoint_version__".into(),
                expected: Some(expected_ver),
                actual: Some(actual_ver),
            });
        }
    }
    Ok(())
}

fn find_matching_signal(
    bucket: &ConversationBucket,
    signal_key: &str,
    exclude_signal_ids: &[String],
    max_received_at_ms: Option<u64>,
) -> Option<SignalRecord> {
    bucket
        .signals
        .iter()
        .find(|s| {
            s.signal_key == signal_key
                && s.consumed_at_ms.is_none()
                && max_received_at_ms.is_none_or(|deadline| s.received_at_ms <= deadline)
                && !exclude_signal_ids.iter().any(|id| id == &s.signal_id)
        })
        .cloned()
}

fn validate_batch_conversation_ids(tx: &CommitBatch) -> Result<(), DurableError> {
    if let Some(conv) = &tx.upsert_conversation
        && conv.conversation_id != tx.conversation_id
    {
        return Err(DurableError::InvalidState(format!(
            "CommitBatch conversation_id mismatch in upsert_conversation: expected {}, got {}",
            tx.conversation_id, conv.conversation_id
        )));
    }
    if let Some(sub) = &tx.upsert_submission
        && sub.conversation_id != tx.conversation_id
    {
        return Err(DurableError::InvalidState(format!(
            "CommitBatch conversation_id mismatch in upsert_submission: expected {}, got {}",
            tx.conversation_id, sub.conversation_id
        )));
    }
    for tool_rec in &tx.record_completed_tools {
        if tool_rec.conversation_id != tx.conversation_id {
            return Err(DurableError::InvalidState(format!(
                "CommitBatch conversation_id mismatch in record_completed_tools: expected {}, got {}",
                tx.conversation_id, tool_rec.conversation_id
            )));
        }
    }
    Ok(())
}

fn commit_locked(
    guard: &mut HashMap<String, ConversationBucket>,
    tx: CommitBatch,
) -> Result<CommitReceipt, DurableError> {
    validate_batch_conversation_ids(&tx)?;

    // Phase 1: Validate existence, checkpoint/head preconditions, and every optimistic CAS precondition BEFORE any mutation.
    let exists = guard.contains_key(&tx.conversation_id);
    if !exists && tx.upsert_conversation.is_none() {
        return Err(DurableError::ConversationNotFound(tx.conversation_id));
    }

    if let Some(bucket) = guard.get(&tx.conversation_id) {
        if tx.require_no_checkpoint
            && let Some(active_cp) = &bucket.checkpoint
        {
            return Err(DurableError::ConversationBusy {
                conversation_id: tx.conversation_id,
                active_submission_id: active_cp.submission_id().to_string(),
            });
        }
        if let Some(expected_head) = &tx.expected_head_entry_id {
            let actual_head = bucket.record.as_ref().and_then(|r| r.head_entry_id.clone());
            if &actual_head != expected_head {
                return Err(DurableError::Conflict {
                    conversation_id: tx.conversation_id,
                    key: "__head_entry_id__".into(),
                    expected: None,
                    actual: None,
                });
            }
        }
        validate_lease_and_version(&tx, bucket.record.as_ref())?;
    } else {
        validate_lease_and_version(&tx, None)?;
    }

    let mut staged_revisions: HashMap<String, Option<u64>> = HashMap::new();
    let mut planned_docs: Vec<(String, u64, Value)> = Vec::with_capacity(tx.document_writes.len());
    for write in &tx.document_writes {
        let current_rev = if let Some(staged) = staged_revisions.get(&write.key) {
            *staged
        } else {
            guard
                .get(&tx.conversation_id)
                .and_then(|b| b.documents.get(&write.key))
                .map(|d| d.revision)
        };
        if current_rev != write.expected_revision {
            return Err(DurableError::Conflict {
                conversation_id: tx.conversation_id,
                key: write.key.clone(),
                expected: write.expected_revision,
                actual: current_rev,
            });
        }
        let next_rev = current_rev.unwrap_or(0) + 1;
        staged_revisions.insert(write.key.clone(), Some(next_rev));
        planned_docs.push((write.key.clone(), next_rev, write.value.clone()));
    }

    // Phase 2: Apply all mutations atomically while holding the lock.
    let advances_version = tx.advances_checkpoint_version();
    let bucket = guard.entry(tx.conversation_id.clone()).or_default();
    if let Some(conv) = tx.upsert_conversation {
        bucket.record = Some(conv);
    }
    let record = bucket
        .record
        .as_mut()
        .ok_or_else(|| DurableError::ConversationNotFound(tx.conversation_id.clone()))?;

    if let Some(status) = tx.update_status {
        record.status = status;
    }
    if let Some(model) = tx.update_model {
        record.active_model = model;
    }

    let mut last_seq = bucket.entries.last().map(|(s, _)| *s).unwrap_or(0);
    for entry in tx.append_entries {
        last_seq += 1;
        record.head_entry_id = Some(entry.id.clone());
        bucket.entries.push((last_seq, entry));
    }
    if advances_version {
        record.checkpoint_version = record.checkpoint_version.saturating_add(1);
    }
    if tx.release_lease {
        record.lease_owner = None;
        record.lease_expires_at_ms = None;
    } else if let Some(expires_at) = tx.renew_lease_expires_at_ms {
        record.lease_expires_at_ms = Some(expires_at);
    }
    record.updated_at_ms = tx.now_ms;
    let head_entry_id = record.head_entry_id.clone();
    let checkpoint_version = record.checkpoint_version;

    if let Some(sub) = tx.upsert_submission {
        bucket.submissions.insert(sub.submission_id.clone(), sub);
    }

    match tx.checkpoint {
        CheckpointMutation::Unchanged => {}
        CheckpointMutation::Set(cp) => bucket.checkpoint = Some(cp),
        CheckpointMutation::Clear => bucket.checkpoint = None,
    }

    for tool_rec in tx.record_completed_tools {
        bucket
            .completed_tools
            .entry((
                tool_rec.assistant_entry_id.clone(),
                tool_rec.tool_call_id.clone(),
            ))
            .or_insert(tool_rec);
    }

    for sig_id in tx.consume_signal_ids {
        if let Some(sig) = bucket.signals.iter_mut().find(|s| s.signal_id == sig_id)
            && sig.consumed_at_ms.is_none()
        {
            sig.consumed_at_ms = Some(tx.now_ms);
        }
    }

    let mut document_revisions = Vec::with_capacity(planned_docs.len());
    for (key, revision, value) in planned_docs {
        bucket.documents.insert(
            key.clone(),
            DocumentRecord {
                conversation_id: tx.conversation_id.clone(),
                key: key.clone(),
                revision,
                value,
                updated_at_ms: tx.now_ms,
            },
        );
        document_revisions.push((key, revision));
    }

    Ok(CommitReceipt {
        conversation_id: tx.conversation_id,
        head_entry_id,
        last_seq,
        checkpoint_version,
        document_revisions,
    })
}
