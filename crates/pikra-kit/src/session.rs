use crate::codemode::{CODEMODE_EXEC_TOOL_NAME, CodemodeExecTool};
use crate::deferred::{
    CALL_TOOL_NAME, CallToolGateway, DeferredToolRegistry, TOOL_SEARCH_NAME, ToolSearchTool,
};
use crate::error::KitError;
use crate::instructions::{
    InstructionCatalog, InstructionContextState, InstructionDiscoveryConfig,
};
use crate::mcp::{McpClient, McpToolRegistrationMode};
use crate::skills::{SKILL_SEARCH_NAME, SkillCatalog, SkillDiscoveryConfig, SkillSearchTool};
use crate::wakeup::{
    Activity, ActivityGuard, BackgroundEventSender, BackgroundInbox, KitTurnOrigin,
};
use futures_channel::mpsc;
use futures_core::Stream;
use futures_util::StreamExt;
use pikra_agent::{
    Agent, AgentConfig, AgentEvent, AgentMessage, AgentMiddleware, AgentTool, ApiKeyResolver,
    QueueMode, ToolExecutionMode,
};
use pikra_ai::{
    BoxFuture, CacheRetention, CancelToken, ContentPart, Message, Model, SimpleStreamOptions,
    StopReason, StreamProvider, ThinkingLevel, ToolDefinition, UserMessage,
};
use pikra_codemode::CodemodeSandboxOptions;
use pikra_session::{
    CompactionConfig, CompactionResult, CompactionStrategy, MemorySessionStore, SessionConfig,
    SessionEntry, SessionEntryPayload, SessionEvent, SessionManager, SessionStore, SessionTreeNode,
    baseline_to_system_message, context_block_to_agent_message,
};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
use std::time::{SystemTime, UNIX_EPOCH};

use crate::skill_context::{SKILLS_CONTEXT_NAME, SkillContextState};
const DEFAULT_AUTO_DEFER_BYTES: usize = 8192;
const DEFERRED_TOOLS_CONTEXT_NAME: &str = "deferred_tools";

fn now_ms() -> u64 {
    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    {
        0
    }
    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// Controls how a tool is exposed to the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KitToolRegistrationMode {
    Resident,
    Deferred,
    Auto,
}

#[derive(Clone)]
pub struct KitToolSpec {
    pub tool: Arc<dyn AgentTool>,
    pub mode: KitToolRegistrationMode,
}

impl KitToolSpec {
    pub fn resident(tool: Arc<dyn AgentTool>) -> Self {
        Self {
            tool,
            mode: KitToolRegistrationMode::Resident,
        }
    }

    pub fn deferred(tool: Arc<dyn AgentTool>) -> Self {
        Self {
            tool,
            mode: KitToolRegistrationMode::Deferred,
        }
    }

    pub fn auto(tool: Arc<dyn AgentTool>) -> Self {
        Self {
            tool,
            mode: KitToolRegistrationMode::Auto,
        }
    }
}

/// Configuration for building a [`KitSession`].
#[derive(Clone)]
pub struct KitSessionConfig {
    pub model: Model,
    pub system_prompt: String,
    pub tools: Vec<KitToolSpec>,
    pub thinking_level: ThinkingLevel,
    pub fast: bool,
    pub skill_roots: Vec<PathBuf>,
    pub skill_discovery: SkillDiscoveryConfig,
    pub instruction_discovery: Option<InstructionDiscoveryConfig>,
    /// Expose a resident skill search tool (default: true).
    pub enable_skill_search: bool,
    /// Resident schema budget for automatic placement. Explicit resident tools are pinned.
    /// When omitted, tools explicitly marked Auto use an 8 KiB budget.
    pub auto_defer_threshold_bytes: Option<usize>,
    /// Pause background turns after an error or abort, until a foreground turn succeeds
    /// or the host calls resume_background (default: true).
    pub pause_background_on_error: bool,
    /// Expose resident `tool_search` & `call_tool` gateway tools (default: `true`).
    pub enable_deferred_gateway: bool,
    /// Expose resident `exec` tool backed by `pikra_codemode::CodemodeSandbox` (default: `false`).
    pub enable_codemode: bool,
    pub codemode_options: CodemodeSandboxOptions,
    pub store: Option<Arc<dyn SessionStore>>,
    pub cwd: Option<String>,
    pub compaction: CompactionConfig,
    pub compaction_strategy: Option<Arc<dyn CompactionStrategy>>,
    pub tool_execution_mode: ToolExecutionMode,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub middlewares: Vec<Arc<dyn AgentMiddleware>>,
    pub api_key: Option<String>,
    pub get_api_key: Option<ApiKeyResolver>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    pub cache_retention: CacheRetention,
    pub headers: Vec<(String, Option<String>)>,
    pub sampling_params: Map<String, Value>,
}

