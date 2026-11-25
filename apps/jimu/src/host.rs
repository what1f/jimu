//! Unified host runtime assembly shared across TUI, ACP, RPC, and one-shot (`-p`) modes.
//!
//! Every mode constructs its session through this single pipeline so built-in tools, deferred
//! gateway (`tool_search` / `call_tool`), QuickJS extensions, MCP servers, skills, instructions,
//! session file locking, and default system prompts stay 100% consistent regardless of interaction
//! surface.

use crate::{
    mcp::{McpServerConfig, McpServerEntry, resolve_mcp_entries},
    providers::{AuthStreamProvider, ProviderStore, ProviderStoreConfig, now_ms, parse_base_url},
    tools::{ALL_BUILTIN_TOOL_NAMES, ToolSelection},
};
use jimu_agent::{AgentMiddleware, AgentTool};
use jimu_ai::{ApiKind, Client, HttpTransport, Model, StreamProvider, ThinkingLevel};
use jimu_http::ReqwestTransport;
use jimu_js::{JsExtensionConfig, JsExtensionRuntime};
use jimu_kit::{
    BackgroundEventSender, BackgroundNotification, InstructionDiscoveryConfig, KitSession,
    KitSessionConfig, KitToolSpec, SkillDiscoveryConfig, WalkBoundary,
};
use jimu_session::{
    JsonlSessionStore, SessionEntry, SessionEntryPayload, SessionHeader, SessionModelMetadata,
    SessionStore,
};
use jimu_tools::{
    BackgroundEventCallback, BackgroundToolEvent, BuiltinToolSet, SleepTool, WakeGate,
    WebFetchConfig, WebFetchTool, WorkspaceTools, WorkspaceToolsConfig,
    format_background_events_xml,
};
#[cfg(not(target_os = "ios"))]
use jimu_tools::{
    ExecCommandTool, MonitorManager, MonitorTool, ProcessSessionManager, WriteStdinTool,
};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File, TryLockError},
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Shared configuration for launching any Jimu front-end (TUI, ACP, RPC, or one-shot `-p`).
#[derive(Clone)]
pub struct HostConfig {
    pub cwd: PathBuf,
    pub jimu_home: PathBuf,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api: Option<ApiKind>,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
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

impl Default for HostConfig {
    fn default() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let home = std::env::home_dir().unwrap_or_else(|| cwd.clone());
        Self {
            cwd,
            jimu_home: std::env::var_os("JIMU_HOME")
                .map_or_else(|| home.join(".jimu"), PathBuf::from),
            provider: None,
            model: None,
            api: None,
            base_url: None,
            api_key_env: None,
            persist_sessions: true,
            skill_dirs: Vec::new(),
            skill_walk_boundary: WalkBoundary::default(),
            instruction_discovery: Some(InstructionDiscoveryConfig::default()),
            extensions: Vec::new(),
            tools: ToolSelection::all(),
            enable_codemode: false,
            compact_tokens: None,
            context_window: None,
            max_output_tokens: None,
            thinking: ThinkingLevel::Off,
            fast: false,
            initial_mode: "default".into(),
            system_prompt: None,
            mcp_configs: Vec::new(),
            mcp_servers: BTreeMap::new(),
            provider_override: None,
            transport_override: None,
        }
    }
}

impl HostConfig {
    pub fn provider_store_config(&self) -> ProviderStoreConfig {
        ProviderStoreConfig {
            jimu_home: self.jimu_home.clone(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            api: self.api.clone(),
            base_url: self.base_url.clone(),
            api_key_env: self.api_key_env.clone(),
            context_window: self.context_window,
            max_output_tokens: self.max_output_tokens,
            provider_override: self.provider_override.is_some(),
        }
    }

    pub fn resolve_mcp_entries(
        &self,
        extra_explicit: &[(String, McpServerConfig)],
    ) -> Result<(Vec<McpServerEntry>, Vec<String>), String> {
        resolve_mcp_entries(
            &self.jimu_home,
            &self.mcp_configs,
            &self.mcp_servers,
            extra_explicit,
        )
    }
}

/// Active-branch preferences restored from a persisted JSONL session.
#[derive(Clone, Debug, Default)]
pub struct BranchPreferences {
    pub model: Option<Model>,
    pub thinking: Option<(ThinkingLevel, bool)>,
}

/// Compare two directory paths canonically (falling back to lexical equality when absent).
pub fn paths_same_dir(a: &Path, b: &Path) -> bool {
    let ca = fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let cb = fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    ca == cb
}

/// Canonicalize a JSONL session path using OS path resolution (canonicalizing the file when it
/// exists, or its parent directory + file name when it has not been created yet).
pub fn canonical_jsonl_path(jsonl_path: &Path) -> PathBuf {
    if let Ok(canon) = fs::canonicalize(jsonl_path) {
        return canon;
    }
    if let Ok(target) = fs::read_link(jsonl_path) {
        let joined = if target.is_absolute() {
            target
        } else {
            jsonl_path
                .parent()
                .map_or_else(|| target.clone(), |p| p.join(&target))
        };
        if let Ok(canon) = fs::canonicalize(&joined) {
            return canon;
        }
        if let (Some(parent), Some(name)) = (joined.parent(), joined.file_name()) {
            let base = if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            };
            if let Ok(canon_parent) = fs::canonicalize(base) {
                return canon_parent.join(name);
            }
        }
        return joined;
    }
    if let (Some(parent), Some(name)) = (jsonl_path.parent(), jsonl_path.file_name()) {
        let base = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        if let Ok(canon_parent) = fs::canonicalize(base) {
            return canon_parent.join(name);
        }
    }
    jsonl_path.to_path_buf()
}

