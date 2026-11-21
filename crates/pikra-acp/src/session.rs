use crate::{
    AcpServer,
    auth::{Auth, model_key, now_ms, read_optional, save},
    rpc::{Result, failed, invalid},
    schema::v1 as acp,
    server::Connection,
    tools_bridge::Bridge,
    turn::{Compaction, ModelStream, TurnSlot},
};
use pikra_agent::AgentMiddleware;
use pikra_ai::{ApiKind, CancelToken, ThinkingLevel};
use pikra_js::{JsExtensionConfig, JsExtensionRuntime};
use pikra_kit::{
    KitSession, KitSessionConfig, KitToolSpec, McpClient, McpToolRegistrationMode,
    SkillDiscoveryConfig, StdioMcpConfig, StdioMcpTransport,
};
use pikra_session::{
    CURRENT_SESSION_VERSION, JsonlSessionStore, MemorySessionStore, SessionEntry, SessionHeader,
    SessionStore,
};
use pikra_tools::{
    BuiltinToolSet, WebFetchConfig, WebFetchTool, WorkspaceTools, WorkspaceToolsConfig,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, TryLockError},
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use uuid::Uuid;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Mode {
    Default,
    AcceptEdits,
    Plan,
    #[serde(rename = "bypassPermissions")]
    Bypass,
}
impl Mode {
    const ALL: [Self; 4] = [Self::Default, Self::AcceptEdits, Self::Plan, Self::Bypass];

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "default" | "ask" => Some(Self::Default),
            "acceptEdits" | "auto_edit" => Some(Self::AcceptEdits),
            "plan" | "read_only" => Some(Self::Plan),
            "bypassPermissions" | "yolo" => Some(Self::Bypass),
            _ => None,
        }
    }

    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::Bypass => "bypassPermissions",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Default => "Ask before changes",
            Self::AcceptEdits => "Accept workspace edits",
            Self::Plan => "Plan",
            Self::Bypass => "Allow all tools",
        }
    }

    pub(crate) fn state(self) -> acp::SessionModeState {
        acp::SessionModeState::new(
            self.id(),
            Self::ALL
                .into_iter()
                .map(|mode| acp::SessionMode::new(mode.id(), mode.name()))
                .collect(),
        )
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Metadata {
    pub(crate) session_id: String,
    pub(crate) cwd: PathBuf,
    pub(crate) additional_directories: Vec<PathBuf>,
    pub(crate) title: Option<String>,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
    pub(crate) archived: bool,
    pub(crate) mode: Mode,
    pub(crate) provider: String,
    pub(crate) model_id: String,
    pub(crate) api: ApiKind,
    pub(crate) base_url: String,
    pub(crate) thinking_level: ThinkingLevel,
    pub(crate) fast: bool,
}

impl Metadata {
    pub(crate) fn new(
        server: &AcpServer,
        cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
    ) -> Self {
        let model = server.auth.default_model();
        let mut thinking_level = server.config.thinking;
        if let Some(allowed) = server.auth.thinking_levels(&model.provider, &model.id)
            && !allowed.contains(&thinking_level)
        {
            thinking_level = allowed[0];
        }
        let now = now_ms();
        Self {
            session_id: Uuid::now_v7().to_string(),
            cwd,
            additional_directories,
            title: None,
            created_at: now,
            updated_at: now,
            archived: false,
            mode: server.mode,
            provider: model.provider,
            model_id: model.id,
            api: model.api,
            base_url: model.base_url,
            thinking_level,
            fast: server.config.fast,
        }
    }

    pub(crate) fn info(&self) -> acp::SessionInfo {
        acp::SessionInfo::new(self.session_id.clone(), &self.cwd)
            .title(self.title.clone())
            .additional_directories(self.additional_directories.clone())
            .updated_at(iso_date(self.updated_at))
            .archived(self.archived)
    }

    pub(crate) fn options(
        &self,
        auth: &Auth,
        capabilities: &acp::ClientCapabilities,
    ) -> Vec<acp::SessionConfigOption> {
        use acp::{
            SessionConfigOption as Option, SessionConfigOptionCategory as Category,
            SessionConfigSelectOption as Item,
        };
        let model_id = format!("{}/{}", self.provider, self.model_id);
        let catalog = auth.models();
        let multi_provider = catalog
            .first()
            .is_some_and(|first| catalog.iter().any(|m| m.provider != first.provider));
        let mut models: Vec<_> = catalog
            .into_iter()
            .map(|model| {
                let label = if multi_provider {
                    format!("{} ({})", model.name, model.provider)
                } else {
                    model.name.clone()
                };
                Item::new(model_key(&model), label)
            })
            .collect();
        if !models.iter().any(|item| item.value.0.as_ref() == model_id) {
            models.push(Item::new(model_id.clone(), self.model_id.clone()));
        }
        let modes = Mode::ALL
            .into_iter()
            .map(|mode| Item::new(mode.id(), mode.name()))
            .collect::<Vec<_>>();
        let allowed_levels = auth
            .thinking_levels(&self.provider, &self.model_id)
            .unwrap_or_else(|| {
                vec![
                    ThinkingLevel::Off,
                    ThinkingLevel::Low,
                    ThinkingLevel::Medium,
                    ThinkingLevel::High,
                    ThinkingLevel::Xhigh,
                    ThinkingLevel::Max,
                ]
            });
        let effective_thinking = if allowed_levels.contains(&self.thinking_level) {
            self.thinking_level
        } else {
            allowed_levels[0]
        };
        let levels = allowed_levels
            .iter()
            .map(|&level| {
                let (id, name) = thinking_choice(level);
                Item::new(id, name)
            })
            .collect::<Vec<_>>();
        let speed = if supports_boolean(capabilities) {
            Option::boolean("fast", "Fast mode", self.fast)
        } else {
            Option::select(
                "fast",
                "Fast mode",
                if self.fast { "on" } else { "off" },
                vec![Item::new("off", "Off"), Item::new("on", "On")],
            )
        };
        vec![
            Option::select("mode", "Mode", self.mode.id(), modes).category(Category::Mode),
            Option::select("model", "Model", model_id, models).category(Category::Model),
            Option::select(
                "thought_level",
                "Thinking",
                thinking_choice(effective_thinking).0,
                levels,
            )
            .category(Category::ThoughtLevel),
            speed.category(Category::Speed),
        ]
    }

    pub(crate) fn set_option(
        &mut self,
        auth: &Auth,
        caps: &acp::ClientCapabilities,
        id: &str,
        value: &acp::SessionConfigOptionValue,
    ) -> bool {
        let text = value
            .as_value_id()
            .map(|id| id.0.as_ref())
            .unwrap_or_default();
        match id {
            "mode" => {
                let Some(mode) = Mode::parse(text).filter(|mode| mode.id() == text) else {
                    return false;
                };
                self.mode = mode;
            }
            "model" => {
                let Some(model) = auth.select_model(text) else {
                    return false;
                };
                self.provider = model.provider;
                self.model_id = model.id;
                self.api = model.api;
                self.base_url = model.base_url;
                if let Some(allowed) = auth.thinking_levels(&self.provider, &self.model_id)
                    && !allowed.contains(&self.thinking_level)
                {
                    self.thinking_level = allowed[0];
                }
            }
            "thought_level" => {
                let next = match text {
                    "off" => ThinkingLevel::Off,
                    "minimal" => ThinkingLevel::Minimal,
                    "low" => ThinkingLevel::Low,
                    "medium" => ThinkingLevel::Medium,
                    "high" => ThinkingLevel::High,
                    "xhigh" => ThinkingLevel::Xhigh,
                    "max" => ThinkingLevel::Max,
                    _ => return false,
                };
                if let Some(allowed) = auth.thinking_levels(&self.provider, &self.model_id)
                    && !allowed.contains(&next)
                {
                    return false;
                }
                self.thinking_level = next;
            }
            "fast" => {
                self.fast = match (value.as_bool(), text) {
                    (Some(fast), _) if supports_boolean(caps) => fast,
                    (None, "on") => true,
                    (None, "off") => false,
                    _ => return false,
                };
            }
            _ => return false,
        }
        true
    }
}

fn supports_boolean(caps: &acp::ClientCapabilities) -> bool {
    caps.session
        .as_ref()
        .and_then(|s| s.config_options.as_ref())
        .is_some_and(|options| options.boolean.is_some())
}

fn thinking_choice(level: ThinkingLevel) -> (&'static str, &'static str) {
    match level {
        ThinkingLevel::Off | ThinkingLevel::None => ("off", "Off"),
        ThinkingLevel::Minimal => ("minimal", "Minimal"),
        ThinkingLevel::Low => ("low", "Low"),
        ThinkingLevel::Medium => ("medium", "Medium"),
        ThinkingLevel::High => ("high", "High"),
        ThinkingLevel::Xhigh => ("xhigh", "Extra high"),
        ThinkingLevel::Max => ("max", "Max"),
    }
}

pub(crate) fn iso_date(ms: u64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(i64::try_from(ms).ok()?)
        .map(|date| date.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

pub(crate) fn validate_roots(cwd: &Path, extra: &[PathBuf]) -> Result<()> {
    if !cwd.is_absolute() || extra.iter().any(|path| !path.is_absolute()) {
        return Err(invalid("Workspace directories must be absolute paths."));
    }
    Ok(())
}

pub(crate) fn validate_prompt(prompt: Option<&acp::SystemPrompt>) -> Result<()> {
    let Some(prompt) = prompt else { return Ok(()) };
    if !matches!(
        prompt.mode,
        acp::SystemPromptMode::Append | acp::SystemPromptMode::Override
    ) {
        return Err(invalid("Unsupported system prompt mode."));
    }
    if prompt.content.is_empty()
        || prompt
            .content
            .iter()
            .any(|part| !matches!(part, acp::ContentBlock::Text(_)))
    {
        return Err(invalid("Enter system instructions as text."));
    }
    Ok(())
}

pub(crate) fn initial_options(
    meta: &mut Metadata,
    auth: &Auth,
    caps: &acp::ClientCapabilities,
    options: &[acp::InitialSessionConfigOption],
) -> Result<()> {
    let mut seen = HashSet::new();
    for option in options {
        if !seen.insert(&option.config_id) {
            return Err(invalid("A session option was supplied more than once."));
        }
        meta.set_option(auth, caps, option.config_id.0.as_ref(), &option.value);
    }
    Ok(())
}

pub(crate) struct Record {
    pub(crate) metadata: Mutex<Metadata>,
    pub(crate) store: Arc<dyn SessionStore>,
}

pub(crate) struct Sessions {
    directory: Option<PathBuf>,
    records: Mutex<HashMap<String, Arc<Record>>>,
    leased: Mutex<HashSet<String>>,
}

struct LeaseInner {
    server: Arc<AcpServer>,
    id: String,
    _file_lock: Option<File>,
}

pub(crate) struct Lease {
    inner: Mutex<Option<LeaseInner>>,
}

impl Lease {
    pub(crate) fn release(&self) {
        if let Some(inner) = self.inner.lock().unwrap_or_else(|e| e.into_inner()).take() {
            drop(inner._file_lock);
            inner
                .server
                .sessions
                .leased
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&inner.id);
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.release();
    }
}

impl Sessions {
    pub(crate) fn new(directory: Option<PathBuf>) -> std::result::Result<Self, String> {
        let mut records = HashMap::new();
        if let Some(dir) = &directory {
            fs::create_dir_all(dir).map_err(|_| "Could not open the session directory.")?;
            let files = fs::read_dir(dir).map_err(|_| "Could not read the session directory.")?;
            for item in files.flatten() {
                let path = item.path();
                let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                let Some(id) = name.strip_suffix(".meta.json").filter(|id| uuid_id(id)) else {
                    continue;
                };
                if let Ok(Some(meta)) = read_optional::<Metadata>(&path)
                    && meta.session_id == id
                    && dir.join(format!("{id}.jsonl")).is_file()
                {
                    records.insert(
                        id.into(),
                        Arc::new(Record {
                            metadata: Mutex::new(meta),
                            store: Arc::new(JsonlSessionStore::new(
                                dir.join(format!("{id}.jsonl")),
                            )),
                        }),
                    );
                }
            }
        }
        Ok(Self {
            directory,
            records: Mutex::new(records),
            leased: Mutex::default(),
        })
    }

    fn lock_path(&self, id: &str) -> Option<PathBuf> {
        self.directory
            .as_ref()
            .map(|dir| dir.join(format!("{id}.lock")))
    }

    pub(crate) fn try_lock_file(&self, id: &str) -> Result<Option<File>> {
        let Some(path) = self.lock_path(id) else {
            return Ok(None);
        };
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|_| failed("Could not open the session directory."))?;
        }
        let mut options = fs::OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&path)
            .map_err(|_| failed("Could not lock the session."))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(TryLockError::WouldBlock) => {
                Err(failed("This session is open in another connection."))
            }
            Err(_) => Err(failed("Could not lock the session.")),
        }
    }

    pub(crate) fn claim(server: &Arc<AcpServer>, id: &str) -> Result<Lease> {
        if !server
            .sessions
            .leased
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.into())
        {
            return Err(failed("This session is open in another connection."));
        }
        let file_lock = match server.sessions.try_lock_file(id) {
            Ok(lock) => lock,
            Err(error) => {
                server
                    .sessions
                    .leased
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(id);
                return Err(error);
            }
        };
        Ok(Lease {
            inner: Mutex::new(Some(LeaseInner {
                server: server.clone(),
                id: id.into(),
                _file_lock: file_lock,
            })),
        })
    }

    pub(crate) fn is_open(&self, id: &str) -> bool {
        self.leased
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(id)
    }

    pub(crate) fn get(&self, id: &str) -> Result<Arc<Record>> {
        if !uuid_id(id) {
            return Err(invalid("Unknown session."));
        }
        self.records
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| invalid("Unknown session."))
    }

    pub(crate) fn snapshots(&self) -> Vec<Metadata> {
        self.records
            .lock()
            .unwrap()
            .values()
            .map(|record| record.metadata.lock().unwrap().clone())
            .collect()
    }

    fn durable(&self, id: &str) -> Option<(PathBuf, PathBuf)> {
        let dir = self.directory.as_ref()?;
        Some((
            dir.join(format!("{id}.jsonl")),
            dir.join(format!("{id}.meta.json")),
        ))
    }

    pub(crate) fn prepare(
        &self,
        meta: Metadata,
        parent: Option<&str>,
        entries: &[SessionEntry],
    ) -> Result<Arc<Record>> {
        let store: Arc<dyn SessionStore> = self.durable(&meta.session_id).map_or_else(
            || Arc::new(MemorySessionStore::new()) as Arc<_>,
            |(path, _)| Arc::new(JsonlSessionStore::new(path)) as Arc<_>,
        );
        let initialize = || {
            store.write_header(&SessionHeader {
                version: CURRENT_SESSION_VERSION,
                id: meta.session_id.clone(),
                created_at: meta.created_at,
                cwd: Some(meta.cwd.to_string_lossy().into_owned()),
                parent_session: parent.map(str::to_owned),
            })?;
            for entry in entries {
                store.append_entry(entry)?;
            }
            Ok::<_, pikra_session::SessionError>(())
        };
        if initialize().is_err() {
            self.discard(&meta.session_id);
            return Err(failed("Could not create the session."));
        }
        Ok(Arc::new(Record {
            metadata: Mutex::new(meta),
            store,
        }))
    }

    pub(crate) fn register(&self, record: Arc<Record>) -> Result<()> {
        let meta = record.metadata.lock().unwrap();
        self.persist(&meta)?;
        let id = meta.session_id.clone();
        drop(meta);
        self.records.lock().unwrap().insert(id, record);
        Ok(())
    }

    pub(crate) fn update(
        &self,
        record: &Record,
        apply: impl FnOnce(&mut Metadata),
    ) -> Result<Metadata> {
        let records = self.records.lock().unwrap();
        let mut meta = record.metadata.lock().unwrap();
        if records
            .get(&meta.session_id)
            .is_none_or(|current| !std::ptr::eq(&**current, record))
        {
            return Err(invalid("Unknown session."));
        }
        let mut next = meta.clone();
        apply(&mut next);
        next.updated_at = now_ms();
        self.persist(&next)?;
        *meta = next.clone();
        Ok(next)
    }

    fn persist(&self, meta: &Metadata) -> Result<()> {
        if let Some((_, path)) = self.durable(&meta.session_id) {
            save(&path, meta).map_err(|_| failed("Could not save the session."))?;
        }
        Ok(())
    }

    pub(crate) fn discard(&self, id: &str) {
        if let Some((log, meta)) = self.durable(id) {
            let _ = fs::remove_file(meta);
            let _ = fs::remove_file(log);
        }
        if let Some(lock) = self.lock_path(id) {
            let _ = fs::remove_file(lock);
        }
    }

    pub(crate) fn delete(&self, id: &str) -> Result<()> {
        let mut records = self.records.lock().unwrap_or_else(|e| e.into_inner());
        if !records.contains_key(id) {
            return Err(invalid("Unknown session."));
        }
        if let Some((log, meta)) = self.durable(id) {
            // The catalog entry is the commit point. A leftover log is never loaded.
            fs::remove_file(meta).map_err(|_| failed("Could not delete the session."))?;
            if let Err(err) = fs::remove_file(log)
                && err.kind() != ErrorKind::NotFound
            {
                records.remove(id);
                return Err(failed("The session history file could not be removed."));
            }
        }
        if let Some(lock) = self.lock_path(id) {
            let _ = fs::remove_file(lock);
        }
        records.remove(id);
        Ok(())
    }
}