impl KitSessionConfig {
    pub fn new(model: Model) -> Self {
        Self {
            model,
            system_prompt: String::new(),
            tools: Vec::new(),
            thinking_level: ThinkingLevel::Off,
            fast: false,
            skill_roots: Vec::new(),
            skill_discovery: SkillDiscoveryConfig::default(),
            instruction_discovery: None,
            enable_skill_search: true,
            auto_defer_threshold_bytes: None,
            pause_background_on_error: true,
            enable_deferred_gateway: true,
            enable_codemode: false,
            codemode_options: CodemodeSandboxOptions::default(),
            store: None,
            cwd: None,
            compaction: CompactionConfig::default(),
            compaction_strategy: None,
            tool_execution_mode: ToolExecutionMode::Parallel,
            steering_mode: QueueMode::OneAtATime,
            follow_up_mode: QueueMode::OneAtATime,
            middlewares: Vec::new(),
            api_key: None,
            get_api_key: None,
            temperature: None,
            max_tokens: None,
            cache_retention: CacheRetention::default(),
            headers: Vec::new(),
            sampling_params: Map::new(),
        }
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = prompt.into();
        self
    }

    pub fn with_tool(mut self, spec: KitToolSpec) -> Self {
        self.tools.push(spec);
        self
    }

    pub fn with_resident_tool(mut self, tool: Arc<dyn AgentTool>) -> Self {
        self.tools.push(KitToolSpec::resident(tool));
        self
    }

    pub fn with_deferred_tool(mut self, tool: Arc<dyn AgentTool>) -> Self {
        self.tools.push(KitToolSpec::deferred(tool));
        self
    }

    pub fn with_skill_root(mut self, path: impl Into<PathBuf>) -> Self {
        self.skill_roots.push(path.into());
        self
    }

    pub fn with_skill_discovery(mut self, discovery: SkillDiscoveryConfig) -> Self {
        self.skill_discovery = discovery;
        self
    }

    pub fn with_instruction_discovery(mut self, discovery: InstructionDiscoveryConfig) -> Self {
        self.instruction_discovery = Some(discovery);
        self
    }

    pub fn with_codemode(mut self, enabled: bool) -> Self {
        self.enable_codemode = enabled;
        self
    }

    pub fn with_store(mut self, store: Arc<dyn SessionStore>) -> Self {
        self.store = Some(store);
        self
    }

    pub fn with_compaction(mut self, compaction: CompactionConfig) -> Self {
        self.compaction = compaction;
        self
    }

    pub fn with_compaction_strategy(mut self, strategy: Arc<dyn CompactionStrategy>) -> Self {
        self.compaction_strategy = Some(strategy);
        self
    }
}

/// Unified stream events yielded by [`KitSession::prompt_stream`].
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum KitEvent {
    Agent(AgentEvent),
    Session(SessionEvent),
    Error(String),
}

/// Runtime-agnostic stream returned by [`KitSession::prompt_stream`].
pub struct KitEventStream<'a> {
    rx: mpsc::UnboundedReceiver<KitEvent>,
    driver: Option<BoxFuture<'a, Result<(), KitError>>>,
    terminal_error: Option<String>,
    origin: KitTurnOrigin,
}

impl KitEventStream<'_> {
    pub fn origin(&self) -> KitTurnOrigin {
        self.origin
    }
}

impl Stream for KitEventStream<'_> {
    type Item = KitEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Poll::Ready(Some(ev)) = Pin::new(&mut self.rx).poll_next(cx) {
            return Poll::Ready(Some(ev));
        }

        if let Some(driver) = self.driver.as_mut() {
            match driver.as_mut().poll(cx) {
                Poll::Ready(res) => {
                    self.driver = None;
                    if let Err(err) = res {
                        self.terminal_error = Some(err.to_string());
                    }
                    return match Pin::new(&mut self.rx).poll_next(cx) {
                        Poll::Ready(Some(item)) => Poll::Ready(Some(item)),
                        Poll::Ready(None) | Poll::Pending => {
                            Poll::Ready(self.terminal_error.take().map(KitEvent::Error))
                        }
                    };
                }
                Poll::Pending => {
                    if let Poll::Ready(Some(ev)) = Pin::new(&mut self.rx).poll_next(cx) {
                        return Poll::Ready(Some(ev));
                    }
                    return Poll::Pending;
                }
            }
        }

        match Pin::new(&mut self.rx).poll_next(cx) {
            Poll::Ready(Some(ev)) => Poll::Ready(Some(ev)),
            Poll::Ready(None) => Poll::Ready(self.terminal_error.take().map(KitEvent::Error)),
            Poll::Pending => {
                if let Some(err) = self.terminal_error.take() {
                    Poll::Ready(Some(KitEvent::Error(err)))
                } else {
                    Poll::Pending
                }
            }
        }
    }
}

pub struct ToolSchemaBudget {
    remaining: usize,
}

impl ToolSchemaBudget {
    pub fn new(threshold: Option<usize>, pinned: &[ToolDefinition]) -> Self {
        let budget = threshold.unwrap_or(DEFAULT_AUTO_DEFER_BYTES);
        let occupied = pinned.iter().fold(0usize, |total, definition| {
            total.saturating_add(Self::size(definition))
        });
        Self {
            remaining: budget.saturating_sub(occupied),
        }
    }

