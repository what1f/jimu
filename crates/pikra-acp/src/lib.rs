//! ACP v1 over newline-delimited JSON-RPC.
pub use agent_client_protocol_schema as schema;

pub mod args;
mod auth;
mod edits;
mod rpc;
mod server;
mod session;
mod tools_bridge;
mod turn;

pub use rpc::{serve_io, serve_stdio};
pub use server::AcpServer;

use pikra_ai::{ApiKind, HttpTransport, StreamProvider, ThinkingLevel};
use std::{path::PathBuf, sync::Arc};

/// Defaults and dependencies for an ACP server. Each client opens its own sessions.
#[derive(Clone)]
pub struct AcpConfig {
    pub cwd: PathBuf,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api: Option<ApiKind>,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub pikra_home: PathBuf,
    pub persist_sessions: bool,
    pub skill_dirs: Vec<PathBuf>,
    pub extensions: Vec<PathBuf>,
    pub enable_tools: bool,
    pub enable_codemode: bool,
    pub compact_tokens: Option<u32>,
    pub context_window: Option<u32>,
    pub thinking: ThinkingLevel,
    pub fast: bool,
    pub initial_mode: String,
    pub system_prompt: Option<String>,
    pub provider_override: Option<Arc<dyn StreamProvider>>,
    pub transport_override: Option<Arc<dyn HttpTransport>>,
}

impl Default for AcpConfig {
    fn default() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let home = std::env::home_dir().unwrap_or_else(|| cwd.clone());
        Self {
            cwd,
            provider: None,
            model: None,
            api: None,
            base_url: None,
            api_key_env: None,
            pikra_home: std::env::var_os("PIKRA_HOME")
                .map_or_else(|| home.join(".pikra"), PathBuf::from),
            persist_sessions: true,
            skill_dirs: Vec::new(),
            extensions: Vec::new(),
            enable_tools: true,
            enable_codemode: false,
            compact_tokens: None,
            context_window: None,
            thinking: ThinkingLevel::Off,
            fast: false,
            initial_mode: "default".into(),
            system_prompt: None,
            provider_override: None,
            transport_override: None,
        }
    }
}