/// Error returned when acquiring an exclusive session file lock fails.
#[derive(Debug)]
pub enum SessionLockError {
    Busy(String),
    Io(String),
}

impl std::fmt::Display for SessionLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy(msg) | Self::Io(msg) => f.write_str(msg),
        }
    }
}

/// Exclusive advisory OS file lock guarding a persisted JSONL session file across TUI, RPC, and ACP.
///
/// Resolves symlinks (including dangling symlinks) before locking `<real_dir>/<real_stem>.lock`,
/// and intentionally keeps the lock file stable on disk (never unlinking while active/closing) so
/// concurrent Unix `flock` contenders always synchronize on the same inode.
pub struct SessionFileLock {
    _file: File,
}

impl SessionFileLock {
    pub fn lock_path_for(jsonl_path: &Path) -> PathBuf {
        let real = canonical_jsonl_path(jsonl_path);
        let stem = real
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("session");
        match real.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir.join(format!("{stem}.lock")),
            _ => PathBuf::from(format!("{stem}.lock")),
        }
    }

    pub fn try_acquire_typed(jsonl_path: &Path) -> Result<Self, SessionLockError> {
        let lock_path = Self::lock_path_for(jsonl_path);
        if let Some(dir) = lock_path.parent()
            && !dir.as_os_str().is_empty()
        {
            fs::create_dir_all(dir).map_err(|err| {
                SessionLockError::Io(format!("failed to open session directory: {err}"))
            })?;
        }
        let mut options = fs::OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&lock_path).map_err(|err| {
            SessionLockError::Io(format!(
                "failed to lock session `{}`: {err}",
                jsonl_path.display()
            ))
        })?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => Err(SessionLockError::Busy(format!(
                "session `{}` is already open in another jimu instance",
                jsonl_path.display()
            ))),
            Err(err) => Err(SessionLockError::Io(format!(
                "failed to lock session `{}`: {err}",
                jsonl_path.display()
            ))),
        }
    }

    pub fn try_acquire(jsonl_path: &Path) -> Result<Self, String> {
        Self::try_acquire_typed(jsonl_path).map_err(|err| err.to_string())
    }
}

/// Returns `Ok(true)` if the resolved JSONL session file already exists and has non-empty content,
/// `Ok(false)` for non-existent or 0-byte placeholder files, or `Err` on I/O/type errors.
pub fn has_existing_session_content(jsonl_path: &Path) -> Result<bool, String> {
    let real = canonical_jsonl_path(jsonl_path);
    match fs::metadata(&real) {
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
        Err(err) => Err(format!(
            "failed to inspect session `{}`: {err}",
            jsonl_path.display()
        )),
        Ok(meta) if !meta.is_file() => Err(format!(
            "session path `{}` is not a regular file",
            jsonl_path.display()
        )),
        Ok(meta) => Ok(meta.len() > 0),
    }
}

/// Read the recorded working directory from a persisted JSONL session header, if present.
pub fn read_session_cwd(jsonl_path: &Path) -> Option<PathBuf> {
    let real = canonical_jsonl_path(jsonl_path);
    let store = JsonlSessionStore::new(&real);
    let (header, _) = store.load().ok()?;
    header.and_then(|h| h.cwd).map(PathBuf::from)
}

