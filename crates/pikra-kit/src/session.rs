use crate::codemode::{CODEMODE_EXEC_TOOL_NAME, CodemodeExecTool};
use crate::deferred::{
    CALL_TOOL_NAME, CallToolGateway, DeferredToolRegistry, TOOL_SEARCH_NAME, ToolSearchTool,
};
use crate::error::KitError;
use crate::mcp::{McpClient, McpToolRegistrationMode};
use crate::skills::SkillCatalog;
use futures_channel::mpsc;
use futures_core::Stream;
use futures_util::StreamExt;
use pikra_agent::{
    Agent, AgentConfig, AgentEvent, AgentMessage, AgentMiddleware, AgentTool, ApiKeyResolver,
    QueueMode, ToolExecutionMode,
};
use pikra_ai::{
    BoxFuture, CacheRetention, CancelToken, ContentPart, Message, Model, SimpleStreamOptions,
    StreamProvider, ThinkingLevel, ToolDefinition, UserMessage,
};
use pikra_codemode::CodemodeSandboxOptions;
use pikra_session::{
    CompactionConfig, CompactionResult, CompactionStrategy, MemorySessionStore, SessionConfig,
    SessionEntry, SessionEntryPayload, SessionEvent, SessionManager, SessionStore, SessionTreeNode,
    context_block_to_agent_message,
};
use serde_json::{Map, Value};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{SystemTime, UNIX_EPOCH};

const SKILLS_CONTEXT_NAME: &str = "skills";
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

/// Tool registration specification distinguishing resident vs deferred tools.
#[derive(Clone)]
pub struct KitToolSpec {
    pub tool: Arc<dyn AgentTool>,
    /// When `true`, tool is registered in the deferred gateway (`tool_search` + `call_tool`)
    /// and NEVER enters the top-level resident `tools` array.
    pub deferred: bool,
}

impl KitToolSpec {
    pub fn resident(tool: Arc<dyn AgentTool>) -> Self {
        Self {
            tool,
            deferred: false,
        }
    }

