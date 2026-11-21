use thiserror::Error;

/// Errors returned by `pikra-session` persistence, tree, and compaction operations.
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("session serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("invalid session file: {0}")]
    InvalidSessionFile(String),

    #[error("session entry not found: {0}")]
    EntryNotFound(String),

    #[error("compaction failed: {0}")]
    CompactionFailed(String),
}