fn uuid_id(id: &str) -> bool {
    Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id)
}

pub(crate) fn branch(entries: Vec<SessionEntry>) -> Vec<SessionEntry> {
    let mut tip = entries.last().map(|entry| entry.id.clone());
    let mut by_id: HashMap<_, _> = entries
        .into_iter()
        .map(|entry| (entry.id.clone(), entry))
        .collect();
    let mut path = Vec::new();
    while let Some(id) = tip.take() {
        let Some(entry) = by_id.remove(&id) else {
            break;
        };
        tip.clone_from(&entry.parent_id);
        path.push(entry);
    }
    path.reverse();
    path
}

pub(crate) type SteeringOrder = (
    Option<tokio::sync::oneshot::Receiver<()>>,
    tokio::sync::oneshot::Sender<()>,
);

pub(crate) struct Session {
    pub(crate) record: Arc<Record>,
    pub(crate) kit: KitSession,
    pub(crate) bridge: Arc<Bridge>,
    pub(crate) turn: Arc<TurnSlot>,
    pub(crate) compaction: Arc<Compaction>,
    pub(crate) steering: tokio::sync::Mutex<()>,
    last_steer: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    pub(crate) mcp_config: Vec<acp::McpServer>,
    mcp: Vec<Arc<McpClient>>,
    #[cfg(not(target_os = "ios"))]
    local_processes: Option<(
        pikra_tools::ProcessSessionManager,
        pikra_tools::MonitorManager,
    )>,
    lease: Arc<Lease>,
}

