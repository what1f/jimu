//! Unified MCP configuration, validation, connection, and lifecycle management shared across
//! TUI, ACP, RPC, and one-shot (`-p`) modes.

use crate::settings::UserSettings;
use agent_client_protocol_schema::v1 as acp;
use futures_util::future::join_all;
use jimu_ai::{CancelToken, HttpTransport};
use jimu_kit::{KitSession, McpClient, McpToolRegistrationMode, StdioMcpConfig, StdioMcpTransport};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

const MCP_SETUP_TIMEOUT: Duration = Duration::from_secs(30);
const MCP_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

fn is_false(value: &bool) -> bool {
    !*value
}

/// Policy controlling how tools discovered from an MCP server are registered into a session.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum McpRegistrationPolicy {
    /// Admit small schemas into the resident tool list up to the session budget and defer the rest.
    #[default]
    Auto,
    /// Always expose tools through the deferred gateway (`tool_search` / `call_tool`).
    Deferred,
    /// Always register tools directly into the top-level resident tool list.
    Resident,
}

impl From<McpRegistrationPolicy> for McpToolRegistrationMode {
    fn from(policy: McpRegistrationPolicy) -> Self {
        match policy {
            McpRegistrationPolicy::Auto => Self::Auto,
            McpRegistrationPolicy::Deferred => Self::Deferred,
            McpRegistrationPolicy::Resident => Self::Resident,
        }
    }
}

/// Transport-agnostic MCP server configuration used in `~/.jimu/settings.json`,
/// `--mcp-config <path>`, `--mcp-server <name>=<spec>`, and ACP `NewSessionRequest.mcpServers`.
#[derive(Clone, Default, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    /// Stdio command path or executable name (mutually exclusive with `url`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Arguments passed to the stdio MCP command.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Environment variables injected into the stdio MCP process.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Optional working directory for the stdio MCP process (defaults to session cwd).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Streamable HTTP MCP endpoint URL (mutually exclusive with `command`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// HTTP headers sent with Streamable HTTP MCP requests.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Disable this server entry without deleting it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub disabled: bool,
    /// Optional registration policy (`auto`, `deferred`, or `resident`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<McpRegistrationPolicy>,
}

impl McpServerConfig {
    /// Convert an ACP `McpServer` descriptor into the shared [`McpServerConfig`].
    pub fn from_acp(descriptor: &acp::McpServer) -> Result<(String, Self), &'static str> {
        match descriptor {
            acp::McpServer::Stdio(spec) => {
                if !spec.command.is_absolute() {
                    return Err("MCP stdio command path must be absolute.");
                }
                let command = spec
                    .command
                    .to_str()
                    .ok_or("Invalid MCP command path.")?
                    .to_owned();
                let env = spec
                    .env
                    .iter()
                    .map(|item| (item.name.clone(), item.value.clone()))
                    .collect();
                Ok((
                    spec.name.clone(),
                    Self {
                        command: Some(command),
                        args: spec.args.clone(),
                        env,
                        ..Self::default()
                    },
                ))
            }
            acp::McpServer::Http(spec) => {
                let headers = spec
                    .headers
                    .iter()
                    .map(|header| (header.name.clone(), header.value.clone()))
                    .collect();
                Ok((
                    spec.name.clone(),
                    Self {
                        url: Some(spec.url.clone()),
                        headers,
                        ..Self::default()
                    },
                ))
            }
            _ => Err("Unsupported MCP transport."),
        }
    }
}

/// Standalone JSON file schema accepted by `--mcp-config <path>`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfigFile {
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
}

/// Origin of a configured MCP server, determining whether failures warn or abort startup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpServerSource {
    /// Persistent user config from `~/.jimu/settings.json` (bad/unreachable servers warn and skip).
    Settings,
    /// Explicit per-run CLI (`--mcp-config`, `--mcp-server`) or ACP request (fail fast).
    Explicit,
}

/// Merged MCP server entry ready for validation and connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServerEntry {
    pub name: String,
    pub config: McpServerConfig,
    pub source: McpServerSource,
}

#[derive(Debug, Clone)]
pub enum McpConnectError {
    Invalid(String),
    Failed(String),
    Cancelled,
}

impl std::fmt::Display for McpConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(msg) | Self::Failed(msg) => f.write_str(msg),
            Self::Cancelled => f.write_str("MCP setup cancelled"),
        }
    }
}

impl std::error::Error for McpConnectError {}