/// Resolve and validate the working directory for a target JSONL session file.
///
/// Only confirmed non-existent (or 0-byte empty) new files fall back to `fallback_cwd`. Any
/// existing non-empty session file MUST be readable, MUST include a `cwd` header, and that
/// directory MUST exist — otherwise an error is returned rather than silently binding workspace
/// tools to the caller's directory.
pub fn resolve_existing_session_cwd(
    jsonl_path: &Path,
    fallback_cwd: &Path,
) -> Result<PathBuf, String> {
    let real = canonical_jsonl_path(jsonl_path);
    if !has_existing_session_content(&real)? {
        return Ok(fallback_cwd.to_path_buf());
    }
    let store = JsonlSessionStore::new(&real);
    let (header, _) = store.load().map_err(|err| {
        format!(
            "failed to read existing session `{}`: {err}",
            jsonl_path.display()
        )
    })?;
    let Some(recorded) = header.and_then(|h| h.cwd).map(PathBuf::from) else {
        return Err(format!(
            "existing session `{}` is missing a working directory (`cwd`) header",
            jsonl_path.display()
        ));
    };
    if !recorded.is_absolute() {
        return Err(format!(
            "existing session `{}` records a relative working directory `{}`; an absolute path is required",
            jsonl_path.display(),
            recorded.display()
        ));
    }
    if !recorded.is_dir() {
        return Err(format!(
            "session `{}` references unavailable working directory `{}`",
            jsonl_path.display(),
            recorded.display()
        ));
    }
    Ok(recorded)
}

/// Resolve persisted [`SessionModelMetadata`] into a runnable [`Model`], using full recorded
/// non-secret routing (`api`, `base_url`, capabilities) when present and falling back to `store`
/// only for legacy bare `(provider, model_id)` records.
pub fn resolve_model_metadata(
    meta: Option<&SessionModelMetadata>,
    store: &ProviderStore,
) -> Option<Model> {
    let meta = meta?;
    Some(
        meta.to_full_model()
            .unwrap_or_else(|| store.model(&meta.provider, &meta.model_id)),
    )
}

/// Restore the latest effective model (preserving explicit custom endpoints) and thinking/fast
/// settings recorded on the session root header and active branch.
pub fn restore_branch_preferences(
    header: Option<&SessionHeader>,
    branch_entries: &[SessionEntry],
    store: &ProviderStore,
) -> BranchPreferences {
    let mut latest_model = header.and_then(|h| h.initial_model.clone());
    let mut latest_thinking = header.and_then(|h| {
        h.initial_thinking
            .map(|level| (level, h.initial_fast.unwrap_or(false)))
    });

    for entry in branch_entries {
        match &entry.payload {
            SessionEntryPayload::ModelChange {
                provider,
                model_id,
                api,
                base_url,
                context_window,
                max_output_tokens,
                supports_images,
                supports_reasoning,
            } => {
                latest_model = Some(SessionModelMetadata {
                    provider: provider.clone(),
                    model_id: model_id.clone(),
                    api: api.clone(),
                    base_url: base_url.clone(),
                    context_window: *context_window,
                    max_output_tokens: *max_output_tokens,
                    supports_images: *supports_images,
                    supports_reasoning: *supports_reasoning,
                });
            }
            SessionEntryPayload::ThinkingChange { level, fast } => {
                latest_thinking = Some((*level, *fast));
            }
            _ => {}
        }
    }

    BranchPreferences {
        model: resolve_model_metadata(latest_model.as_ref(), store),
        thinking: latest_thinking,
    }
}

/// Restore the latest model (including custom `api`/`base_url` if recorded) on `session`'s active branch.
pub fn restore_branch_model(session: &KitSession, store: &ProviderStore) -> Option<Model> {
    resolve_model_metadata(session.reconstructed_model_metadata().as_ref(), store)
}

/// Validate that a custom model endpoint URL is `http`/`https` with no embedded credentials,
/// query string, or fragment so secrets are never persisted into session history.
pub fn validate_base_url(raw: &str) -> Result<String, String> {
    parse_base_url(raw).map(|_| raw.to_owned()).ok_or_else(|| {
        "invalid base_url: must be an http/https URL without credentials, query string, or fragment"
            .to_owned()
    })
}

/// Atomically persist ONE complete non-secret `ModelChange` record and switch the active model
/// only after persistence succeeds.
pub fn set_model_atomic(session: &KitSession, model: Model) -> Result<(), String> {
    if !model.base_url.is_empty() {
        validate_base_url(&model.base_url)?;
    }
    session.set_model(model).map_err(|err| err.to_string())
}

