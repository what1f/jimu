//! Interactive terminal UI host (`--features tui`), built on the shared [`HostConfig`]
//! assembly pipeline used across TUI, ACP, RPC, and one-shot (`-p`) modes.

use crate::{
    host::{
        BuiltWorkspaceTools, HostConfig, LoadedExtensions, SessionFileLock,
        apply_session_model_on_open, attach_extensions_to_config, build_auth_provider,
        build_http_transport, build_instruction_discovery, build_skill_discovery,
        build_workspace_tools, canonical_jsonl_path, load_js_extensions, make_background_callback,
        paths_same_dir, resolve_existing_session_cwd, resolve_system_prompt,
        should_enable_deferred_gateway,
    },
    mcp::McpHub,
    providers::{ProviderStore, model_key},
    settings::UserSettings,
};
use jimu_agent::{
    AgentMessage, AgentMiddleware, BeforeToolCallCtx, BeforeToolCallResult, QueueMode,
};
use jimu_ai::{BoxFuture, CancelToken, ContentPart, HttpTransport, Message, Model, StreamProvider};
#[cfg(not(target_os = "ios"))]
use jimu_kit::SkillWatcher;
use jimu_kit::{BackgroundEventSender, KitSession, KitSessionConfig, KitToolSpec};
use jimu_session::{JsonlSessionStore, SessionEntryPayload, SessionStore};
#[cfg(not(target_os = "ios"))]
use jimu_tools::{MonitorManager, ProcessSessionManager};
use jimu_tui::{
    app::{ScreenMode, TuiApp, TuiAppConfig},
    engine::theme::ThemeKind,
    harness::{
        BoxFuture as TuiBoxFuture, ChromeConfig, ChromeSlots, ChromeState, CompletionItem, McpCap,
        McpServerSummary, NoticeLevel, PermissionModeCap, PermissionModeOption, SessionOption,
        SlashCommand, SlashContext,
    },
    jimu::{JimuApprovalMiddleware, JimuHarness, JimuInteractionSink, JimuSessionController},
};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, RwLock},
    time::UNIX_EPOCH,
};
use uuid::Uuid;

/// Launch configuration for interactive TUI mode, embedding [`HostConfig`] so TUI, ACP, RPC,
/// and `-p` share one model, auth, tool, MCP, skill, instruction, and session pipeline.
#[derive(Clone)]
pub struct TuiLaunchConfig {
    pub host: HostConfig,
    pub session_file: Option<PathBuf>,
    pub continue_last: bool,
    pub open_resume_picker: bool,
    pub theme: Option<String>,
    pub initial_prompt: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionMode {
    Default,
    AcceptEdits,
    BypassPermissions,
}

impl PermissionMode {
    const ALL: [Self; 3] = [Self::Default, Self::AcceptEdits, Self::BypassPermissions];

    fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim() {
            "default" => Ok(Self::Default),
            "acceptEdits" => Ok(Self::AcceptEdits),
            "bypassPermissions" => Ok(Self::BypassPermissions),
            other => Err(format!(
                "unsupported permission mode `{other}` (expected default, acceptEdits, or bypassPermissions)"
            )),
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::BypassPermissions => "bypassPermissions",
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::Default => "Ask before edits and commands",
            Self::AcceptEdits => "Auto-approve file edits",
            Self::BypassPermissions => "Auto-approve all tools",
        }
    }
}

struct SharedMcpCap {
    hub: Arc<McpHub>,
}

impl McpCap for SharedMcpCap {
    fn list_mcp_servers(&self) -> Vec<McpServerSummary> {
        self.hub
            .summaries()
            .into_iter()
            .map(|(name, tool_count)| McpServerSummary {
                name,
                status: "●".into(),
                tool_count,
            })
            .collect()
    }
}

struct SharedPermissionMode {
    mode: Arc<Mutex<PermissionMode>>,
    jimu_home: PathBuf,
}

impl PermissionModeCap for SharedPermissionMode {
    fn current_mode(&self) -> String {
        self.mode
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .label()
            .to_owned()
    }

