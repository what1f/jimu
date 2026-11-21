//! High-level headless `KitSession`, skill discovery, deferred tool gateway, codemode, and MCP client.

mod codemode;
mod deferred;
mod error;
pub mod mcp;
mod session;
mod skills;

pub use codemode::{CodemodeExecTool, CODEMODE_EXEC_TOOL_NAME};
pub use deferred::{
    CallToolGateway, DeferredToolRegistry, ToolSearchTool, CALL_TOOL_NAME, TOOL_SEARCH_NAME,
};
pub use error::KitError;
pub use mcp::{
    JsonRpcErrorObject, JsonRpcId, JsonRpcResponse, McpClient, McpError, McpInitializeResult,
    McpServerInfo, McpToolCallResult, McpToolInfo, McpToolRegistrationMode, McpTransport,
    StreamableHttpMcpTransport, MCP_PROTOCOL_VERSION,
};
pub use session::{KitEvent, KitEventStream, KitSession, KitSessionConfig, KitToolSpec};
pub use skills::{Skill, SkillCatalog, SkillDiagnostic};