/// Synchronize a newly opened or reopened session's active model with either the active branch's
/// recorded route (plus optional CLI `--api`/`--base-url`/`--context-window` overrides) or an
/// explicit `--provider`/`--model` override.
pub fn apply_session_model_on_open(
    session: &KitSession,
    provider_store: &ProviderStore,
    requested_model: &Model,
    restore_from_branch: bool,
    api_override: Option<ApiKind>,
    base_url_override: Option<String>,
    context_window_override: Option<u32>,
) -> Result<(), String> {
    if restore_from_branch {
        if let Some(restored) = restore_branch_model(session, provider_store) {
            let mut effective = restored.clone();
            if let Some(api) = api_override {
                effective.api = api;
            }
            if let Some(url) = base_url_override {
                effective.base_url = url;
            }
            if let Some(cw) = context_window_override {
                effective.context_window = cw;
            }
            let overridden = effective.api != restored.api
                || effective.base_url != restored.base_url
                || effective.context_window != restored.context_window;
            if overridden {
                set_model_atomic(session, effective)?;
            } else {
                session.restore_model(restored).map_err(|e| e.to_string())?;
            }
        }
    } else {
        let same_route = restore_branch_model(session, provider_store).is_some_and(|recorded| {
            recorded.provider == requested_model.provider
                && recorded.id == requested_model.id
                && recorded.api == requested_model.api
                && recorded.base_url == requested_model.base_url
                && recorded.context_window == requested_model.context_window
        });
        if !same_route && !requested_model.id.trim().is_empty() {
            set_model_atomic(session, requested_model.clone())?;
        }
    }
    Ok(())
}

/// Build the shared HTTP transport (or reuse an injected test override).
pub fn build_http_transport(
    override_transport: Option<Arc<dyn HttpTransport>>,
) -> Result<Arc<dyn HttpTransport>, String> {
    match override_transport {
        Some(transport) => Ok(transport),
        None => Ok(Arc::new(ReqwestTransport::new().map_err(|err| {
            format!("failed to initialize HTTP transport: {err}")
        })?)),
    }
}

/// Wrap the base streaming client with shared `providers.json` + `auth.json` resolution.
pub fn build_auth_provider(
    store: Arc<ProviderStore>,
    transport: Arc<dyn HttpTransport>,
    provider_override: Option<Arc<dyn StreamProvider>>,
    allow_override: bool,
    preserve_explicit_endpoint: bool,
) -> Arc<dyn StreamProvider> {
    let inner_provider: Arc<dyn StreamProvider> =
        provider_override.unwrap_or_else(|| Arc::new(Client::new(Arc::clone(&transport))));
    Arc::new(AuthStreamProvider {
        store,
        provider: inner_provider,
        transport,
        allow_override,
        preserve_explicit_endpoint,
    })
}

/// Build unified [`SkillDiscoveryConfig`] including `.jimu/skills`, `~/.jimu/skills`, CLI roots,
/// and any extra workspace directories.
pub fn build_skill_discovery(
    cwd: &Path,
    jimu_home: &Path,
    walk_boundary: WalkBoundary,
    extra_skill_dirs: &[PathBuf],
    additional_directories: &[PathBuf],
) -> SkillDiscoveryConfig {
    SkillDiscoveryConfig {
        cwd: Some(cwd.to_path_buf()),
        walk_boundary,
        project_skill_dirs: vec![".jimu/skills".into()],
        user_skill_dirs: vec![jimu_home.join("skills")],
        extra_roots: extra_skill_dirs
            .iter()
            .cloned()
            .chain(
                additional_directories
                    .iter()
                    .flat_map(|path| [path.join(".agents/skills"), path.join(".jimu/skills")]),
            )
            .collect(),
        ..SkillDiscoveryConfig::default()
    }
}

/// Build unified [`InstructionDiscoveryConfig`] with `~/.jimu` prioritized first in `user_dirs`.
pub fn build_instruction_discovery(
    base: Option<&InstructionDiscoveryConfig>,
    cwd: &Path,
    jimu_home: &Path,
    additional_directories: &[PathBuf],
) -> Option<InstructionDiscoveryConfig> {
    base.cloned().map(|mut discovery| {
        discovery.cwd = Some(cwd.to_path_buf());
        discovery
            .additional_directories
            .extend(additional_directories.iter().cloned());
        if !discovery.user_dirs.iter().any(|dir| dir == jimu_home) {
            discovery.user_dirs.insert(0, jimu_home.to_path_buf());
        }
        discovery
    })
}