/// Parse `--mcp-server <name>=<spec>`.
///
/// Supported `<spec>` forms:
/// - `http://...` / `https://...` -> Streamable HTTP MCP server
/// - `{...}` -> JSON [`McpServerConfig`] object (for full `command`/`args`/`env`/`headers`/`mode`)
/// - anything else -> stdio executable path or command name
pub fn parse_cli_mcp_server(raw: &str) -> Result<(String, McpServerConfig), String> {
    let (name_raw, spec_raw) = raw.split_once('=').ok_or_else(|| {
        format!("invalid `--mcp-server` value `{raw}` (expected `<name>=<url|command|json>`)")
    })?;
    let name = name_raw.trim();
    let spec = spec_raw.trim();
    if name.is_empty() || spec.is_empty() {
        return Err(format!(
            "invalid `--mcp-server` value `{raw}` (name and target must be non-empty)"
        ));
    }

    let config = if spec.starts_with('{') {
        serde_json::from_str::<McpServerConfig>(spec)
            .map_err(|err| format!("invalid JSON for `--mcp-server {name}`: {err}"))?
    } else if spec.starts_with("http://") || spec.starts_with("https://") {
        McpServerConfig {
            url: Some(spec.to_owned()),
            ..McpServerConfig::default()
        }
    } else {
        McpServerConfig {
            command: Some(spec.to_owned()),
            ..McpServerConfig::default()
        }
    };

    if !config.disabled {
        validate_server(name, &config).map_err(|err| err.to_string())?;
    }

    Ok((name.to_owned(), config))
}

/// Load a standalone MCP configuration file (`{"mcp_servers": { ... }}`).
/// Rejects unknown fields at both the top level and inside individual server entries.
pub fn load_mcp_config_file(path: &Path) -> Result<BTreeMap<String, McpServerConfig>, String> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RawMcpConfigFile {
        mcp_servers: BTreeMap<String, serde_json::Value>,
    }

    let raw = fs::read_to_string(path)
        .map_err(|err| format!("failed to read MCP config `{}`: {err}", path.display()))?;
    let parsed: RawMcpConfigFile = serde_json::from_str(&raw)
        .map_err(|err| format!("failed to parse MCP config `{}`: {err}", path.display()))?;
    let mut out = BTreeMap::new();
    for (name, val) in parsed.mcp_servers {
        let cfg: McpServerConfig = serde_json::from_value(val)
            .map_err(|err| format!("invalid MCP server `{name}` in `{}`: {err}", path.display()))?;
        out.insert(name, cfg);
    }
    Ok(out)
}

/// Returns true when any active stdio MCP server inherits the session working directory
/// (i.e. has no explicit `cwd`), requiring reconnection when switching to a different cwd.
pub fn has_cwd_dependent_stdio(entries: &[McpServerEntry]) -> bool {
    entries.iter().any(|entry| {
        !entry.config.disabled && entry.config.command.is_some() && entry.config.cwd.is_none()
    })
}

/// Merge MCP servers across layers in ascending priority order:
/// `settings.json` < `--mcp-config` files < `--mcp-server` flags < explicit protocol overrides (ACP).
/// Returns `(entries, settings_warnings)`.
pub fn resolve_mcp_entries(
    jimu_home: &Path,
    cli_config_files: &[PathBuf],
    cli_servers: &BTreeMap<String, McpServerConfig>,
    extra_explicit: &[(String, McpServerConfig)],
) -> Result<(Vec<McpServerEntry>, Vec<String>), String> {
    let (settings, settings_warnings) = UserSettings::load_with_warnings(jimu_home);
    let mut merged: BTreeMap<String, (McpServerConfig, McpServerSource)> = BTreeMap::new();

    for (name, config) in settings.mcp_servers {
        merged.insert(name, (config, McpServerSource::Settings));
    }
    for path in cli_config_files {
        for (name, config) in load_mcp_config_file(path)? {
            merged.insert(name, (config, McpServerSource::Explicit));
        }
    }
    for (name, config) in cli_servers {
        merged.insert(name.clone(), (config.clone(), McpServerSource::Explicit));
    }
    for (name, config) in extra_explicit {
        merged.insert(name.clone(), (config.clone(), McpServerSource::Explicit));
    }

    let entries = merged
        .into_iter()
        .filter(|(_, (config, _))| !config.disabled)
        .map(|(name, (config, source))| McpServerEntry {
            name,
            config,
            source,
        })
        .collect();
    Ok((entries, settings_warnings))
}

