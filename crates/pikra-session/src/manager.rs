use crate::compaction::{
    CompactionConfig, CompactionInput, CompactionOutput, CompactionResult, CompactionStrategy,
    ContextTokenEstimate, DefaultCompactionStrategy, cap_summary_length, estimate_context_tokens,
    select_first_kept_entry_id, should_compact,
};
use crate::error::SessionError;
use crate::store::{MemorySessionStore, SessionStore};
use crate::types::{
    CURRENT_SESSION_VERSION, CompactionCheckpoint, ReconstructedContext, SessionEntry,
    SessionEntryPayload, SessionEvent, SessionHeader, SessionTreeNode, baseline_to_system_message,
    compaction_summary_to_agent_message, context_block_to_agent_message, now_ms,
};
use pikra_agent::AgentMessage;
use pikra_ai::{
    ContentPart, Message, Model, SimpleStreamOptions, StreamProvider, TextContent, ThinkingLevel,
    ToolDefinition, UserMessage,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use uuid::Uuid;

/// Identifier returned by [`SessionManager::subscribe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionSubscriptionId(u64);

type SessionListener = Arc<dyn Fn(&SessionEvent) + Send + Sync>;

/// Prepared snapshot for executing compaction asynchronously without holding a mutex on
/// [`SessionManager`].
#[derive(Clone)]
pub struct PreparedCompaction {
    pub input: CompactionInput,
    pub strategy: Arc<dyn CompactionStrategy>,
    pub expected_leaf_id: Option<String>,
}

fn sha256_bytes(text: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// Configuration for creating or opening a [`SessionManager`].
#[derive(Clone)]
pub struct SessionConfig {
    pub system_prompt: String,
    pub tools: Vec<ToolDefinition>,
    pub cwd: Option<String>,
    pub parent_session: Option<String>,
    pub compaction: CompactionConfig,
    pub compaction_strategy: Option<Arc<dyn CompactionStrategy>>,
}

impl SessionConfig {
    pub fn new(system_prompt: impl Into<String>, tools: Vec<ToolDefinition>) -> Self {
        Self {
            system_prompt: system_prompt.into(),
            tools,
            cwd: None,
            parent_session: None,
            compaction: CompactionConfig::default(),
            compaction_strategy: None,
        }
    }

    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn with_compaction(mut self, compaction: CompactionConfig) -> Self {
        self.compaction = compaction;
        self
    }

    pub fn with_compaction_strategy(mut self, strategy: Arc<dyn CompactionStrategy>) -> Self {
        self.compaction_strategy = Some(strategy);
        self
    }
}

/// Append-only session tree manager with branching (`fork`), deduplicated `<system-reminder>`
/// context blocks, and prefix-cache-preserving compaction.
pub struct SessionManager {
    store: Arc<dyn SessionStore>,
    header: SessionHeader,
    entries: Vec<SessionEntry>,
    by_id: HashMap<String, usize>,
    leaf_id: Option<String>,

    /// Desired named context blocks in deterministic insertion order.
    desired_context: Vec<(String, String)>,
    /// SHA-256 of the latest value for each named context block currently active on the branch
    /// (since the latest compaction).
    active_context_hashes: HashMap<String, [u8; 32]>,
    /// Named context blocks waiting to be flushed before the next user prompt.
    pending_context: Vec<(String, String)>,
    /// One-off context blocks and raw meta messages queued for the next turn.
    pending_ephemeral_context: Vec<(String, String)>,
    pending_ephemeral_meta: Vec<Vec<ContentPart>>,

    compaction_config: CompactionConfig,
    compaction_strategy: Arc<dyn CompactionStrategy>,

    listeners: Vec<(SessionSubscriptionId, SessionListener)>,
    next_sub_id: AtomicU64,
}

impl SessionManager {
    /// Create a new in-memory session with an initial `SystemBaseline`.
    pub fn in_memory(config: SessionConfig) -> Result<Self, SessionError> {
        Self::open(Arc::new(MemorySessionStore::new()), config)
    }

    /// Open an existing store (replaying entries and setting `leaf_id` to the latest entry) or
    /// initialize a new session with `SessionHeader` + `SystemBaseline`.
    pub fn open(store: Arc<dyn SessionStore>, config: SessionConfig) -> Result<Self, SessionError> {
        let (existing_header, existing_entries) = store.load()?;

        let header = match existing_header {
            Some(h) => h,
            None => {
                let h = SessionHeader {
                    version: CURRENT_SESSION_VERSION,
                    id: Uuid::now_v7().to_string(),
                    created_at: now_ms(),
                    cwd: config.cwd.clone(),
                    parent_session: config.parent_session.clone(),
                };
                store.write_header(&h)?;
                h
            }
        };

        let mut by_id = HashMap::with_capacity(existing_entries.len());
        for (idx, entry) in existing_entries.iter().enumerate() {
            by_id.insert(entry.id.clone(), idx);
        }
        let leaf_id = existing_entries.last().map(|e| e.id.clone());

        let strategy = config
            .compaction_strategy
            .unwrap_or_else(|| Arc::new(DefaultCompactionStrategy));

        let mut manager = Self {
            store,
            header,
            entries: existing_entries,
            by_id,
            leaf_id,
            desired_context: Vec::new(),
            active_context_hashes: HashMap::new(),
            pending_context: Vec::new(),
            pending_ephemeral_context: Vec::new(),
            pending_ephemeral_meta: Vec::new(),
            compaction_config: config.compaction,
            compaction_strategy: strategy,
            listeners: Vec::new(),
            next_sub_id: AtomicU64::new(1),
        };

        if manager.entries.is_empty() {
            manager.append_payload(SessionEntryPayload::SystemBaseline {
                system_prompt: config.system_prompt,
                tools: config.tools,
            })?;
        } else {
            manager.rebuild_context_tracking_from_branch();
        }

        Ok(manager)
    }

    // ---- Inspection ----

    pub fn header(&self) -> &SessionHeader {
        &self.header
    }

    pub fn session_id(&self) -> &str {
        &self.header.id
    }

    pub fn leaf_id(&self) -> Option<&str> {
        self.leaf_id.as_deref()
    }

    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }

    pub fn entry(&self, id: &str) -> Option<&SessionEntry> {
        self.by_id.get(id).map(|&idx| &self.entries[idx])
    }

    pub fn compaction_config(&self) -> &CompactionConfig {
        &self.compaction_config
    }

    pub fn set_compaction_config(&mut self, config: CompactionConfig) {
        self.compaction_config = config;
    }

    pub fn set_compaction_strategy(&mut self, strategy: Arc<dyn CompactionStrategy>) {
        self.compaction_strategy = strategy;
    }

    // ---- Event Subscription ----

    pub fn subscribe(
        &mut self,
        listener: impl Fn(&SessionEvent) + Send + Sync + 'static,
    ) -> SessionSubscriptionId {
        let id = SessionSubscriptionId(self.next_sub_id.fetch_add(1, Ordering::Relaxed));
        self.listeners.push((id, Arc::new(listener)));
        id
    }

    pub fn unsubscribe(&mut self, id: SessionSubscriptionId) -> bool {
        let before = self.listeners.len();
        self.listeners.retain(|(sub_id, _)| *sub_id != id);
        self.listeners.len() != before
    }

    fn emit(&self, event: SessionEvent) {
        for (_, listener) in &self.listeners {
            listener(&event);
        }
    }

    // ---- Append Operations ----

    fn append_payload(
        &mut self,
        payload: SessionEntryPayload,
    ) -> Result<SessionEntry, SessionError> {
        let entry = SessionEntry {
            id: Uuid::now_v7().to_string(),
            parent_id: self.leaf_id.clone(),
            timestamp: now_ms(),
            payload,
        };
        self.store.append_entry(&entry)?;
        let idx = self.entries.len();
        self.by_id.insert(entry.id.clone(), idx);
        self.leaf_id = Some(entry.id.clone());
        self.entries.push(entry.clone());
        self.emit(SessionEvent::EntryAppended {
            entry: entry.clone(),
        });
        Ok(entry)
    }

    pub fn append_message(&mut self, message: AgentMessage) -> Result<SessionEntry, SessionError> {
        self.append_payload(SessionEntryPayload::Message { message })
    }

    pub fn append_system_baseline(
        &mut self,
        system_prompt: impl Into<String>,
        tools: Vec<ToolDefinition>,
    ) -> Result<SessionEntry, SessionError> {
        self.append_payload(SessionEntryPayload::SystemBaseline {
            system_prompt: system_prompt.into(),
            tools,
        })
    }

    pub fn append_model_change(
        &mut self,
        provider: impl Into<String>,
        model_id: impl Into<String>,
    ) -> Result<SessionEntry, SessionError> {
        self.append_payload(SessionEntryPayload::ModelChange {
            provider: provider.into(),
            model_id: model_id.into(),
        })
    }

    pub fn append_thinking_change(
        &mut self,
        level: ThinkingLevel,
        fast: bool,
    ) -> Result<SessionEntry, SessionError> {
        self.append_payload(SessionEntryPayload::ThinkingChange { level, fast })
    }

    pub fn append_custom(
        &mut self,
        custom_type: impl Into<String>,
        data: Value,
    ) -> Result<SessionEntry, SessionError> {
        self.append_payload(SessionEntryPayload::Custom {
            custom_type: custom_type.into(),
            data,
        })
    }

    // ---- Named & Ephemeral Context Blocks ----

    /// Register or update a named context block.
    ///
    /// Deduplicates by `(name, value)`: if `name` already has the exact same `text` active on the
    /// current branch since the latest compaction, this is a NO-OP returning `false` (preserving
    /// prompt cache bytes). Otherwise queues it for emission before the next user prompt and
    /// returns `true`.
    pub fn set_context(&mut self, name: impl Into<String>, text: impl Into<String>) -> bool {
        let name = name.into();
        let text = text.into();
        let hash = sha256_bytes(&text);

        if let Some(existing) = self.desired_context.iter_mut().find(|(k, _)| k == &name) {
            existing.1 = text.clone();
        } else {
            self.desired_context.push((name.clone(), text.clone()));
        }

        if self.active_context_hashes.get(&name) == Some(&hash) {
            // Already active on the branch; remove any stale queued update.
            self.pending_context.retain(|(k, _)| k != &name);
            return false;
        }

        if let Some(pending) = self.pending_context.iter_mut().find(|(k, _)| k == &name) {
            pending.1 = text;
        } else {
            self.pending_context.push((name, text));
        }
        true
    }

    /// Queue a one-off context block to be injected before the next user prompt.
    pub fn append_context(&mut self, text: impl Into<String>) {
        self.append_named_context("context", text);
    }

    /// Queue a one-off context block without changing the state set by `set_context`.
    pub fn append_named_context(&mut self, name: impl Into<String>, text: impl Into<String>) {
        self.pending_ephemeral_context
            .push((name.into(), text.into()));
    }

    /// Queue a raw meta message for the next turn.
    pub fn append_meta(&mut self, text: impl Into<String>) {
        self.append_meta_parts(vec![ContentPart::Text(TextContent::plain(text))]);
    }

    /// Queue a raw multimodal meta message for the next turn.
    pub fn append_meta_parts(&mut self, content: Vec<ContentPart>) {
        self.pending_ephemeral_meta.push(content);
    }

    /// Returns `true` if there is queued context or meta input waiting for the next turn.
    pub fn has_pending_context(&self) -> bool {
        !self.pending_context.is_empty()
            || !self.pending_ephemeral_context.is_empty()
            || !self.pending_ephemeral_meta.is_empty()
    }

    /// Flush all pending context and meta messages into the append-only session log,
    /// emitting [`SessionEvent::ContextAppended`] for each written block.
    ///
    /// Returns the appended context blocks and messages in transcript order.
    pub fn flush_pending_context(&mut self) -> Result<Vec<SessionEntry>, SessionError> {
        let mut appended = Vec::with_capacity(
            self.pending_context.len()
                + self.pending_ephemeral_context.len()
                + self.pending_ephemeral_meta.len(),
        );

        while let Some((name, text)) = self.pending_context.first().cloned() {
            let hash = sha256_bytes(&text);
            if self.active_context_hashes.get(&name) == Some(&hash) {
                self.pending_context.remove(0);
                continue;
            }
            let entry = self.append_payload(SessionEntryPayload::ContextBlock {
                name: name.clone(),
                text: text.clone(),
                ephemeral: false,
            })?;
            self.pending_context.remove(0);
            self.active_context_hashes.insert(name.clone(), hash);
            self.emit(SessionEvent::ContextAppended { name, text });
            appended.push(entry);
        }

        while let Some((name, text)) = self.pending_ephemeral_context.first().cloned() {
            let entry = self.append_payload(SessionEntryPayload::ContextBlock {
                name: name.clone(),
                text: text.clone(),
                ephemeral: true,
            })?;
            self.pending_ephemeral_context.remove(0);
            self.emit(SessionEvent::ContextAppended { name, text });
            appended.push(entry);
        }

        while let Some(parts) = self.pending_ephemeral_meta.first().cloned() {
            let message = Message::User(UserMessage::meta_parts(parts, now_ms())).into();
            appended.push(self.append_message(message)?);
            self.pending_ephemeral_meta.remove(0);
        }

        Ok(appended)
    }

    // ---- Tree & Branching ----

    /// Move the active `leaf_id` to `entry_id` without deleting any sibling entries.
    pub fn fork(&mut self, entry_id: &str) -> Result<(), SessionError> {
        if !self.by_id.contains_key(entry_id) {
            return Err(SessionError::EntryNotFound(entry_id.to_string()));
        }
        let from_leaf_id = self.leaf_id.clone();
        self.leaf_id = Some(entry_id.to_string());
        self.rebuild_context_tracking_from_branch();
        self.emit(SessionEvent::Forked {
            from_leaf_id,
            to_leaf_id: entry_id.to_string(),
        });
        Ok(())
    }

    /// Return the linear entry path from root to `leaf_id` (inclusive).
    pub fn active_branch(&self) -> Vec<SessionEntry> {
        self.branch_to(self.leaf_id.as_deref())
    }

    /// Return the linear entry path from root to the given entry ID (inclusive).
    pub fn branch_to(&self, target_id: Option<&str>) -> Vec<SessionEntry> {
        let mut path = Vec::new();
        let mut current = target_id;
        let mut guard_steps = 0;
        while let Some(id) = current {
            let Some(&idx) = self.by_id.get(id) else {
                break;
            };
            let entry = &self.entries[idx];
            path.push(entry.clone());
            current = entry.parent_id.as_deref();
            guard_steps += 1;
            if guard_steps > self.entries.len() {
                break;
            }
        }
        path.reverse();
        path
    }

    /// Build a defensive copy of the full session branch tree.
    pub fn tree(&self) -> Vec<SessionTreeNode> {
        let mut children_map: HashMap<Option<&str>, Vec<usize>> = HashMap::new();
        for (idx, entry) in self.entries.iter().enumerate() {
            children_map
                .entry(entry.parent_id.as_deref())
                .or_default()
                .push(idx);
        }

        fn build_node(
            idx: usize,
            entries: &[SessionEntry],
            children_map: &HashMap<Option<&str>, Vec<usize>>,
        ) -> SessionTreeNode {
            let entry = entries[idx].clone();
            let children = children_map
                .get(&Some(entry.id.as_str()))
                .map(|child_indices| {
                    child_indices
                        .iter()
                        .map(|&c_idx| build_node(c_idx, entries, children_map))
                        .collect()
                })
                .unwrap_or_default();
            SessionTreeNode { entry, children }
        }

        children_map
            .get(&None)
            .map(|roots| {
                roots
                    .iter()
                    .map(|&r_idx| build_node(r_idx, &self.entries, &children_map))
                    .collect()
            })
            .unwrap_or_default()
    }

    // ---- Context Reconstruction ----

    /// Walk root -> active leaf and reconstruct the LLM-visible context.
    ///
    /// Invariants:
    /// - The latest `SystemBaseline` on the active branch is ALWAYS placed at index 0.
    /// - If one or more `Compaction` entries exist on the branch, replay starts from ONLY the
    ///   latest `Compaction` summary + its kept tail (`first_kept_entry_id..compaction`) + entries
    ///   appended after the compaction (never stacking old summaries).
    /// - `ContextBlock` entries render as `<system-reminder name="{name}">\n{text}\n</system-reminder>`.
    pub fn reconstruct_context(&self) -> ReconstructedContext {
        let branch = self.active_branch();

        let mut system_prompt = String::new();
        let mut tools: Vec<ToolDefinition> = Vec::new();
        let mut baseline_timestamp: u64 = 0;

        let mut model: Option<(String, String)> = None;
        let mut thinking: Option<(ThinkingLevel, bool)> = None;
        let mut all_branch_context_blocks: BTreeMap<String, String> = BTreeMap::new();
        let mut latest_compaction_idx: Option<usize> = None;

        for (idx, entry) in branch.iter().enumerate() {
            match &entry.payload {
                SessionEntryPayload::SystemBaseline {
                    system_prompt: sp,
                    tools: t,
                } => {
                    system_prompt = sp.clone();
                    tools = t.clone();
                    baseline_timestamp = entry.timestamp;
                }
                SessionEntryPayload::ModelChange { provider, model_id } => {
                    model = Some((provider.clone(), model_id.clone()));
                }
                SessionEntryPayload::ThinkingChange { level, fast } => {
                    thinking = Some((*level, *fast));
                }
                SessionEntryPayload::ContextBlock {
                    name,
                    text,
                    ephemeral: false,
                } => {
                    all_branch_context_blocks.insert(name.clone(), text.clone());
                }
                SessionEntryPayload::Compaction { .. } => {
                    latest_compaction_idx = Some(idx);
                }
                SessionEntryPayload::Message { .. }
                | SessionEntryPayload::Custom { .. }
                | SessionEntryPayload::ContextBlock {
                    ephemeral: true, ..
                } => {}
            }
        }

        let mut messages = Vec::new();
        // Always place the original SystemBaseline at index 0.
        messages.push(baseline_to_system_message(
            &system_prompt,
            &tools,
            baseline_timestamp,
        ));

        let mut visible_context_blocks: BTreeMap<String, String> = BTreeMap::new();
        let mut latest_compaction: Option<CompactionCheckpoint> = None;

        let push_entry_message =
            |entry: &SessionEntry,
             messages: &mut Vec<AgentMessage>,
             visible_ctx: &mut BTreeMap<String, String>| {
                match &entry.payload {
                    SessionEntryPayload::Message { message } => {
                        messages.push(message.clone());
                    }
                    SessionEntryPayload::ContextBlock {
                        name,
                        text,
                        ephemeral,
                    } => {
                        if !ephemeral {
                            visible_ctx.insert(name.clone(), text.clone());
                        }
                        messages.push(context_block_to_agent_message(name, text, entry.timestamp));
                    }
                    SessionEntryPayload::SystemBaseline { .. }
                    | SessionEntryPayload::ModelChange { .. }
                    | SessionEntryPayload::ThinkingChange { .. }
                    | SessionEntryPayload::Compaction { .. }
                    | SessionEntryPayload::Custom { .. } => {}
                }
            };

        if let Some(comp_idx) = latest_compaction_idx {
            let comp_entry = &branch[comp_idx];
            if let SessionEntryPayload::Compaction {
                summary,
                first_kept_entry_id,
                tokens_before,
                usage,
            } = &comp_entry.payload
            {
                latest_compaction = Some(CompactionCheckpoint {
                    entry_id: comp_entry.id.clone(),
                    summary: summary.clone(),
                    first_kept_entry_id: first_kept_entry_id.clone(),
                    tokens_before: *tokens_before,
                    usage: usage.clone(),
                });
                messages.push(compaction_summary_to_agent_message(
                    summary,
                    comp_entry.timestamp,
                ));

                // Replay kept tail prior to compaction (if first_kept_entry_id resolves before comp_idx).
                if !first_kept_entry_id.is_empty()
                    && let Some(kept_start) = branch[..comp_idx]
                        .iter()
                        .position(|e| e.id == *first_kept_entry_id)
                {
                    for entry in &branch[kept_start..comp_idx] {
                        push_entry_message(entry, &mut messages, &mut visible_context_blocks);
                    }
                }
            }

            // Replay all entries appended AFTER the latest compaction.
            for entry in &branch[comp_idx + 1..] {
                push_entry_message(entry, &mut messages, &mut visible_context_blocks);
            }
        } else {
            for entry in &branch {
                push_entry_message(entry, &mut messages, &mut visible_context_blocks);
            }
        }

        ReconstructedContext {
            system_prompt,
            tools,
            messages,
            model,
            thinking,
            latest_compaction,
            visible_context_blocks,
            all_branch_context_blocks,
        }
    }

    /// Estimate current context tokens from the reconstructed branch.
    pub fn estimate_tokens(&self) -> ContextTokenEstimate {
        let ctx = self.reconstruct_context();
        estimate_context_tokens(&ctx.messages)
    }

    /// Returns `true` when auto-compaction should run for `model`.
    pub fn needs_compaction(&self, model: &Model) -> bool {
        let est = self.estimate_tokens();
        should_compact(est.total_tokens, model, &self.compaction_config)
    }

    // ---- Compaction Lifecycle (Split Prepare/Run/Commit for Lock-Free Async Hosts) ----

    /// Prepare a compaction call snapshot (sync, does not perform network I/O).
    pub fn prepare_compaction(
        &self,
        model: &Model,
        options: SimpleStreamOptions,
    ) -> Option<PreparedCompaction> {
        let branch = self.active_branch();
        // Avoid compacting an empty session or one whose leaf is already a compaction checkpoint.
        if branch.is_empty()
            || matches!(
                branch.last().map(|e| &e.payload),
                Some(SessionEntryPayload::Compaction { .. })
            )
        {
            return None;
        }

        let reconstructed = self.reconstruct_context();
        // Need at least one non-system message to summarize.
        if reconstructed.messages.len() <= 1 {
            return None;
        }

        let tokens_before = estimate_context_tokens(&reconstructed.messages).total_tokens;
        let first_kept_entry_id =
            select_first_kept_entry_id(&branch, self.compaction_config.keep_recent_tokens);
        let previous_summary = reconstructed
            .latest_compaction
            .as_ref()
            .map(|c| c.summary.clone());

        let transcript = reconstructed.to_transcript_context();

        Some(PreparedCompaction {
            input: CompactionInput {
                model: model.clone(),
                transcript,
                branch_entries: branch,
                previous_summary,
                first_kept_entry_id,
                tokens_before,
                config: self.compaction_config.clone(),
                options,
            },
            strategy: Arc::clone(&self.compaction_strategy),
            expected_leaf_id: self.leaf_id.clone(),
        })
    }

    /// Emit [`SessionEvent::CompactionStart`] (used by hosts splitting prepare/run/commit).
    pub fn notify_compaction_start(&self, tokens_before: u64) {
        self.emit(SessionEvent::CompactionStart { tokens_before });
    }

    /// Execute the prepared compaction strategy against `provider` without borrowing `SessionManager`.
    pub async fn execute_prepared_compaction(
        provider: &dyn StreamProvider,
        prepared: PreparedCompaction,
    ) -> Result<CompactionOutput, SessionError> {
        let context_window = prepared.input.model.context_window;
        let max_summary_chars = prepared.input.config.max_summary_chars;
        let mut output = prepared.strategy.compact(provider, prepared.input).await?;
        output.summary = cap_summary_length(&output.summary, max_summary_chars, context_window);
        Ok(output)
    }

    /// Commit a completed [`CompactionOutput`] into the append-only session tree, emit
    /// [`SessionEvent::CompactionEnd`], and automatically re-queue active named context blocks
    /// that were compacted away so they are restored before the next turn.
    pub fn commit_compaction(
        &mut self,
        output: CompactionOutput,
        tokens_before: u64,
    ) -> Result<CompactionResult, SessionError> {
        let entry = self.append_payload(SessionEntryPayload::Compaction {
            summary: output.summary.clone(),
            first_kept_entry_id: output.first_kept_entry_id.clone(),
            tokens_before,
            usage: output.usage.clone(),
        })?;

        // Mark compacted-away named context blocks for automatic restoration before next turn.
        self.requeue_missing_context_blocks_after_compaction();

        self.emit(SessionEvent::CompactionEnd {
            summary: output.summary.clone(),
            tokens_before,
            first_kept_entry_id: output.first_kept_entry_id.clone(),
            usage: output.usage.clone(),
        });

        Ok(CompactionResult {
            entry_id: entry.id,
            summary: output.summary,
            first_kept_entry_id: output.first_kept_entry_id,
            tokens_before,
            usage: output.usage,
        })
    }

    /// Commit a completed [`CompactionOutput`] only if `self.leaf_id` still matches
    /// `expected_leaf_id` (prevents stale compaction from writing onto a newly forked branch).
    pub fn commit_compaction_if_leaf_matches(
        &mut self,
        output: CompactionOutput,
        tokens_before: u64,
        expected_leaf_id: Option<&str>,
    ) -> Result<Option<CompactionResult>, SessionError> {
        if self.leaf_id.as_deref() != expected_leaf_id {
            return Ok(None);
        }
        self.commit_compaction(output, tokens_before).map(Some)
    }

    /// Perform a full compaction immediately. Returns `Ok(None)` if there is nothing to compact.
    pub async fn compact(
        &mut self,
        provider: &dyn StreamProvider,
        model: &Model,
        options: SimpleStreamOptions,
    ) -> Result<Option<CompactionResult>, SessionError> {
        let Some(prepared) = self.prepare_compaction(model, options) else {
            return Ok(None);
        };
        let tokens_before = prepared.input.tokens_before;
        let expected_leaf = prepared.expected_leaf_id.clone();
        self.notify_compaction_start(tokens_before);
        let output = Self::execute_prepared_compaction(provider, prepared).await?;
        self.commit_compaction_if_leaf_matches(output, tokens_before, expected_leaf.as_deref())
    }

    /// Run compaction only if estimated context tokens exceed the model threshold.
    pub async fn auto_compact_if_needed(
        &mut self,
        provider: &dyn StreamProvider,
        model: &Model,
        options: SimpleStreamOptions,
    ) -> Result<Option<CompactionResult>, SessionError> {
        if !self.needs_compaction(model) {
            return Ok(None);
        }
        self.compact(provider, model, options).await
    }

    // ---- Internal Context Block Tracking ----

    fn rebuild_context_tracking_from_branch(&mut self) {
        let reconstructed = self.reconstruct_context();

        // Preserve any host-set desired_context, merging in branch-known named context blocks
        // (ignoring ephemeral "context" entries).
        for (name, text) in &reconstructed.all_branch_context_blocks {
            if name == "context" {
                continue;
            }
            if !self.desired_context.iter().any(|(k, _)| k == name) {
                self.desired_context.push((name.clone(), text.clone()));
            }
        }

        self.active_context_hashes.clear();
        for (name, text) in &reconstructed.visible_context_blocks {
            if name == "context" {
                continue;
            }
            self.active_context_hashes
                .insert(name.clone(), sha256_bytes(text));
        }

        // Queue any desired named context block not currently active with its desired hash.
        self.pending_context.clear();
        for (name, text) in &self.desired_context {
            let hash = sha256_bytes(text);
            if self.active_context_hashes.get(name) != Some(&hash) {
                self.pending_context.push((name.clone(), text.clone()));
            }
        }
    }

    fn requeue_missing_context_blocks_after_compaction(&mut self) {
        let reconstructed = self.reconstruct_context();
        self.active_context_hashes.clear();
        for (name, text) in &reconstructed.visible_context_blocks {
            if name == "context" {
                continue;
            }
            self.active_context_hashes
                .insert(name.clone(), sha256_bytes(text));
        }

        // Re-queue every desired named context block whose current value is no longer visible.
        let mut restored = Vec::new();
        for (name, text) in &self.desired_context {
            let hash = sha256_bytes(text);
            if self.active_context_hashes.get(name) != Some(&hash) {
                restored.push((name.clone(), text.clone()));
            }
        }
        // Preserve any already-queued updates that override desired_context.
        for (name, text) in std::mem::take(&mut self.pending_context) {
            if let Some(existing) = restored.iter_mut().find(|(k, _)| k == &name) {
                existing.1 = text;
            } else {
                restored.push((name, text));
            }
        }
        self.pending_context = restored;
    }
}
