//! ACP v1 over newline-delimited JSON-RPC.
pub use agent_client_protocol_schema as schema;

pub mod args;
pub(crate) mod auth;
mod edits;
mod rpc;
mod server;
mod session;
mod tools_bridge;
mod turn;

pub use rpc::{serve_io, serve_stdio};
pub use server::AcpServer;

use crate::{host::HostConfig, mcp::McpServerConfig, tools::ToolSelection};
use jimu_ai::{ApiKind, HttpTransport, StreamProvider, ThinkingLevel};
use jimu_kit::{InstructionDiscoveryConfig, WalkBoundary};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

/// Defaults and dependencies for an ACP server. Each client opens its own sessions.
#[derive(Clone)]
pub struct AcpConfig {
    pub cwd: PathBuf,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api: Option<ApiKind>,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub jimu_home: PathBuf,
    pub persist_sessions: bool,
    pub skill_dirs: Vec<PathBuf>,
    pub skill_walk_boundary: WalkBoundary,
    pub instruction_discovery: Option<InstructionDiscoveryConfig>,
    pub extensions: Vec<PathBuf>,
    pub tools: ToolSelection,
    pub enable_codemode: bool,
    pub compact_tokens: Option<u32>,
    pub context_window: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub thinking: ThinkingLevel,
    pub fast: bool,
    pub initial_mode: String,
    pub system_prompt: Option<String>,
    pub mcp_configs: Vec<PathBuf>,
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
    pub provider_override: Option<Arc<dyn StreamProvider>>,
    pub transport_override: Option<Arc<dyn HttpTransport>>,
}

impl Default for AcpConfig {
    fn default() -> Self {
        Self::from(HostConfig::default())
    }
}

impl From<HostConfig> for AcpConfig {
    fn from(host: HostConfig) -> Self {
        Self {
            cwd: host.cwd,
            provider: host.provider,
            model: host.model,
            api: host.api,
            base_url: host.base_url,
            api_key_env: host.api_key_env,
            jimu_home: host.jimu_home,
            persist_sessions: host.persist_sessions,
            skill_dirs: host.skill_dirs,
            skill_walk_boundary: host.skill_walk_boundary,
            instruction_discovery: host.instruction_discovery,
            extensions: host.extensions,
            tools: host.tools,
            enable_codemode: host.enable_codemode,
            compact_tokens: host.compact_tokens,
            context_window: host.context_window,
            max_output_tokens: host.max_output_tokens,
            thinking: host.thinking,
            fast: host.fast,
            initial_mode: host.initial_mode,
            system_prompt: host.system_prompt,
            mcp_configs: host.mcp_configs,
            mcp_servers: host.mcp_servers,
            provider_override: host.provider_override,
            transport_override: host.transport_override,
        }
    }
}

impl From<&AcpConfig> for HostConfig {
    fn from(acp: &AcpConfig) -> Self {
        Self {
            cwd: acp.cwd.clone(),
            jimu_home: acp.jimu_home.clone(),
            provider: acp.provider.clone(),
            model: acp.model.clone(),
            api: acp.api.clone(),
            base_url: acp.base_url.clone(),
            api_key_env: acp.api_key_env.clone(),
            persist_sessions: acp.persist_sessions,
            skill_dirs: acp.skill_dirs.clone(),
            skill_walk_boundary: acp.skill_walk_boundary,
            instruction_discovery: acp.instruction_discovery.clone(),
            extensions: acp.extensions.clone(),
            tools: acp.tools.clone(),
            enable_codemode: acp.enable_codemode,
            compact_tokens: acp.compact_tokens,
            context_window: acp.context_window,
            max_output_tokens: acp.max_output_tokens,
            thinking: acp.thinking,
            fast: acp.fast,
            initial_mode: acp.initial_mode.clone(),
            system_prompt: acp.system_prompt.clone(),
            mcp_configs: acp.mcp_configs.clone(),
            mcp_servers: acp.mcp_servers.clone(),
            provider_override: acp.provider_override.clone(),
            transport_override: acp.transport_override.clone(),
        }
    }
}