    fn available_modes(&self) -> Vec<PermissionModeOption> {
        PermissionMode::ALL
            .iter()
            .map(|m| PermissionModeOption {
                id: m.label().to_owned(),
                description: m.description().to_owned(),
            })
            .collect()
    }

    fn set_mode(&self, mode: &str) -> Result<(), String> {
        let parsed = PermissionMode::parse(mode)?;
        *self.mode.lock().unwrap_or_else(|e| e.into_inner()) = parsed;
        let _ = UserSettings::update(&self.jimu_home, |settings| {
            settings.default_permission_mode = Some(parsed.label().to_owned());
        });
        Ok(())
    }
}

/// Per-session permission middleware supporting live `/permissions` and `Shift+Tab` switching
/// while keeping "always allow" grants scoped to the active session.
struct DynamicPermissionMiddleware {
    approval: JimuApprovalMiddleware,
}

impl DynamicPermissionMiddleware {
    fn new(
        mode: Arc<Mutex<PermissionMode>>,
        enabled_builtins: Arc<HashSet<String>>,
        sink: JimuInteractionSink,
    ) -> Self {
        let approval = JimuApprovalMiddleware::new(sink, move |ctx| {
            let current = *mode.lock().unwrap_or_else(|e| e.into_inner());
            is_approval_required(current, ctx.tool_call.name.as_str(), &enabled_builtins)
        });
        Self { approval }
    }
}

impl AgentMiddleware for DynamicPermissionMiddleware {
    fn before_tool_call<'a>(
        &'a self,
        ctx: BeforeToolCallCtx<'a>,
        cancel: CancelToken,
    ) -> BoxFuture<'a, Option<BeforeToolCallResult>> {
        self.approval.before_tool_call(ctx, cancel)
    }
}

struct DynamicChromeSlots {
    active_session: Arc<RwLock<Arc<KitSession>>>,
}

impl ChromeSlots for DynamicChromeSlots {
    fn status_flags(&self, _state: &ChromeState<'_>) -> Vec<String> {
        let session = self
            .active_session
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if session.fast() {
            vec!["fast".into()]
        } else {
            Vec::new()
        }
    }
}

struct FastCommand {
    active_session: Arc<RwLock<Arc<KitSession>>>,
    jimu_home: PathBuf,
}

impl SlashCommand for FastCommand {
    fn name(&self) -> &str {
        "fast"
    }

    fn description(&self) -> &str {
        "Toggle fast mode (/fast [on|off])"
    }

    fn completions(&self, query: &str) -> Vec<CompletionItem> {
        let q = query.to_ascii_lowercase();
        let active = self
            .active_session
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .fast();
        [("on", active), ("off", !active)]
            .into_iter()
            .filter(|(label, _)| label.contains(&q))
            .map(|(label, is_cur)| CompletionItem {
                token: label.to_owned(),
                description: is_cur.then(|| "●".to_owned()),
            })
            .collect()
    }

    fn run(&self, ctx: SlashContext) -> TuiBoxFuture<'_, ()> {
        Box::pin(async move {
            let session = self
                .active_session
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let next = match ctx.args.trim().to_ascii_lowercase().as_str() {
                "" => !session.fast(),
                "on" | "true" | "1" => true,
                "off" | "false" | "0" => false,
                other => {
                    ctx.controller.push_notice(
                        NoticeLevel::Warning,
                        format!("invalid /fast argument `{other}` (use on or off)"),
                    );
                    return;
                }
            };
            match session.set_fast(next) {
                Ok(()) => {
                    let _ = UserSettings::update(&self.jimu_home, |settings| {
                        settings.fast = Some(next);
                    });
                    ctx.controller.push_notice(
                        NoticeLevel::Info,
                        if next {
                            "Fast mode: on"
                        } else {
                            "Fast mode: off"
                        },
                    );
                    ctx.controller.request_redraw();
                }
                Err(err) => ctx
                    .controller
                    .push_notice(NoticeLevel::Error, err.to_string()),
            }
        })
    }
}

struct BuiltTuiSession {
    session: Arc<KitSession>,
    built_tools: Arc<BuiltWorkspaceTools>,
    warnings: Vec<String>,
}

