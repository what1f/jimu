//! High-level headless `KitSession`, skill discovery, deferred tool gateway, codemode, and MCP client.

mod bm25;
mod codemode;
mod deferred;
mod error;
pub mod mcp;
mod session;
mod skills;
mod wakeup;

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
pub use session::{
    KitEvent, KitEventStream, KitSession, KitSessionConfig, KitToolRegistrationMode, KitToolSpec,
};
pub use skills::{
    SKILL_SEARCH_NAME, Skill, SkillCatalog, SkillDiagnostic, SkillDiscoveryConfig, SkillEnableRule,
    SkillSearchTool, SkillSelector,
};
pub use wakeup::{BackgroundEventSender, BackgroundInbox, BackgroundNotification, KitTurnOrigin};
