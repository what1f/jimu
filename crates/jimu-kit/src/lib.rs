//! High-level headless `KitSession`, skill discovery, deferred tool gateway, codemode, and MCP client.

mod bm25;
mod codemode;
mod deferred;
mod error;
mod instructions;
pub mod mcp;
mod session;
mod skill_context;
#[cfg(all(feature = "watch", not(any(target_arch = "wasm32", target_os = "ios"))))]
mod skill_watcher;
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
pub use jimu_agent::{QueuedInputKind, QueuedInputSummary};
pub use jimu_session::ContextUsage;
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
#[cfg(all(feature = "watch", not(any(target_arch = "wasm32", target_os = "ios"))))]
pub use skill_watcher::SkillWatcher;
pub use skills::{
    Skill, SkillCatalog, SkillDiagnostic, SkillDiscoveryConfig, SkillEnableRule, SkillSelector,
};
pub use wakeup::{BackgroundEventSender, BackgroundInbox, BackgroundNotification, KitTurnOrigin};
