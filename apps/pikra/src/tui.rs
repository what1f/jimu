//! Interactive terminal UI host (`--features tui`), sharing auth, models, built-in tools,
//! codemode, JS extensions, skills, instructions, thinking levels, fast mode, and session
//! persistence with ACP.

use crate::acp::{
    AcpConfig,
    auth::{Auth, now_ms},
};
use futures_util::{StreamExt, stream};
use pikra_agent::{
    AgentMessage, AgentMiddleware, AgentTool, BeforeToolCallCtx, BeforeToolCallResult, QueueMode,
};
use pikra_ai::{
    AssistantEventStream, AssistantMessage, AssistantMessageEvent, BoxFuture, CancelToken, Client,
    ContentPart, HttpTransport, Message, Model, SimpleStreamOptions, StopReason, StreamProvider,
    TranscriptContext, Usage,
};
use pikra_http::ReqwestTransport;
use pikra_js::{JsExtensionConfig, JsExtensionRuntime};
use pikra_kit::{
    BackgroundEventSender, BackgroundNotification, KitSession, KitSessionConfig, KitToolSpec,
    SkillDiscoveryConfig,
};
use pikra_session::{JsonlSessionStore, SessionEntryPayload, SessionStore};
use pikra_tools::{
    BackgroundEventCallback, BackgroundToolEvent, ExecCommandTool, MonitorManager, MonitorTool,
    ProcessSessionManager, SleepTool, WakeGate, WebFetchConfig, WebFetchTool, WorkspaceTools,
    WorkspaceToolsConfig, WriteStdinTool, format_background_events_xml,
};
use pikra_tui::{
    app::{ScreenMode, TuiApp, TuiAppConfig},
    engine::theme::ThemeKind,
    harness::{
        BoxFuture as TuiBoxFuture, ChromeConfig, ChromeSlots, ChromeState, NoticeLevel,
        PermissionModeCap, PermissionModeOption, SessionOption, SlashCommand, SlashContext,
    },
    pikra::{PikraApprovalMiddleware, PikraHarness, PikraInteractionSink, PikraSessionController},
};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::UNIX_EPOCH,
};
use uuid::Uuid;

/// Launch configuration for `pikra tui`, embedding [`AcpConfig`] so TUI and ACP share one model,
/// auth, tool, skill, instruction, and session pipeline.
#[derive(Clone)]
pub struct TuiLaunchConfig {
    pub acp: AcpConfig,
    pub session_file: Option<PathBuf>,
    pub theme: Option<String>,
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

struct SharedPermissionMode {
    mode: Arc<Mutex<PermissionMode>>,
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
        Ok(())
    }
}

struct AuthStreamProvider {
    auth: Arc<Auth>,
    provider: Arc<dyn StreamProvider>,
    transport: Arc<dyn HttpTransport>,
    override_provider: bool,
}

impl StreamProvider for AuthStreamProvider {
    fn stream(
        &self,
        model: &Model,
        context: &TranscriptContext,
        mut options: SimpleStreamOptions,
    ) -> AssistantEventStream {
        let model = model.clone();
        let context = context.clone();
        let auth = Arc::clone(&self.auth);
        let provider = Arc::clone(&self.provider);
        let transport = Arc::clone(&self.transport);
        let override_provider = self.override_provider;

        Box::pin(
            stream::once(async move {
                let (mut resolved, headers) = match auth.resolve_call(&model, override_provider) {
                    Ok(pair) => pair,
                    Err(message) => {
                        let error = error_assistant_message(&model, message);
                        return Box::pin(stream::once(async move {
                            AssistantMessageEvent::Error {
                                reason: StopReason::Error,
                                error,
                            }
                        })) as AssistantEventStream;
                    }
                };
                if !override_provider {
                    options.api_key = auth.api_key(&resolved, &*transport, &options.cancel).await;
                    resolved.headers.extend(headers);
                }
                provider.stream(&resolved, &context, options)
            })
            .flatten(),
        )
    }
}

/// Dynamic permission middleware supporting live `/permissions` and `Shift+Tab` switching.
struct DynamicPermissionMiddleware {
    approval: PikraApprovalMiddleware,
}