struct SessionTemplate {
    host: HostConfig,
    transport: Arc<dyn HttpTransport>,
    provider_store: Arc<ProviderStore>,
    provider: Arc<dyn StreamProvider>,
    active_model: Model,
    explicit_model_override: bool,
    shared_mode: Arc<Mutex<PermissionMode>>,
    sink: JimuInteractionSink,
    extensions: Option<Arc<LoadedExtensions>>,
    mcp_hub: Arc<McpHub>,
}

impl SessionTemplate {
    async fn build(
        &self,
        store: Option<Arc<dyn SessionStore>>,
        restore_model: bool,
    ) -> Result<BuiltTuiSession, String> {
        let host = &self.host;
        let cwd = &host.cwd;

        let mut opts = KitSessionConfig::new(self.active_model.clone());
        opts.store = store;
        opts.cwd = Some(cwd.to_string_lossy().into_owned());
        opts.thinking_level = host.thinking;
        opts.fast = host.fast;
        opts.enable_codemode = host.enable_codemode;
        opts.steering_mode = QueueMode::All;
        opts.compaction.compaction_reserve_tokens = host.compact_tokens;
        opts.skill_discovery = build_skill_discovery(
            cwd,
            &host.jimu_home,
            host.skill_walk_boundary,
            &host.skill_dirs,
            &[],
        );
        opts.instruction_discovery = build_instruction_discovery(
            host.instruction_discovery.as_ref(),
            cwd,
            &host.jimu_home,
            &[],
        );

        // Each session gets its own dedicated background sender and workspace process/monitor
        // managers so switching sessions never leaks events or background tasks across sessions.
        let bg_sender: Arc<OnceLock<BackgroundEventSender>> = Arc::new(OnceLock::new());
        let bg_slot = Arc::clone(&bg_sender);
        let bg_callback = make_background_callback(move || bg_slot.get().cloned());
        let built_tools = build_workspace_tools(
            cwd,
            &self.transport,
            &host.tools,
            true,
            Some(bg_callback),
            None,
        );
        let enabled_builtins: Arc<HashSet<String>> = Arc::new(
            built_tools
                .tools
                .iter()
                .map(|tool| tool.definition().name)
                .collect(),
        );
        opts.tools
            .extend(built_tools.tools.iter().cloned().map(KitToolSpec::resident));

        let has_extensions = if let Some(loaded) = &self.extensions {
            if let Err(err) = attach_extensions_to_config(&mut opts, loaded) {
                built_tools.shutdown().await;
                return Err(err);
            }
            true
        } else {
            false
        };

        // Install approval middleware AFTER JS extensions so user approval always evaluates the
        // final post-hook arguments (matching ACP middleware ordering). Each session receives its
        // own JimuApprovalMiddleware instance so "always allow" grants never leak across sessions.
        opts.middlewares
            .push(Arc::new(DynamicPermissionMiddleware::new(
                Arc::clone(&self.shared_mode),
                enabled_builtins,
                self.sink.clone(),
            )));

        let has_mcp = !self.mcp_hub.is_empty();
        opts.enable_deferred_gateway = should_enable_deferred_gateway(
            &host.tools,
            host.enable_codemode,
            has_extensions,
            has_mcp,
        );
        let has_tools = !opts.tools.is_empty() || host.enable_codemode || has_mcp;
        opts.system_prompt = resolve_system_prompt(
            host.system_prompt.as_deref(),
            cwd,
            &host.jimu_home,
            &host.tools,
            has_tools,
        );

        let requested_model = opts.model.clone();
        let session = match KitSession::new(Arc::clone(&self.provider), opts) {
            Ok(session) => Arc::new(session),
            Err(err) => {
                built_tools.shutdown().await;
                return Err(format!("failed to initialize session: {err}"));
            }
        };
        let _ = bg_sender.set(session.background_sender());

        if let Err(err) = apply_session_model_on_open(
            &session,
            &self.provider_store,
            &requested_model,
            restore_model && !self.explicit_model_override,
            self.host.api.clone(),
            self.host.base_url.clone(),
            self.host.context_window,
        ) {
            built_tools.shutdown().await;
            return Err(err);
        }

        let warnings = if !self.mcp_hub.is_empty() {
            match self
                .mcp_hub
                .attach_to_session(&session, CancelToken::new())
                .await
            {
                Ok(warnings) => warnings,
                Err(err) => {
                    built_tools.shutdown().await;
                    return Err(err.to_string());
                }
            }
        } else {
            Vec::new()
        };

        Ok(BuiltTuiSession {
            session,
            built_tools: Arc::new(built_tools),
            warnings,
        })
    }
}

