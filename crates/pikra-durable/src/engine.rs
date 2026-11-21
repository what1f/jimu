use crate::clock::{Clock, RetryPolicy, SystemClock};
use crate::error::DurableError;
use crate::session_bridge::open_session_view;
use crate::store::DurableStore;
use crate::tool::{
    AgentToolAdapter, DurableTool, DurableToolContext, DurableToolOutcome, SignalResolution,
};
use crate::types::{
    CheckpointMutation, CommitBatch, CommitReceipt, CompletedToolRecord, ConversationRecord,
    ConversationStatus, DurableEvent, LeaseClaimResult, ParkState, ParkedToolCall, RunOutcome,
    SignalDeliveryResult, SignalRecord, SubmissionRecord, SubmissionState, TaskCheckpoint,
};
use futures_util::StreamExt;
use futures_util::lock::Mutex as AsyncMutex;
use pikra_agent::{AgentMessage, AgentTool, AgentToolResult};
use pikra_ai::{
    AssistantEventStream, AssistantMessage, AssistantMessageEvent, CacheRetention, CancelToken,
    Message, Model, SimpleStreamOptions, StopReason, StreamProvider, ThinkingLevel, ToolCall,
    ToolDefinition, ToolResultMessage, Usage, UserMessage,
};
use pikra_schema::CompiledSchema;
use pikra_session::{
    CompactionConfig, CompactionResult, CompactionStrategy, ReconstructedContext, SessionEntry,
    SessionEntryPayload, SessionManager,
};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

type DurableEventListener = Arc<dyn Fn(&DurableEvent) + Send + Sync>;

/// Identifier returned by [`DurableEngine::subscribe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DurableSubscriptionId(u64);

/// Default execution lease duration (60 seconds).
pub const DEFAULT_LEASE_TTL_MS: u64 = 60_000;

/// Configuration knobs for [`DurableEngine`].
#[derive(Clone)]
pub struct DurableEngineConfig {
    pub retry_policy: RetryPolicy,
    pub compaction: CompactionConfig,
    pub compaction_strategy: Option<Arc<dyn CompactionStrategy>>,
    pub max_turns_per_submission: u32,
    pub lease_ttl_ms: u64,
    pub api_key: Option<String>,
    pub thinking_level: ThinkingLevel,
    pub fast: bool,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    pub cache_retention: CacheRetention,
    pub headers: Vec<(String, Option<String>)>,
    pub sampling_params: Map<String, Value>,
}

impl Default for DurableEngineConfig {
    fn default() -> Self {
        Self {
            retry_policy: RetryPolicy::default(),
            compaction: CompactionConfig::default(),
            compaction_strategy: None,
            max_turns_per_submission: 64,
            lease_ttl_ms: DEFAULT_LEASE_TTL_MS,
            api_key: None,
            thinking_level: ThinkingLevel::Off,
            fast: false,
            temperature: None,
            max_tokens: None,
            cache_retention: CacheRetention::default(),
            headers: Vec::new(),
            sampling_params: Map::new(),
        }
    }
}

/// Embedded single-node durable execution engine backed by [`DurableStore`].
pub struct DurableEngine {
    engine_id: String,
    store: Arc<dyn DurableStore>,
    provider: Arc<dyn StreamProvider>,
    clock: Arc<dyn Clock>,
    tools: Vec<Arc<dyn DurableTool>>,
    config: DurableEngineConfig,
    listeners: Mutex<Vec<(DurableSubscriptionId, DurableEventListener)>>,
    next_sub_id: AtomicU64,
    conv_locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

enum ToolStepResolution {
    Completed(CompletedToolRecord),
    Parked(ParkState),
}

struct ConversationLeaseGuard {
    store: Arc<dyn DurableStore>,
    clock: Arc<dyn Clock>,
    conversation_id: String,
    lease_owner: String,
    lease_ttl_ms: u64,
    checkpoint_version: u64,
    lease_expires_at_ms: u64,
    released: bool,
}

impl ConversationLeaseGuard {
    fn try_acquire(
        store: Arc<dyn DurableStore>,
        clock: Arc<dyn Clock>,
        conversation_id: &str,
        lease_owner: &str,
        lease_ttl_ms: u64,
    ) -> Result<Result<Self, (String, u64)>, DurableError> {
        let ttl = lease_ttl_ms.max(1);
        let now = clock.now_ms();
        match store.try_claim_conversation(conversation_id, lease_owner, now, ttl)? {
            LeaseClaimResult::Acquired {
                checkpoint_version,
                lease_expires_at_ms,
            } => Ok(Ok(Self {
                store,
                clock,
                conversation_id: conversation_id.to_string(),
                lease_owner: lease_owner.to_string(),
                lease_ttl_ms: ttl,
                checkpoint_version,
                lease_expires_at_ms,
                released: false,
            })),
            LeaseClaimResult::HeldByOther {
                lease_owner,
                lease_expires_at_ms,
            } => Ok(Err((lease_owner, lease_expires_at_ms))),
        }
    }

    fn renew(&mut self) -> Result<(), DurableError> {
        let now = self.clock.now_ms();
        match self.store.try_claim_conversation(
            &self.conversation_id,
            &self.lease_owner,
            now,
            self.lease_ttl_ms,
        )? {
            LeaseClaimResult::Acquired {
                checkpoint_version,
                lease_expires_at_ms,
            } => {
                self.checkpoint_version = checkpoint_version;
                self.lease_expires_at_ms = lease_expires_at_ms;
                Ok(())
            }
            LeaseClaimResult::HeldByOther {
                lease_owner,
                lease_expires_at_ms,
            } => {
                self.released = true;
                Err(DurableError::Busy {
                    conversation_id: self.conversation_id.clone(),
                    lease_owner,
                    lease_expires_at_ms,
                })
            }
        }
    }

    fn maybe_renew(&mut self) -> Result<(), DurableError> {
        let now = self.clock.now_ms();
        let half_ttl = (self.lease_ttl_ms / 2).max(1);
        if now.saturating_add(half_ttl) >= self.lease_expires_at_ms {
            self.renew()?;
        }
        Ok(())
    }

    fn prepare_batch(&self, batch: &mut CommitBatch, release_lease: bool) {
        batch.expected_lease_owner = Some(self.lease_owner.clone());
        batch.expected_checkpoint_version = Some(self.checkpoint_version);
        if release_lease {
            batch.release_lease = true;
            batch.renew_lease_expires_at_ms = None;
        } else {
            batch.release_lease = false;
            batch.renew_lease_expires_at_ms = Some(batch.now_ms.saturating_add(self.lease_ttl_ms));
        }
    }

    fn commit(
        &mut self,
        mut batch: CommitBatch,
        release_lease: bool,
    ) -> Result<CommitReceipt, DurableError> {
        self.prepare_batch(&mut batch, release_lease);
        let next_expires = batch.renew_lease_expires_at_ms;
        let receipt = self.store.commit(batch)?;
        self.checkpoint_version = receipt.checkpoint_version;
        if release_lease {
            self.released = true;
        } else if let Some(expires_at) = next_expires {
            self.lease_expires_at_ms = expires_at;
        }
        Ok(receipt)
    }

    fn commit_or_peek_signal(
        &mut self,
        mut batch: CommitBatch,
        signal_key: &str,
        exclude_signal_ids: &[String],
        max_received_at_ms: Option<u64>,
        release_lease: bool,
    ) -> Result<Result<CommitReceipt, SignalRecord>, DurableError> {
        self.prepare_batch(&mut batch, release_lease);
        let next_expires = batch.renew_lease_expires_at_ms;
        match self.store.commit_or_peek_signal(
            batch,
            signal_key,
            exclude_signal_ids,
            max_received_at_ms,
        )? {
            Ok(receipt) => {
                self.checkpoint_version = receipt.checkpoint_version;
                if release_lease {
                    self.released = true;
                } else if let Some(expires_at) = next_expires {
                    self.lease_expires_at_ms = expires_at;
                }
                Ok(Ok(receipt))
            }
            Err(sig) => Ok(Err(sig)),
        }
    }
}

impl Drop for ConversationLeaseGuard {
    fn drop(&mut self) {
        if !self.released {
            let _ = self.store.release_conversation(
                &self.conversation_id,
                &self.lease_owner,
                self.clock.now_ms(),
            );
        }
    }
}

impl DurableEngine {
    pub fn new(store: Arc<dyn DurableStore>, provider: Arc<dyn StreamProvider>) -> Self {
        Self {
            engine_id: Uuid::now_v7().to_string(),
            store,
            provider,
            clock: Arc::new(SystemClock),
            tools: Vec::new(),
            config: DurableEngineConfig::default(),
            listeners: Mutex::new(Vec::new()),
            next_sub_id: AtomicU64::new(1),
            conv_locks: Mutex::new(HashMap::new()),
        }
    }

