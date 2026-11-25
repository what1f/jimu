use crate::{
    acp::{
        AcpServer,
        auth::{Auth, model_key, now_ms, read_optional, save},
        rpc::{Result, failed, invalid},
        schema::v1 as acp,
        server::Connection,
        tools_bridge::Bridge,
        turn::{Compaction, ModelStream, TurnSlot},
    },
    host::{
        BuiltWorkspaceTools, HostConfig, SessionFileLock, SessionLockError,
        attach_extensions_to_config, build_instruction_discovery, build_skill_discovery,
        build_workspace_tools, load_js_extensions, make_background_callback, resolve_system_prompt,
        restore_branch_model, restore_branch_preferences, set_model_atomic,
        should_enable_deferred_gateway,
    },
    mcp::{McpConnectError, McpHub, McpServerConfig, sanitize_mcp_prefix},
};
use jimu_agent::AgentMiddleware;
use jimu_ai::{ApiKind, CancelToken, Model, ThinkingLevel};
#[cfg(not(target_os = "ios"))]
use jimu_kit::SkillWatcher;
use jimu_kit::{KitSession, KitSessionConfig, KitToolSpec, SkillCatalog};
use jimu_session::{
    CURRENT_SESSION_VERSION, JsonlSessionStore, MemorySessionStore, SessionEntry, SessionHeader,
    SessionModelMetadata, SessionStore,
};
use jimu_tools::{WebFetchConfig, WebFetchTool};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
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
        if !self.model_id.is_empty() && !models.iter().any(|item| item.value.0.as_ref() == model_id)
        {
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
                let Some(model) = auth.select_model(text, Some(&self.provider)) else {
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

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ExplicitOptions {
    pub(crate) model: bool,
    pub(crate) thinking: bool,
    pub(crate) fast: bool,
}

pub(crate) fn initial_options(
    meta: &mut Metadata,
    auth: &Auth,
    caps: &acp::ClientCapabilities,
    options: &[acp::InitialSessionConfigOption],
) -> Result<ExplicitOptions> {
    let mut seen = HashSet::new();
    let mut explicit = ExplicitOptions::default();
    for option in options {
        if !seen.insert(&option.config_id) {
            return Err(invalid("A session option was supplied more than once."));
        }
        let applied = meta.set_option(auth, caps, option.config_id.0.as_ref(), &option.value);
        if applied {
            match option.config_id.0.as_ref() {
                "model" => explicit.model = true,
                "thought_level" => explicit.thinking = true,
                "fast" => explicit.fast = true,
                _ => {}
            }
        }
    }
    Ok(explicit)
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
    _file_lock: Option<SessionFileLock>,
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
    pub(crate) fn new(
        directory: Option<PathBuf>,
        auth: &Auth,
        default_mode: Mode,
        default_thinking: ThinkingLevel,
        default_fast: bool,
    ) -> std::result::Result<Self, String> {
        let mut records = HashMap::new();
        if let Some(dir) = &directory {
            fs::create_dir_all(dir).map_err(|_| "Could not open the session directory.")?;
            let files = fs::read_dir(dir).map_err(|_| "Could not read the session directory.")?;
            let mut jsonl_candidates = Vec::new();
            for item in files.flatten() {
                let path = item.path();
                let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                if let Some(id) = name.strip_suffix(".meta.json").filter(|id| uuid_id(id)) {
                    let jsonl_path = dir.join(format!("{id}.jsonl"));
                    if let Ok(Some(mut meta)) = read_optional::<Metadata>(&path)
                        && meta.session_id == id
                        && jsonl_path.is_file()
                    {
                        let store = Arc::new(JsonlSessionStore::new(&jsonl_path));
                        // Synchronize sidecar metadata against the live JSONL active branch so
                        // model/endpoint/effort changes made by TUI or RPC are never clobbered.
                        if let Ok((header, entries)) = store.load() {
                            let active_branch = branch(entries);
                            let prefs = restore_branch_preferences(
                                header.as_ref(),
                                &active_branch,
                                auth.store(),
                            );
                            if let Some(model) = prefs.model {
                                meta.provider = model.provider;
                                meta.model_id = model.id;
                                meta.api = model.api;
                                meta.base_url = model.base_url;
                            }
                            if let Some((level, fast)) = prefs.thinking {
                                meta.thinking_level = level;
                                meta.fast = fast;
                            }
                        }
                        records.insert(
                            id.into(),
                            Arc::new(Record {
                                metadata: Mutex::new(meta),
                                store,
                            }),
                        );
                    }
                } else if let Some(id) = name.strip_suffix(".jsonl").filter(|id| uuid_id(id)) {
                    jsonl_candidates.push((id.to_owned(), path));
                }
            }
            // Also load shared JSONL sessions created by TUI / RPC without a sidecar `.meta.json`.
            for (id, path) in jsonl_candidates {
                if records.contains_key(&id) {
                    continue;
                }
                let store = Arc::new(JsonlSessionStore::new(&path));
                let Ok((Some(header), entries)) = store.load() else {
                    continue;
                };
                let Some(cwd_str) = header.cwd.clone() else {
                    continue;
                };
                let mut updated_at = header.created_at;
                for entry in &entries {
                    updated_at = updated_at.max(entry.timestamp);
                }
                let active_branch = branch(entries);
                let prefs = restore_branch_preferences(Some(&header), &active_branch, auth.store());
                let resolved = prefs.model.unwrap_or_else(|| auth.default_model());
                let (thinking_level, fast) =
                    prefs.thinking.unwrap_or((default_thinking, default_fast));
                let mut title = None;
                for entry in &active_branch {
                    if let jimu_session::SessionEntryPayload::Message {
                        message: jimu_agent::AgentMessage::Llm(jimu_ai::Message::User(user)),
                    } = &entry.payload
                        && !user.is_meta
                        && title.is_none()
                    {
                        let text = user
                            .content
                            .iter()
                            .filter_map(|part| match part {
                                jimu_ai::ContentPart::Text(t) => Some(t.text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join(" ");
                        let trimmed = text.trim();
                        if !trimmed.is_empty() {
                            title = Some(trimmed.chars().take(80).collect());
                        }
                    }
                }
                let meta = Metadata {
                    session_id: id.clone(),
                    cwd: PathBuf::from(cwd_str),
                    additional_directories: Vec::new(),
                    title,
                    created_at: header.created_at,
                    updated_at,
                    archived: false,
                    mode: default_mode,
                    provider: resolved.provider,
                    model_id: resolved.id,
                    api: resolved.api,
                    base_url: resolved.base_url,
                    thinking_level,
                    fast,
                };
                records.insert(
                    id,
                    Arc::new(Record {
                        metadata: Mutex::new(meta),
                        store,
                    }),
                );
            }
        }
        Ok(Self {
            directory,
            records: Mutex::new(records),
            leased: Mutex::default(),
        })
    }

    pub(crate) fn try_lock_file(&self, id: &str) -> Result<Option<SessionFileLock>> {
        let Some((jsonl_path, _)) = self.durable(id) else {
            return Ok(None);
        };
        match SessionFileLock::try_acquire_typed(&jsonl_path) {
            Ok(lock) => Ok(Some(lock)),
            Err(SessionLockError::Busy(_)) => {
                Err(failed("This session is open in another connection."))
            }
            Err(SessionLockError::Io(_)) => Err(failed("Could not lock the session.")),
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
        initial_model: &Model,
        parent: Option<&str>,
        inherited_header: Option<&SessionHeader>,
        entries: &[SessionEntry],
    ) -> Result<Arc<Record>> {
        let store: Arc<dyn SessionStore> = self.durable(&meta.session_id).map_or_else(
            || Arc::new(MemorySessionStore::new()) as Arc<_>,
            |(path, _)| Arc::new(JsonlSessionStore::new(path)) as Arc<_>,
        );
        let (header_model, header_thinking, header_fast) =
            if let Some(parent_hdr) = inherited_header {
                (
                    parent_hdr.initial_model.clone(),
                    parent_hdr.initial_thinking,
                    parent_hdr.initial_fast,
                )
            } else {
                (
                    Some(SessionModelMetadata::from_model(initial_model)),
                    Some(meta.thinking_level),
                    Some(meta.fast),
                )
            };
        let initialize = || {
            store.write_header(&SessionHeader {
                version: CURRENT_SESSION_VERSION,
                id: meta.session_id.clone(),
                created_at: meta.created_at,
                cwd: Some(meta.cwd.to_string_lossy().into_owned()),
                parent_session: parent.map(str::to_owned),
                initial_model: header_model.clone(),
                initial_thinking: header_thinking,
                initial_fast: header_fast,
            })?;
            for entry in entries {
                store.append_entry(entry)?;
            }
            Ok::<_, jimu_session::SessionError>(())
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
    }

    pub(crate) fn delete(&self, id: &str) -> Result<()> {
        let mut records = self.records.lock().unwrap_or_else(|e| e.into_inner());
        if !records.contains_key(id) {
            return Err(invalid("Unknown session."));
        }
        if let Some((log, meta)) = self.durable(id) {
            // Sidecar metadata is optional for sessions created by TUI/RPC.
            if let Err(err) = fs::remove_file(meta)
                && err.kind() != ErrorKind::NotFound
            {
                return Err(failed("Could not delete the session."));
            }
            if let Err(err) = fs::remove_file(log)
                && err.kind() != ErrorKind::NotFound
            {
                records.remove(id);
                return Err(failed("The session history file could not be removed."));
            }
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

fn map_mcp_error(err: McpConnectError) -> acp::Error {
    match err {
        McpConnectError::Invalid(msg) => invalid(&msg),
        McpConnectError::Failed(msg) => failed(&msg),
        McpConnectError::Cancelled => acp::Error::request_cancelled(),
    }
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
    base_system_prompt: String,
    pub(crate) mcp_config: Vec<acp::McpServer>,
    mcp_hub: McpHub,
    built_tools: BuiltWorkspaceTools,
    #[cfg(not(target_os = "ios"))]
    _skill_watcher: Option<SkillWatcher>,
    lease: Arc<Lease>,
}

impl Session {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn open(
        connection: &Connection,
        capabilities: acp::ClientCapabilities,
        record: Arc<Record>,
        meta: &Metadata,
        mcp_config: Vec<acp::McpServer>,
        lease: Arc<Lease>,
        explicit: ExplicitOptions,
        cancel: &CancelToken,
    ) -> Result<Self> {
        let server = &connection.server;
        let cfg = &server.config;
        let host_cfg = HostConfig::from(cfg);

        // Convert and pre-check ACP-supplied MCP servers for uniqueness before session creation.
        let mut acp_mcp_servers = Vec::with_capacity(mcp_config.len());
        let mut acp_prefixes = HashSet::new();
        for descriptor in &mcp_config {
            let (name, spec) = McpServerConfig::from_acp(descriptor).map_err(invalid)?;
            let prefix = sanitize_mcp_prefix(&name);
            if prefix.is_empty() || !acp_prefixes.insert(prefix) {
                return Err(invalid("MCP server names must be unique."));
            }
            acp_mcp_servers.push((name, spec));
        }
        let (mcp_entries, mcp_settings_warnings) = host_cfg
            .resolve_mcp_entries(&acp_mcp_servers)
            .map_err(|err| invalid(&err))?;
        for warning in mcp_settings_warnings {
            eprintln!("warning: {warning}");
        }
        let has_mcp = !mcp_entries.is_empty();

        let web_fetch = cfg.tools.is_enabled("web_fetch").then(|| {
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
        // Re-read the active branch from the durable JSONL store so model/endpoint/effort changes
        // written by TUI or RPC take precedence over stale sidecars.
        let mut effective_meta = meta.clone();
        let mut restored_branch_model: Option<Model> = None;
        if let Ok((header, entries)) = record.store.load() {
            let active_branch = branch(entries);
            let prefs =
                restore_branch_preferences(header.as_ref(), &active_branch, server.auth.store());
            if !explicit.model
                && let Some(branch_model) = prefs.model
            {
                effective_meta.provider.clone_from(&branch_model.provider);
                effective_meta.model_id.clone_from(&branch_model.id);
                effective_meta.api.clone_from(&branch_model.api);
                effective_meta.base_url.clone_from(&branch_model.base_url);
                restored_branch_model = Some(branch_model);
            }
            if let Some((level, fast)) = prefs.thinking {
                if !explicit.thinking {
                    effective_meta.thinking_level = level;
                }
                if !explicit.fast {
                    effective_meta.fast = fast;
                }
            }
        }
        let meta = &effective_meta;

        let model = if let Some(restored) = restored_branch_model.clone() {
            restored
        } else {
            let mut m = server.auth.model(&meta.provider, &meta.model_id);
            m.api.clone_from(&meta.api);
            m.base_url.clone_from(&meta.base_url);
            m
        };
        let requested_model = model.clone();

        let mut opts = KitSessionConfig::new(model);
        opts.store = Some(record.store.clone());
        opts.cwd = Some(meta.cwd.to_string_lossy().into_owned());
        opts.thinking_level = meta.thinking_level;
        opts.fast = meta.fast;
        opts.enable_codemode = cfg.enable_codemode;
        opts.steering_mode = jimu_agent::QueueMode::All;
        opts.skill_discovery = build_skill_discovery(
            &meta.cwd,
            &cfg.jimu_home,
            cfg.skill_walk_boundary,
            &cfg.skill_dirs,
            &meta.additional_directories,
        );
        opts.instruction_discovery = build_instruction_discovery(
            cfg.instruction_discovery.as_ref(),
            &meta.cwd,
            &cfg.jimu_home,
            &meta.additional_directories,
        );
        opts.compaction.compaction_reserve_tokens = cfg.compact_tokens;
        opts.compaction_strategy = Some(compaction.clone());

        let bg_sender: Arc<std::sync::OnceLock<jimu_kit::BackgroundEventSender>> =
            Arc::new(std::sync::OnceLock::new());
        let bg_slot = Arc::clone(&bg_sender);
        let bg_callback = make_background_callback(move || bg_slot.get().cloned());

        let built_tools = build_workspace_tools(
            &meta.cwd,
            &server.transport,
            &cfg.tools,
            cfg.tools.is_explicit_allow(),
            Some(bg_callback),
            web_fetch,
        );
        opts.tools.extend(
            built_tools
                .tools
                .iter()
                .cloned()
                .map(|tool| KitToolSpec::resident(bridge.wrap(tool))),
        );

        let existing_names: HashSet<String> = opts
            .tools
            .iter()
            .map(|spec| spec.tool.definition().name)
            .collect();
        let has_extensions = if let Some(loaded) =
            load_js_extensions(&cfg.extensions, &existing_names).map_err(|err| failed(&err))?
        {
            attach_extensions_to_config(&mut opts, &loaded).map_err(|err| failed(&err))?;
            true
        } else {
            false
        };

        opts.enable_deferred_gateway = should_enable_deferred_gateway(
            &cfg.tools,
            cfg.enable_codemode,
            has_extensions,
            has_mcp,
        );
        let has_tools = !opts.tools.is_empty() || cfg.enable_codemode || has_mcp;
        let base_system_prompt = resolve_system_prompt(
            cfg.system_prompt.as_deref(),
            &meta.cwd,
            &cfg.jimu_home,
            &cfg.tools,
            has_tools,
        );
        opts.system_prompt = base_system_prompt.clone();
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
        bridge.update_skills(kit.skills());
        #[cfg(not(target_os = "ios"))]
        let skill_watcher = {
            let watcher_bridge = Arc::clone(&bridge);
            kit.watch_skills(move |catalog| {
                watcher_bridge.update_skills(catalog.clone());
                watcher_bridge.update(Self::commands_for_catalog(&catalog));
            })
            .ok()
        };
        let _ = bg_sender.set(kit.background_sender());

        let mut session = Self {
            record,
            kit,
            bridge,
            turn,
            compaction,
            steering: tokio::sync::Mutex::new(()),
            last_steer: Mutex::default(),
            base_system_prompt,
            mcp_config,
            mcp_hub: McpHub::default(),
            built_tools,
            #[cfg(not(target_os = "ios"))]
            _skill_watcher: skill_watcher,
            lease,
        };

        let combined_cancel = CancelToken::new();
        let cancel_forward = {
            let req_cancel = cancel.clone();
            let peer_closed = session.bridge.peer.closed.clone();
            let target = combined_cancel.clone();
            tokio::spawn(async move {
                tokio::select! {
                    () = req_cancel.cancelled() => target.cancel(),
                    () = peer_closed.cancelled() => target.cancel(),
                }
            })
        };

        let setup = async {
            let (hub, connect_warnings) = McpHub::connect(
                mcp_entries,
                &session.bridge.cwd,
                server.transport.clone(),
                combined_cancel.clone(),
            )
            .await
            .map_err(map_mcp_error)?;
            for warning in connect_warnings {
                eprintln!("warning: {warning}");
            }
            session.mcp_hub = hub;
            let attach_warnings = session
                .mcp_hub
                .attach_to_session(&session.kit, combined_cancel.clone())
                .await
                .map_err(map_mcp_error)?;
            for warning in attach_warnings {
                eprintln!("warning: {warning}");
            }
            if explicit.model {
                let same_route = restore_branch_model(&session.kit, server.auth.store())
                    .is_some_and(|recorded| {
                        recorded.provider == requested_model.provider
                            && recorded.id == requested_model.id
                            && recorded.api == requested_model.api
                            && recorded.base_url == requested_model.base_url
                            && recorded.context_window == requested_model.context_window
                    });
                if !same_route {
                    set_model_atomic(&session.kit, requested_model)
                        .map_err(|_| failed("Could not record the selected model."))?;
                }
            } else if let Some(restored) = restored_branch_model {
                session
                    .kit
                    .restore_model(restored)
                    .map_err(|_| failed("Could not restore the session model."))?;
            }
            if session.kit.thinking_level() != effective_meta.thinking_level {
                session
                    .kit
                    .set_thinking_level(effective_meta.thinking_level)
                    .map_err(|_| failed("Could not set the thinking level."))?;
            }
            if session.kit.fast() != effective_meta.fast {
                session
                    .kit
                    .set_fast(effective_meta.fast)
                    .map_err(|_| failed("Could not set the speed."))?;
            }
            Ok::<(), acp::Error>(())
        }
        .await;
        cancel_forward.abort();

        if let Err(error) = setup {
            session.shutdown().await;
            return Err(error);
        }
        {
            let mut slot = session
                .record
                .metadata
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let (title, archived) = (slot.title.clone(), slot.archived);
            *slot = effective_meta;
            slot.title = title;
            slot.archived = archived;
        }
        Ok(session)
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
        self.built_tools.shutdown().await;
        self.mcp_hub.shutdown().await;
    }

    pub(crate) async fn close(&self) {
        self.shutdown().await;
        self.lease.release();
    }

    pub(crate) fn apply_metadata(&self, server: &AcpServer, next: &Metadata) -> Result<()> {
        let before = self.metadata();
        let set = |meta: &Metadata| -> Result<()> {
            if self.kit.model().provider != meta.provider
                || self.kit.model().id != meta.model_id
                || self.kit.model().api != meta.api
                || self.kit.model().base_url != meta.base_url
            {
                let mut model = server.auth.model(&meta.provider, &meta.model_id);
                model.api.clone_from(&meta.api);
                model.base_url.clone_from(&meta.base_url);
                set_model_atomic(&self.kit, model)
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

    pub(crate) fn apply_prompt(&self, prompt: Option<acp::SystemPrompt>) -> Result<()> {
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
            let base_text = self.base_system_prompt.as_str();
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
                    jimu_session::SessionEntryPayload::Message { .. }
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
        Self::commands_for_catalog(&self.kit.skills())
    }

    pub(crate) fn commands_for_catalog(catalog: &SkillCatalog) -> acp::SessionUpdate {
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
        for skill in catalog.skills() {
            if !commands.iter().any(|command| command.name == skill.name) {
                commands.push(acp::AvailableCommand::new(&skill.name, &skill.description));
            }
        }
        acp::SessionUpdate::AvailableCommandsUpdate(acp::AvailableCommandsUpdate::new(commands))
    }

    pub(crate) fn sync_skills(&self, force_notify: bool) {
        let previous = self.kit.skills();
        self.kit.reload_skills();
        let current = self.kit.skills();
        let changed = !previous.has_same_catalog_metadata(&current);
        self.bridge.update_skills(current.clone());
        if force_notify || changed {
            self.bridge.update(Self::commands_for_catalog(&current));
        }
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