struct ActiveSessionState {
    id: String,
    path: Option<PathBuf>,
    auto_created: bool,
    _file_lock: Option<SessionFileLock>,
    built_tools: Arc<BuiltWorkspaceTools>,
}

struct TuiSessionManager {
    template: SessionTemplate,
    sessions_dir: Option<PathBuf>,
    cwd: PathBuf,
    active_session: Arc<RwLock<Arc<KitSession>>>,
    #[cfg(not(target_os = "ios"))]
    skill_watcher: Mutex<Option<SkillWatcher>>,
    state: Mutex<ActiveSessionState>,
    switch_lock: tokio::sync::Mutex<()>,
}

impl TuiSessionManager {
    fn cleanup_empty_session(session: &KitSession, state: &ActiveSessionState) {
        if !state.auto_created {
            return;
        }
        let Some(path) = &state.path else {
            return;
        };
        // Check the entire session tree rather than `visible_messages()` so rewinding before the
        // first user turn never deletes a file containing sibling branches. Keep the advisory
        // `.lock` file stable on disk so concurrent Unix `flock` callers never race on inodes.
        let has_any_message = session
            .entries()
            .iter()
            .any(|entry| matches!(entry.payload, SessionEntryPayload::Message { .. }));
        if !has_any_message {
            let _ = fs::remove_file(path);
        }
    }

    async fn shutdown_current(&self) {
        let _guard = self.switch_lock.lock().await;
        let session = self
            .active_session
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let tools = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            Self::cleanup_empty_session(&session, &state);
            Arc::clone(&state.built_tools)
        };
        tools.shutdown().await;
    }

    async fn activate(&self, next_session: Arc<KitSession>, next_state: ActiveSessionState) {
        // Shut down the outgoing session's background tools BEFORE mutating active state so
        // cancelling the switch future at the await point never leaves active_session, file lock,
        // and harness diverged. The actual swap below is synchronous with zero await points.
        let prev_tools = Arc::clone(
            &self
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .built_tools,
        );
        prev_tools.shutdown().await;

        let prev_session = {
            let mut slot = self
                .active_session
                .write()
                .unwrap_or_else(|e| e.into_inner());
            std::mem::replace(&mut *slot, Arc::clone(&next_session))
        };
        let prev_state = {
            let mut slot = self.state.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::replace(&mut *slot, next_state)
        };
        #[cfg(not(target_os = "ios"))]
        {
            *self.skill_watcher.lock().unwrap_or_else(|e| e.into_inner()) =
                next_session.watch_skills(|_| {}).ok();
        }
        Self::cleanup_empty_session(&prev_session, &prev_state);
    }
}

impl JimuSessionController for TuiSessionManager {
    fn current_session_id(&self) -> Option<String> {
        Some(
            self.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .id
                .clone(),
        )
    }