    fn size(definition: &ToolDefinition) -> usize {
        serde_json::to_vec(definition).map_or(usize::MAX, |bytes| bytes.len())
    }

    pub fn admit(&mut self, definition: &ToolDefinition) -> bool {
        let Some(remaining) = self.remaining.checked_sub(Self::size(definition)) else {
            self.remaining = 0;
            return false;
        };
        self.remaining = remaining;
        true
    }
}

/// High-level headless session facade combining `Agent`, `SessionManager`, `SkillCatalog`,
/// deferred tool gateway (`tool_search` + `call_tool`), optional `exec` codemode sandbox, and MCP.
pub struct KitSession {
    provider: Arc<dyn StreamProvider>,
    agent: Agent,
    session: Arc<Mutex<SessionManager>>,
    /// Resident tools excluding `exec` so `CodemodeExecTool` never forms an `Arc` cycle.
    resident_tools: Arc<Mutex<Vec<Arc<dyn AgentTool>>>>,
    codemode_exec_tool: Option<Arc<dyn AgentTool>>,
    deferred_registry: Arc<DeferredToolRegistry>,
    skills: Arc<Mutex<SkillCatalog>>,
    skill_discovery: SkillDiscoveryConfig,
    known_skills: Mutex<SkillContextState>,
    instructions: Mutex<InstructionContextState>,
    auto_defer_threshold_bytes: Option<usize>,
    pause_background_on_error: bool,
    inbox: BackgroundInbox,
    had_deferred_catalog: Mutex<bool>,
    api_key: Option<String>,
    get_api_key: Option<ApiKeyResolver>,
    cache_retention: CacheRetention,
    headers: Vec<(String, Option<String>)>,
}