    pub fn deferred(tool: Arc<dyn AgentTool>) -> Self {
        Self {
            tool,
            deferred: true,
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

/// High-level headless session facade combining `Agent`, `SessionManager`, `SkillCatalog`,
/// deferred tool gateway (`tool_search` + `call_tool`), optional `exec` codemode sandbox, and MCP.
pub struct KitSession {
    provider: Arc<dyn StreamProvider>,
    agent: Agent,
    session: Arc<Mutex<SessionManager>>,
    resident_tools: Arc<Mutex<Vec<Arc<dyn AgentTool>>>>,
    deferred_registry: Arc<DeferredToolRegistry>,
    skills: Mutex<SkillCatalog>,
    skill_roots: Vec<PathBuf>,
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
        config: KitSessionConfig,
    ) -> Result<Self, KitError> {
        let deferred_registry = Arc::new(DeferredToolRegistry::new());
        let mut resident: Vec<Arc<dyn AgentTool>> = Vec::new();
        let mut deferred: Vec<Arc<dyn AgentTool>> = Vec::new();

        for spec in config.tools {
            if spec.deferred {
                deferred.push(spec.tool);
            } else {
                resident.push(spec.tool);
            }
        }

        deferred_registry.upsert(deferred);

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

        let resident_tools_handle = Arc::new(Mutex::new(resident.clone()));

        if config.enable_codemode
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
            resident.push(exec_tool);
            *resident_tools_handle
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = resident.clone();
        }

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

        let skills = SkillCatalog::discover(&config.skill_roots);
        if let Some(skills_xml) = skills.format_for_context() {
            session_manager.set_context(SKILLS_CONTEXT_NAME, skills_xml);
        }

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
            deferred_registry,
            skills: Mutex::new(skills),
            skill_roots: config.skill_roots,
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
        self.agent.is_running()
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

    /// Re-scan configured `skill_roots` and update the `"skills"` context block only if changed.
    pub fn reload_skills(&self) -> bool {
        let discovered = SkillCatalog::discover(&self.skill_roots);
        let formatted = discovered.format_for_context();
        *self.skills.lock().unwrap_or_else(|e| e.into_inner()) = discovered;
        if let Some(xml) = formatted {
            self.session
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .set_context(SKILLS_CONTEXT_NAME, xml)
        } else {
            false
        }
    }

    // ---- Idle Configuration Updates (Zero Spurious Busy) ----

    pub fn set_model(&self, model: Model) -> Result<(), KitError> {
        self.agent.set_model(model.clone())?;
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .append_model_change(model.provider, model.id)?;
        Ok(())
    }

    pub fn set_thinking_level(&self, level: ThinkingLevel) -> Result<(), KitError> {
        self.agent.set_thinking_level(level)?;
        let fast = self.agent.fast();
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .append_thinking_change(level, fast)?;
        Ok(())
    }

    pub fn set_fast(&self, fast: bool) -> Result<(), KitError> {
        self.agent.set_fast(fast)?;
        let level = self.agent.thinking_level();
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .append_thinking_change(level, fast)?;
        Ok(())
    }

    pub fn set_system_prompt(&self, prompt: impl Into<String>) -> Result<(), KitError> {
        self.agent.set_system_prompt(prompt)?;
        Ok(())
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
        if self.agent.is_running() {
            return Err(KitError::Busy);
        }
        self.deferred_registry.upsert(tools);
        self.sync_deferred_tools_context();
        Ok(())
    }

    /// Dynamically remove deferred tools by name while idle.
    ///
    /// NEVER mutates the top-level resident `tools` array; updates the `"deferred_tools"`
    /// `<system-reminder>` catalog only when changed.
    pub fn remove_tools(&self, names: &[&str]) -> Result<usize, KitError> {
        if self.agent.is_running() {
            return Err(KitError::Busy);
        }
        let removed = self.deferred_registry.remove(names);
        if removed > 0 {
            self.sync_deferred_tools_context();
        }
        Ok(removed)
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
        if self.agent.is_running() {
            return Err(KitError::Busy);
        }
        let discovered = client.list_tools(cancel).await?;
        let agent_tools = client.to_agent_tools(&discovered, name_prefix);
        let names: Vec<String> = agent_tools.iter().map(|t| t.definition().name).collect();

        match mode {
            McpToolRegistrationMode::Deferred => {
                self.upsert_tools(agent_tools)?;
            }
            McpToolRegistrationMode::Resident => {
                let mut resident = self
                    .resident_tools
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                for tool in agent_tools {
                    let name = tool.definition().name;
                    if let Some(existing) =
                        resident.iter_mut().find(|t| t.definition().name == name)
                    {
                        *existing = tool;
                    } else {
                        resident.push(tool);
                    }
                }
                let updated = resident.clone();
                drop(resident);
                self.agent.set_tools(updated)?;
            }
        }
        Ok(names)
    }

    /// Branch the session to `entry_id` without deleting sibling entries and re-sync `Agent`.
    pub fn fork(&self, entry_id: &str) -> Result<(), KitError> {
        if self.agent.is_running() {
            return Err(KitError::Busy);
        }
        let reconstructed = {
            let mut session = self.session.lock().unwrap_or_else(|e| e.into_inner());
            session.fork(entry_id)?;
            session.reconstruct_context()
        };
        self.deferred_registry.clear_seen_schemas();
        self.agent.replace_messages(reconstructed.messages)?;
        Ok(())
    }

    /// Run manual compaction immediately while idle.
    pub async fn compact(&self) -> Result<Option<CompactionResult>, KitError> {
        if self.agent.is_running() {
            return Err(KitError::Busy);
        }
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
        self.agent.abort_now();
    }

    /// Cancel any active run and await complete teardown so immediate subsequent calls never
    /// spuriously fail with `Busy`.
    pub async fn abort(&self) {
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
        if self.agent.is_running() {
            return Err(KitError::Busy);
        }

        let (prompt_messages, flushed_count) = self.prepare_turn_prompt_messages(user_msg)?;
        let new_messages = self.agent.prompt_messages(prompt_messages).await?;

        {
            let mut session = self.session.lock().unwrap_or_else(|e| e.into_inner());
            for msg in new_messages.iter().skip(flushed_count) {
                session.append_message(msg.clone())?;
            }
        }

        // Auto-compact if threshold exceeded after turn completes.
        let _ = self.run_compaction_internal(true, None).await?;

        Ok(new_messages)
    }

    /// Run a text prompt as a [`KitEventStream`] yielding `AgentEvent` and `SessionEvent` items.
    pub fn prompt_stream(&self, text: impl Into<String>) -> Result<KitEventStream<'_>, KitError> {
        if self.agent.is_running() {
            return Err(KitError::Busy);
        }
        let user_msg = AgentMessage::Llm(Message::User(UserMessage::text(text, now_ms())));
        let (prompt_messages, flushed_count, flushed_events) =
            self.prepare_turn_prompt_messages_with_events(user_msg)?;

        let mut agent_stream = self.agent.prompt_messages_stream(prompt_messages)?;
        let (tx, rx) = mpsc::unbounded();
        for ev in flushed_events {
            let _ = tx.unbounded_send(KitEvent::Session(ev));
        }

        let driver: BoxFuture<'_, Result<(), KitError>> = Box::pin(async move {
            let mut seen_ends = 0usize;
            while let Some(ev) = agent_stream.next().await {
                if let AgentEvent::MessageEnd { message } = &ev {
                    if seen_ends >= flushed_count {
                        let mut session = self.session.lock().unwrap_or_else(|e| e.into_inner());
                        session.append_message(message.clone())?;
                    }
                    seen_ends += 1;
                }
                let _ = tx.unbounded_send(KitEvent::Agent(ev));
            }
            // Agent stream has finished and released active_run; safe to auto-compact now.
            let _ = self.run_compaction_internal(true, Some(&tx)).await?;
            Ok(())
        });

        Ok(KitEventStream {
            rx,
            driver: Some(driver),
            terminal_error: None,
        })
    }

    fn prepare_turn_prompt_messages(
        &self,
        user_msg: AgentMessage,
    ) -> Result<(Vec<AgentMessage>, usize), KitError> {
        let (msgs, count, _) = self.prepare_turn_prompt_messages_with_events(user_msg)?;
        Ok((msgs, count))
    }

    fn prepare_turn_prompt_messages_with_events(
        &self,
        user_msg: AgentMessage,
    ) -> Result<(Vec<AgentMessage>, usize, Vec<SessionEvent>), KitError> {
        let flushed = {
            let mut session = self.session.lock().unwrap_or_else(|e| e.into_inner());
            session.flush_pending_context()?
        };

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