    pub fn engine_id(&self) -> &str {
        &self.engine_id
    }

    pub fn with_lease_ttl_ms(mut self, lease_ttl_ms: u64) -> Self {
        self.config.lease_ttl_ms = lease_ttl_ms.max(1);
        self
    }

    fn try_acquire_lease(
        &self,
        conversation_id: &str,
    ) -> Result<Result<ConversationLeaseGuard, (String, u64)>, DurableError> {
        ConversationLeaseGuard::try_acquire(
            Arc::clone(&self.store),
            Arc::clone(&self.clock),
            conversation_id,
            &self.engine_id,
            self.config.lease_ttl_ms,
        )
    }

    fn conversation_mutex(&self, conversation_id: &str) -> Arc<AsyncMutex<()>> {
        let mut map = self.conv_locks.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(
            map.entry(conversation_id.to_string())
                .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
        )
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_config(mut self, config: DurableEngineConfig) -> Self {
        self.config = config;
        self
    }

    pub fn with_retry_policy(mut self, retry_policy: RetryPolicy) -> Self {
        self.config.retry_policy = retry_policy;
        self
    }

    pub fn with_compaction(mut self, compaction: CompactionConfig) -> Self {
        self.config.compaction = compaction;
        self
    }

    pub fn with_tool(mut self, tool: impl DurableTool + 'static) -> Self {
        self.tools.push(Arc::new(tool));
        self
    }

    pub fn with_tool_arc(mut self, tool: Arc<dyn DurableTool>) -> Self {
        self.tools.push(tool);
        self
    }

    pub fn with_tools(mut self, tools: Vec<Arc<dyn DurableTool>>) -> Self {
        self.tools.extend(tools);
        self
    }

    pub fn with_agent_tool(mut self, tool: impl AgentTool + 'static) -> Self {
        self.tools.push(Arc::new(AgentToolAdapter::new(tool)));
        self
    }

    pub fn store(&self) -> &Arc<dyn DurableStore> {
        &self.store
    }

    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    pub fn subscribe(
        &self,
        listener: impl Fn(&DurableEvent) + Send + Sync + 'static,
    ) -> DurableSubscriptionId {
        let id = DurableSubscriptionId(self.next_sub_id.fetch_add(1, Ordering::Relaxed));
        let mut guard = self.listeners.lock().unwrap_or_else(|e| e.into_inner());
        guard.push((id, Arc::new(listener)));
        id
    }

    pub fn unsubscribe(&self, id: DurableSubscriptionId) -> bool {
        let mut guard = self.listeners.lock().unwrap_or_else(|e| e.into_inner());
        let before = guard.len();
        guard.retain(|(sub_id, _)| *sub_id != id);
        guard.len() != before
    }

    fn emit(&self, event: DurableEvent) {
        let snapshot: Vec<DurableEventListener> = {
            let guard = self.listeners.lock().unwrap_or_else(|e| e.into_inner());
            guard.iter().map(|(_, l)| Arc::clone(l)).collect()
        };
        for listener in snapshot {
            listener(&event);
        }
    }

    fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.tools.iter().map(|t| t.definition()).collect()
    }

    fn find_tool(&self, name: &str) -> Option<Arc<dyn DurableTool>> {
        self.tools
            .iter()
            .find(|t| t.definition().name == name)
            .cloned()
    }

    /// Create a new durable conversation initialized with a locked `SystemBaseline` entry.
    pub fn create_conversation(
        &self,
        conversation_id: impl Into<String>,
        model: Model,
        system_prompt: impl Into<String>,
    ) -> Result<ConversationRecord, DurableError> {
        let now = self.clock.now_ms();
        let conversation_id = conversation_id.into();
        let system_prompt = system_prompt.into();
        let baseline = SessionEntry {
            id: Uuid::now_v7().to_string(),
            parent_id: None,
            timestamp: now,
            payload: SessionEntryPayload::SystemBaseline {
                system_prompt: system_prompt.clone(),
                tools: self.tool_definitions(),
            },
        };
        let mut record = ConversationRecord {
            conversation_id,
            session_id: Uuid::now_v7().to_string(),
            head_entry_id: Some(baseline.id.clone()),
            status: ConversationStatus::Idle,
            active_model: model,
            system_prompt,
            checkpoint_version: 0,
            lease_owner: None,
            lease_expires_at_ms: None,
            created_at_ms: now,
            updated_at_ms: now,
        };
        let receipt = self
            .store
            .create_conversation(record.clone(), vec![baseline])?;
        record.head_entry_id = receipt.head_entry_id;
        Ok(record)
    }

    /// Reconstruct the current LLM-visible context for a conversation using `pikra_session`.
    pub fn reconstruct_context(
        &self,
        conversation_id: &str,
    ) -> Result<ReconstructedContext, DurableError> {
        let conv = self.require_conversation(conversation_id)?;
        let entries = self.store.load_entries(conversation_id)?;
        let manager = open_session_view(
            &conv,
            entries,
            self.tool_definitions(),
            self.config.compaction.clone(),
            self.config.compaction_strategy.clone(),
        )?;
        Ok(manager.reconstruct_context())
    }

    /// Idempotently deliver an external signal to a conversation.
    pub fn deliver_signal(
        &self,
        conversation_id: &str,
        signal_key: &str,
        signal_id: &str,
        payload: Value,
    ) -> Result<SignalDeliveryResult, DurableError> {
        self.store.deliver_signal(
            conversation_id,
            signal_key,
            signal_id,
            payload,
            self.clock.now_ms(),
        )
    }

    /// Read a named JSON state document as `(revision, value)`.
    pub fn get_document(
        &self,
        conversation_id: &str,
        key: &str,
    ) -> Result<Option<(u64, Value)>, DurableError> {
        self.store.get_document(conversation_id, key)
    }

    /// Optimistic compare-and-swap write of a named JSON state document.
    pub fn put_document(
        &self,
        conversation_id: &str,
        key: &str,
        expected_revision: Option<u64>,
        value: Value,
    ) -> Result<u64, DurableError> {
        self.store.put_document(
            conversation_id,
            key,
            expected_revision,
            value,
            self.clock.now_ms(),
        )
    }

    /// Trigger prefix-preserving compaction explicitly on an idle conversation.
    pub async fn compact(
        &self,
        conversation_id: &str,
    ) -> Result<Option<CompactionResult>, DurableError> {
        let conv_mutex = self.conversation_mutex(conversation_id);
        let _conv_guard = conv_mutex.lock().await;

        let _ = self.require_conversation(conversation_id)?;
        let mut lease = match self.try_acquire_lease(conversation_id)? {
            Ok(lease) => lease,
            Err(_) => return Ok(None),
        };
        let conv = self.require_conversation(conversation_id)?;
        if self.store.get_checkpoint(conversation_id)?.is_some() {
            return Ok(None);
        }
        let entries = self.store.load_entries(conversation_id)?;
        let manager = open_session_view(
            &conv,
            entries,
            self.tool_definitions(),
            self.config.compaction.clone(),
            self.config.compaction_strategy.clone(),
        )?;
        let opts = self.stream_options(&conv, CancelToken::new());
        let Some(prepared) = manager.prepare_compaction(&conv.active_model, opts) else {
            return Ok(None);
        };
        let tokens_before = prepared.input.tokens_before;
        let expected_leaf_id = prepared.expected_leaf_id.clone();
        let output = SessionManager::execute_prepared_compaction(&*self.provider, prepared).await?;

        let now = self.clock.now_ms();
        let entry = SessionEntry {
            id: Uuid::now_v7().to_string(),
            parent_id: expected_leaf_id.clone(),
            timestamp: now,
            payload: SessionEntryPayload::Compaction {
                summary: output.summary.clone(),
                first_kept_entry_id: output.first_kept_entry_id.clone(),
                tokens_before,
                usage: output.usage.clone(),
            },
        };
        let mut batch = CommitBatch::new(conversation_id, now);
        batch.expected_head_entry_id = Some(expected_leaf_id);
        batch.require_no_checkpoint = true;
        batch.append_entries.push(entry.clone());
        match lease.commit(batch, true) {
            Ok(_) => {}
            Err(DurableError::Conflict { .. } | DurableError::ConversationBusy { .. }) => {
                return Ok(None);
            }
            Err(err) => return Err(err),
        }

        self.emit(DurableEvent::Compacted {
            conversation_id: conversation_id.to_string(),
            entry_id: entry.id.clone(),
            tokens_before,
            first_kept_entry_id: output.first_kept_entry_id.clone(),
        });

        Ok(Some(CompactionResult {
            entry_id: entry.id,
            summary: output.summary,
            first_kept_entry_id: output.first_kept_entry_id,
            tokens_before,
            usage: output.usage,
        }))
    }

