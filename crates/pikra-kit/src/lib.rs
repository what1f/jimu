//! High-level headless `KitSession`, skill discovery, deferred tool gateway, codemode, and MCP client.

mod bm25;
mod codemode;
mod deferred;
mod error;
mod instructions;
pub mod mcp;
mod session;
mod skill_context;
mod skills;
mod wakeup;

pub use codemode::{CODEMODE_EXEC_TOOL_NAME, CodemodeExecTool};
pub use deferred::{
    CALL_TOOL_NAME, CallToolGateway, DeferredToolRegistry, TOOL_SEARCH_NAME, ToolSearchTool,
};
pub use error::KitError;
pub use instructions::{
    DEFAULT_INSTRUCTION_MAX_BYTES, INSTRUCTIONS_CONTEXT_NAME, InstructionCatalog,
    InstructionContextState, InstructionDiscoveryConfig, InstructionEntry, WalkBoundary,
};
pub use mcp::{
    JsonRpcErrorObject, JsonRpcId, JsonRpcResponse, MCP_PROTOCOL_VERSION, McpClient, McpError,
    McpInitializeResult, McpServerInfo, McpToolCallResult, McpToolInfo, McpToolRegistrationMode,
    McpTransport, StreamableHttpMcpTransport,
};
#[cfg(all(feature = "stdio", not(target_arch = "wasm32")))]
pub use mcp::{StdioMcpConfig, StdioMcpTransport};
pub use session::{
    KitEvent, KitEventStream, KitSession, KitSessionConfig, KitToolRegistrationMode, KitToolSpec,
    ToolSchemaBudget,
};
pub use skill_context::{SKILLS_CONTEXT_NAME, SkillContextState};
pub use skills::{
    SKILL_SEARCH_NAME, Skill, SkillCatalog, SkillDiagnostic, SkillDiscoveryConfig, SkillEnableRule,
    SkillSearchTool, SkillSelector,
};
pub use wakeup::{BackgroundEventSender, BackgroundInbox, BackgroundNotification, KitTurnOrigin};