impl KitSession {
    /// Construct a new or reopened [`KitSession`].
    pub fn new(
        provider: Arc<dyn StreamProvider>,
        mut config: KitSessionConfig,
    ) -> Result<Self, KitError> {
        let deferred_registry = Arc::new(DeferredToolRegistry::new());
        let mut resident: Vec<Arc<dyn AgentTool>> = Vec::new();
        let mut deferred: Vec<Arc<dyn AgentTool>> = Vec::new();

        let mut skill_discovery = config.skill_discovery;
        skill_discovery.extra_roots.append(&mut config.skill_roots);
        if skill_discovery.cwd.is_none() {
            skill_discovery.cwd = config.cwd.as_ref().map(PathBuf::from);
        }
        let skills = Arc::new(Mutex::new(SkillCatalog::discover_with_config(
            &skill_discovery,
        )));

        let pinned: Vec<_> = config
            .tools
            .iter()
            .filter(|spec| spec.mode == KitToolRegistrationMode::Resident)
            .map(|spec| spec.tool.definition())
            .collect();
        let mut budget = ToolSchemaBudget::new(config.auto_defer_threshold_bytes, &pinned);
        for spec in config.tools {
            match spec.mode {
                KitToolRegistrationMode::Resident => resident.push(spec.tool),
                KitToolRegistrationMode::Auto if budget.admit(&spec.tool.definition()) => {
                    resident.push(spec.tool)
                }
                KitToolRegistrationMode::Auto | KitToolRegistrationMode::Deferred => {
                    deferred.push(spec.tool)
                }
            }
        }
        deferred_registry.upsert(deferred);
        if config.enable_skill_search
            && !resident
                .iter()
                .any(|tool| tool.definition().name == SKILL_SEARCH_NAME)
        {
            resident.push(Arc::new(SkillSearchTool::shared(Arc::clone(&skills))));
        }

        // Always keep built-in deferred gateway resident (when enabled) so adding/removing
        // deferred tools later NEVER mutates the leading resident `tools` array!
        if config.enable_deferred_gateway {
            if !resident
                .iter()
                .any(|t| t.definition().name == TOOL_SEARCH_NAME)
            {
                resident.push(Arc::new(ToolSearchTool::new(Arc::clone(
                    &deferred_registry,
                ))));
            }
            if !resident
                .iter()
                .any(|t| t.definition().name == CALL_TOOL_NAME)
            {
                resident.push(Arc::new(
                    CallToolGateway::new(Arc::clone(&deferred_registry))
                        .with_middlewares(config.middlewares.clone()),
                ));
            }
        }

        // Keep `exec` out of `resident_tools_handle` so `CodemodeExecTool` never holds an
        // `Arc` cycle back to itself (which would leak user tools after `KitSession` drops).
        let resident_tools_handle = Arc::new(Mutex::new(
            resident
                .iter()
                .filter(|t| t.definition().name != CODEMODE_EXEC_TOOL_NAME)
                .cloned()
                .collect(),
        ));

        let codemode_exec_tool = if config.enable_codemode
            && !resident
                .iter()
                .any(|t| t.definition().name == CODEMODE_EXEC_TOOL_NAME)
        {
            let exec_tool: Arc<dyn AgentTool> = Arc::new(
                CodemodeExecTool::new(
                    Arc::clone(&resident_tools_handle),
                    Arc::clone(&deferred_registry),
                    config.codemode_options.clone(),
                )
                .with_middlewares(config.middlewares.clone()),
            );
            resident.push(Arc::clone(&exec_tool));
            Some(exec_tool)
        } else {
            resident
                .iter()
                .find(|t| t.definition().name == CODEMODE_EXEC_TOOL_NAME)
                .cloned()
        };

        let resident_defs: Vec<ToolDefinition> = resident.iter().map(|t| t.definition()).collect();
        let store = config
            .store
            .unwrap_or_else(|| Arc::new(MemorySessionStore::new()));

        let mut session_cfg = SessionConfig::new(&config.system_prompt, resident_defs)
            .with_compaction(config.compaction.clone());
        if let Some(cwd) = &config.cwd {
            session_cfg = session_cfg.with_cwd(cwd.clone());
        }
        if let Some(strategy) = config.compaction_strategy.clone() {
            session_cfg = session_cfg.with_compaction_strategy(strategy);
        }

        let mut session_manager = SessionManager::open(store, session_cfg)?;
        let reconstructed = session_manager.reconstruct_context();
        let session_id = session_manager.session_id().to_string();

        let effective_thinking = reconstructed
            .thinking
            .map(|(lvl, _)| lvl)
            .unwrap_or(config.thinking_level);
        let effective_fast = reconstructed
            .thinking
            .map(|(_, f)| f)
            .unwrap_or(config.fast);

        let known_skills = SkillContextState::from_branch(&session_manager);
        let instructions = InstructionContextState::new(
            config.instruction_discovery,
            config.cwd.as_deref().map(Path::new),
            &mut session_manager,
            &reconstructed,
        );

        let mut had_deferred_catalog = false;
        if let Some(catalog) = deferred_registry.format_catalog() {
            session_manager.set_context(DEFERRED_TOOLS_CONTEXT_NAME, catalog);
            had_deferred_catalog = true;
        }

        let mut agent_cfg = AgentConfig::new(config.model)
            .with_system_prompt(reconstructed.system_prompt)
            .with_tools(resident)
            .with_thinking_level(effective_thinking)
            .with_fast(effective_fast)
            .with_tool_execution_mode(config.tool_execution_mode)
            .with_steering_mode(config.steering_mode)
            .with_follow_up_mode(config.follow_up_mode);
        agent_cfg.middlewares = config.middlewares;
        agent_cfg.api_key = config.api_key.clone();
        agent_cfg.get_api_key = config.get_api_key.clone();
        agent_cfg.temperature = config.temperature;
        agent_cfg.max_tokens = config.max_tokens;
        agent_cfg.cache_retention = config.cache_retention;
        agent_cfg.session_id = Some(session_id);
        agent_cfg.headers = config.headers.clone();
        agent_cfg.sampling_params = config.sampling_params;
        agent_cfg.initial_messages = reconstructed.messages;

        let agent = Agent::new(Arc::clone(&provider), agent_cfg);

        Ok(Self {
            provider,
            agent,
            session: Arc::new(Mutex::new(session_manager)),
            resident_tools: resident_tools_handle,
            codemode_exec_tool,
            deferred_registry,
            skills,
            skill_discovery,
            known_skills: Mutex::new(known_skills),
            instructions: Mutex::new(instructions),
            auto_defer_threshold_bytes: config.auto_defer_threshold_bytes,
            pause_background_on_error: config.pause_background_on_error,
            inbox: BackgroundInbox::default(),
            had_deferred_catalog: Mutex::new(had_deferred_catalog),
            api_key: config.api_key,
            get_api_key: config.get_api_key,
            cache_retention: config.cache_retention,
            headers: config.headers,
        })
    }

    // ---- Observable State ----

    pub fn session_id(&self) -> String {
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .session_id()
            .to_string()
    }

    pub fn leaf_id(&self) -> Option<String> {
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .leaf_id()
            .map(ToString::to_string)
    }

    pub fn entries(&self) -> Vec<SessionEntry> {
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries()
            .to_vec()
    }

    pub fn tree(&self) -> Vec<SessionTreeNode> {
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .tree()
    }

    pub fn messages(&self) -> Vec<AgentMessage> {
        self.agent.messages()
    }

    pub fn model(&self) -> Model {
        self.agent.model()
    }

    pub fn thinking_level(&self) -> ThinkingLevel {
        self.agent.thinking_level()
    }

    pub fn fast(&self) -> bool {
        self.agent.fast()
    }

    pub fn system_prompt(&self) -> String {
        self.agent.system_prompt()
    }

    pub fn is_running(&self) -> bool {
        self.inbox.is_active() || self.agent.is_running()
    }

    fn try_begin_activity(&self, activity: Activity) -> Result<ActivityGuard, KitError> {
        if self.agent.is_running() {
            return Err(KitError::Busy);
        }
        Ok(self
            .inbox
            .begin(activity, self.pause_background_on_error)?
            .expect("foreground prompts and compaction do not require a background event"))
    }