    /// Submit a user prompt idempotently under `submission_id` and drive the durable turn loop.
    pub async fn submit(
        &self,
        conversation_id: &str,
        submission_id: &str,
        prompt: impl Into<String>,
    ) -> Result<RunOutcome, DurableError> {
        self.submit_with_cancel(conversation_id, submission_id, prompt, CancelToken::new())
            .await
    }

    /// Submit a user prompt idempotently with a cooperative [`CancelToken`].
    pub async fn submit_with_cancel(
        &self,
        conversation_id: &str,
        submission_id: &str,
        prompt: impl Into<String>,
        cancel: CancelToken,
    ) -> Result<RunOutcome, DurableError> {
        let conv_mutex = self.conversation_mutex(conversation_id);
        let _conv_guard = conv_mutex.lock().await;

        let _ = self.require_conversation(conversation_id)?;

        // Fast idempotency check for already-settled submissions (no lease required).
        if let Some(existing) = self.store.get_submission(conversation_id, submission_id)?
            && let Some(settled) = settled_submission_outcome(existing)
        {
            return Ok(settled);
        }

        let mut lease = match self.try_acquire_lease(conversation_id)? {
            Ok(lease) => lease,
            Err((lease_owner, lease_expires_at_ms)) => {
                return Err(DurableError::Busy {
                    conversation_id: conversation_id.to_string(),
                    lease_owner,
                    lease_expires_at_ms,
                });
            }
        };

        // Re-check submission under the exclusive store lease to prevent TOCTOU races.
        if let Some(existing) = self.store.get_submission(conversation_id, submission_id)? {
            if let Some(settled) = settled_submission_outcome(existing) {
                return Ok(settled);
            }
            return self
                .drive_from_checkpoint(conversation_id, cancel, &mut lease)
                .await;
        }

        if let Some(active_cp) = self.store.get_checkpoint(conversation_id)? {
            return Err(DurableError::ConversationBusy {
                conversation_id: conversation_id.to_string(),
                active_submission_id: active_cp.submission_id().to_string(),
            });
        }

        let conv = self.require_conversation(conversation_id)?;
        let now = self.clock.now_ms();
        let user_entry = SessionEntry {
            id: Uuid::now_v7().to_string(),
            parent_id: conv.head_entry_id.clone(),
            timestamp: now,
            payload: SessionEntryPayload::Message {
                message: AgentMessage::Llm(Message::User(UserMessage::text(prompt, now))),
            },
        };
        let submission = SubmissionRecord {
            conversation_id: conversation_id.to_string(),
            submission_id: submission_id.to_string(),
            user_entry_id: Some(user_entry.id.clone()),
            state: SubmissionState::Running,
            created_at_ms: now,
            updated_at_ms: now,
        };

        let mut batch = CommitBatch::new(conversation_id, now);
        batch.expected_head_entry_id = Some(conv.head_entry_id);
        batch.require_no_checkpoint = true;
        batch.update_status = Some(ConversationStatus::Running);
        batch.append_entries.push(user_entry);
        batch.upsert_submission = Some(submission);
        batch.checkpoint = CheckpointMutation::Set(TaskCheckpoint::Generating {
            submission_id: submission_id.to_string(),
            turn_index: 0,
            attempt: 0,
        });
        lease.commit(batch, false)?;

        self.emit(DurableEvent::SubmissionStarted {
            conversation_id: conversation_id.to_string(),
            submission_id: submission_id.to_string(),
        });

        self.drive_from_checkpoint(conversation_id, cancel, &mut lease)
            .await
    }

    /// Reopen and recover an interrupted or parked conversation from its persisted checkpoint.
    pub async fn recover(&self, conversation_id: &str) -> Result<RunOutcome, DurableError> {
        self.recover_with_cancel(conversation_id, CancelToken::new())
            .await
    }

    /// Reopen and recover a conversation with a cooperative [`CancelToken`].
    pub async fn recover_with_cancel(
        &self,
        conversation_id: &str,
        cancel: CancelToken,
    ) -> Result<RunOutcome, DurableError> {
        let conv_mutex = self.conversation_mutex(conversation_id);
        let _conv_guard = conv_mutex.lock().await;
        self.recover_locked(conversation_id, cancel).await
    }

    async fn recover_locked(
        &self,
        conversation_id: &str,
        cancel: CancelToken,
    ) -> Result<RunOutcome, DurableError> {
        let _ = self.require_conversation(conversation_id)?;
        if self.store.get_checkpoint(conversation_id)?.is_none() {
            return Ok(RunOutcome::Idle);
        }
        let mut lease = match self.try_acquire_lease(conversation_id)? {
            Ok(lease) => lease,
            // Another active engine holds a live lease on this conversation; skip instead of double-executing.
            Err(_) => return Ok(RunOutcome::Idle),
        };
        if self.store.get_checkpoint(conversation_id)?.is_none() {
            return Ok(RunOutcome::Idle);
        }
        self.drive_from_checkpoint(conversation_id, cancel, &mut lease)
            .await
    }

    // ---- Core Durable State Machine ----

    async fn drive_from_checkpoint(
        &self,
        conversation_id: &str,
        cancel: CancelToken,
        lease: &mut ConversationLeaseGuard,
    ) -> Result<RunOutcome, DurableError> {
        loop {
            lease.renew()?;
            let Some(checkpoint) = self.store.get_checkpoint(conversation_id)? else {
                return Ok(RunOutcome::Idle);
            };

            match checkpoint {
                TaskCheckpoint::Compacting {
                    submission_id,
                    turn_index,
                    expected_leaf_id,
                } => {
                    if let Some(outcome) = self
                        .step_compacting(
                            conversation_id,
                            &submission_id,
                            turn_index,
                            expected_leaf_id,
                            &cancel,
                            lease,
                        )
                        .await?
                    {
                        return Ok(outcome);
                    }
                }
                TaskCheckpoint::Generating {
                    submission_id,
                    turn_index,
                    attempt,
                } => {
                    if let Some(outcome) = self
                        .step_generating(
                            conversation_id,
                            &submission_id,
                            turn_index,
                            attempt,
                            &cancel,
                            lease,
                        )
                        .await?
                    {
                        return Ok(outcome);
                    }
                }
                TaskCheckpoint::ExecutingTools {
                    submission_id,
                    turn_index,
                    assistant_entry_id,
                    tool_calls,
                } => {
                    if let Some(outcome) = self
                        .step_executing_tools(
                            conversation_id,
                            &submission_id,
                            turn_index,
                            &assistant_entry_id,
                            &tool_calls,
                            &cancel,
                            lease,
                        )
                        .await?
                    {
                        return Ok(outcome);
                    }
                }
                TaskCheckpoint::Parked {
                    submission_id,
                    turn_index,
                    park,
                } => {
                    if let Some(outcome) = self
                        .step_parked(
                            conversation_id,
                            &submission_id,
                            turn_index,
                            park,
                            &cancel,
                            lease,
                        )
                        .await?
                    {
                        return Ok(outcome);
                    }
                }
            }
        }
    }

