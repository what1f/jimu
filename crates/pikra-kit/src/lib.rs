//! High-level headless `KitSession`, skill discovery, deferred tool gateway, codemode, and MCP client.

mod codemode;
mod deferred;
mod error;
pub mod mcp;
mod session;
mod skills;

pub use codemode::{CODEMODE_EXEC_TOOL_NAME, CodemodeExecTool};
pub use deferred::{
    CALL_TOOL_NAME, CallToolGateway, DeferredToolRegistry, TOOL_SEARCH_NAME, ToolSearchTool,
};
pub use error::KitError;
pub use mcp::{
    JsonRpcErrorObject, JsonRpcId, JsonRpcResponse, MCP_PROTOCOL_VERSION, McpClient, McpError,
    McpInitializeResult, McpServerInfo, McpToolCallResult, McpToolInfo, McpToolRegistrationMode,
    McpTransport, StreamableHttpMcpTransport,
};
#[cfg(all(feature = "stdio", not(target_arch = "wasm32")))]
pub use mcp::{StdioMcpConfig, StdioMcpTransport};
pub use session::{KitEvent, KitEventStream, KitSession, KitSessionConfig, KitToolSpec};
pub use skills::{Skill, SkillCatalog, SkillDiagnostic};