/// Format workspace background tool events into [`BackgroundNotification`]s for a session inbox.
pub fn make_background_callback<F>(resolve_sender: F) -> BackgroundEventCallback
where
    F: Fn() -> Option<BackgroundEventSender> + Send + Sync + 'static,
{
    Arc::new(move |event: BackgroundToolEvent| {
        let Some(sender) = resolve_sender() else {
            return;
        };
        let (source_id, kind, summary) = match &event {
            BackgroundToolEvent::MonitorLines { id, name, .. } => (
                id.clone(),
                "monitor_lines".to_string(),
                format!("Monitor {name} produced output"),
            ),
            BackgroundToolEvent::MonitorExited { id, name, .. } => (
                id.clone(),
                "monitor_exited".to_string(),
                format!("Monitor {name} exited"),
            ),
            BackgroundToolEvent::ShellExited {
                session_id,
                command,
                ..
            } => (
                session_id.to_string(),
                "shell_exited".to_string(),
                format!("Shell command exited: {command}"),
            ),
        };
        let xml_payload = format_background_events_xml(&[event]);
        sender.send(BackgroundNotification {
            source_id,
            kind,
            summary,
            xml_payload,
            timestamp_ms: now_ms(),
        });
    })
}

/// Built-in tools and managed background process handles produced by [`build_workspace_tools`].
#[derive(Default)]
pub struct BuiltWorkspaceTools {
    pub tools: Vec<Arc<dyn AgentTool>>,
    #[cfg(not(target_os = "ios"))]
    pub processes: Option<ProcessSessionManager>,
    #[cfg(not(target_os = "ios"))]
    pub monitors: Option<MonitorManager>,
}

impl BuiltWorkspaceTools {
    pub async fn shutdown(&self) {
        #[cfg(not(target_os = "ios"))]
        {
            if let Some(processes) = &self.processes {
                processes.shutdown().await;
            }
            if let Some(monitors) = &self.monitors {
                monitors.shutdown().await;
            }
        }
    }
}

/// Build the configured subset of built-in workspace tools (`read`, `write`, `edit`, `apply_patch`,
/// `find`, `grep`, `exec_command`, `write_stdin`, `monitor`, optional `sleep`, and `web_fetch`).
pub fn build_workspace_tools(
    cwd: &Path,
    transport: &Arc<dyn HttpTransport>,
    selection: &ToolSelection,
    include_sleep: bool,
    #[cfg_attr(target_os = "ios", allow(unused_variables))] bg_callback: Option<
        BackgroundEventCallback,
    >,
    existing_web_fetch: Option<Arc<WebFetchTool>>,
) -> BuiltWorkspaceTools {
    if selection.is_empty() {
        return BuiltWorkspaceTools::default();
    }

    let enabled = selection.workspace_tool_set();
    let workspace = WorkspaceToolsConfig::new(cwd)
        .with_enabled_tools(enabled.clone())
        .with_env_provider(Arc::new(|| {
            Box::pin(async {
                if std::env::var_os("AI_AGENT").is_none() {
                    vec![("AI_AGENT".to_string(), "jimu".to_string())]
                } else {
                    Vec::new()
                }
            })
        }));

    let mut tools: Vec<Arc<dyn AgentTool>> = if enabled == BuiltinToolSet::empty() {
        Vec::new()
    } else {
        WorkspaceTools::build(workspace.clone())
    };

    let wake_gate = WakeGate::new();

    #[cfg(not(target_os = "ios"))]
    let mut local_processes = None;
    #[cfg(not(target_os = "ios"))]
    let mut local_monitors = None;

    #[cfg(not(target_os = "ios"))]
    {
        let workspace = Arc::new(workspace);
        if selection.uses_process_manager() {
            let processes = ProcessSessionManager::new(
                Arc::clone(&workspace),
                wake_gate.clone(),
                bg_callback.clone(),
            );
            if selection.is_enabled("exec_command") {
                tools.push(Arc::new(ExecCommandTool::new(processes.clone())));
            }
            if selection.is_enabled("write_stdin") {
                tools.push(Arc::new(WriteStdinTool::new(processes.clone())));
            }
            local_processes = Some(processes);
        }

        if selection.uses_monitor_manager() {
            let monitors = MonitorManager::new(workspace, wake_gate.clone(), bg_callback);
            tools.push(Arc::new(MonitorTool::new(monitors.clone())));
            local_monitors = Some(monitors);
        }
    }

    if include_sleep && selection.is_enabled("sleep") {
        tools.push(Arc::new(SleepTool::new(wake_gate)));
    }

    if selection.is_enabled("web_fetch") {
        let tool = existing_web_fetch.unwrap_or_else(|| {
            Arc::new(WebFetchTool::new(
                Arc::clone(transport),
                WebFetchConfig::default(),
            ))
        });
        tools.push(tool);
    }

    BuiltWorkspaceTools {
        tools,
        #[cfg(not(target_os = "ios"))]
        processes: local_processes,
        #[cfg(not(target_os = "ios"))]
        monitors: local_monitors,
    }
}

