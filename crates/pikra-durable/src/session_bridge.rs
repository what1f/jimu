use crate::error::DurableError;
use crate::types::ConversationRecord;
use pikra_ai::ToolDefinition;
use pikra_session::{
    CompactionConfig, CompactionStrategy, SessionConfig, SessionEntry, SessionError,
    SessionHeader, SessionManager, SessionStore, CURRENT_SESSION_VERSION,
};
use std::sync::Arc;

/// Read-only in-memory [`SessionStore`] snapshot backed by rows loaded from [`crate::DurableStore`].
///
/// Allows `pikra-durable` to reuse `pikra_session::SessionManager`'s exact context reconstruction,
/// token estimation, turn-boundary `first_kept_entry_id` selection, and prefix-preserving compaction.
struct SnapshotSessionStore {
    header: SessionHeader,
    entries: Vec<SessionEntry>,
}

impl SessionStore for SnapshotSessionStore {
    fn load(&self) -> Result<(Option<SessionHeader>, Vec<SessionEntry>), SessionError> {
        Ok((Some(self.header.clone()), self.entries.clone()))
    }

    fn write_header(&self, _header: &SessionHeader) -> Result<(), SessionError> {
        Ok(())
    }

    fn append_entry(&self, _entry: &SessionEntry) -> Result<(), SessionError> {
        Ok(())
    }
}

/// Build a [`SessionManager`] view over a conversation's persisted [`SessionEntry`] rows.
pub(crate) fn open_session_view(
    conversation: &ConversationRecord,
    entries: Vec<SessionEntry>,
    tools: Vec<ToolDefinition>,
    compaction: CompactionConfig,
    strategy: Option<Arc<dyn CompactionStrategy>>,
) -> Result<SessionManager, DurableError> {
    let header = SessionHeader {
        version: CURRENT_SESSION_VERSION,
        id: conversation.session_id.clone(),
        created_at: conversation.created_at_ms,
        cwd: None,
        parent_session: None,
    };

    let store = Arc::new(SnapshotSessionStore { header, entries });
    let mut cfg = SessionConfig::new(conversation.system_prompt.clone(), tools)
        .with_compaction(compaction);
    if let Some(s) = strategy {
        cfg = cfg.with_compaction_strategy(s);
    }

    let mut manager = SessionManager::open(store, cfg)?;
    if let Some(head_id) = conversation.head_entry_id.as_deref()
        && manager.leaf_id() != Some(head_id)
        && manager.entry(head_id).is_some()
    {
        manager.fork(head_id)?;
    }

    Ok(manager)
}