    #[cfg(not(target_os = "ios"))]
    fn active_process_manager(&self) -> Option<ProcessSessionManager> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .built_tools
            .processes
            .clone()
    }

    #[cfg(not(target_os = "ios"))]
    fn active_monitor_manager(&self) -> Option<MonitorManager> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .built_tools
            .monitors
            .clone()
    }

    fn list_sessions(&self) -> TuiBoxFuture<'_, Result<Vec<SessionOption>, String>> {
        Box::pin(async move {
            let Some(dir) = &self.sessions_dir else {
                return Ok(Vec::new());
            };
            let current_id = self
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .id
                .clone();
            Ok(scan_sessions(dir, &current_id, &self.cwd))
        })
    }

    fn open_session(&self, session_id: &str) -> TuiBoxFuture<'_, Result<Arc<KitSession>, String>> {
        let id = session_id.to_owned();
        Box::pin(async move {
            let _guard = self.switch_lock.lock().await;
            let dir = self
                .sessions_dir
                .as_ref()
                .ok_or_else(|| "session persistence is disabled".to_owned())?;
            let raw_path = dir.join(format!("{id}.jsonl"));
            let path = canonical_jsonl_path(&raw_path);
            if !path.is_file() {
                return Err(format!("session `{id}` not found"));
            }
            // If reopening the already-active session, return it without re-acquiring the lock.
            {
                let cur = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if cur.id == id {
                    return Ok(self
                        .active_session
                        .read()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone());
                }
            }
            let file_lock = SessionFileLock::try_acquire(&path)?;
            let recorded_cwd = resolve_existing_session_cwd(&path, &self.cwd)?;
            if !paths_same_dir(&recorded_cwd, &self.cwd) {
                return Err(format!(
                    "session `{id}` belongs to a different directory (`{}`); reopen jimu with `-C`",
                    recorded_cwd.display()
                ));
            }
            let store: Arc<dyn SessionStore> = Arc::new(JsonlSessionStore::new(&path));
            let built = self.template.build(Some(store), true).await?;
            for warning in &built.warnings {
                eprintln!("warning: {warning}");
            }
            self.activate(
                Arc::clone(&built.session),
                ActiveSessionState {
                    id,
                    path: Some(path),
                    auto_created: false,
                    _file_lock: Some(file_lock),
                    built_tools: built.built_tools,
                },
            )
            .await;
            Ok(built.session)
        })
    }

    fn create_session(&self) -> TuiBoxFuture<'_, Result<Arc<KitSession>, String>> {
        Box::pin(async move {
            let _guard = self.switch_lock.lock().await;
            let id = Uuid::now_v7().to_string();
            let (store, path, auto_created, file_lock) = if let Some(dir) = &self.sessions_dir {
                fs::create_dir_all(dir)
                    .map_err(|err| format!("failed to open session directory: {err}"))?;
                let file = dir.join(format!("{id}.jsonl"));
                let lock = SessionFileLock::try_acquire(&file)?;
                (
                    Some(Arc::new(JsonlSessionStore::new(&file)) as Arc<dyn SessionStore>),
                    Some(file),
                    true,
                    Some(lock),
                )
            } else {
                (None, None, false, None)
            };
            let built = self.template.build(store, false).await?;
            for warning in &built.warnings {
                eprintln!("warning: {warning}");
            }
            self.activate(
                Arc::clone(&built.session),
                ActiveSessionState {
                    id,
                    path,
                    auto_created,
                    _file_lock: file_lock,
                    built_tools: built.built_tools,
                },
            )
            .await;
            Ok(built.session)
        })
    }
}

fn scan_sessions(dir: &Path, current_id: &str, cwd: &Path) -> Vec<SessionOption> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut rows: Vec<(u64, SessionOption)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(id) = name.strip_suffix(".jsonl") else {
            continue;
        };
        let store = JsonlSessionStore::new(&path);
        let Ok((header, items)) = store.load() else {
            continue;
        };
        if let Some(header_cwd) = header.as_ref().and_then(|h| h.cwd.as_deref())
            && !paths_same_dir(Path::new(header_cwd), cwd)
        {
            continue;
        }
        let message_count = items
            .iter()
            .filter(|item| matches!(item.payload, SessionEntryPayload::Message { .. }))
            .count();
        if message_count == 0 && id != current_id {
            continue;
        }
        let meta_title = read_meta_title(&dir.join(format!("{id}.meta.json")));
        let first_user = items.iter().find_map(|item| match &item.payload {
            SessionEntryPayload::Message {
                message: AgentMessage::Llm(Message::User(user)),
            } if !user.is_meta => {
                let text = user
                    .content
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text(t) if !t.text.trim().is_empty() => Some(t.text.as_str()),
                        ContentPart::Image(_) => Some("[image]"),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                let collapsed = collapse_preview(&text, 64);
                (!collapsed.is_empty()).then_some(collapsed)
            }
            _ => None,
        });
        let title = meta_title
            .or(first_user)
            .unwrap_or_else(|| id.chars().take(12).collect());
        let last_ts = items
            .iter()
            .map(|item| item.timestamp)
            .max()
            .or_else(|| header.map(|h| h.created_at))
            .unwrap_or(0);
        let sort_ms = if last_ts > 0 {
            last_ts
        } else {
            fs::metadata(&path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
        };
        rows.push((
            sort_ms,
            SessionOption {
                id: id.to_owned(),
                title,
                updated_at: format_session_time(sort_ms),
                message_count: Some(message_count),
            },
        ));
    }
    rows.sort_by_key(|row| std::cmp::Reverse(row.0));
    rows.into_iter().map(|(_, opt)| opt).collect()
}

