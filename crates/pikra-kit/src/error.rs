use crate::mcp::McpError;
use pikra_agent::AgentError;
use pikra_session::SessionError;
use thiserror::Error;

/// Errors returned by `pikra-kit` high-level session operations.
#[derive(Debug, Error)]
pub enum KitError {
    #[error(transparent)]
    Agent(#[from] AgentError),

    #[error(transparent)]
    Session(#[from] SessionError),

    #[error(transparent)]
    Mcp(#[from] McpError),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("session is currently busy")]
    Busy,

    #[error("tool error: {0}")]
    Tool(String),
}