pub fn sanitize_mcp_prefix(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

struct ValidatedServer {
    entry: McpServerEntry,
    prefix: String,
    mode: McpToolRegistrationMode,
}

fn validate_server(name: &str, config: &McpServerConfig) -> Result<String, McpConnectError> {
    let has_command = config
        .command
        .as_deref()
        .is_some_and(|c| !c.trim().is_empty());
    let has_url = config.url.as_deref().is_some_and(|u| !u.trim().is_empty());

    match (has_command, has_url) {
        (true, true) => {
            return Err(McpConnectError::Invalid(format!(
                "MCP server `{name}` cannot set both `command` and `url`."
            )));
        }
        (false, false) => {
            return Err(McpConnectError::Invalid(format!(
                "MCP server `{name}` must set either `command` or `url`."
            )));
        }
        (true, false) => {
            if !config.headers.is_empty() {
                return Err(McpConnectError::Invalid(format!(
                    "MCP stdio server `{name}` cannot set HTTP `headers`."
                )));
            }
        }
        (false, true) => {
            if !config.args.is_empty() || !config.env.is_empty() || config.cwd.is_some() {
                return Err(McpConnectError::Invalid(format!(
                    "MCP HTTP server `{name}` cannot set stdio `args`, `env`, or `cwd`."
                )));
            }
            let raw_url = config.url.as_deref().unwrap_or_default();
            let valid_url = url::Url::parse(raw_url).ok().is_some_and(|url| {
                matches!(url.scheme(), "http" | "https")
                    && url.host().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
            });
            if !valid_url {
                return Err(McpConnectError::Invalid(format!(
                    "Invalid MCP server URL for `{name}`."
                )));
            }
        }
    }

    let prefix = sanitize_mcp_prefix(name);
    if prefix.is_empty() {
        return Err(McpConnectError::Invalid(
            "MCP server names must be non-empty.".into(),
        ));
    }
    Ok(prefix)
}

struct ConnectedMcpServer {
    name: String,
    prefix: String,
    mode: McpToolRegistrationMode,
    source: McpServerSource,
    client: Arc<McpClient>,
    tool_count: AtomicUsize,
}

/// Manages connected MCP clients for a host runtime or session, supporting re-attachment across
/// session switches and clean shutdown of all stdio child process groups.
#[derive(Default)]
pub struct McpHub {
    servers: Vec<ConnectedMcpServer>,
}

impl McpHub {
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// Snapshot connected MCP servers as `(name, exposed_tool_count)` pairs for inspection UIs.
    pub fn summaries(&self) -> Vec<(String, usize)> {
        self.servers
            .iter()
            .map(|s| (s.name.clone(), s.tool_count.load(Ordering::Relaxed)))
            .collect()
    }

    /// Validate, spawn/connect, and initialize all configured MCP servers.
    ///
    /// - `Explicit` entries fail fast and shut down any already-started servers before returning.
    /// - `Settings` entries emit warnings and are skipped individually if invalid or unreachable.
    pub async fn connect(
        entries: Vec<McpServerEntry>,
        default_cwd: &Path,
        transport: Arc<dyn HttpTransport>,
        cancel: CancelToken,
    ) -> Result<(Self, Vec<String>), McpConnectError> {
        let mut warnings = Vec::new();
        let mut seen_prefixes = HashSet::new();
        let mut validated = Vec::with_capacity(entries.len());

        // Pre-validate all descriptors and normalized prefixes before spawning any child process.
        // Process Explicit entries first so persistent Settings entries can never block explicit
        // CLI/ACP servers via prefix collision.
        let mut entries = entries;
        entries.sort_by_key(|entry| match entry.source {
            McpServerSource::Explicit => 0u8,
            McpServerSource::Settings => 1u8,
        });

        for entry in entries {
            match validate_server(&entry.name, &entry.config) {
                Ok(prefix) => {
                    if !seen_prefixes.insert(prefix.clone()) {
                        let msg = format!(
                            "MCP server `{}` collides with another server prefix `mcp_{prefix}`.",
                            entry.name
                        );
                        match entry.source {
                            McpServerSource::Explicit => return Err(McpConnectError::Invalid(msg)),
                            McpServerSource::Settings => {
                                warnings.push(msg);
                                continue;
                            }
                        }
                    }
                    let mode = entry.config.mode.unwrap_or_default().into();
                    validated.push(ValidatedServer {
                        entry,
                        prefix,
                        mode,
                    });
                }
                Err(err) => match entry.source {
                    McpServerSource::Explicit => return Err(err),
                    McpServerSource::Settings => warnings.push(err.to_string()),
                },
            }
        }

        let mut hub = Self::default();
        for item in validated {
            if cancel.is_cancelled() {
                hub.shutdown().await;
                return Err(McpConnectError::Cancelled);
            }

            let client = match build_mcp_client(&item.entry.config, default_cwd, &transport) {
                Ok(client) => Arc::new(client),
                Err(err) => match item.entry.source {
                    McpServerSource::Explicit => {
                        hub.shutdown().await;
                        return Err(McpConnectError::Failed(format!(
                            "Could not start MCP server `{}`: {err}",
                            item.entry.name
                        )));
                    }
                    McpServerSource::Settings => {
                        warnings.push(format!("MCP server `{}` skipped: {err}", item.entry.name));
                        continue;
                    }
                },
            };

            let req_cancel = CancelToken::new();
            let init_future =
                client.initialize("jimu", env!("CARGO_PKG_VERSION"), req_cancel.clone());
            let init_outcome = tokio::select! {
                () = cancel.cancelled() => {
                    req_cancel.cancel();
                    let _ = tokio::time::timeout(MCP_SHUTDOWN_TIMEOUT, client.close()).await;
                    hub.shutdown().await;
                    return Err(McpConnectError::Cancelled);
                }
                res = tokio::time::timeout(MCP_SETUP_TIMEOUT, init_future) => res,
            };

            match init_outcome {
                Ok(Ok(_)) => {
                    hub.servers.push(ConnectedMcpServer {
                        name: item.entry.name,
                        prefix: item.prefix,
                        mode: item.mode,
                        source: item.entry.source,
                        client,
                        tool_count: AtomicUsize::new(0),
                    });
                }
                Ok(Err(err)) => {
                    req_cancel.cancel();
                    let _ = tokio::time::timeout(MCP_SHUTDOWN_TIMEOUT, client.close()).await;
                    let msg = format!(
                        "Could not connect to MCP server `{}`: {err}",
                        item.entry.name
                    );
                    match item.entry.source {
                        McpServerSource::Explicit => {
                            hub.shutdown().await;
                            return Err(McpConnectError::Failed(msg));
                        }
                        McpServerSource::Settings => warnings.push(msg),
                    }
                }
                Err(_) => {
                    req_cancel.cancel();
                    let _ = tokio::time::timeout(MCP_SHUTDOWN_TIMEOUT, client.close()).await;
                    let msg = format!(
                        "MCP server `{}` did not respond within {}s.",
                        item.entry.name,
                        MCP_SETUP_TIMEOUT.as_secs()
                    );
                    match item.entry.source {
                        McpServerSource::Explicit => {
                            hub.shutdown().await;
                            return Err(McpConnectError::Failed(msg));
                        }
                        McpServerSource::Settings => warnings.push(msg),
                    }
                }
            }
        }

        Ok((hub, warnings))
    }

    /// Discover tools from all connected MCP servers and attach them onto `session` in a single
    /// atomic transaction so multi-server sessions preserve a byte-identical prompt-cache baseline.
    /// Returns any non-fatal warnings from `Settings`-origin servers.
    pub async fn attach_to_session(
        &self,
        session: &KitSession,
        cancel: CancelToken,
    ) -> Result<Vec<String>, McpConnectError> {
        let mut warnings = Vec::new();
        let mut discovered_batches = Vec::with_capacity(self.servers.len());
        let active_resident = session.active_resident_tool_names();
        let existing_deferred: HashSet<String> = session
            .deferred_tools()
            .into_iter()
            .map(|def| def.name)
            .collect();
        let mut claimed_names: HashSet<String> = HashSet::new();

        for (idx, server) in self.servers.iter().enumerate() {
            if cancel.is_cancelled() {
                return Err(McpConnectError::Cancelled);
            }
            let req_cancel = CancelToken::new();
            let list_fut = server.client.list_tools(req_cancel.clone());
            tokio::pin!(list_fut);
            let outcome = tokio::select! {
                () = cancel.cancelled() => {
                    req_cancel.cancel();
                    let _ = tokio::time::timeout(Duration::from_millis(200), &mut list_fut).await;
                    return Err(McpConnectError::Cancelled);
                }
                () = tokio::time::sleep(MCP_SETUP_TIMEOUT) => {
                    // Cancel child token and drive the pinned future briefly so stdio sends
                    // `notifications/cancelled` and removes its pending waiter cleanly.
                    req_cancel.cancel();
                    let _ = tokio::time::timeout(Duration::from_millis(200), &mut list_fut).await;
                    Err(())
                }
                res = &mut list_fut => Ok(res),
            };
            match outcome {
                Ok(Ok(discovered)) => {
                    let prefix = format!("mcp_{}", server.prefix);
                    let tools = server.client.to_agent_tools(&discovered, Some(&prefix));
                    let mut server_seen = HashSet::new();
                    let mut collision = None;
                    for tool in &tools {
                        let name = tool.definition().name;
                        if active_resident.contains(&name) {
                            collision = Some(format!(
                                "MCP server `{}` tool `{name}` collides with an existing resident tool",
                                server.name
                            ));
                            break;
                        }
                        if existing_deferred.contains(&name) {
                            collision = Some(format!(
                                "MCP server `{}` tool `{name}` collides with an existing deferred tool",
                                server.name
                            ));
                            break;
                        }
                        if claimed_names.contains(&name) || !server_seen.insert(name.clone()) {
                            collision = Some(format!(
                                "MCP server `{}` defines duplicate tool `{name}`",
                                server.name
                            ));
                            break;
                        }
                    }
                    if let Some(msg) = collision {
                        server.tool_count.store(0, Ordering::Relaxed);
                        match server.source {
                            McpServerSource::Explicit => return Err(McpConnectError::Failed(msg)),
                            McpServerSource::Settings => warnings.push(msg),
                        }
                        continue;
                    }
                    claimed_names.extend(server_seen);
                    discovered_batches.push((idx, tools, server.mode));
                }
                Ok(Err(err)) => {
                    req_cancel.cancel();
                    server.tool_count.store(0, Ordering::Relaxed);
                    let msg = format!(
                        "Could not load tools from MCP server `{}`: {err}",
                        server.name
                    );
                    match server.source {
                        McpServerSource::Explicit => return Err(McpConnectError::Failed(msg)),
                        McpServerSource::Settings => warnings.push(msg),
                    }
                }
                Err(()) => {
                    server.tool_count.store(0, Ordering::Relaxed);
                    let msg = format!(
                        "MCP server `{}` timed out while listing tools.",
                        server.name
                    );
                    match server.source {
                        McpServerSource::Explicit => return Err(McpConnectError::Failed(msg)),
                        McpServerSource::Settings => warnings.push(msg),
                    }
                }
            }
        }

        if !discovered_batches.is_empty() {
            let indices: Vec<usize> = discovered_batches.iter().map(|(i, _, _)| *i).collect();
            let batches = discovered_batches
                .into_iter()
                .map(|(_, tools, mode)| (tools, mode))
                .collect();
            let attached = session.attach_mcp_tool_batches(batches).map_err(|err| {
                McpConnectError::Failed(format!("Could not attach MCP tools: {err}"))
            })?;
            for (server_idx, names) in indices.into_iter().zip(attached) {
                self.servers[server_idx]
                    .tool_count
                    .store(names.len(), Ordering::Relaxed);
            }
        }

        Ok(warnings)
    }

    /// Concurrently close all connected MCP transports with per-server timeouts so an unresponsive
    /// remote HTTP server never blocks stdio child process group cleanup or session release.
    pub async fn shutdown(&self) {
        let futs = self.servers.iter().map(|server| async {
            let _ = tokio::time::timeout(MCP_SHUTDOWN_TIMEOUT, server.client.close()).await;
        });
        let _ = join_all(futs).await;
    }
}

fn build_mcp_client(
    config: &McpServerConfig,
    default_cwd: &Path,
    transport: &Arc<dyn HttpTransport>,
) -> Result<McpClient, String> {
    if let Some(command) = config.command.as_deref() {
        let mut stdio = StdioMcpConfig::new(command);
        stdio.args.clone_from(&config.args);
        stdio.env = config
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        stdio.cwd = Some(
            config
                .cwd
                .clone()
                .unwrap_or_else(|| default_cwd.to_path_buf()),
        );
        let child = StdioMcpTransport::spawn(stdio).map_err(|err| err.to_string())?;
        Ok(McpClient::new(Arc::new(child)))
    } else if let Some(url) = config.url.as_deref() {
        let headers = config
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        Ok(McpClient::with_http(Arc::clone(transport), url, headers))
    } else {
        Err("missing MCP command or url".into())
    }
}