fn read_meta_title(path: &Path) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    let val: serde_json::Value = serde_json::from_str(&raw).ok()?;
    val.get("title")
        .and_then(|v| v.as_str())
        .map(|s| collapse_preview(s, 64))
        .filter(|s| !s.is_empty())
}

fn collapse_preview(raw: &str, max_chars: usize) -> String {
    let collapsed: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    for (i, ch) in collapsed.chars().enumerate() {
        if i >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

fn format_session_time(ms: u64) -> Option<String> {
    if ms == 0 {
        return None;
    }
    let dt = chrono::DateTime::from_timestamp_millis(ms as i64)?;
    Some(
        dt.with_timezone(&chrono::Local)
            .format("%m-%d %H:%M")
            .to_string(),
    )
}

fn is_read_only_builtin(name: &str) -> bool {
    matches!(name, "read" | "find" | "grep" | "sleep")
}

fn is_delegating_gateway_tool(name: &str) -> bool {
    // Delegated inner tools (MCP, extension, or workspace tools) pass through middleware individually.
    matches!(name, "call_tool" | "exec")
}

fn is_edit_builtin(name: &str) -> bool {
    matches!(name, "write" | "edit" | "apply_patch")
}

fn is_approval_required(
    mode: PermissionMode,
    tool_name: &str,
    enabled_builtins: &HashSet<String>,
) -> bool {
    if tool_name == "tool_search" || is_delegating_gateway_tool(tool_name) {
        return false;
    }
    if enabled_builtins.contains(tool_name) && is_read_only_builtin(tool_name) {
        return false;
    }
    match mode {
        PermissionMode::Default => true,
        PermissionMode::AcceptEdits => {
            !(enabled_builtins.contains(tool_name) && is_edit_builtin(tool_name))
        }
        PermissionMode::BypassPermissions => false,
    }
}

/// Run the interactive TUI with full shared host capabilities (tools, MCP, extensions, skills).
pub async fn run(config: TuiLaunchConfig) -> Result<(), String> {
    let host = config.host;
    let initial_mode = PermissionMode::parse(&host.initial_mode)?;
    let theme_kind = match config.theme.as_deref() {
        Some(raw) => ThemeKind::from_name(raw).ok_or_else(|| format!("unknown --theme `{raw}`"))?,
        None => ThemeKind::Auto,
    };

    let cwd = host.cwd.clone();
    let transport = build_http_transport(host.transport_override.clone())?;
    let override_provider = host.provider_override.is_some();

    let provider_store = Arc::new(ProviderStore::new(host.provider_store_config()));
    let mut startup_warnings = provider_store.warnings();
    let active_model = provider_store
        .default_model()
        .unwrap_or_else(|| provider_store.unconfigured_placeholder_model());
    let selectable_models = provider_store.models();
    let explicit_model_override = host.provider.is_some() || host.model.is_some();

    let provider = build_auth_provider(
        Arc::clone(&provider_store),
        Arc::clone(&transport),
        host.provider_override.clone(),
        override_provider,
        true,
    );

    let (mcp_entries, mcp_settings_warnings) = host.resolve_mcp_entries(&[])?;
    startup_warnings.extend(mcp_settings_warnings);
    let (mcp_hub, mcp_warnings) = McpHub::connect(
        mcp_entries,
        &cwd,
        Arc::clone(&transport),
        CancelToken::new(),
    )
    .await
    .map_err(|err| err.to_string())?;
    startup_warnings.extend(mcp_warnings);
    let mcp_hub = Arc::new(mcp_hub);

    let loaded_extensions = match load_js_extensions(&host.extensions, &HashSet::new()) {
        Ok(loaded) => loaded.map(Arc::new),
        Err(err) => {
            mcp_hub.shutdown().await;
            return Err(err);
        }
    };

    let sink = JimuInteractionSink::new();
    let shared_mode = Arc::new(Mutex::new(initial_mode));
    let jimu_home = host.jimu_home.clone();
    let sessions_dir = host
        .persist_sessions
        .then(|| host.jimu_home.join("sessions"));

    let template = SessionTemplate {
        host,
        transport,
        provider_store: Arc::clone(&provider_store),
        provider,
        active_model: active_model.clone(),
        explicit_model_override,
        shared_mode: Arc::clone(&shared_mode),
        sink: sink.clone(),
        extensions: loaded_extensions,
        mcp_hub: Arc::clone(&mcp_hub),
    };

    let (
        initial_store,
        initial_id,
        initial_path,
        initial_auto_created,
        initial_lock,
        should_restore,
    ) = if let Some(explicit) = config.session_file.clone()
        && template.host.persist_sessions
    {
        let resolved_explicit = canonical_jsonl_path(&explicit);
        let lock = match SessionFileLock::try_acquire(&resolved_explicit) {
            Ok(lock) => lock,
            Err(err) => {
                mcp_hub.shutdown().await;
                return Err(err);
            }
        };
        let recorded_cwd = match resolve_existing_session_cwd(&resolved_explicit, &cwd) {
            Ok(dir) => dir,
            Err(err) => {
                mcp_hub.shutdown().await;
                return Err(err);
            }
        };
        if !paths_same_dir(&recorded_cwd, &cwd) {
            mcp_hub.shutdown().await;
            return Err(format!(
                "session `{}` belongs to `{}`; pass `-C {}` to open it safely",
                explicit.display(),
                recorded_cwd.display(),
                recorded_cwd.display()
            ));
        }
        let id = resolved_explicit
            .file_stem()
            .and_then(|s| s.to_str())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| Uuid::now_v7().to_string());
        (
            Some(Arc::new(JsonlSessionStore::new(&resolved_explicit)) as Arc<dyn SessionStore>),
            id,
            Some(resolved_explicit),
            false,
            Some(lock),
            true,
        )
    } else if config.continue_last
        && let Some(dir) = &sessions_dir
        && let Some((id, file)) = crate::find_latest_session_for_cwd(dir, &cwd)
    {
        let lock = match SessionFileLock::try_acquire(&file) {
            Ok(lock) => lock,
            Err(err) => {
                mcp_hub.shutdown().await;
                return Err(err);
            }
        };
        (
            Some(Arc::new(JsonlSessionStore::new(&file)) as Arc<dyn SessionStore>),
            id,
            Some(file),
            false,
            Some(lock),
            true,
        )
    } else if let Some(dir) = &sessions_dir {
        if let Err(err) = fs::create_dir_all(dir) {
            mcp_hub.shutdown().await;
            return Err(format!("failed to open session directory: {err}"));
        }
        let id = Uuid::now_v7().to_string();
        let file = dir.join(format!("{id}.jsonl"));
        let lock = match SessionFileLock::try_acquire(&file) {
            Ok(lock) => lock,
            Err(err) => {
                mcp_hub.shutdown().await;
                return Err(err);
            }
        };
        (
            Some(Arc::new(JsonlSessionStore::new(&file)) as Arc<dyn SessionStore>),
            id,
            Some(file),
            true,
            Some(lock),
            false,
        )
    } else {
        (None, Uuid::now_v7().to_string(), None, false, None, false)
    };

    let initial_built = match template.build(initial_store, should_restore).await {
        Ok(built) => built,
        Err(err) => {
            mcp_hub.shutdown().await;
            return Err(err);
        }
    };
    startup_warnings.extend(initial_built.warnings);

    let session = initial_built.session;
    #[cfg(not(target_os = "ios"))]
    let initial_watcher = session.watch_skills(|_| {}).ok();

    let active_session = Arc::new(RwLock::new(Arc::clone(&session)));
    let session_manager = Arc::new(TuiSessionManager {
        template,
        sessions_dir,
        cwd: cwd.clone(),
        active_session: Arc::clone(&active_session),
        #[cfg(not(target_os = "ios"))]
        skill_watcher: Mutex::new(initial_watcher),
        state: Mutex::new(ActiveSessionState {
            id: initial_id,
            path: initial_path,
            auto_created: initial_auto_created,
            _file_lock: initial_lock,
            built_tools: initial_built.built_tools,
        }),
        switch_lock: tokio::sync::Mutex::new(()),
    });

    let catalog_store = Arc::clone(&provider_store);
    let resolver_store = Arc::clone(&provider_store);
    let levels_store = Arc::clone(&provider_store);
    let model_home = jimu_home.clone();
    let effort_home = jimu_home.clone();
    let theme_home = jimu_home.clone();
    let harness = JimuHarness::with_interaction_sink(Arc::clone(&session), sink)
        .with_models(selectable_models)
        .with_model_catalog(Arc::new(move || catalog_store.models()))
        .with_model_resolver(Arc::new(move |key, current_provider| {
            resolver_store.select_model(key, current_provider)
        }))
        .with_thinking_levels_resolver(Arc::new(move |provider, id| {
            levels_store.thinking_levels(provider, id)
        }))
        .with_on_model_change(Arc::new(move |model| {
            let key = model_key(model);
            let _ = UserSettings::update(&model_home, |settings| {
                settings.default_model = Some(key);
            });
        }))
        .with_on_thinking_change(Arc::new(move |level| {
            let _ = UserSettings::update(&effort_home, |settings| {
                settings.default_effort = Some(level);
            });
        }))
        .with_permission_mode(Arc::new(SharedPermissionMode {
            mode: Arc::clone(&shared_mode),
            jimu_home: jimu_home.clone(),
        }))
        .with_mcp(Arc::new(SharedMcpCap {
            hub: Arc::clone(&mcp_hub),
        }))
        .with_session_controller(Arc::clone(&session_manager) as Arc<dyn JimuSessionController>);

    let effective_model = session.model();
    let auth_hint = if effective_model.id.is_empty() {
        Some(
            "No model configured — run `/model <provider/id>` or edit ~/.jimu/providers.json"
                .into(),
        )
    } else if provider_store.authenticated_for(&effective_model) {
        if effective_model.provider == "openai-codex" {
            Some("Signed in with ChatGPT / Codex".to_string())
        } else {
            Some(format!("Signed in ({})", effective_model.provider))
        }
    } else {
        Some(format!(
            "No credentials for {} — set API key or run `jimu login`",
            effective_model.provider
        ))
    };

    let chrome = ChromeConfig {
        slots: Arc::new(DynamicChromeSlots {
            active_session: Arc::clone(&active_session),
        }),
        app_name: Some("jimu".into()),
        app_version: Some(env!("CARGO_PKG_VERSION").into()),
        footer_hint: auth_hint,
        osc7501_enabled: true,
        ..ChromeConfig::default()
    };

    let slash_commands: Vec<Arc<dyn SlashCommand>> = vec![Arc::new(FastCommand {
        active_session: Arc::clone(&active_session),
        jimu_home: jimu_home.clone(),
    })];

    let initial_notices = startup_warnings
        .into_iter()
        .map(|msg| (NoticeLevel::Warning, msg))
        .collect();

    let tui_cfg = TuiAppConfig {
        screen_mode: ScreenMode::Fullscreen,
        theme_kind,
        chrome,
        slash_commands,
        directory: Some(cwd),
        initial_prompt: config.initial_prompt,
        open_resume_picker: config.open_resume_picker,
        initial_notices,
        on_theme_change: Some(Arc::new(move |kind| {
            let _ = UserSettings::update(&theme_home, |settings| {
                settings.theme = Some(kind.display_name().to_owned());
            });
        })),
        ..Default::default()
    };

    let result = TuiApp::new(Arc::new(harness), tui_cfg)
        .run()
        .await
        .map_err(|err| err.to_string());

    session_manager.shutdown_current().await;
    mcp_hub.shutdown().await;

    result
}