    async fn step_compacting(
        &self,
        conversation_id: &str,
        submission_id: &str,
        turn_index: u32,
        expected_leaf_id: Option<String>,
        cancel: &CancelToken,
        lease: &mut ConversationLeaseGuard,
    ) -> Result<Option<RunOutcome>, DurableError> {
        if cancel.is_cancelled() {
            return Ok(Some(self.commit_cancelled(
                conversation_id,
                submission_id,
                lease,
            )?));
        }

        let conv = self.require_conversation(conversation_id)?;
        let entries = self.store.load_entries(conversation_id)?;
        let manager = open_session_view(
            &conv,
            entries,
            self.tool_definitions(),
            self.config.compaction.clone(),
            self.config.compaction_strategy.clone(),
        )?;

        if conv.head_entry_id == expected_leaf_id {
            let opts = self.stream_options(&conv, cancel.clone());
            if let Some(prepared) = manager.prepare_compaction(&conv.active_model, opts) {
                let tokens_before = prepared.input.tokens_before;
                let output =
                    match SessionManager::execute_prepared_compaction(&*self.provider, prepared)
                        .await
                    {
                        Ok(out) => out,
                        Err(_) if cancel.is_cancelled() => {
                            return Ok(Some(self.commit_cancelled(
                                conversation_id,
                                submission_id,
                                lease,
                            )?));
                        }
                        Err(err) => return Err(err.into()),
                    };
                if cancel.is_cancelled() {
                    return Ok(Some(self.commit_cancelled(
                        conversation_id,
                        submission_id,
                        lease,
                    )?));
                }

                let now = self.clock.now_ms();
                let comp_entry = SessionEntry {
                    id: Uuid::now_v7().to_string(),
                    parent_id: conv.head_entry_id.clone(),
                    timestamp: now,
                    payload: SessionEntryPayload::Compaction {
                        summary: output.summary,
                        first_kept_entry_id: output.first_kept_entry_id.clone(),
                        tokens_before,
                        usage: output.usage,
                    },
                };
                let mut batch = CommitBatch::new(conversation_id, now);
                batch.expected_head_entry_id = Some(conv.head_entry_id);
                batch.append_entries.push(comp_entry.clone());
                batch.checkpoint = CheckpointMutation::Set(TaskCheckpoint::Generating {
                    submission_id: submission_id.to_string(),
                    turn_index,
                    attempt: 0,
                });
                lease.commit(batch, false)?;

                self.emit(DurableEvent::Compacted {
                    conversation_id: conversation_id.to_string(),
                    entry_id: comp_entry.id,
                    tokens_before,
                    first_kept_entry_id: output.first_kept_entry_id,
                });
                return Ok(None);
            }
        }

        // Leaf moved or nothing to compact: advance cleanly into Generating preserving `turn_index`.
        let now = self.clock.now_ms();
        let mut batch = CommitBatch::new(conversation_id, now);
        batch.checkpoint = CheckpointMutation::Set(TaskCheckpoint::Generating {
            submission_id: submission_id.to_string(),
            turn_index,
            attempt: 0,
        });
        lease.commit(batch, false)?;
        Ok(None)
    }

    async fn step_generating(
        &self,
        conversation_id: &str,
        submission_id: &str,
        turn_index: u32,
        attempt: u32,
        cancel: &CancelToken,
        lease: &mut ConversationLeaseGuard,
    ) -> Result<Option<RunOutcome>, DurableError> {
        if cancel.is_cancelled() {
            return Ok(Some(self.commit_cancelled(
                conversation_id,
                submission_id,
                lease,
            )?));
        }

        if turn_index >= self.config.max_turns_per_submission {
            let err = format!(
                "exceeded max turns per submission ({})",
                self.config.max_turns_per_submission
            );
            return Ok(Some(self.commit_failed(
                conversation_id,
                submission_id,
                err,
                lease,
            )?));
        }

        let conv = self.require_conversation(conversation_id)?;
        let entries = self.store.load_entries(conversation_id)?;
        let manager = open_session_view(
            &conv,
            entries,
            self.tool_definitions(),
            self.config.compaction.clone(),
            self.config.compaction_strategy.clone(),
        )?;

        // Check auto-compaction before invoking the LLM on fresh turn attempts.
        if attempt == 0 && manager.needs_compaction(&conv.active_model) {
            let probe_opts = self.stream_options(&conv, cancel.clone());
            if manager
                .prepare_compaction(&conv.active_model, probe_opts)
                .is_some()
            {
                let now = self.clock.now_ms();
                let mut batch = CommitBatch::new(conversation_id, now);
                batch.checkpoint = CheckpointMutation::Set(TaskCheckpoint::Compacting {
                    submission_id: submission_id.to_string(),
                    turn_index,
                    expected_leaf_id: conv.head_entry_id,
                });
                lease.commit(batch, false)?;
                return Ok(None);
            }
        }

        let transcript = manager.reconstruct_context().to_transcript_context();
        let stream_opts = self.stream_options(&conv, cancel.clone());
        let stream = self
            .provider
            .stream(&conv.active_model, &transcript, stream_opts);
        let assistant_msg = self
            .consume_assistant_stream(
                conversation_id,
                submission_id,
                turn_index,
                &conv.active_model,
                stream,
                lease,
            )
            .await?;

        if assistant_msg.stop_reason == StopReason::Aborted || cancel.is_cancelled() {
            return Ok(Some(self.commit_cancelled(
                conversation_id,
                submission_id,
                lease,
            )?));
        }

        if assistant_msg.stop_reason == StopReason::Error {
            let is_retryable = assistant_msg.diagnostics.iter().any(|d| d.retryable);
            let max_attempts = self.config.retry_policy.effective_max_attempts();
            if is_retryable && attempt + 1 < max_attempts {
                let now = self.clock.now_ms();
                let mut batch = CommitBatch::new(conversation_id, now);
                batch.checkpoint = CheckpointMutation::Set(TaskCheckpoint::Generating {
                    submission_id: submission_id.to_string(),
                    turn_index,
                    attempt: attempt + 1,
                });
                lease.commit(batch, false)?;
                return Ok(None);
            }
            let err = assistant_msg
                .error_message
                .unwrap_or_else(|| "assistant generation failed".to_string());
            return Ok(Some(self.commit_failed(
                conversation_id,
                submission_id,
                err,
                lease,
            )?));
        }

        let now = self.clock.now_ms();
        let tool_calls: Vec<ToolCall> = assistant_msg.tool_calls().into_iter().cloned().collect();
        let final_text = assistant_msg.text_output();
        let stop_reason = assistant_msg.stop_reason;
        let usage = assistant_msg.usage.clone();

        let assistant_entry = SessionEntry {
            id: Uuid::now_v7().to_string(),
            parent_id: conv.head_entry_id,
            timestamp: now,
            payload: SessionEntryPayload::Message {
                message: AgentMessage::Llm(Message::Assistant(assistant_msg)),
            },
        };

        // Case 1: No tool calls -> commit assistant message and complete submission atomically.
        if tool_calls.is_empty() {
            let sub = self.updated_submission(
                conversation_id,
                submission_id,
                SubmissionState::Completed {
                    assistant_entry_id: assistant_entry.id.clone(),
                    final_text: final_text.clone(),
                    stop_reason,
                    usage: usage.clone(),
                },
                now,
            )?;
            let mut batch = CommitBatch::new(conversation_id, now);
            batch.update_status = Some(ConversationStatus::Idle);
            batch.append_entries.push(assistant_entry.clone());
            batch.upsert_submission = Some(sub);
            batch.checkpoint = CheckpointMutation::Clear;
            lease.commit(batch, true)?;

            self.emit(DurableEvent::SubmissionCompleted {
                conversation_id: conversation_id.to_string(),
                submission_id: submission_id.to_string(),
                assistant_entry_id: assistant_entry.id.clone(),
                final_text: final_text.clone(),
                stop_reason,
                usage: usage.clone(),
            });

            return Ok(Some(RunOutcome::Completed {
                submission_id: submission_id.to_string(),
                assistant_entry_id: assistant_entry.id,
                final_text,
                stop_reason,
                usage,
                deduplicated: false,
            }));
        }

        // Case 2: Truncated tool calls due to output length -> fail each call without executing.
        if stop_reason == StopReason::Length {
            let mut batch = CommitBatch::new(conversation_id, now);
            let mut parent_id = Some(assistant_entry.id.clone());
            batch.append_entries.push(assistant_entry);
            for tc in &tool_calls {
                let err_msg = ToolResultMessage {
                    tool_call_id: tc.id.clone(),
                    tool_name: tc.name.clone(),
                    content: AgentToolResult::error(format!(
                        "Tool call \"{}\" was not executed: response truncated by output token limit.",
                        tc.name
                    ))
                    .content,
                    is_error: true,
                    timestamp: now,
                };
                let tr_entry = SessionEntry {
                    id: Uuid::now_v7().to_string(),
                    parent_id: parent_id.clone(),
                    timestamp: now,
                    payload: SessionEntryPayload::Message {
                        message: AgentMessage::Llm(Message::ToolResult(err_msg)),
                    },
                };
                parent_id = Some(tr_entry.id.clone());
                batch.append_entries.push(tr_entry);
            }
            batch.checkpoint = CheckpointMutation::Set(TaskCheckpoint::Generating {
                submission_id: submission_id.to_string(),
                turn_index: turn_index + 1,
                attempt: 0,
            });
            lease.commit(batch, false)?;
            return Ok(None);
        }

        // Case 3: Normal tool calls -> commit assistant message + transition to ExecutingTools.
        let mut batch = CommitBatch::new(conversation_id, now);
        batch.append_entries.push(assistant_entry.clone());
        batch.checkpoint = CheckpointMutation::Set(TaskCheckpoint::ExecutingTools {
            submission_id: submission_id.to_string(),
            turn_index,
            assistant_entry_id: assistant_entry.id,
            tool_calls,
        });
        lease.commit(batch, false)?;
        Ok(None)
    }

