use thiserror::Error;

/// Errors returned by [`crate::Agent`] operations.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AgentError {
    #[error("agent is already processing a run")]
    Busy,
    #[error("cannot continue: {0}")]
    CannotContinue(String),
    #[error("prompt must contain at least one message")]
    EmptyPrompt,
}