    fn with_idle_mutation<R>(
        &self,
        f: impl FnOnce() -> Result<R, KitError>,
    ) -> Result<R, KitError> {
        self.inbox
            .with_idle(|| {
                if self.agent.is_running() {
                    return Err(KitError::Busy);
                }
                f()
            })
            .ok_or(KitError::Busy)?
    }

    fn sync_agent_leading_system_message(
        &self,
        system_prompt: &str,
        tools: &[ToolDefinition],
    ) -> Result<(), KitError> {
        let mut messages = self.agent.messages();
        if let Some(AgentMessage::Llm(Message::System(sys))) = messages.first_mut() {
            sys.content = system_prompt.to_string();
            sys.tools_added = tools.to_vec();
        } else {
            messages.insert(
                0,
                baseline_to_system_message(system_prompt, tools, now_ms()),
            );
        }
        self.agent.replace_messages(messages)?;
        Ok(())
    }

    pub fn resident_tool_definitions(&self) -> Vec<ToolDefinition> {
        self.agent.tool_definitions()
    }

    pub fn deferred_tools(&self) -> Vec<ToolDefinition> {
        self.deferred_registry
            .list_tools()
            .into_iter()
            .map(|t| t.definition())
            .collect()
    }

    pub fn skills(&self) -> SkillCatalog {
        self.skills
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Re-scan configured skill roots. The next prompt delivers the net additions and removals.
    /// Returns whether model-visible skill names changed.
    pub fn reload_skills(&self) -> bool {
        let mut skills = self.skills.lock().unwrap_or_else(|e| e.into_inner());
        let discovered = SkillCatalog::discover_with_config(&self.skill_discovery);
        let changed = !skills.has_same_visible_names(&discovered);
        *skills = discovered;
        changed
    }

    pub fn instructions(&self) -> InstructionCatalog {
        self.instructions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .catalog()
            .clone()
    }

    /// Refresh the instruction catalog and queue any change for the next prompt.
    pub fn reload_instructions(&self) -> bool {
        let mut session = self.session.lock().unwrap_or_else(|e| e.into_inner());
        self.instructions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reload(&mut session)
    }

    // ---- Idle Configuration Updates (Zero Spurious Busy) ----

    pub fn set_model(&self, model: Model) -> Result<(), KitError> {
        self.with_idle_mutation(|| {
            self.agent.set_model(model.clone())?;
            self.session
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .append_model_change(model.provider, model.id)?;
            Ok(())
        })
    }

    pub fn set_thinking_level(&self, level: ThinkingLevel) -> Result<(), KitError> {
        self.with_idle_mutation(|| {
            self.agent.set_thinking_level(level)?;
            let fast = self.agent.fast();
            self.session
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .append_thinking_change(level, fast)?;
            Ok(())
        })
    }

    pub fn set_fast(&self, fast: bool) -> Result<(), KitError> {
        self.with_idle_mutation(|| {
            self.agent.set_fast(fast)?;
            let level = self.agent.thinking_level();
            self.session
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .append_thinking_change(level, fast)?;
            Ok(())
        })
    }

    pub fn set_system_prompt(&self, prompt: impl Into<String>) -> Result<(), KitError> {
        let prompt = prompt.into();
        self.with_idle_mutation(|| {
            let tool_defs = self.agent.tool_definitions();
            self.session
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .append_system_baseline(&prompt, tool_defs.clone())?;
            self.agent.set_system_prompt(&prompt)?;
            self.sync_agent_leading_system_message(&prompt, &tool_defs)?;
            Ok(())
        })
    }

    /// Set or update a named `<system-reminder>` context block (deduplicated; NO-OP if unchanged).
    pub fn set_context(&self, name: impl Into<String>, text: impl Into<String>) -> bool {
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_context(name, text)
    }

    /// Queue a one-off `<system-reminder>` context block for the next turn.
    pub fn append_context(&self, text: impl Into<String>) {
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .append_context(text);
    }

    /// Dynamically add or update deferred tools while idle.
    ///
    /// NEVER mutates the top-level resident `tools` array; updates the `"deferred_tools"`
    /// `<system-reminder>` catalog only when changed.
    pub fn upsert_tools(&self, tools: Vec<Arc<dyn AgentTool>>) -> Result<(), KitError> {
        self.with_idle_mutation(|| {
            self.deferred_registry.upsert(tools);
            self.sync_deferred_tools_context();
            Ok(())
        })
    }

    /// Dynamically remove deferred tools by name while idle.
    ///
    /// NEVER mutates the top-level resident `tools` array; updates the `"deferred_tools"`
    /// `<system-reminder>` catalog only when changed.
    pub fn remove_tools(&self, names: &[&str]) -> Result<usize, KitError> {
        self.with_idle_mutation(|| {
            let removed = self.deferred_registry.remove(names);
            if removed > 0 {
                self.sync_deferred_tools_context();
            }
            Ok(removed)
        })
    }

    fn sync_deferred_tools_context(&self) {
        let mut had_catalog = self
            .had_deferred_catalog
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match self.deferred_registry.format_catalog() {
            Some(catalog) => {
                *had_catalog = true;
                self.session
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .set_context(DEFERRED_TOOLS_CONTEXT_NAME, catalog);
            }
            None if *had_catalog => {
                self.session
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .set_context(
                        DEFERRED_TOOLS_CONTEXT_NAME,
                        "No deferred tools are currently registered.",
                    );
            }
            None => {}
        }
    }

    /// Discover tools from an initialized [`McpClient`] and attach them to this session as
    /// either resident or deferred tools. Returns the exposed tool names.
    pub async fn attach_mcp_client(
        &self,
        client: Arc<McpClient>,
        mode: McpToolRegistrationMode,
        name_prefix: Option<&str>,
        cancel: CancelToken,
    ) -> Result<Vec<String>, KitError> {
        if self.is_running() {
            return Err(KitError::Busy);
        }
        let discovered = client.list_tools(cancel).await?;
        let agent_tools = client.to_agent_tools(&discovered, name_prefix);
        let names: Vec<String> = agent_tools.iter().map(|t| t.definition().name).collect();

        self.with_idle_mutation(|| {
            let (resident, deferred): (Vec<_>, Vec<_>) = match mode {
                McpToolRegistrationMode::Resident => (agent_tools, Vec::new()),
                McpToolRegistrationMode::Deferred => (Vec::new(), agent_tools),
                McpToolRegistrationMode::Auto => {
                    let definitions = self.agent.tool_definitions();
                    let existing: std::collections::HashSet<_> =
                        definitions.iter().map(|def| def.name.as_str()).collect();
                    let mut budget =
                        ToolSchemaBudget::new(self.auto_defer_threshold_bytes, &definitions);
                    let has_started = self
                        .session
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .entries()
                        .iter()
                        .any(|entry| matches!(entry.payload, SessionEntryPayload::Message { .. }));
                    agent_tools
                        .into_iter()
                        .filter(|tool| !existing.contains(tool.definition().name.as_str()))
                        .partition(|tool| {
                            let definition = tool.definition();
                            !has_started
                                && self.deferred_registry.get(&definition.name).is_none()
                                && budget.admit(&definition)
                        })
                }
            };
            if !resident.is_empty() {
                self.attach_resident_tools(resident)?;
            }
            if !deferred.is_empty() {
                self.deferred_registry.upsert(deferred);
                self.sync_deferred_tools_context();
            }
            Ok(())
        })?;
        Ok(names)
    }

    fn attach_resident_tools(&self, tools: Vec<Arc<dyn AgentTool>>) -> Result<(), KitError> {
        let mut resident = self
            .resident_tools
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut next_resident = resident.clone();
        let mut exec = self.codemode_exec_tool.clone();
        for tool in tools {
            let name = tool.definition().name;
            if name == CODEMODE_EXEC_TOOL_NAME {
                exec = Some(tool);
            } else if let Some(existing) = next_resident
                .iter_mut()
                .find(|existing| existing.definition().name == name)
            {
                *existing = tool;
            } else {
                next_resident.push(tool);
            }
        }
        let mut full_tools = next_resident.clone();
        full_tools.extend(exec);
        let definitions: Vec<_> = full_tools.iter().map(|tool| tool.definition()).collect();
        let prompt = self.agent.system_prompt();
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .append_system_baseline(&prompt, definitions.clone())?;
        self.agent.set_tools(full_tools)?;
        self.sync_agent_leading_system_message(&prompt, &definitions)?;
        *resident = next_resident;
        Ok(())
    }

    /// Branch the session to `entry_id` without deleting sibling entries and re-sync `Agent`.
    pub fn fork(&self, entry_id: &str) -> Result<(), KitError> {
        self.with_idle_mutation(|| {
            let reconstructed = {
                let mut session = self.session.lock().unwrap_or_else(|e| e.into_inner());
                session.fork(entry_id)?;
                let reconstructed = session.reconstruct_context();
                self.instructions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .sync_branch(&mut session, &reconstructed);
                let mut known = self.known_skills.lock().unwrap_or_else(|e| e.into_inner());
                known.restore_fork(
                    &mut session,
                    reconstructed
                        .all_branch_context_blocks
                        .get(SKILLS_CONTEXT_NAME)
                        .map(String::as_str),
                );
                reconstructed
            };
            self.deferred_registry.clear_seen_schemas();
            self.agent.set_system_prompt(&reconstructed.system_prompt)?;
            self.agent.replace_messages(reconstructed.messages)?;
            Ok(())
        })
    }

    /// Run manual compaction immediately while idle.
    pub async fn compact(&self) -> Result<Option<CompactionResult>, KitError> {
        let _activity_guard = self.try_begin_activity(Activity::Compacting)?;
        self.run_compaction_internal(false, None).await
    }

    // ---- Steering, Follow-Up, Abort ----

    pub fn steer(&self, id: impl Into<String>, text: impl Into<String>) {
        self.agent.steer(id, text);
    }

    pub fn drain_steering(&self) -> Vec<(String, Vec<ContentPart>)> {
        self.agent.drain_steering()
    }

    pub fn follow_up(&self, id: impl Into<String>, text: impl Into<String>) {
        self.agent.follow_up(id, text);
    }

    pub fn abort_now(&self) {
        self.inbox.abort_prompt();
        self.agent.abort_now();
    }

    /// Cancel any active run and await complete teardown so immediate subsequent calls never
    /// spuriously fail with `Busy`.
    pub async fn abort(&self) {
        self.inbox.abort_prompt();
        self.agent.abort().await;
    }

    pub async fn wait_for_idle(&self) {
        self.agent.wait_for_idle().await;
    }

    // ---- Prompt Execution ----

    /// Run a text prompt to completion, persisting entries and triggering auto-compaction if needed.
    pub async fn prompt(&self, text: impl Into<String>) -> Result<Vec<AgentMessage>, KitError> {
        let user_msg = AgentMessage::Llm(Message::User(UserMessage::text(text, now_ms())));
        self.prompt_agent_message(user_msg).await
    }

    /// Run a multimodal prompt to completion.
    pub async fn prompt_parts(
        &self,
        content: Vec<ContentPart>,
    ) -> Result<Vec<AgentMessage>, KitError> {
        let user_msg = AgentMessage::Llm(Message::User(UserMessage {
            id: None,
            content,
            timestamp: now_ms(),
        }));
        self.prompt_agent_message(user_msg).await
    }

    async fn prompt_agent_message(
        &self,
        user_msg: AgentMessage,
    ) -> Result<Vec<AgentMessage>, KitError> {
        let activity = self.try_begin_activity(Activity::Prompt(KitTurnOrigin::Foreground))?;
        self.drive_prompt(Some(user_msg), activity, None).await
    }

    pub fn background_sender(&self) -> BackgroundEventSender {
        self.inbox.sender()
    }

    pub fn drain_background_prompt(&self) -> Option<String> {
        self.inbox.drain_prompt()
    }

    pub fn subscribe_background_wakeups(&self) -> mpsc::UnboundedReceiver<()> {
        self.inbox.subscribe()
    }

    pub fn resume_background(&self) {
        self.inbox.resume();
    }

    pub fn is_background_paused(&self) -> bool {
        self.inbox.is_paused()
    }

    /// Reserve one background turn. The returned stream takes the batch when first polled.
    /// Hosts can give user input priority by starting it before acting on a background wakeup.
    pub fn run_background_turn(&self) -> Result<Option<KitEventStream<'_>>, KitError> {
        if self.agent.is_running() {
            return Err(KitError::Busy);
        }
        let origin = KitTurnOrigin::Background;
        Ok(self
            .inbox
            .begin(Activity::Prompt(origin), self.pause_background_on_error)?
            .map(|activity| self.start_prompt_stream(None, origin, activity)))
    }