    #[allow(clippy::too_many_arguments)]
    async fn step_executing_tools(
        &self,
        conversation_id: &str,
        submission_id: &str,
        turn_index: u32,
        assistant_entry_id: &str,
        tool_calls: &[ToolCall],
        cancel: &CancelToken,
        lease: &mut ConversationLeaseGuard,
    ) -> Result<Option<RunOutcome>, DurableError> {
        let existing = self
            .store
            .list_completed_tools(conversation_id, assistant_entry_id)?;
        let mut completed_map: HashMap<String, CompletedToolRecord> = existing
            .into_iter()
            .map(|r| (r.tool_call_id.clone(), r))
            .collect();

        for tc in tool_calls {
            if let Some(replayed) = completed_map.get(&tc.id) {
                self.emit(DurableEvent::ToolCompleted {
                    conversation_id: conversation_id.to_string(),
                    submission_id: submission_id.to_string(),
                    tool_call_id: replayed.tool_call_id.clone(),
                    tool_name: replayed.tool_name.clone(),
                    is_error: replayed.message.is_error,
                    replayed: true,
                });
                continue;
            }

            if cancel.is_cancelled() {
                return Ok(Some(self.commit_cancelled(
                    conversation_id,
                    submission_id,
                    lease,
                )?));
            }

            lease.renew()?;

            let raw_args = Value::Object(tc.arguments.clone());
            self.emit(DurableEvent::ToolStarted {
                conversation_id: conversation_id.to_string(),
                submission_id: submission_id.to_string(),
                tool_call_id: tc.id.clone(),
                tool_name: tc.name.clone(),
                args: raw_args,
            });

            let (tool_opt, prepared_or_err) = self.prepare_tool_call(tc);
            let tool_ctx = self.make_tool_context(conversation_id, submission_id, &tc.id, cancel);

            let initial_outcome = match (tool_opt, prepared_or_err) {
                (Some(tool), Ok(validated_args)) => {
                    let staged_mark = tool_ctx.staged_len();
                    let res = tool.execute(&tool_ctx, validated_args.clone()).await;
                    (
                        Some((tool, validated_args)),
                        outcome_from_tool_result(res, &tool_ctx, staged_mark),
                    )
                }
                (_, Err(immediate_err)) => (None, DurableToolOutcome::Completed(immediate_err)),
                (None, Ok(_)) => (
                    None,
                    DurableToolOutcome::Completed(AgentToolResult::error(format!(
                        "Unknown tool: {}",
                        tc.name
                    ))),
                ),
            };

            match self
                .settle_tool_outcome(
                    conversation_id,
                    submission_id,
                    turn_index,
                    assistant_entry_id,
                    tc,
                    tool_calls,
                    &tool_ctx,
                    initial_outcome.0.as_ref().map(|(t, a)| (t.as_ref(), a)),
                    initial_outcome.1,
                    Vec::new(),
                    false,
                    lease,
                )
                .await?
            {
                ToolStepResolution::Completed(rec) => {
                    self.emit(DurableEvent::ToolCompleted {
                        conversation_id: conversation_id.to_string(),
                        submission_id: submission_id.to_string(),
                        tool_call_id: rec.tool_call_id.clone(),
                        tool_name: rec.tool_name.clone(),
                        is_error: rec.message.is_error,
                        replayed: false,
                    });
                    completed_map.insert(rec.tool_call_id.clone(), rec);
                }
                ToolStepResolution::Parked(park) => {
                    return Ok(Some(RunOutcome::Parked {
                        submission_id: submission_id.to_string(),
                        park,
                    }));
                }
            }
        }

        // All tool calls in this turn are now durably recorded. Commit ToolResultMessages in source order.
        let conv = self.require_conversation(conversation_id)?;
        let now = self.clock.now_ms();
        let mut parent_id = conv.head_entry_id;
        let mut tr_entries = Vec::with_capacity(tool_calls.len());
        let mut all_terminate = !tool_calls.is_empty();

        for tc in tool_calls {
            let rec = completed_map.get(&tc.id).ok_or_else(|| {
                DurableError::InvalidState(format!("missing completed tool record for {}", tc.id))
            })?;
            if !rec.terminate {
                all_terminate = false;
            }
            let entry = SessionEntry {
                id: Uuid::now_v7().to_string(),
                parent_id: parent_id.clone(),
                timestamp: now,
                payload: SessionEntryPayload::Message {
                    message: AgentMessage::Llm(Message::ToolResult(rec.message.clone())),
                },
            };
            parent_id = Some(entry.id.clone());
            tr_entries.push(entry);
        }

        if all_terminate {
            let entries = self.store.load_entries(conversation_id)?;
            let (final_text, stop_reason, usage) = entries
                .iter()
                .find(|e| e.id == assistant_entry_id)
                .and_then(|e| match &e.payload {
                    SessionEntryPayload::Message {
                        message: AgentMessage::Llm(Message::Assistant(a)),
                    } => Some((a.text_output(), a.stop_reason, a.usage.clone())),
                    _ => None,
                })
                .unwrap_or_else(|| (String::new(), StopReason::Stop, Usage::default()));

            let sub = self.updated_submission(
                conversation_id,
                submission_id,
                SubmissionState::Completed {
                    assistant_entry_id: assistant_entry_id.to_string(),
                    final_text: final_text.clone(),
                    stop_reason,
                    usage: usage.clone(),
                },
                now,
            )?;
            let mut batch = CommitBatch::new(conversation_id, now);
            batch.update_status = Some(ConversationStatus::Idle);
            batch.append_entries = tr_entries;
            batch.upsert_submission = Some(sub);
            batch.checkpoint = CheckpointMutation::Clear;
            lease.commit(batch, true)?;

            self.emit(DurableEvent::SubmissionCompleted {
                conversation_id: conversation_id.to_string(),
                submission_id: submission_id.to_string(),
                assistant_entry_id: assistant_entry_id.to_string(),
                final_text: final_text.clone(),
                stop_reason,
                usage: usage.clone(),
            });

            return Ok(Some(RunOutcome::Completed {
                submission_id: submission_id.to_string(),
                assistant_entry_id: assistant_entry_id.to_string(),
                final_text,
                stop_reason,
                usage,
                deduplicated: false,
            }));
        }

        let mut batch = CommitBatch::new(conversation_id, now);
        batch.append_entries = tr_entries;
        batch.checkpoint = CheckpointMutation::Set(TaskCheckpoint::Generating {
            submission_id: submission_id.to_string(),
            turn_index: turn_index + 1,
            attempt: 0,
        });
        lease.commit(batch, false)?;
        Ok(None)
    }