impl DynamicPermissionMiddleware {
    fn new(mode: Arc<Mutex<PermissionMode>>, sink: PikraInteractionSink) -> Self {
        let approval = PikraApprovalMiddleware::new(sink, move |ctx| {
            let current = *mode.lock().unwrap_or_else(|e| e.into_inner());
            is_approval_required(current, ctx.tool_call.name.as_str())
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
}

impl SlashCommand for FastCommand {
    fn name(&self) -> &str {
        "fast"
    }

    fn description(&self) -> &str {
        "Toggle fast mode (/fast [on|off])"
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

struct SessionTemplate {
    provider: Arc<dyn StreamProvider>,
    base_config: KitSessionConfig,
}

impl SessionTemplate {
    fn build(&self, store: Option<Arc<dyn SessionStore>>) -> Result<Arc<KitSession>, String> {
        let mut opts = self.base_config.clone();
        opts.store = store;
        KitSession::new(Arc::clone(&self.provider), opts)
            .map(Arc::new)
            .map_err(|err| format!("failed to initialize session: {err}"))
    }
}

struct ActiveSessionState {
    id: String,
    path: Option<PathBuf>,
    auto_created: bool,
}

struct TuiSessionManager {
    template: SessionTemplate,
    sessions_dir: Option<PathBuf>,
    active_session: Arc<RwLock<Arc<KitSession>>>,
    bg_sender: Arc<RwLock<Option<BackgroundEventSender>>>,
    state: Mutex<ActiveSessionState>,
}

impl TuiSessionManager {
    fn cleanup_empty_session(&self, session: &KitSession, state: &ActiveSessionState) {
        if !state.auto_created {
            return;
        }
        let Some(path) = &state.path else {
            return;
        };
        if session.visible_messages().is_empty() {
            let _ = fs::remove_file(path);
        }
    }

    fn cleanup_current_if_empty(&self) {
        let session = self
            .active_session
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.cleanup_empty_session(&session, &state);
    }

    fn activate(&self, next_session: Arc<KitSession>, next_state: ActiveSessionState) {
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
        *self.bg_sender.write().unwrap_or_else(|e| e.into_inner()) =
            Some(next_session.background_sender());
        self.cleanup_empty_session(&prev_session, &prev_state);
    }
}

impl PikraSessionController for TuiSessionManager {
    fn current_session_id(&self) -> Option<String> {
        Some(
            self.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .id
                .clone(),
        )
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
            Ok(scan_sessions(dir, &current_id))
        })
    }

    fn open_session(&self, session_id: &str) -> TuiBoxFuture<'_, Result<Arc<KitSession>, String>> {
        let id = session_id.to_owned();
        Box::pin(async move {
            let dir = self
                .sessions_dir
                .as_ref()
                .ok_or_else(|| "session persistence is disabled".to_owned())?;
            let path = dir.join(format!("{id}.jsonl"));
            if !path.is_file() {
                return Err(format!("session `{id}` not found"));
            }
            let store: Arc<dyn SessionStore> = Arc::new(JsonlSessionStore::new(&path));
            let next = self.template.build(Some(store))?;
            self.activate(
                Arc::clone(&next),
                ActiveSessionState {
                    id,
                    path: Some(path),
                    auto_created: false,
                },
            );
            Ok(next)
        })
    }

    fn create_session(&self) -> TuiBoxFuture<'_, Result<Arc<KitSession>, String>> {
        Box::pin(async move {
            let id = Uuid::now_v7().to_string();
            let (store, path, auto_created) = if let Some(dir) = &self.sessions_dir {
                fs::create_dir_all(dir)
                    .map_err(|err| format!("failed to open session directory: {err}"))?;
                let file = dir.join(format!("{id}.jsonl"));
                (
                    Some(Arc::new(JsonlSessionStore::new(&file)) as Arc<dyn SessionStore>),
                    Some(file),
                    true,
                )
            } else {
                (None, None, false)
            };
            let next = self.template.build(store)?;
            self.activate(
                Arc::clone(&next),
                ActiveSessionState {
                    id,
                    path,
                    auto_created,
                },
            );
            Ok(next)
        })
    }
}

fn scan_sessions(dir: &Path, current_id: &str) -> Vec<SessionOption> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(u64, SessionOption)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(id) = name.strip_suffix(".jsonl") else {
            continue;
        };
        let meta_title = read_meta_title(&dir.join(format!("{id}.meta.json")));
        let store = JsonlSessionStore::new(&path);
        let Ok((_header, entries)) = store.load() else {
            continue;
        };
        let mut msg_count = 0usize;
        let mut first_user: Option<String> = None;
        let mut last_ts = 0u64;
        for item in &entries {
            last_ts = last_ts.max(item.timestamp);
            match &item.payload {
                SessionEntryPayload::Message {
                    message: AgentMessage::Llm(Message::User(user)),
                } => {
                    msg_count += 1;
                    if first_user.is_none() {
                        let text = user
                            .content
                            .iter()
                            .filter_map(|c| match c {
                                ContentPart::Text(t) => Some(t.text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join(" ");
                        let line = collapse_preview(&text, 64);
                        if !line.is_empty() {
                            first_user = Some(line);
                        }
                    }
                }
                SessionEntryPayload::Message {
                    message: AgentMessage::Llm(Message::Assistant(_)),
                } => {
                    msg_count += 1;
                }
                _ => {}
            }
        }
        if msg_count == 0 && id != current_id {
            continue;
        }
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
        let title = meta_title
            .or(first_user)
            .unwrap_or_else(|| format!("Session {}", &id[..id.len().min(8)]));
        let updated_at = format_session_time(sort_ms);
        found.push((
            sort_ms,
            SessionOption {
                id: id.to_owned(),
                title,
                updated_at,
                message_count: Some(msg_count),
            },
        ));
    }
    found.sort_by_key(|a| std::cmp::Reverse(a.0));
    found.into_iter().map(|(_, item)| item).collect()
}

fn read_meta_title(path: &Path) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    let val: serde_json::Value = serde_json::from_str(&raw).ok()?;
    val.get("title")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

fn collapse_preview(raw: &str, max_chars: usize) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    for (idx, ch) in collapsed.chars().enumerate() {
        if idx >= max_chars {
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

fn is_edit_tool(name: &str) -> bool {
    matches!(name, "write" | "edit" | "apply_patch")
}

fn is_mutating_or_exec_tool(name: &str) -> bool {
    is_edit_tool(name) || matches!(name, "exec_command" | "write_stdin" | "monitor" | "exec")
}

fn is_approval_required(mode: PermissionMode, tool_name: &str) -> bool {
    match mode {
        PermissionMode::Default => is_mutating_or_exec_tool(tool_name) || tool_name == "web_fetch",
        PermissionMode::AcceptEdits => {
            !is_edit_tool(tool_name)
                && (is_mutating_or_exec_tool(tool_name) || tool_name == "web_fetch")
        }
        PermissionMode::BypassPermissions => false,
    }
}

fn error_assistant_message(model: &Model, message: String) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_id: None,
        thinking_level: None,
        usage: Usage::default(),
        stop_reason: StopReason::Error,
        error_message: Some(message),
        diagnostics: Vec::new(),
        timestamp: now_ms(),
        duration_ms: None,
    }
}

fn make_bg_callback(
    sender_slot: Arc<RwLock<Option<BackgroundEventSender>>>,
) -> BackgroundEventCallback {
    Arc::new(move |event: BackgroundToolEvent| {
        let sender = sender_slot
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(sender) = sender else {
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

/// Run the interactive TUI with full ACP-equivalent capabilities.
pub async fn run(config: TuiLaunchConfig) -> Result<(), String> {
    let initial_mode = PermissionMode::parse(&config.acp.initial_mode)?;
    let theme_kind = match config.theme.as_deref() {
        Some(raw) => ThemeKind::from_name(raw).ok_or_else(|| format!("unknown --theme `{raw}`"))?,
        None => ThemeKind::Auto,
    };

    let cwd = config.acp.cwd.clone();
    let transport: Arc<dyn HttpTransport> = match config.acp.transport_override.clone() {
        Some(t) => t,
        None => Arc::new(
            ReqwestTransport::new()
                .map_err(|err| format!("failed to initialize HTTP transport: {err}"))?,
        ),
    };
    let override_provider = config.acp.provider_override.is_some();
    let inner_provider: Arc<dyn StreamProvider> = config
        .acp
        .provider_override
        .clone()
        .unwrap_or_else(|| Arc::new(Client::new(Arc::clone(&transport))));

    let auth = Arc::new(Auth::new(config.acp.clone()));
    let active_model = auth.default_model();
    let mut selectable_models = auth.models();
    if !selectable_models
        .iter()
        .any(|m| m.provider == active_model.provider && m.id == active_model.id)
    {
        selectable_models.insert(0, active_model.clone());
    }

    let provider: Arc<dyn StreamProvider> = Arc::new(AuthStreamProvider {
        auth: Arc::clone(&auth),
        provider: inner_provider,
        transport: Arc::clone(&transport),
        override_provider,
    });

    let mut base_config = KitSessionConfig::new(active_model.clone());
    base_config.cwd = Some(cwd.to_string_lossy().into_owned());
    base_config.thinking_level = config.acp.thinking;
    base_config.fast = config.acp.fast;
    base_config.enable_codemode = config.acp.enable_codemode;
    base_config.steering_mode = QueueMode::All;
    base_config.compaction.compaction_reserve_tokens = config.acp.compact_tokens;

    base_config.skill_discovery = SkillDiscoveryConfig {
        cwd: Some(cwd.clone()),
        walk_boundary: config.acp.skill_walk_boundary,
        project_skill_dirs: vec![".pikra/skills".into()],
        user_skill_dirs: vec![config.acp.pikra_home.join("skills")],
        extra_roots: config.acp.skill_dirs.clone(),
        ..Default::default()
    };
    base_config.instruction_discovery =
        config
            .acp
            .instruction_discovery
            .clone()
            .map(|mut discovery| {
                discovery.cwd = Some(cwd.clone());
                discovery.user_dirs.insert(0, config.acp.pikra_home.clone());
                discovery
            });

    let sink = PikraInteractionSink::new();
    let shared_mode = Arc::new(Mutex::new(initial_mode));
    base_config
        .middlewares
        .push(Arc::new(DynamicPermissionMiddleware::new(
            Arc::clone(&shared_mode),
            sink.clone(),
        )));

    let bg_sender: Arc<RwLock<Option<BackgroundEventSender>>> = Arc::new(RwLock::new(None));
    let mut local_processes: Option<ProcessSessionManager> = None;
    let mut local_monitors: Option<MonitorManager> = None;

    if !config.acp.tools.is_empty() {
        let enabled = config.acp.tools.workspace_tool_set();
        let workspace = WorkspaceToolsConfig::new(&cwd)
            .with_enabled_tools(enabled.clone())
            .with_env_provider(Arc::new(|| {
                Box::pin(async {
                    if std::env::var_os("AI_AGENT").is_none() {
                        vec![("AI_AGENT".to_string(), "pikra".to_string())]
                    } else {
                        Vec::new()
                    }
                })
            }));
        let mut tools: Vec<Arc<dyn AgentTool>> = if enabled == pikra_tools::BuiltinToolSet::empty()
        {
            Vec::new()
        } else {
            WorkspaceTools::build(workspace.clone())
        };

        let wake_gate = WakeGate::new();
        let bg_callback = make_bg_callback(Arc::clone(&bg_sender));
        let workspace = Arc::new(workspace);

        if config.acp.tools.uses_process_manager() {
            let processes = ProcessSessionManager::new(
                Arc::clone(&workspace),
                wake_gate.clone(),
                Some(Arc::clone(&bg_callback)),
            );
            if config.acp.tools.is_enabled("exec_command") {
                tools.push(Arc::new(ExecCommandTool::new(processes.clone())));
            }
            if config.acp.tools.is_enabled("write_stdin") {
                tools.push(Arc::new(WriteStdinTool::new(processes.clone())));
            }
            local_processes = Some(processes);
        }

        if config.acp.tools.uses_monitor_manager() {
            let monitors =
                MonitorManager::new(workspace, wake_gate.clone(), Some(Arc::clone(&bg_callback)));
            tools.push(Arc::new(MonitorTool::new(monitors.clone())));
            local_monitors = Some(monitors);
        }

        if config.acp.tools.is_enabled("sleep") {
            tools.push(Arc::new(SleepTool::new(wake_gate)));
        }

        if config.acp.tools.is_enabled("web_fetch") {
            tools.push(Arc::new(WebFetchTool::new(
                Arc::clone(&transport),
                WebFetchConfig::default(),
            )));
        }

        base_config
            .tools
            .extend(tools.into_iter().map(KitToolSpec::resident));
    }

    if !config.acp.extensions.is_empty() {
        let js = config
            .acp
            .extensions
            .iter()
            .fold(JsExtensionConfig::new(), |js, file| js.with_file(file));
        let extension = Arc::new(
            JsExtensionRuntime::new(js)
                .map_err(|err| format!("failed to load JS extensions: {err}"))?,
        );
        let mut names: HashSet<_> = base_config
            .tools
            .iter()
            .map(|spec| spec.tool.definition().name)
            .collect();
        names.extend(["exec", "call_tool", "tool_search"].map(str::to_owned));
        for tool in extension.tools() {
            if !names.insert(tool.definition().name) {
                return Err("an extension tool uses a reserved or duplicate name".into());
            }
            base_config.tools.push(KitToolSpec::deferred(tool));
        }
        base_config
            .middlewares
            .push(extension as Arc<dyn AgentMiddleware>);
    }

    let has_tools = !base_config.tools.is_empty() || config.acp.enable_codemode;
    base_config.system_prompt = config
        .acp
        .system_prompt
        .clone()
        .unwrap_or_else(|| crate::prompt::build_default_system_prompt(&cwd, has_tools));

    let template = SessionTemplate {
        provider,
        base_config,
    };

    let sessions_dir = config
        .acp
        .persist_sessions
        .then(|| config.acp.pikra_home.join("sessions"));
    let (initial_store, initial_state) = if let Some(explicit) = config.session_file.clone()
        && config.acp.persist_sessions
    {
        let id = explicit
            .file_stem()
            .and_then(|s| s.to_str())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| Uuid::now_v7().to_string());
        (
            Some(Arc::new(JsonlSessionStore::new(&explicit)) as Arc<dyn SessionStore>),
            ActiveSessionState {
                id,
                path: Some(explicit),
                auto_created: false,
            },
        )
    } else if let Some(dir) = &sessions_dir {
        fs::create_dir_all(dir)
            .map_err(|err| format!("failed to open session directory: {err}"))?;
        let id = Uuid::now_v7().to_string();
        let file = dir.join(format!("{id}.jsonl"));
        (
            Some(Arc::new(JsonlSessionStore::new(&file)) as Arc<dyn SessionStore>),
            ActiveSessionState {
                id,
                path: Some(file),
                auto_created: true,
            },
        )
    } else {
        (
            None,
            ActiveSessionState {
                id: Uuid::now_v7().to_string(),
                path: None,
                auto_created: false,
            },
        )
    };

    let session = template.build(initial_store)?;
    *bg_sender.write().unwrap_or_else(|e| e.into_inner()) = Some(session.background_sender());

    let active_session = Arc::new(RwLock::new(Arc::clone(&session)));
    let session_manager = Arc::new(TuiSessionManager {
        template,
        sessions_dir,
        active_session: Arc::clone(&active_session),
        bg_sender,
        state: Mutex::new(initial_state),
    });

    let mut harness = PikraHarness::with_interaction_sink(Arc::clone(&session), sink)
        .with_models(selectable_models)
        .with_permission_mode(Arc::new(SharedPermissionMode {
            mode: Arc::clone(&shared_mode),
        }))
        .with_session_controller(Arc::clone(&session_manager) as Arc<dyn PikraSessionController>);
    if let Some(ref processes) = local_processes {
        harness = harness.with_process_manager(processes.clone());
    }
    if let Some(ref monitors) = local_monitors {
        harness = harness.with_monitor_manager(monitors.clone());
    }

    let auth_hint = if auth.authenticated_for(&active_model) {
        if active_model.provider == "openai-codex" {
            Some("Signed in with ChatGPT / Codex".to_string())
        } else {
            Some(format!("Signed in ({})", active_model.provider))
        }
    } else {
        Some(format!(
            "No credentials for {} — set API key or run `pikra login`",
            active_model.provider
        ))
    };

    let chrome = ChromeConfig {
        slots: Arc::new(DynamicChromeSlots {
            active_session: Arc::clone(&active_session),
        }),
        app_name: Some("pikra".into()),
        app_version: Some(env!("CARGO_PKG_VERSION").into()),
        footer_hint: auth_hint,
        osc7501_enabled: true,
        ..ChromeConfig::default()
    };

    let slash_commands: Vec<Arc<dyn SlashCommand>> = vec![Arc::new(FastCommand {
        active_session: Arc::clone(&active_session),
    })];

    let tui_cfg = TuiAppConfig {
        screen_mode: ScreenMode::Fullscreen,
        theme_kind,
        chrome,
        slash_commands,
        directory: Some(cwd),
        ..Default::default()
    };

    let result = TuiApp::new(Arc::new(harness), tui_cfg)
        .run()
        .await
        .map_err(|err| err.to_string());

    session_manager.cleanup_current_if_empty();

    if let Some(processes) = local_processes {
        processes.shutdown().await;
    }
    if let Some(monitors) = local_monitors {
        monitors.shutdown().await;
    }

    result
}