/// Loaded QuickJS extensions runtime and its validated tools.
pub struct LoadedExtensions {
    pub runtime: Arc<JsExtensionRuntime>,
    pub tools: Vec<Arc<dyn AgentTool>>,
}

/// Reserved names that QuickJS extensions may never claim (gateway tools + all built-in tools,
/// even when a built-in tool is disabled via `--disable-tools`).
pub fn reserved_extension_tool_names(extra_existing: &HashSet<String>) -> HashSet<String> {
    let mut forbidden = extra_existing.clone();
    forbidden.extend(["exec", "call_tool", "tool_search"].map(str::to_owned));
    forbidden.extend(ALL_BUILTIN_TOOL_NAMES.iter().map(|name| (*name).to_owned()));
    forbidden
}

/// Validate extension tool names against reserved gateway/built-in names and existing tools.
pub fn validate_extension_tools(
    tools: &[Arc<dyn AgentTool>],
    existing_names: &HashSet<String>,
) -> Result<(), String> {
    let mut seen = reserved_extension_tool_names(existing_names);
    for tool in tools {
        let name = tool.definition().name;
        if !seen.insert(name.clone()) {
            return Err(format!(
                "extension tool `{name}` uses a reserved or duplicate name"
            ));
        }
    }
    Ok(())
}

/// Load QuickJS extensions and validate their registered tool names.
pub fn load_js_extensions(
    paths: &[PathBuf],
    existing_names: &HashSet<String>,
) -> Result<Option<LoadedExtensions>, String> {
    if paths.is_empty() {
        return Ok(None);
    }
    let js_cfg = paths
        .iter()
        .fold(JsExtensionConfig::new(), |cfg, path| cfg.with_file(path));
    let runtime = Arc::new(
        JsExtensionRuntime::new(js_cfg)
            .map_err(|err| format!("failed to load JS extensions: {err}"))?,
    );
    let tools = runtime.tools();
    validate_extension_tools(&tools, existing_names)?;
    Ok(Some(LoadedExtensions { runtime, tools }))
}

/// Attach loaded JS extensions onto a [`KitSessionConfig`] (tools as deferred + hooks middleware).
pub fn attach_extensions_to_config(
    config: &mut KitSessionConfig,
    extensions: &LoadedExtensions,
) -> Result<(), String> {
    let existing_names: HashSet<String> = config
        .tools
        .iter()
        .map(|spec| spec.tool.definition().name)
        .collect();
    validate_extension_tools(&extensions.tools, &existing_names)?;
    for tool in &extensions.tools {
        config.tools.push(KitToolSpec::deferred(Arc::clone(tool)));
    }
    config
        .middlewares
        .push(Arc::clone(&extensions.runtime) as Arc<dyn AgentMiddleware>);
    Ok(())
}

/// Consistent deferred gateway policy shared by all modes:
/// expose resident `tool_search` & `call_tool` whenever tools, codemode, extensions, or MCP are
/// enabled; omit them in pure `--no-tools` chat-only runs.
pub fn should_enable_deferred_gateway(
    selection: &ToolSelection,
    enable_codemode: bool,
    has_extensions: bool,
    has_mcp: bool,
) -> bool {
    !selection.is_empty() || enable_codemode || has_extensions || has_mcp
}

/// Materialize built-in user docs (when `read` is enabled and no custom prompt was provided) and
/// resolve the base system prompt.
pub fn resolve_system_prompt(
    explicit: Option<&str>,
    cwd: &Path,
    jimu_home: &Path,
    selection: &ToolSelection,
    has_any_tools: bool,
) -> String {
    if let Some(prompt) = explicit {
        return prompt.to_owned();
    }
    let docs_dir = if selection.is_enabled("read") {
        crate::docs::ensure_user_docs(jimu_home)
    } else {
        None
    };
    crate::prompt::build_default_system_prompt(cwd, has_any_tools, docs_dir.as_deref())
}