    async fn step_parked(
        &self,
        conversation_id: &str,
        submission_id: &str,
        turn_index: u32,
        park: ParkState,
        cancel: &CancelToken,
        lease: &mut ConversationLeaseGuard,
    ) -> Result<Option<RunOutcome>, DurableError> {
        if cancel.is_cancelled() {
            return Ok(Some(self.commit_cancelled(
                conversation_id,
                submission_id,
                lease,
            )?));
        }

        let now = self.clock.now_ms();
        match &park {
            ParkState::SleepUntil {
                wake_at_ms,
                tool_call,
            } => {
                if now < *wake_at_ms {
                    return Ok(Some(RunOutcome::Parked {
                        submission_id: submission_id.to_string(),
                        park,
                    }));
                }

                let Some(parked_tc) = tool_call else {
                    let sub = self.updated_submission(
                        conversation_id,
                        submission_id,
                        SubmissionState::Running,
                        now,
                    )?;
                    let mut batch = CommitBatch::new(conversation_id, now);
                    batch.update_status = Some(ConversationStatus::Running);
                    batch.upsert_submission = Some(sub);
                    batch.checkpoint = CheckpointMutation::Set(TaskCheckpoint::Generating {
                        submission_id: submission_id.to_string(),
                        turn_index,
                        attempt: 0,
                    });
                    lease.commit(batch, false)?;
                    return Ok(None);
                };

                self.resume_parked_tool(
                    conversation_id,
                    submission_id,
                    turn_index,
                    parked_tc,
                    None,
                    Vec::new(),
                    cancel,
                    lease,
                )
                .await
            }
            ParkState::AwaitSignal {
                signal_key,
                timeout_at_ms,
                tool_call,
            } => {
                let maybe_signal = self.store.peek_next_unconsumed_signal(
                    conversation_id,
                    signal_key,
                    &[],
                    *timeout_at_ms,
                )?;
                let (resolution, consumed_sig_id) = if let Some(sig) = maybe_signal {
                    (SignalResolution::Received(sig.payload), Some(sig.signal_id))
                } else if timeout_at_ms.is_some_and(|t| now >= t) {
                    (SignalResolution::TimedOut, None)
                } else {
                    return Ok(Some(RunOutcome::Parked {
                        submission_id: submission_id.to_string(),
                        park,
                    }));
                };

                let consume_ids: Vec<String> = consumed_sig_id.into_iter().collect();
                let Some(parked_tc) = tool_call else {
                    let sub = self.updated_submission(
                        conversation_id,
                        submission_id,
                        SubmissionState::Running,
                        now,
                    )?;
                    let mut batch = CommitBatch::new(conversation_id, now);
                    batch.update_status = Some(ConversationStatus::Running);
                    batch.upsert_submission = Some(sub);
                    batch.consume_signal_ids = consume_ids;
                    batch.checkpoint = CheckpointMutation::Set(TaskCheckpoint::Generating {
                        submission_id: submission_id.to_string(),
                        turn_index,
                        attempt: 0,
                    });
                    lease.commit(batch, false)?;
                    return Ok(None);
                };

                self.resume_parked_tool(
                    conversation_id,
                    submission_id,
                    turn_index,
                    parked_tc,
                    Some(resolution),
                    consume_ids,
                    cancel,
                    lease,
                )
                .await
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn resume_parked_tool(
        &self,
        conversation_id: &str,
        submission_id: &str,
        turn_index: u32,
        parked_tc: &ParkedToolCall,
        signal_resolution: Option<SignalResolution>,
        consume_signal_ids: Vec<String>,
        cancel: &CancelToken,
        lease: &mut ConversationLeaseGuard,
    ) -> Result<Option<RunOutcome>, DurableError> {
        let tc = &parked_tc.tool_call;
        let (tool_opt, prepared_or_err) = self.prepare_tool_call(tc);
        let tool_ctx = self.make_tool_context(conversation_id, submission_id, &tc.id, cancel);

        let (tool_and_args, initial_outcome) = match (tool_opt, prepared_or_err) {
            (Some(tool), Ok(validated_args)) => {
                let staged_mark = tool_ctx.staged_len();
                let res = match signal_resolution {
                    Some(resolution) => {
                        tool.resume_with_signal(
                            &tool_ctx,
                            validated_args.clone(),
                            parked_tc.checkpoint.clone(),
                            resolution,
                        )
                        .await
                    }
                    None => {
                        tool.resume_after_sleep(
                            &tool_ctx,
                            validated_args.clone(),
                            parked_tc.checkpoint.clone(),
                        )
                        .await
                    }
                };
                (
                    Some((tool, validated_args)),
                    outcome_from_tool_result(res, &tool_ctx, staged_mark),
                )
            }
            (_, Err(err_res)) => (None, DurableToolOutcome::Completed(err_res)),
            (None, Ok(_)) => (
                None,
                DurableToolOutcome::Completed(AgentToolResult::error(format!(
                    "Unknown tool during resume: {}",
                    tc.name
                ))),
            ),
        };

        match self
            .settle_tool_outcome(
                conversation_id,
                submission_id,
                turn_index,
                &parked_tc.assistant_entry_id,
                tc,
                &parked_tc.batch_tool_calls,
                &tool_ctx,
                tool_and_args.as_ref().map(|(t, a)| (t.as_ref(), a)),
                initial_outcome,
                consume_signal_ids,
                true,
                lease,
            )
            .await?
        {
            ToolStepResolution::Completed(rec) => {
                self.emit(DurableEvent::ToolCompleted {
                    conversation_id: conversation_id.to_string(),
                    submission_id: submission_id.to_string(),
                    tool_call_id: rec.tool_call_id,
                    tool_name: rec.tool_name,
                    is_error: rec.message.is_error,
                    replayed: false,
                });
                Ok(None)
            }
            ToolStepResolution::Parked(next_park) => Ok(Some(RunOutcome::Parked {
                submission_id: submission_id.to_string(),
                park: next_park,
            })),
        }
    }

    /// Drive a tool outcome until it either commits a [`CompletedToolRecord`] or durably parks.
    ///
    /// Handles pre-buffered signals and already-elapsed timers immediately while keeping all
    /// signal consumptions and staged document mutations atomic with the final committed record
    /// or park checkpoint.
    #[allow(clippy::too_many_arguments)]
    async fn settle_tool_outcome(
        &self,
        conversation_id: &str,
        submission_id: &str,
        turn_index: u32,
        assistant_entry_id: &str,
        tc: &ToolCall,
        batch_tool_calls: &[ToolCall],
        tool_ctx: &DurableToolContext,
        tool_and_args: Option<(&dyn DurableTool, &Value)>,
        mut outcome: DurableToolOutcome,
        mut consume_signal_ids: Vec<String>,
        resuming_from_park: bool,
        lease: &mut ConversationLeaseGuard,
    ) -> Result<ToolStepResolution, DurableError> {
        loop {
            match outcome {
                DurableToolOutcome::Completed(result) => {
                    let now = self.clock.now_ms();
                    let record = CompletedToolRecord {
                        conversation_id: conversation_id.to_string(),
                        tool_call_id: tc.id.clone(),
                        assistant_entry_id: assistant_entry_id.to_string(),
                        tool_name: tc.name.clone(),
                        message: ToolResultMessage {
                            tool_call_id: tc.id.clone(),
                            tool_name: tc.name.clone(),
                            content: result.content,
                            is_error: result.is_error,
                            timestamp: now,
                        },
                        details: result.details,
                        structured_content: result.structured_content,
                        terminate: result.terminate,
                        completed_at_ms: now,
                    };

                    let mut batch = CommitBatch::new(conversation_id, now);
                    batch.record_completed_tools.push(record.clone());
                    batch.consume_signal_ids = consume_signal_ids;
                    batch.document_writes = tool_ctx.peek_staged_document_writes();
                    if resuming_from_park {
                        let sub = self.updated_submission(
                            conversation_id,
                            submission_id,
                            SubmissionState::Running,
                            now,
                        )?;
                        batch.update_status = Some(ConversationStatus::Running);
                        batch.upsert_submission = Some(sub);
                        batch.checkpoint =
                            CheckpointMutation::Set(TaskCheckpoint::ExecutingTools {
                                submission_id: submission_id.to_string(),
                                turn_index,
                                assistant_entry_id: assistant_entry_id.to_string(),
                                tool_calls: batch_tool_calls.to_vec(),
                            });
                    }
                    lease.commit(batch, false)?;
                    tool_ctx.clear_staged_document_writes();
                    return Ok(ToolStepResolution::Completed(record));
                }
                DurableToolOutcome::SleepUntil {
                    wake_at_ms,
                    checkpoint,
                } => {
                    if self.clock.now_ms() >= wake_at_ms
                        && let Some((tool, args)) = tool_and_args
                    {
                        let staged_mark = tool_ctx.staged_len();
                        let res = tool
                            .resume_after_sleep(tool_ctx, args.clone(), checkpoint)
                            .await;
                        outcome = outcome_from_tool_result(res, tool_ctx, staged_mark);
                        continue;
                    }

                    let park = ParkState::SleepUntil {
                        wake_at_ms,
                        tool_call: Some(ParkedToolCall {
                            assistant_entry_id: assistant_entry_id.to_string(),
                            tool_call: tc.clone(),
                            batch_tool_calls: batch_tool_calls.to_vec(),
                            checkpoint,
                        }),
                    };
                    self.commit_park(
                        conversation_id,
                        submission_id,
                        turn_index,
                        tc,
                        &park,
                        consume_signal_ids,
                        tool_ctx,
                        lease,
                    )?;
                    return Ok(ToolStepResolution::Parked(park));
                }
                DurableToolOutcome::AwaitSignal {
                    signal_key,
                    timeout_at_ms,
                    checkpoint,
                } => {
                    if let Some((tool, args)) = tool_and_args {
                        if let Some(sig) = self.next_unconsumed_signal(
                            conversation_id,
                            &signal_key,
                            &consume_signal_ids,
                            timeout_at_ms,
                        )? {
                            consume_signal_ids.push(sig.signal_id);
                            let staged_mark = tool_ctx.staged_len();
                            let res = tool
                                .resume_with_signal(
                                    tool_ctx,
                                    args.clone(),
                                    checkpoint,
                                    SignalResolution::Received(sig.payload),
                                )
                                .await;
                            outcome = outcome_from_tool_result(res, tool_ctx, staged_mark);
                            continue;
                        }
                        if timeout_at_ms.is_some_and(|t| self.clock.now_ms() >= t) {
                            let staged_mark = tool_ctx.staged_len();
                            let res = tool
                                .resume_with_signal(
                                    tool_ctx,
                                    args.clone(),
                                    checkpoint,
                                    SignalResolution::TimedOut,
                                )
                                .await;
                            outcome = outcome_from_tool_result(res, tool_ctx, staged_mark);
                            continue;
                        }
                    }

                    let park = ParkState::AwaitSignal {
                        signal_key: signal_key.clone(),
                        timeout_at_ms,
                        tool_call: Some(ParkedToolCall {
                            assistant_entry_id: assistant_entry_id.to_string(),
                            tool_call: tc.clone(),
                            batch_tool_calls: batch_tool_calls.to_vec(),
                            checkpoint: checkpoint.clone(),
                        }),
                    };

                    // Atomically check if a matching signal arrived concurrently before committing `Parked`.
                    if let Some(arrived_sig) = self.commit_park_await_signal(
                        conversation_id,
                        submission_id,
                        turn_index,
                        tc,
                        &park,
                        &signal_key,
                        timeout_at_ms,
                        &consume_signal_ids,
                        tool_ctx,
                        lease,
                    )? && let Some((tool, args)) = tool_and_args
                    {
                        consume_signal_ids.push(arrived_sig.signal_id);
                        let staged_mark = tool_ctx.staged_len();
                        let res = tool
                            .resume_with_signal(
                                tool_ctx,
                                args.clone(),
                                checkpoint,
                                SignalResolution::Received(arrived_sig.payload),
                            )
                            .await;
                        outcome = outcome_from_tool_result(res, tool_ctx, staged_mark);
                        continue;
                    }
                    return Ok(ToolStepResolution::Parked(park));
                }
            }
        }
    }

    fn next_unconsumed_signal(
        &self,
        conversation_id: &str,
        signal_key: &str,
        pending_consumed: &[String],
        max_received_at_ms: Option<u64>,
    ) -> Result<Option<SignalRecord>, DurableError> {
        self.store.peek_next_unconsumed_signal(
            conversation_id,
            signal_key,
            pending_consumed,
            max_received_at_ms,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_park_await_signal(
        &self,
        conversation_id: &str,
        submission_id: &str,
        turn_index: u32,
        tc: &ToolCall,
        park: &ParkState,
        signal_key: &str,
        timeout_at_ms: Option<u64>,
        consume_signal_ids: &[String],
        tool_ctx: &DurableToolContext,
        lease: &mut ConversationLeaseGuard,
    ) -> Result<Option<SignalRecord>, DurableError> {
        let now = self.clock.now_ms();
        let sub =
            self.updated_submission(conversation_id, submission_id, SubmissionState::Parked, now)?;
        let mut batch = CommitBatch::new(conversation_id, now);
        batch.update_status = Some(ConversationStatus::Parked);
        batch.upsert_submission = Some(sub);
        batch.consume_signal_ids = consume_signal_ids.to_vec();
        batch.document_writes = tool_ctx.peek_staged_document_writes();
        batch.checkpoint = CheckpointMutation::Set(TaskCheckpoint::Parked {
            submission_id: submission_id.to_string(),
            turn_index,
            park: park.clone(),
        });
        match lease.commit_or_peek_signal(
            batch,
            signal_key,
            consume_signal_ids,
            timeout_at_ms,
            true,
        )? {
            Ok(_) => {
                tool_ctx.clear_staged_document_writes();
                self.emit(DurableEvent::ToolParked {
                    conversation_id: conversation_id.to_string(),
                    submission_id: submission_id.to_string(),
                    tool_call_id: tc.id.clone(),
                    tool_name: tc.name.clone(),
                    park: park.clone(),
                });
                self.emit(DurableEvent::ConversationParked {
                    conversation_id: conversation_id.to_string(),
                    submission_id: submission_id.to_string(),
                    park: park.clone(),
                });
                Ok(None)
            }
            Err(sig) => Ok(Some(sig)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_park(
        &self,
        conversation_id: &str,
        submission_id: &str,
        turn_index: u32,
        tc: &ToolCall,
        park: &ParkState,
        consume_signal_ids: Vec<String>,
        tool_ctx: &DurableToolContext,
        lease: &mut ConversationLeaseGuard,
    ) -> Result<(), DurableError> {
        let now = self.clock.now_ms();
        let sub =
            self.updated_submission(conversation_id, submission_id, SubmissionState::Parked, now)?;
        let mut batch = CommitBatch::new(conversation_id, now);
        batch.update_status = Some(ConversationStatus::Parked);
        batch.upsert_submission = Some(sub);
        batch.consume_signal_ids = consume_signal_ids;
        batch.document_writes = tool_ctx.peek_staged_document_writes();
        batch.checkpoint = CheckpointMutation::Set(TaskCheckpoint::Parked {
            submission_id: submission_id.to_string(),
            turn_index,
            park: park.clone(),
        });
        lease.commit(batch, true)?;
        tool_ctx.clear_staged_document_writes();

        self.emit(DurableEvent::ToolParked {
            conversation_id: conversation_id.to_string(),
            submission_id: submission_id.to_string(),
            tool_call_id: tc.id.clone(),
            tool_name: tc.name.clone(),
            park: park.clone(),
        });
        self.emit(DurableEvent::ConversationParked {
            conversation_id: conversation_id.to_string(),
            submission_id: submission_id.to_string(),
            park: park.clone(),
        });
        Ok(())
    }

    fn commit_failed(
        &self,
        conversation_id: &str,
        submission_id: &str,
        error: String,
        lease: &mut ConversationLeaseGuard,
    ) -> Result<RunOutcome, DurableError> {
        let now = self.clock.now_ms();
        let sub = self.updated_submission(
            conversation_id,
            submission_id,
            SubmissionState::Failed {
                error: error.clone(),
            },
            now,
        )?;
        let mut batch = CommitBatch::new(conversation_id, now);
        batch.update_status = Some(ConversationStatus::Failed);
        batch.upsert_submission = Some(sub);
        batch.checkpoint = CheckpointMutation::Clear;
        lease.commit(batch, true)?;

        self.emit(DurableEvent::SubmissionFailed {
            conversation_id: conversation_id.to_string(),
            submission_id: submission_id.to_string(),
            error: error.clone(),
        });
        Ok(RunOutcome::Failed {
            submission_id: submission_id.to_string(),
            error,
        })
    }

    fn commit_cancelled(
        &self,
        conversation_id: &str,
        submission_id: &str,
        lease: &mut ConversationLeaseGuard,
    ) -> Result<RunOutcome, DurableError> {
        let now = self.clock.now_ms();
        let conv = self.require_conversation(conversation_id)?;
        let checkpoint = self.store.get_checkpoint(conversation_id)?;

        // If cancelling mid-tool-batch (`ExecutingTools` or tool-`Parked`), flush already-completed
        // tool results plus synthetic cancelled results in source order so transcript tool calls stay
        // 1:1 balanced and completed side-effect outputs remain visible to subsequent turns!
        let mut flush_entries = Vec::new();
        if let Some(cp) = checkpoint {
            let tool_batch: Option<(String, Vec<ToolCall>)> = match cp {
                TaskCheckpoint::ExecutingTools {
                    assistant_entry_id,
                    tool_calls,
                    ..
                } => Some((assistant_entry_id, tool_calls)),
                TaskCheckpoint::Parked {
                    park:
                        ParkState::SleepUntil {
                            tool_call: Some(ptc),
                            ..
                        }
                        | ParkState::AwaitSignal {
                            tool_call: Some(ptc),
                            ..
                        },
                    ..
                } => Some((ptc.assistant_entry_id, ptc.batch_tool_calls)),
                _ => None,
            };

            if let Some((assistant_entry_id, batch_calls)) = tool_batch {
                let completed: HashMap<String, CompletedToolRecord> = self
                    .store
                    .list_completed_tools(conversation_id, &assistant_entry_id)?
                    .into_iter()
                    .map(|r| (r.tool_call_id.clone(), r))
                    .collect();
                let mut parent_id = conv.head_entry_id;
                for tc in &batch_calls {
                    let msg = if let Some(done) = completed.get(&tc.id) {
                        done.message.clone()
                    } else {
                        ToolResultMessage {
                            tool_call_id: tc.id.clone(),
                            tool_name: tc.name.clone(),
                            content: AgentToolResult::error("Tool call cancelled").content,
                            is_error: true,
                            timestamp: now,
                        }
                    };
                    let entry = SessionEntry {
                        id: Uuid::now_v7().to_string(),
                        parent_id: parent_id.clone(),
                        timestamp: now,
                        payload: SessionEntryPayload::Message {
                            message: AgentMessage::Llm(Message::ToolResult(msg)),
                        },
                    };
                    parent_id = Some(entry.id.clone());
                    flush_entries.push(entry);
                }
            }
        }

        let sub = self.updated_submission(
            conversation_id,
            submission_id,
            SubmissionState::Cancelled,
            now,
        )?;
        let mut batch = CommitBatch::new(conversation_id, now);
        batch.update_status = Some(ConversationStatus::Cancelled);
        batch.append_entries = flush_entries;
        batch.upsert_submission = Some(sub);
        batch.checkpoint = CheckpointMutation::Clear;
        lease.commit(batch, true)?;

        Ok(RunOutcome::Cancelled {
            submission_id: submission_id.to_string(),
        })
    }

    fn require_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<ConversationRecord, DurableError> {
        self.store
            .get_conversation(conversation_id)?
            .ok_or_else(|| DurableError::ConversationNotFound(conversation_id.to_string()))
    }

    fn updated_submission(
        &self,
        conversation_id: &str,
        submission_id: &str,
        state: SubmissionState,
        now_ms: u64,
    ) -> Result<SubmissionRecord, DurableError> {
        let existing = self.store.get_submission(conversation_id, submission_id)?;
        Ok(SubmissionRecord {
            conversation_id: conversation_id.to_string(),
            submission_id: submission_id.to_string(),
            user_entry_id: existing.as_ref().and_then(|s| s.user_entry_id.clone()),
            state,
            created_at_ms: existing.map(|s| s.created_at_ms).unwrap_or(now_ms),
            updated_at_ms: now_ms,
        })
    }

    fn make_tool_context(
        &self,
        conversation_id: &str,
        submission_id: &str,
        tool_call_id: &str,
        cancel: &CancelToken,
    ) -> DurableToolContext {
        DurableToolContext::new(
            conversation_id.to_string(),
            submission_id.to_string(),
            tool_call_id.to_string(),
            Arc::clone(&self.clock),
            Arc::clone(&self.store),
            cancel.clone(),
        )
    }

    fn prepare_tool_call(
        &self,
        tc: &ToolCall,
    ) -> (Option<Arc<dyn DurableTool>>, Result<Value, AgentToolResult>) {
        if let Some(arg_err) = &tc.arguments_error {
            return (None, Err(AgentToolResult::error(arg_err.clone())));
        }
        let Some(tool) = self.find_tool(&tc.name) else {
            let available: Vec<String> = self.tools.iter().map(|t| t.definition().name).collect();
            let msg = if available.is_empty() {
                format!("Unknown tool: {} (no tools available)", tc.name)
            } else {
                format!(
                    "Unknown tool: {}. Available tools: {}",
                    tc.name,
                    available.join(", ")
                )
            };
            return (None, Err(AgentToolResult::error(msg)));
        };

        let compiled = match CompiledSchema::compile(tool.definition().parameters) {
            Ok(c) => c,
            Err(err) => {
                return (
                    Some(tool),
                    Err(AgentToolResult::error(format!(
                        "Invalid schema for tool {}: {err}",
                        tc.name
                    ))),
                );
            }
        };

        let raw_value = Value::Object(tc.arguments.clone());
        match compiled.coerce_and_validate(&raw_value) {
            Ok(val) => (Some(tool), Ok(val)),
            Err(validation_err) => (
                Some(tool),
                Err(AgentToolResult::error(validation_err.to_string())),
            ),
        }
    }

    fn stream_options(
        &self,
        conv: &ConversationRecord,
        cancel: CancelToken,
    ) -> SimpleStreamOptions {
        SimpleStreamOptions {
            api_key: self.config.api_key.clone(),
            reasoning: if self.config.thinking_level == ThinkingLevel::Off {
                None
            } else {
                Some(self.config.thinking_level)
            },
            fast: Some(self.config.fast),
            temperature: self.config.temperature,
            max_tokens: self.config.max_tokens,
            cache_retention: self.config.cache_retention,
            session_id: Some(conv.session_id.clone()),
            headers: self.config.headers.clone(),
            sampling_params: self.config.sampling_params.clone(),
            cancel,
        }
    }

    async fn consume_assistant_stream(
        &self,
        conversation_id: &str,
        submission_id: &str,
        turn_index: u32,
        model: &Model,
        mut stream: AssistantEventStream,
        lease: &mut ConversationLeaseGuard,
    ) -> Result<AssistantMessage, DurableError> {
        let mut final_msg: Option<AssistantMessage> = None;
        while let Some(event) = stream.next().await {
            if !matches!(
                event,
                AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
            ) {
                lease.maybe_renew()?;
            }
            self.emit(DurableEvent::AssistantEvent {
                conversation_id: conversation_id.to_string(),
                submission_id: submission_id.to_string(),
                turn_index,
                event: event.clone(),
            });
            match event {
                AssistantMessageEvent::Done { message, .. } => {
                    final_msg = Some(message);
                }
                AssistantMessageEvent::Error { error, .. } => {
                    final_msg = Some(error);
                }
                _ => {}
            }
        }
        Ok(final_msg.unwrap_or_else(|| AssistantMessage {
            content: Vec::new(),
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_id: None,
            thinking_level: Some(self.config.thinking_level),
            usage: Usage::default(),
            stop_reason: StopReason::Error,
            error_message: Some("provider stream ended without terminal event".to_string()),
            diagnostics: Vec::new(),
            timestamp: self.clock.now_ms(),
            duration_ms: None,
        }))
    }
}

fn settled_submission_outcome(existing: SubmissionRecord) -> Option<RunOutcome> {
    match existing.state {
        SubmissionState::Completed {
            assistant_entry_id,
            final_text,
            stop_reason,
            usage,
        } => Some(RunOutcome::Completed {
            submission_id: existing.submission_id,
            assistant_entry_id,
            final_text,
            stop_reason,
            usage,
            deduplicated: true,
        }),
        SubmissionState::Failed { error } => Some(RunOutcome::Failed {
            submission_id: existing.submission_id,
            error,
        }),
        SubmissionState::Cancelled => Some(RunOutcome::Cancelled {
            submission_id: existing.submission_id,
        }),
        SubmissionState::Running | SubmissionState::Parked => None,
    }
}

fn outcome_from_tool_result(
    res: Result<DurableToolOutcome, String>,
    tool_ctx: &DurableToolContext,
    staged_mark: usize,
) -> DurableToolOutcome {
    match res {
        Ok(outcome) => outcome,
        Err(err) => {
            tool_ctx.truncate_staged(staged_mark);
            DurableToolOutcome::Completed(AgentToolResult::error(err))
        }
    }
}