impl Session {
    pub(crate) async fn open(
        connection: &Connection,
        capabilities: acp::ClientCapabilities,
        record: Arc<Record>,
        meta: &Metadata,
        mcp_config: Vec<acp::McpServer>,
        lease: Arc<Lease>,
        cancel: &CancelToken,
    ) -> Result<Self> {
        let server = &connection.server;
        let cfg = &server.config;
        let web_fetch = cfg.enable_tools.then(|| {
            Arc::new(WebFetchTool::new(
                server.transport.clone(),
                WebFetchConfig::default(),
            ))
        });
        let bridge = Arc::new(Bridge::new(
            meta.session_id.clone().into(),
            meta.cwd.clone(),
            &meta.additional_directories,
            meta.mode,
            capabilities,
            connection.peer.clone(),
            web_fetch.clone(),
        ));
        let turn = Arc::new(TurnSlot::default());
        let compaction = Arc::new(Compaction::new(bridge.clone(), turn.clone()));
        let mut model = server.auth.model(&meta.provider, &meta.model_id);
        model.api.clone_from(&meta.api);
        model.base_url.clone_from(&meta.base_url);
        let mut opts = KitSessionConfig::new(model);
        opts.store = Some(record.store.clone());
        opts.cwd = Some(meta.cwd.to_string_lossy().into_owned());
        opts.system_prompt = cfg.system_prompt.clone().unwrap_or_default();
        opts.thinking_level = meta.thinking_level;
        opts.fast = meta.fast;
        opts.enable_codemode = cfg.enable_codemode;
        opts.steering_mode = pikra_agent::QueueMode::All;
        opts.skill_discovery = SkillDiscoveryConfig {
            cwd: Some(meta.cwd.clone()),
            walk_boundary: cfg.skill_walk_boundary,
            project_skill_dirs: vec![".pikra/skills".into()],
            user_skill_dirs: vec![cfg.pikra_home.join("skills")],
            extra_roots: cfg
                .skill_dirs
                .iter()
                .cloned()
                .chain(
                    meta.additional_directories
                        .iter()
                        .flat_map(|path| [path.join(".agents/skills"), path.join(".pikra/skills")]),
                )
                .collect(),
            ..Default::default()
        };
        opts.instruction_discovery = cfg.instruction_discovery.clone().map(|mut discovery| {
            discovery.cwd = Some(meta.cwd.clone());
            discovery
                .additional_directories
                .extend(meta.additional_directories.iter().cloned());
            discovery.user_dirs.insert(0, cfg.pikra_home.clone());
            discovery
        });
        opts.compaction.compaction_reserve_tokens = cfg.compact_tokens;
        opts.compaction_strategy = Some(compaction.clone());
        #[cfg(not(target_os = "ios"))]
        let mut local_processes = None;
        #[cfg(not(target_os = "ios"))]
        let bg_sender: Arc<std::sync::OnceLock<pikra_kit::BackgroundEventSender>> =
            Arc::new(std::sync::OnceLock::new());
        if cfg.enable_tools {
            let enabled = if cfg!(target_os = "ios") {
                BuiltinToolSet::all()
            } else {
                BuiltinToolSet::all().without(pikra_tools::BuiltinToolKind::ExecCommand)
            };
            let workspace = WorkspaceToolsConfig::new(&meta.cwd)
                .with_enabled_tools(enabled)
                .with_env_provider(Arc::new(|| {
                    Box::pin(async {
                        if std::env::var_os("AI_AGENT").is_none() {
                            vec![("AI_AGENT".to_string(), "pikra".to_string())]
                        } else {
                            Vec::new()
                        }
                    })
                }));
            let mut tools = WorkspaceTools::build(workspace.clone());
            #[cfg(not(target_os = "ios"))]
            {
                use pikra_tools::{
                    BackgroundEventCallback, BackgroundToolEvent, ExecCommandTool, MonitorManager,
                    MonitorTool, ProcessSessionManager, WakeGate, WriteStdinTool,
                    format_background_events_xml,
                };
                let gate = WakeGate::new();
                let bg_slot = Arc::clone(&bg_sender);
                let bg_callback: BackgroundEventCallback = Arc::new(move |event| {
                    let Some(sender) = bg_slot.get() else {
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
                    sender.send(pikra_kit::BackgroundNotification {
                        source_id,
                        kind,
                        summary,
                        xml_payload,
                        timestamp_ms: now_ms(),
                    });
                });
                let workspace = Arc::new(workspace);
                let processes = ProcessSessionManager::new(
                    workspace.clone(),
                    gate.clone(),
                    Some(bg_callback.clone()),
                );
                let monitors = MonitorManager::new(workspace, gate.clone(), Some(bg_callback));
                tools.push(Arc::new(ExecCommandTool::new(processes.clone())));
                tools.push(Arc::new(WriteStdinTool::new(processes.clone())));
                tools.push(Arc::new(MonitorTool::new(monitors.clone())));
                local_processes = Some((processes, monitors));
            }
            tools.extend(web_fetch.map(|tool| tool as Arc<dyn pikra_agent::AgentTool>));
            opts.tools.extend(
                tools
                    .into_iter()
                    .map(|tool| KitToolSpec::resident(bridge.wrap(tool))),
            );
        }
        if !cfg.extensions.is_empty() {
            let js = cfg
                .extensions
                .iter()
                .fold(JsExtensionConfig::new(), |js, file| js.with_file(file));
            let extension = Arc::new(
                JsExtensionRuntime::new(js)
                    .map_err(|_| failed("Could not load the extensions."))?,
            );
            let mut names: HashSet<_> = opts
                .tools
                .iter()
                .map(|spec| spec.tool.definition().name)
                .collect();
            names.extend(["exec", "call_tool", "tool_search"].map(str::to_owned));
            for tool in extension.tools() {
                if !names.insert(tool.definition().name) {
                    return Err(failed(
                        "An extension tool uses a reserved or duplicate name.",
                    ));
                }
                opts.tools.push(KitToolSpec::deferred(tool));
            }
            opts.middlewares.push(extension as Arc<dyn AgentMiddleware>);
        }
        opts.middlewares
            .push(bridge.clone() as Arc<dyn AgentMiddleware>);
        let model_stream = Arc::new(ModelStream::new(
            server,
            &bridge,
            turn.clone(),
            compaction.clone(),
        ));
        let kit = KitSession::new(model_stream, opts)
            .map_err(|_| failed("Could not open the session."))?;
        #[cfg(not(target_os = "ios"))]
        let _ = bg_sender.set(kit.background_sender());
        let mut session = Self {
            record,
            kit,
            bridge,
            turn,
            compaction,
            steering: tokio::sync::Mutex::new(()),
            last_steer: Mutex::default(),
            mcp_config,
            mcp: Vec::new(),
            #[cfg(not(target_os = "ios"))]
            local_processes,
            lease,
        };
        let setup = async {
            let mut names = HashSet::new();
            session.attach_mcp(server, &mut names, cancel).await?;
            if session.kit.thinking_level() != meta.thinking_level {
                session
                    .kit
                    .set_thinking_level(meta.thinking_level)
                    .map_err(|_| failed("Could not set the thinking level."))?;
            }
            if session.kit.fast() != meta.fast {
                session
                    .kit
                    .set_fast(meta.fast)
                    .map_err(|_| failed("Could not set the speed."))?;
            }
            Ok::<(), acp::Error>(())
        }
        .await;
        if let Err(error) = setup {
            session.shutdown().await;
            return Err(error);
        }
        Ok(session)
    }

    async fn attach_mcp(
        &mut self,
        server: &AcpServer,
        names: &mut HashSet<String>,
        cancel: &CancelToken,
    ) -> Result<()> {
        // Pre-validate all descriptors and normalized prefixes BEFORE spawning any stdio child processes.
        let mut validated = Vec::with_capacity(self.mcp_config.len());
        for descriptor in &self.mcp_config {
            let name = match descriptor {
                acp::McpServer::Stdio(spec) => {
                    if spec.command.to_str().is_none() {
                        return Err(invalid("Invalid MCP command path."));
                    }
                    &spec.name
                }
                acp::McpServer::Http(spec) => {
                    url::Url::parse(&spec.url)
                        .ok()
                        .filter(|url| {
                            matches!(url.scheme(), "http" | "https")
                                && url.host().is_some()
                                && url.username().is_empty()
                                && url.password().is_none()
                        })
                        .ok_or_else(|| invalid("Invalid MCP server URL."))?;
                    &spec.name
                }
                _ => return Err(invalid("Unsupported MCP transport.")),
            };
            let prefix: String = name
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            if prefix.is_empty() || !names.insert(prefix.clone()) {
                return Err(invalid("MCP server names must be unique."));
            }
            validated.push((descriptor.clone(), prefix));
        }

        for (descriptor, prefix) in validated {
            if cancel.is_cancelled() || self.bridge.peer.closed.is_cancelled() {
                return Err(acp::Error::request_cancelled());
            }
            let client = match &descriptor {
                acp::McpServer::Stdio(spec) => {
                    let mut config = StdioMcpConfig::new(
                        spec.command
                            .to_str()
                            .ok_or_else(|| invalid("Invalid MCP command path."))?,
                    );
                    config.args.clone_from(&spec.args);
                    config.env = spec
                        .env
                        .iter()
                        .map(|env| (env.name.clone(), env.value.clone()))
                        .collect();
                    config.cwd = Some(self.bridge.cwd.clone());
                    McpClient::new(Arc::new(
                        StdioMcpTransport::spawn(config)
                            .map_err(|_| failed("Could not start an MCP server."))?,
                    ))
                }
                acp::McpServer::Http(spec) => {
                    let headers = spec
                        .headers
                        .iter()
                        .map(|header| (header.name.clone(), header.value.clone()))
                        .collect();
                    McpClient::with_http(server.transport.clone(), spec.url.as_str(), headers)
                }
                _ => return Err(invalid("Unsupported MCP transport.")),
            };
            let client = Arc::new(client);
            self.mcp.push(client.clone());
            let setup = async {
                client
                    .initialize("pikra-acp", env!("CARGO_PKG_VERSION"), cancel.clone())
                    .await
                    .map_err(|_| failed("Could not connect to an MCP server."))?;
                self.kit
                    .attach_mcp_client(
                        client,
                        McpToolRegistrationMode::Auto,
                        Some(&format!("mcp_{prefix}")),
                        cancel.clone(),
                    )
                    .await
                    .map_err(|_| failed("Could not load MCP tools."))?;
                Ok::<(), acp::Error>(())
            };
            tokio::select! {
                () = cancel.cancelled() => return Err(acp::Error::request_cancelled()),
                () = self.bridge.peer.closed.cancelled() => return Err(acp::Error::request_cancelled()),
                result = tokio::time::timeout(Duration::from_secs(30), setup) => {
                    result.map_err(|_| failed("An MCP server did not respond in time."))??;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn order_steering(&self) -> SteeringOrder {
        let (done, next) = tokio::sync::oneshot::channel();
        let previous = self
            .last_steer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(next);
        (previous, done)
    }

    pub(crate) fn metadata(&self) -> Metadata {
        self.record
            .metadata
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(crate) fn lease(&self) -> Arc<Lease> {
        self.lease.clone()
    }

    pub(crate) fn cancel(&self) {
        self.turn.cancel();
        self.kit.abort_now();
    }

    pub(crate) async fn shutdown(&self) {
        self.turn.close();
        self.kit.abort_now();
        self.turn.wait().await;
        self.kit.abort().await;
        #[cfg(not(target_os = "ios"))]
        if let Some((processes, monitors)) = &self.local_processes {
            tokio::join!(processes.shutdown(), monitors.shutdown());
        }
        for client in &self.mcp {
            let _ = client.close().await;
        }
    }

    pub(crate) async fn close(&self) {
        self.shutdown().await;
        self.lease.release();
    }

    pub(crate) fn apply_metadata(&self, server: &AcpServer, next: &Metadata) -> Result<()> {
        let before = self.metadata();
        let set = |meta: &Metadata| -> Result<()> {
            if self.kit.model().provider != meta.provider || self.kit.model().id != meta.model_id {
                let mut model = server.auth.model(&meta.provider, &meta.model_id);
                model.api.clone_from(&meta.api);
                model.base_url.clone_from(&meta.base_url);
                self.kit
                    .set_model(model)
                    .map_err(|_| failed("Could not change the model."))?;
            }
            if self.kit.thinking_level() != meta.thinking_level {
                self.kit
                    .set_thinking_level(meta.thinking_level)
                    .map_err(|_| failed("Could not change the thinking level."))?;
            }
            if self.kit.fast() != meta.fast {
                self.kit
                    .set_fast(meta.fast)
                    .map_err(|_| failed("Could not change the speed."))?;
            }
            *self.bridge.mode.lock().unwrap() = meta.mode;
            Ok(())
        };
        set(next)?;
        if let Err(error) = server.sessions.update(&self.record, |meta| {
            // Title and archive state can change independently of the turn configuration.
            meta.mode = next.mode;
            meta.provider.clone_from(&next.provider);
            meta.model_id.clone_from(&next.model_id);
            meta.api.clone_from(&next.api);
            meta.base_url.clone_from(&next.base_url);
            meta.thinking_level = next.thinking_level;
            meta.fast = next.fast;
        }) {
            let _ = set(&before);
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn apply_prompt(
        &self,
        prompt: Option<acp::SystemPrompt>,
        base: Option<&str>,
    ) -> Result<()> {
        let Some(prompt) = prompt else { return Ok(()) };
        if !matches!(
            prompt.mode,
            acp::SystemPromptMode::Append | acp::SystemPromptMode::Override
        ) {
            return Err(invalid("Unsupported system prompt mode."));
        }
        let mut extra = String::new();
        for item in prompt.content {
            let acp::ContentBlock::Text(part) = item else {
                return Err(invalid("System instructions must be text."));
            };
            if !extra.is_empty() {
                extra.push_str("\n\n");
            }
            extra.push_str(&part.text);
        }
        let full = if prompt.mode == acp::SystemPromptMode::Append {
            let base_text = base.unwrap_or_default();
            if base_text.is_empty() {
                extra.clone()
            } else if extra.is_empty() {
                base_text.to_string()
            } else {
                format!("{base_text}\n\n{extra}")
            }
        } else {
            extra.clone()
        };
        if full != self.kit.system_prompt() {
            // A newly opened session can set its baseline. Later append instructions stay at
            // the tail so a load/fork never rewrites the established model request prefix or
            // duplicates the base prompt.
            if self.kit.entries().iter().any(|entry| {
                matches!(
                    entry.payload,
                    pikra_session::SessionEntryPayload::Message { .. }
                )
            }) {
                if prompt.mode == acp::SystemPromptMode::Override {
                    return Err(invalid(
                        "Start a new session to replace its system instructions.",
                    ));
                }
                if !extra.is_empty() {
                    self.kit.set_context("client_instructions", extra);
                }
            } else {
                self.kit
                    .set_system_prompt(full)
                    .map_err(|_| failed("Could not apply the system instructions."))?;
            }
        }
        Ok(())
    }

    pub(crate) fn commands(&self) -> acp::SessionUpdate {
        let command = |name, description, hint: Option<&str>| {
            acp::AvailableCommand::new(name, description).input(
                hint.map(acp::UnstructuredCommandInput::new)
                    .map(acp::AvailableCommandInput::Unstructured),
            )
        };
        let mut commands = vec![
            command(
                "compact",
                "Summarize the conversation",
                Some("custom focus instructions"),
            ),
            command("plan", "Switch to Plan mode", Some("task to plan")),
            command("reload-skills", "Reload skills", None::<&str>),
        ];
        let catalog = self.kit.skills();
        for skill in catalog.skills() {
            if !commands.iter().any(|command| command.name == skill.name) {
                commands.push(acp::AvailableCommand::new(&skill.name, &skill.description));
            }
        }
        acp::SessionUpdate::AvailableCommandsUpdate(acp::AvailableCommandsUpdate::new(commands))
    }

    pub(crate) fn notify_options(
        &self,
        server: &AcpServer,
        mode_changed: bool,
    ) -> Vec<acp::SessionConfigOption> {
        let meta = self.metadata();
        if mode_changed {
            self.bridge.update(acp::SessionUpdate::CurrentModeUpdate(
                acp::CurrentModeUpdate::new(meta.mode.id()),
            ));
        }
        let options = meta.options(&server.auth, &self.bridge.capabilities);
        self.bridge.update(acp::SessionUpdate::ConfigOptionUpdate(
            acp::ConfigOptionUpdate::new(options.clone()),
        ));
        options
    }

    pub(crate) fn notify_info(&self, meta: &Metadata) {
        self.bridge.update(acp::SessionUpdate::SessionInfoUpdate(
            acp::SessionInfoUpdate::new()
                .title(meta.title.clone())
                .updated_at(iso_date(meta.updated_at)),
        ));
    }
}
