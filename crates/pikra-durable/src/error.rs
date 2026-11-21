use pikra_session::SessionError;
use thiserror::Error;

/// Errors returned by [`crate::DurableStore`] and [`crate::DurableEngine`].
#[derive(Debug, Error)]
pub enum DurableError {
    #[error("conversation not found: {0}")]
    ConversationNotFound(String),

    #[error("conversation already exists: {0}")]
    ConversationAlreadyExists(String),

    #[error("conversation `{conversation_id}` already has an active submission `{active_submission_id}`")]
    ConversationBusy {
        conversation_id: String,
        active_submission_id: String,
    },

    #[error(
        "document CAS conflict on `{conversation_id}/{key}`: expected revision {expected:?}, actual {actual:?}"
    )]
    Conflict {
        conversation_id: String,
        key: String,
        expected: Option<u64>,
        actual: Option<u64>,
    },

    #[error("invalid durable state: {0}")]
    InvalidState(String),

    #[error("compaction error: {0}")]
    Compaction(#[from] SessionError),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("operation aborted")]
    Aborted,
}