    /// Run a text prompt as a stream of agent and session events.
    pub fn prompt_stream(&self, text: impl Into<String>) -> Result<KitEventStream<'_>, KitError> {
        let origin = KitTurnOrigin::Foreground;
        let activity = self.try_begin_activity(Activity::Prompt(origin))?;
        let user_msg = AgentMessage::Llm(Message::User(UserMessage::text(text, now_ms())));
        Ok(self.start_prompt_stream(Some(user_msg), origin, activity))
    }

    fn start_prompt_stream(
        &self,
        user_msg: Option<AgentMessage>,
        origin: KitTurnOrigin,
        activity: ActivityGuard,
    ) -> KitEventStream<'_> {
        let (tx, rx) = mpsc::unbounded();
        // Start and flush only once the stream is polled.
        let driver: BoxFuture<'_, Result<(), KitError>> = Box::pin(async move {
            self.drive_prompt(user_msg, activity, Some(&tx)).await?;
            Ok(())
        });

        KitEventStream {
            rx,
            driver: Some(driver),
            terminal_error: None,
            origin,
        }
    }

    async fn drive_prompt(
        &self,
        user_msg: Option<AgentMessage>,
        mut activity: ActivityGuard,
        tx: Option<&mpsc::UnboundedSender<KitEvent>>,
    ) -> Result<Vec<AgentMessage>, KitError> {
        if activity.aborted() {
            return Ok(Vec::new());
        }

        let origin = if user_msg.is_some() {
            KitTurnOrigin::Foreground
        } else {
            KitTurnOrigin::Background
        };
        let background = activity.start(origin);
        let user_msg = user_msg.unwrap_or_else(|| {
            Message::User(UserMessage::text(
                background.expect("background turn has a reserved batch"),
                now_ms(),
            ))
            .into()
        });
        let (prompt_messages, flushed_count, flushed_events) =
            self.prepare_turn_prompt_messages_with_events(user_msg, origin)?;
        if let Some(tx) = tx {
            for event in flushed_events {
                let _ = tx.unbounded_send(KitEvent::Session(event));
            }
        }

        let mut failed = false;
        let mut seen_ends = 0usize;
        let mut new_messages = Vec::new();
        let mut agent_stream = self.agent.prompt_messages_stream(prompt_messages)?;
        if activity.aborted() {
            self.agent.abort_now();
        }
        while let Some(event) = agent_stream.next().await {
            match &event {
                AgentEvent::MessageEnd { message } => {
                    if seen_ends >= flushed_count {
                        self.session
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .append_message(message.clone())?;
                        if seen_ends == flushed_count
                            && matches!(message, AgentMessage::Llm(Message::User(_)))
                        {
                            activity.delivered();
                        }
                    }
                    seen_ends += 1;
                }
                AgentEvent::TurnEnd { message, .. } => {
                    failed |=
                        matches!(message.stop_reason, StopReason::Aborted | StopReason::Error);
                }
                AgentEvent::AgentEnd { messages, .. } if tx.is_none() => {
                    new_messages = messages.clone();
                }
                _ => {}
            }
            if let Some(tx) = tx {
                let _ = tx.unbounded_send(KitEvent::Agent(event));
            }
        }
        if !failed && !activity.aborted() {
            self.run_compaction_internal(true, tx).await?;
        }
        activity.complete(!failed);
        Ok(new_messages)
    }

    fn prepare_turn_prompt_messages_with_events(
        &self,
        user_msg: AgentMessage,
        origin: KitTurnOrigin,
    ) -> Result<(Vec<AgentMessage>, usize, Vec<SessionEvent>), KitError> {
        let mut session = self.session.lock().unwrap_or_else(|e| e.into_inner());
        let skills = self.skills.lock().unwrap_or_else(|e| e.into_inner());
        let mut known = self.known_skills.lock().unwrap_or_else(|e| e.into_inner());
        let content = match &user_msg {
            AgentMessage::Llm(Message::User(user)) if origin == KitTurnOrigin::Foreground => {
                Some(user.content.as_slice())
            }
            _ => None,
        };
        let flushed = known.flush_for_prompt(&mut session, &skills, content)?;

        let mut prompt_messages = Vec::with_capacity(flushed.len() + 1);
        let mut events = Vec::with_capacity(flushed.len());
        for entry in &flushed {
            if let SessionEntryPayload::ContextBlock { name, text } = &entry.payload {
                prompt_messages.push(context_block_to_agent_message(name, text, entry.timestamp));
                events.push(SessionEvent::ContextAppended {
                    name: name.clone(),
                    text: text.clone(),
                });
            }
        }
        let flushed_count = prompt_messages.len();
        prompt_messages.push(user_msg);
        Ok((prompt_messages, flushed_count, events))
    }

    async fn resolve_compaction_options(&self, model: &Model) -> SimpleStreamOptions {
        let api_key = match &self.get_api_key {
            Some(resolver) => resolver(&model.provider)
                .await
                .or_else(|| self.api_key.clone()),
            None => self.api_key.clone(),
        };
        let level = self.agent.thinking_level();
        SimpleStreamOptions {
            api_key,
            reasoning: (level != ThinkingLevel::Off).then_some(level),
            fast: Some(self.agent.fast()),
            temperature: None,
            max_tokens: None,
            cache_retention: self.cache_retention,
            session_id: Some(self.session_id()),
            headers: self.headers.clone(),
            sampling_params: Map::new(),
            cancel: CancelToken::new(),
        }
    }

    async fn run_compaction_internal(
        &self,
        auto_only: bool,
        stream_tx: Option<&mpsc::UnboundedSender<KitEvent>>,
    ) -> Result<Option<CompactionResult>, KitError> {
        let model = self.agent.model();
        let options = self.resolve_compaction_options(&model).await;

        let prepared = {
            let session = self.session.lock().unwrap_or_else(|e| e.into_inner());
            if auto_only && !session.needs_compaction(&model) {
                return Ok(None);
            }
            let Some(prep) = session.prepare_compaction(&model, options) else {
                return Ok(None);
            };
            session.notify_compaction_start(prep.input.tokens_before);
            prep
        };

        let tokens_before = prepared.input.tokens_before;
        let expected_leaf = prepared.expected_leaf_id.clone();
        if let Some(tx) = stream_tx {
            let _ = tx.unbounded_send(KitEvent::Session(SessionEvent::CompactionStart {
                tokens_before,
            }));
        }

        let output = SessionManager::execute_prepared_compaction(&*self.provider, prepared).await?;

        let (maybe_result, reconstructed) = {
            let mut session = self.session.lock().unwrap_or_else(|e| e.into_inner());
            let res = session.commit_compaction_if_leaf_matches(
                output,
                tokens_before,
                expected_leaf.as_deref(),
            )?;
            let recon = session.reconstruct_context();
            (res, recon)
        };

        let Some(result) = maybe_result else {
            return Ok(None);
        };

        self.deferred_registry.clear_seen_schemas();
        self.agent.replace_messages(reconstructed.messages)?;
        self.reload_skills();
        self.known_skills
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reset_after_compaction();

        if let Some(tx) = stream_tx {
            let _ = tx.unbounded_send(KitEvent::Session(SessionEvent::CompactionEnd {
                summary: result.summary.clone(),
                tokens_before: result.tokens_before,
                first_kept_entry_id: result.first_kept_entry_id.clone(),
                usage: result.usage.clone(),
            }));
        }

        Ok(Some(result))
    }
}
