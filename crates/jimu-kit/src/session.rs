use crate::codemode::{CODEMODE_EXEC_TOOL_NAME, CodemodeExecTool};
use crate::deferred::{
    CALL_TOOL_NAME, CallToolGateway, DeferredToolRegistry, TOOL_SEARCH_NAME, ToolSearchTool,
};
use crate::error::KitError;
use crate::instructions::{
    InstructionCatalog, InstructionContextState, InstructionDiscoveryConfig,
};
use crate::mcp::{McpClient, McpToolRegistrationMode};
use crate::skills::{SkillCatalog, SkillDiscoveryConfig};
use crate::wakeup::{
    Activity, ActivityGuard, BackgroundEventSender, BackgroundInbox, KitTurnOrigin, SharedKitDriver,
};
use futures_channel::{mpsc, oneshot};
use futures_core::Stream;
use futures_util::StreamExt;
use jimu_agent::{
    Agent, AgentConfig, AgentEvent, AgentMessage, AgentMiddleware, AgentTool, AgentToolResult,
    ApiKeyResolver, QueueMode, QueuedInputSummary, ToolExecutionMode, ToolProgressCallback,
};
use jimu_ai::{
    BoxFuture, CacheRetention, CancelToken, ContentPart, Message, Model, SimpleStreamOptions,
    StopReason, StreamProvider, ThinkingLevel, ToolDefinition, UserMessage,
};
use jimu_codemode::CodemodeSandboxOptions;
use jimu_session::{
    CompactionConfig, CompactionResult, CompactionStrategy, ContextUsage, MemorySessionStore,
    SessionConfig, SessionEntry, SessionEntryPayload, SessionEvent, SessionHeader, SessionManager,
    SessionModelMetadata, SessionStore, SessionTreeNode, baseline_to_system_message,
    prepare_flushed_prompt,
};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// Re-scan configured skill roots automatically before each turn (default: `true`).
    pub auto_reload_skills: bool,
    pub instruction_discovery: Option<InstructionDiscoveryConfig>,
    /// Resident schema budget for automatic placement. Explicit resident tools are pinned.
    /// When omitted, tools explicitly marked Auto use an 8 KiB budget.
    pub auto_defer_threshold_bytes: Option<usize>,
    /// Pause background turns after an error or abort, until a foreground turn succeeds
    /// or the host calls resume_background (default: true).
    pub pause_background_on_error: bool,
    /// Expose resident `tool_search` & `call_tool` gateway tools (default: `true`).
    pub enable_deferred_gateway: bool,
    /// Expose resident `exec` tool backed by `jimu_codemode::CodemodeSandbox` (default: `false`).
    pub enable_codemode: bool,
    pub codemode_options: CodemodeSandboxOptions,
    pub store: Option<Arc<dyn SessionStore>>,
    pub cwd: Option<String>,
    pub parent_session: Option<String>,
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
    /// Global byte ceiling applied to each LLM-facing tool result message.
    /// Defaults to `Some(jimu_agent::DEFAULT_MAX_TOOL_RESULT_BYTES)` (64 KiB).
    pub max_tool_result_bytes: Option<usize>,
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
            auto_reload_skills: true,
            instruction_discovery: None,
            auto_defer_threshold_bytes: None,
            pause_background_on_error: true,
            enable_deferred_gateway: true,
            enable_codemode: false,
            codemode_options: CodemodeSandboxOptions::default(),
            store: None,
            cwd: None,
            parent_session: None,
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
            max_tool_result_bytes: Some(jimu_agent::DEFAULT_MAX_TOOL_RESULT_BYTES),
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

    pub fn with_auto_reload_skills(mut self, enabled: bool) -> Self {
        self.auto_reload_skills = enabled;
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
    driver: Arc<SharedKitDriver>,
    origin: KitTurnOrigin,
    _session: PhantomData<&'a ()>,
}

impl KitEventStream<'_> {
    pub fn origin(&self) -> KitTurnOrigin {
        self.origin
    }
}

impl Stream for KitEventStream<'_> {
    type Item = KitEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if let Poll::Ready(Some(ev)) = Pin::new(&mut this.rx).poll_next(cx) {
            return Poll::Ready(Some(ev));
        }

        if !this.driver.completed.load(Ordering::Acquire) {
            this.driver.register_stream_waker(cx.waker());
            match this.driver.poll_step(cx) {
                Poll::Ready(()) => {
                    return match Pin::new(&mut this.rx).poll_next(cx) {
                        Poll::Ready(item) => Poll::Ready(item),
                        Poll::Pending => Poll::Ready(None),
                    };
                }
                Poll::Pending => {
                    if let Poll::Ready(Some(ev)) = Pin::new(&mut this.rx).poll_next(cx) {
                        return Poll::Ready(Some(ev));
                    }
                    return Poll::Pending;
                }
            }
        }

        Pin::new(&mut this.rx).poll_next(cx)
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

/// Compare two `ToolDefinition`s by their exact serialized JSON bytes (including object property
/// key order under `serde_json/preserve_order`, which `IndexMap::PartialEq` ignores).
fn tool_definition_bytes_eq(a: &ToolDefinition, b: &ToolDefinition) -> bool {
    match (serde_json::to_string(a), serde_json::to_string(b)) {
        (Ok(lhs), Ok(rhs)) => lhs == rhs,
        _ => false,
    }
}

/// Placeholder executor occupying a persisted baseline tool slot on a reopened session when the
/// underlying server/tool is currently offline or its schema changed, preserving the exact
/// prompt-cache prefix byte-for-byte.
struct UnavailableBaselineTool {
    definition: ToolDefinition,
}

impl UnavailableBaselineTool {
    fn new(definition: ToolDefinition) -> Self {
        Self { definition }
    }
}

impl AgentTool for UnavailableBaselineTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    fn execute(
        &self,
        _call_id: String,
        _args: Value,
        _cancel: CancelToken,
        _on_update: ToolProgressCallback,
    ) -> BoxFuture<'_, Result<AgentToolResult, String>> {
        let name = self.definition.name.clone();
        Box::pin(async move {
            Err(format!(
                "Tool `{name}` is currently unavailable (server disconnected or tool schema changed)"
            ))
        })
    }
}

/// High-level headless session facade combining `Agent`, `SessionManager`, `SkillCatalog`,
/// deferred tool gateway (`tool_search` + `call_tool`), optional `exec` codemode sandbox, and MCP.
pub struct KitSession {
    provider: Arc<dyn StreamProvider>,
    agent: Arc<Agent>,
    session: Arc<Mutex<SessionManager>>,
    /// Resident tools excluding `exec` so `CodemodeExecTool` never forms an `Arc` cycle.
    resident_tools: Arc<Mutex<Vec<Arc<dyn AgentTool>>>>,
    codemode_exec_tool: Option<Arc<dyn AgentTool>>,
    /// Persisted baseline slots currently backed by [`UnavailableBaselineTool`] placeholders.
    unavailable_baseline_tools: Mutex<HashSet<String>>,
    deferred_registry: Arc<DeferredToolRegistry>,
    skills: Arc<Mutex<SkillCatalog>>,
    skill_discovery: SkillDiscoveryConfig,
    auto_reload_skills: bool,
    known_skills: Arc<Mutex<SkillContextState>>,
    instructions: Mutex<InstructionContextState>,
    auto_defer_threshold_bytes: Option<usize>,
    pause_background_on_error: bool,
    inbox: BackgroundInbox,
    compacting: Arc<AtomicBool>,
    had_deferred_catalog: Mutex<bool>,
    initial_model: Model,
    initial_thinking_level: ThinkingLevel,
    initial_fast: bool,
    api_key: Option<String>,
    get_api_key: Option<ApiKeyResolver>,
    cache_retention: CacheRetention,
    headers: Vec<(String, Option<String>)>,
}

#[derive(Clone)]
struct KitRuntime {
    provider: Arc<dyn StreamProvider>,
    agent: Arc<Agent>,
    session: Arc<Mutex<SessionManager>>,
    deferred_registry: Arc<DeferredToolRegistry>,
    skills: Arc<Mutex<SkillCatalog>>,
    skill_discovery: SkillDiscoveryConfig,
    auto_reload_skills: bool,
    known_skills: Arc<Mutex<SkillContextState>>,
    compacting: Arc<AtomicBool>,
    api_key: Option<String>,
    get_api_key: Option<ApiKeyResolver>,
    cache_retention: CacheRetention,
    headers: Vec<(String, Option<String>)>,
}

struct CompactingFlagGuard(Arc<AtomicBool>);

impl CompactingFlagGuard {
    fn enter(flag: &Arc<AtomicBool>) -> Self {
        flag.store(true, Ordering::Release);
        Self(Arc::clone(flag))
    }
}

impl Drop for CompactingFlagGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
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
            #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
            let normalized_cwd = {
                let p = Path::new(cwd);
                if p.is_absolute() {
                    cwd.clone()
                } else {
                    std::env::current_dir()
                        .map(|base| base.join(p).to_string_lossy().into_owned())
                        .unwrap_or_else(|_| cwd.clone())
                }
            };
            #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
            let normalized_cwd = cwd.clone();
            session_cfg = session_cfg.with_cwd(normalized_cwd);
        }
        session_cfg.parent_session = config.parent_session;
        if !config.model.id.trim().is_empty() {
            session_cfg.initial_model = Some(SessionModelMetadata::from_model(&config.model));
        }
        session_cfg.initial_thinking = Some(config.thinking_level);
        session_cfg.initial_fast = Some(config.fast);
        if let Some(strategy) = config.compaction_strategy.clone() {
            session_cfg = session_cfg.with_compaction_strategy(strategy);
        }

        let mut session_manager = SessionManager::open(store, session_cfg)?;
        let reconstructed = session_manager.reconstruct_context();
        let session_id = session_manager.session_id().to_string();

        // For any already-started persisted session with a SystemBaseline (including an empty
        // `tools: []` baseline from `--no-tools`), freeze the top-level resident tool slots to
        // match `reconstructed.tools` byte-for-byte. Missing or schema-changed slots keep their
        // exact persisted definition via `UnavailableBaselineTool` so reopening never mutates the
        // cached prompt prefix.
        let has_started = session_manager
            .entries()
            .iter()
            .any(|entry| matches!(entry.payload, SessionEntryPayload::Message { .. }));
        let has_persisted_baseline = session_manager
            .entries()
            .iter()
            .any(|entry| matches!(entry.payload, SessionEntryPayload::SystemBaseline { .. }));
        let mut unavailable_slots = HashSet::new();
        let (final_resident, final_codemode_exec_tool) = if has_started && has_persisted_baseline {
            let mut host_by_name: HashMap<String, Arc<dyn AgentTool>> = resident
                .into_iter()
                .map(|tool| (tool.definition().name, tool))
                .collect();
            let mut frozen: Vec<Arc<dyn AgentTool>> = Vec::with_capacity(reconstructed.tools.len());
            for saved_def in &reconstructed.tools {
                if host_by_name
                    .get(&saved_def.name)
                    .is_some_and(|tool| tool_definition_bytes_eq(&tool.definition(), saved_def))
                {
                    frozen.push(host_by_name.remove(&saved_def.name).unwrap());
                } else {
                    unavailable_slots.insert(saved_def.name.clone());
                    frozen.push(Arc::new(UnavailableBaselineTool::new(saved_def.clone())));
                }
            }
            let has_gateway = frozen.iter().any(|t| {
                let name = t.definition().name;
                name == CALL_TOOL_NAME || name == CODEMODE_EXEC_TOOL_NAME
            });
            if has_gateway {
                let overflow: Vec<Arc<dyn AgentTool>> = host_by_name
                    .into_values()
                    .filter(|tool| {
                        let name = tool.definition().name;
                        name != TOOL_SEARCH_NAME
                            && name != CALL_TOOL_NAME
                            && name != CODEMODE_EXEC_TOOL_NAME
                    })
                    .collect();
                if !overflow.is_empty() {
                    deferred_registry.upsert(overflow);
                }
            }
            *resident_tools_handle
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = frozen
                .iter()
                .filter(|t| t.definition().name != CODEMODE_EXEC_TOOL_NAME)
                .cloned()
                .collect();
            let exec_slot = frozen
                .iter()
                .find(|t| t.definition().name == CODEMODE_EXEC_TOOL_NAME)
                .cloned();
            (frozen, exec_slot)
        } else {
            (resident, codemode_exec_tool)
        };

        let same_model_routing = |a: &Model, b: &Model| {
            a.provider == b.provider
                && a.id == b.id
                && a.api == b.api
                && a.base_url == b.base_url
                && a.context_window == b.context_window
                && a.max_output_tokens == b.max_output_tokens
                && a.supports_images == b.supports_images
                && a.supports_reasoning == b.supports_reasoning
        };
        let resolve_meta_model = |meta: &SessionModelMetadata| {
            meta.to_full_model()
                .or_else(|| meta.to_model_with_fallback_base_url(String::new()))
        };
        let header_model = session_manager
            .header()
            .initial_model
            .as_ref()
            .and_then(resolve_meta_model);
        let initial_model = match header_model {
            Some(ref hdr) if !same_model_routing(hdr, &config.model) => hdr.clone(),
            _ => config.model.clone(),
        };
        let branch_model = reconstructed
            .model_metadata
            .as_ref()
            .and_then(resolve_meta_model);
        let effective_model = match branch_model {
            Some(ref restored) if !same_model_routing(restored, &config.model) => restored.clone(),
            _ => config.model.clone(),
        };
        let initial_thinking_level = session_manager
            .header()
            .initial_thinking
            .unwrap_or(config.thinking_level);
        let initial_fast = session_manager.header().initial_fast.unwrap_or(config.fast);
        let effective_thinking = reconstructed
            .thinking
            .map(|(lvl, _)| lvl)
            .unwrap_or(initial_thinking_level);
        let effective_fast = reconstructed
            .thinking
            .map(|(_, f)| f)
            .unwrap_or(initial_fast);

        let known_skills = SkillContextState::from_branch(&session_manager);
        let instructions = InstructionContextState::new(
            config.instruction_discovery,
            config.cwd.as_deref().map(Path::new),
            &mut session_manager,
            &reconstructed,
        );

        let has_gateway = final_resident.iter().any(|t| {
            let name = t.definition().name;
            name == CALL_TOOL_NAME || name == CODEMODE_EXEC_TOOL_NAME
        });
        let mut had_deferred_catalog = false;
        if has_gateway && let Some(catalog) = deferred_registry.format_catalog() {
            session_manager.set_context(DEFERRED_TOOLS_CONTEXT_NAME, catalog);
            had_deferred_catalog = true;
        }

        let mut agent_cfg = AgentConfig::new(effective_model)
            .with_system_prompt(reconstructed.system_prompt)
            .with_tools(final_resident)
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
        agent_cfg.max_tool_result_bytes = config.max_tool_result_bytes;
        agent_cfg.cache_retention = config.cache_retention;
        agent_cfg.session_id = Some(session_id);
        agent_cfg.headers = config.headers.clone();
        agent_cfg.sampling_params = config.sampling_params;
        agent_cfg.initial_messages = reconstructed.messages;

        let agent = Arc::new(Agent::new(Arc::clone(&provider), agent_cfg));

        Ok(Self {
            provider,
            agent,
            session: Arc::new(Mutex::new(session_manager)),
            resident_tools: resident_tools_handle,
            codemode_exec_tool: final_codemode_exec_tool,
            unavailable_baseline_tools: Mutex::new(unavailable_slots),
            deferred_registry,
            skills,
            skill_discovery,
            auto_reload_skills: config.auto_reload_skills,
            known_skills: Arc::new(Mutex::new(known_skills)),
            instructions: Mutex::new(instructions),
            auto_defer_threshold_bytes: config.auto_defer_threshold_bytes,
            pause_background_on_error: config.pause_background_on_error,
            inbox: BackgroundInbox::default(),
            compacting: Arc::new(AtomicBool::new(false)),
            had_deferred_catalog: Mutex::new(had_deferred_catalog),
            initial_model,
            initial_thinking_level,
            initial_fast,
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

    pub fn active_branch(&self) -> Vec<SessionEntry> {
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active_branch()
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

    pub fn visible_messages(&self) -> Vec<AgentMessage> {
        self.agent.visible_messages()
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

    pub fn steering_mode(&self) -> QueueMode {
        self.agent.steering_mode()
    }

    pub fn set_steering_mode(&self, mode: QueueMode) {
        self.agent.set_steering_mode(mode);
    }

    pub fn follow_up_mode(&self) -> QueueMode {
        self.agent.follow_up_mode()
    }

    pub fn set_follow_up_mode(&self, mode: QueueMode) {
        self.agent.set_follow_up_mode(mode);
    }

    pub fn compaction_config(&self) -> CompactionConfig {
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .compaction_config()
            .clone()
    }

    pub fn set_auto_compaction(&self, enabled: bool) {
        let mut session = self.session.lock().unwrap_or_else(|e| e.into_inner());
        let mut cfg = session.compaction_config().clone();
        cfg.enabled = enabled;
        session.set_compaction_config(cfg);
    }

    pub fn is_running(&self) -> bool {
        self.inbox.is_active() || self.agent.is_running()
    }

    pub fn is_compacting(&self) -> bool {
        self.compacting.load(Ordering::Acquire) || self.inbox.is_compacting()
    }

    /// Snapshot current context-window utilization against the active model.
    pub fn context_usage(&self) -> Option<ContextUsage> {
        let model = self.agent.model();
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .context_usage(&model)
    }

    /// Snapshot pending steering and follow-up inputs in FIFO order.
    pub fn pending_inputs(&self) -> Vec<QueuedInputSummary> {
        self.agent.pending_inputs()
    }

    /// Remove a pending steering or follow-up item by id.
    pub fn remove_queued_input(&self, id: &str) -> bool {
        self.agent.remove_queued_input(id)
    }

    /// Return a snapshot of the session's root header (shared across all branches).
    pub fn header(&self) -> SessionHeader {
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .header()
            .clone()
    }

    /// Reconstruct the effective non-secret model routing metadata on the active branch
    /// (seeded from `SessionHeader.initial_model` and updated by branch `ModelChange` entries).
    pub fn reconstructed_model_metadata(&self) -> Option<SessionModelMetadata> {
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reconstruct_context()
            .model_metadata
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
    /// Returns whether model-visible skills (including symlink target changes) changed.
    pub fn reload_skills(&self) -> bool {
        let mut skills = self.skills.lock().unwrap_or_else(|e| e.into_inner());
        let discovered = SkillCatalog::discover_with_config(&self.skill_discovery);
        let changed = !skills.has_same_visible_skills(&discovered);
        *skills = discovered;
        changed
    }

    /// Start watching configured skill roots and invoke `on_change` whenever the discovered
    /// skill catalog metadata changes (additions, removals, moves, symlink retargets, or
    /// description/visibility updates).
    ///
    /// Drop the returned [`crate::SkillWatcher`] to stop watching.
    #[cfg(all(feature = "watch", not(any(target_arch = "wasm32", target_os = "ios"))))]
    pub fn watch_skills(
        &self,
        on_change: impl Fn(SkillCatalog) + Send + Sync + 'static,
    ) -> Result<crate::SkillWatcher, KitError> {
        let skills = Arc::clone(&self.skills);
        let discovery = self.skill_discovery.clone();
        crate::SkillWatcher::start(discovery.clone(), move || {
            let discovered = SkillCatalog::discover_with_config(&discovery);
            let metadata_changed = {
                let mut current = skills.lock().unwrap_or_else(|e| e.into_inner());
                let changed = !current.has_same_catalog_metadata(&discovered);
                *current = discovered.clone();
                changed
            };
            if metadata_changed {
                on_change(discovered);
            }
        })
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

    /// Persist ONE atomic `ModelChange` entry carrying full non-secret model routing metadata,
    /// then update the active model only after persistence succeeds.
    pub fn set_model(&self, model: Model) -> Result<(), KitError> {
        self.with_idle_mutation(|| {
            let metadata = SessionModelMetadata::from_model(&model);
            self.session
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .append_model_metadata(metadata)?;
            self.agent.set_model(model)?;
            Ok(())
        })
    }

    /// Update the active model in memory without appending a new `ModelChange` entry
    /// (used when restoring a persisted model on session open).
    pub fn restore_model(&self, model: Model) -> Result<(), KitError> {
        self.with_idle_mutation(|| {
            self.agent.set_model(model)?;
            Ok(())
        })
    }

    /// Append a host-specific custom entry to the active branch.
    pub fn append_custom(
        &self,
        custom_type: impl Into<String>,
        data: Value,
    ) -> Result<String, KitError> {
        self.with_idle_mutation(|| {
            let entry = self
                .session
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .append_custom(custom_type, data)?;
            Ok(entry.id)
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

    pub fn append_named_context(&self, name: impl Into<String>, text: impl Into<String>) {
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .append_named_context(name, text);
    }

    pub fn append_meta(&self, text: impl Into<String>) {
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .append_meta(text);
    }

    pub fn append_meta_parts(&self, content: Vec<ContentPart>) {
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .append_meta_parts(content);
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
        let has_gateway = self
            .agent
            .tool_definitions()
            .into_iter()
            .any(|def| def.name == CALL_TOOL_NAME || def.name == CODEMODE_EXEC_TOOL_NAME);
        if !has_gateway {
            return;
        }
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
        let mut results = self.attach_mcp_tool_batches(vec![(agent_tools, mode)])?;
        Ok(results.pop().unwrap_or_default())
    }

    /// Return names of currently active (non-placeholder) resident tools.
    pub fn active_resident_tool_names(&self) -> HashSet<String> {
        let unavailable = self
            .unavailable_baseline_tools
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.agent
            .tool_definitions()
            .into_iter()
            .map(|def| def.name)
            .filter(|name| !unavailable.contains(name))
            .collect()
    }

    /// Atomically attach pre-discovered MCP tool batches from one or more servers in a single
    /// idle transaction so reopening a session with multiple MCP servers preserves the exact
    /// persisted prompt-cache baseline byte-for-byte.
    ///
    /// - For sessions that already contain messages (`has_started`), persisted baseline slots stay
    ///   frozen in their exact serialized order: incoming tools whose name and serialized schema
    ///   bytes match an unbound baseline placeholder bind into that slot in-place without writing
    ///   a new `SystemBaseline`, absent slots stay unavailable placeholders, and new or changed
    ///   tools route through the deferred gateway.
    /// - Cross-source collisions (with active resident tools, already-registered deferred tools,
    ///   or duplicates across batches) are rejected atomically before mutating session state.
    pub fn attach_mcp_tool_batches(
        &self,
        batches: Vec<(Vec<Arc<dyn AgentTool>>, McpToolRegistrationMode)>,
    ) -> Result<Vec<Vec<String>>, KitError> {
        self.with_idle_mutation(|| {
            let definitions = self.agent.tool_definitions();
            let mut unavailable = self
                .unavailable_baseline_tools
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut active_resident: HashSet<String> = definitions
                .iter()
                .map(|def| def.name.clone())
                .filter(|name| !unavailable.contains(name))
                .collect();
            let existing_deferred: HashSet<String> = self
                .deferred_registry
                .list_tools()
                .into_iter()
                .map(|tool| tool.definition().name)
                .collect();
            let (has_started, baseline_defs) = {
                let session = self.session.lock().unwrap_or_else(|e| e.into_inner());
                let started = session
                    .entries()
                    .iter()
                    .any(|entry| matches!(entry.payload, SessionEntryPayload::Message { .. }));
                let baseline: HashMap<String, ToolDefinition> = session
                    .reconstruct_context()
                    .tools
                    .into_iter()
                    .map(|def| (def.name.clone(), def))
                    .collect();
                (started, baseline)
            };
            let mut budget = ToolSchemaBudget::new(self.auto_defer_threshold_bytes, &definitions);
            let mut all_resident: Vec<Arc<dyn AgentTool>> = Vec::new();
            let mut bound_placeholder_names: Vec<String> = Vec::new();
            let mut all_deferred: Vec<Arc<dyn AgentTool>> = Vec::new();
            let mut seen_in_batches: HashSet<String> = HashSet::new();
            let mut per_batch_names = Vec::with_capacity(batches.len());

            for (agent_tools, mode) in batches {
                let mut registered_names = Vec::new();
                for tool in agent_tools {
                    let definition = tool.definition();
                    if !seen_in_batches.insert(definition.name.clone()) {
                        return Err(KitError::Tool(format!(
                            "duplicate MCP tool `{}` across attached servers",
                            definition.name
                        )));
                    }
                    if active_resident.contains(&definition.name) {
                        return Err(KitError::Tool(format!(
                            "MCP tool `{}` collides with an existing resident tool",
                            definition.name
                        )));
                    }
                    if existing_deferred.contains(&definition.name) {
                        return Err(KitError::Tool(format!(
                            "MCP tool `{}` collides with an existing deferred tool",
                            definition.name
                        )));
                    }

                    let matches_baseline_slot = unavailable.contains(&definition.name)
                        && baseline_defs
                            .get(&definition.name)
                            .is_some_and(|saved| tool_definition_bytes_eq(saved, &definition));

                    if has_started {
                        registered_names.push(definition.name.clone());
                        if matches_baseline_slot && mode != McpToolRegistrationMode::Deferred {
                            active_resident.insert(definition.name.clone());
                            bound_placeholder_names.push(definition.name);
                            all_resident.push(tool);
                        } else {
                            all_deferred.push(tool);
                        }
                        continue;
                    }

                    match mode {
                        McpToolRegistrationMode::Resident => {
                            registered_names.push(definition.name.clone());
                            active_resident.insert(definition.name);
                            all_resident.push(tool);
                        }
                        McpToolRegistrationMode::Deferred => {
                            registered_names.push(definition.name.clone());
                            all_deferred.push(tool);
                        }
                        McpToolRegistrationMode::Auto => {
                            registered_names.push(definition.name.clone());
                            if matches_baseline_slot || budget.admit(&definition) {
                                active_resident.insert(definition.name);
                                all_resident.push(tool);
                            } else {
                                all_deferred.push(tool);
                            }
                        }
                    }
                }
                per_batch_names.push(registered_names);
            }

            if !all_resident.is_empty() {
                self.attach_resident_tools(all_resident, has_started)?;
                for name in bound_placeholder_names {
                    unavailable.remove(&name);
                }
            }
            if !all_deferred.is_empty() {
                self.deferred_registry.upsert(all_deferred);
                self.sync_deferred_tools_context();
            }
            Ok(per_batch_names)
        })
    }

    fn attach_resident_tools(
        &self,
        tools: Vec<Arc<dyn AgentTool>>,
        preserve_baseline: bool,
    ) -> Result<(), KitError> {
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

        let prompt = self.agent.system_prompt();
        {
            let mut session = self.session.lock().unwrap_or_else(|e| e.into_inner());
            let baseline_tools = session.reconstruct_context().tools;
            // Align tool order to persisted SystemBaseline when reopening so prompt-cache prefix stays byte-identical.
            if !baseline_tools.is_empty() {
                let order: HashMap<&str, usize> = baseline_tools
                    .iter()
                    .enumerate()
                    .map(|(idx, def)| (def.name.as_str(), idx))
                    .collect();
                full_tools.sort_by_key(|t| {
                    order
                        .get(t.definition().name.as_str())
                        .copied()
                        .unwrap_or(usize::MAX)
                });
            }
            let definitions: Vec<_> = full_tools.iter().map(|tool| tool.definition()).collect();
            let baseline_bytes_match = baseline_tools.len() == definitions.len()
                && baseline_tools
                    .iter()
                    .zip(definitions.iter())
                    .all(|(lhs, rhs)| tool_definition_bytes_eq(lhs, rhs));
            if !preserve_baseline && !baseline_bytes_match {
                session.append_system_baseline(&prompt, definitions.clone())?;
            }
            drop(session);
            self.agent.set_tools(full_tools)?;
            self.sync_agent_leading_system_message(&prompt, &definitions)?;
        }
        *resident = next_resident;
        Ok(())
    }

    /// Branch the session to `entry_id` without deleting sibling entries and atomically re-sync
    /// `Agent` transcript, system prompt, thinking/fast settings, and (via `resolve_model`) the
    /// target branch's active model/endpoint in a single idle transaction.
    pub fn fork_with_model_resolver(
        &self,
        entry_id: &str,
        resolve_model: impl FnOnce(Option<&SessionModelMetadata>) -> Option<Model>,
    ) -> Result<(), KitError> {
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
            let (target_thinking, target_fast) = reconstructed
                .thinking
                .unwrap_or((self.initial_thinking_level, self.initial_fast));
            self.agent.set_thinking_level(target_thinking)?;
            self.agent.set_fast(target_fast)?;
            let target_model = resolve_model(reconstructed.model_metadata.as_ref())
                .unwrap_or_else(|| self.initial_model.clone());
            self.agent.set_model(target_model)?;
            Ok(())
        })
    }

    /// Branch the session to `entry_id` without deleting sibling entries and re-sync `Agent`,
    /// restoring the target branch's recorded model/endpoint (from branch `ModelChange` or root
    /// `SessionHeader.initial_model`) along with its thinking/fast settings.
    pub fn fork(&self, entry_id: &str) -> Result<(), KitError> {
        self.fork_with_model_resolver(entry_id, |meta| {
            let meta = meta?;
            meta.to_full_model()
                .or_else(|| meta.to_model_with_fallback_base_url(String::new()))
        })
    }

    /// Run manual compaction immediately while idle, publishing a shared cancellable driver so
    /// concurrent `abort().await` never deadlocks even if this future is parked on `Pending`.
    pub async fn compact(&self) -> Result<Option<CompactionResult>, KitError> {
        let (tx, rx) = oneshot::channel();
        let runtime = self.runtime();
        let driver = self
            .inbox
            .begin_stream(Activity::Compacting, false, None, move |activity| {
                SharedKitDriver::new(Box::pin(async move {
                    let result = if activity.aborted() {
                        Ok(None)
                    } else {
                        let cancel = activity.cancel_token();
                        runtime
                            .run_compaction_internal(false, None, None, None, false, cancel)
                            .await
                    };
                    drop(activity);
                    let _ = tx.send(result);
                }))
            })?
            .ok_or(KitError::Busy)?;
        driver.drive().await;
        rx.await.unwrap_or(Ok(None))
    }

    /// Run manual compaction with optional custom instructions and session lifecycle callbacks.
    pub async fn compact_with_instructions(
        &self,
        custom_instructions: Option<String>,
        on_event: impl Fn(SessionEvent) + Send + Sync + 'static,
    ) -> Result<Option<CompactionResult>, KitError> {
        self.start_compact_with_instructions(custom_instructions, on_event)?
            .await
    }

    /// Synchronously claim `Activity::Compacting` and publish a cancellable driver so callers
    /// spawning compaction (or holding an unpolled future while `abort().await` runs) never race
    /// or deadlock.
    pub fn start_compact_with_instructions(
        &self,
        custom_instructions: Option<String>,
        on_event: impl Fn(SessionEvent) + Send + Sync + 'static,
    ) -> Result<BoxFuture<'static, Result<Option<CompactionResult>, KitError>>, KitError> {
        let (tx, rx) = oneshot::channel();
        let runtime = self.runtime();
        let driver = self
            .inbox
            .begin_stream(Activity::Compacting, false, None, move |activity| {
                SharedKitDriver::new(Box::pin(async move {
                    let result = if activity.aborted() {
                        Err(KitError::Aborted)
                    } else {
                        let cancel = activity.cancel_token();
                        runtime
                            .run_compaction_internal(
                                false,
                                custom_instructions,
                                None,
                                Some(&on_event),
                                true,
                                cancel,
                            )
                            .await
                    };
                    drop(activity);
                    let _ = tx.send(result);
                }))
            })?
            .ok_or(KitError::Busy)?;
        Ok(Box::pin(async move {
            driver.drive().await;
            rx.await.unwrap_or(Err(KitError::Aborted))
        }))
    }

    // ---- Steering, Follow-Up, Abort ----

    pub fn steer(&self, id: impl Into<String>, text: impl Into<String>) {
        self.agent.steer(id, text);
    }

    pub async fn steer_parts(
        &self,
        id: impl Into<String>,
        parts: Vec<ContentPart>,
        is_meta: bool,
    ) -> bool {
        if is_meta {
            self.agent.steer_meta_parts(id, parts);
        } else {
            self.agent.steer_with_parts(id, parts);
        }
        true
    }

    pub fn steer_meta(&self, id: impl Into<String>, text: impl Into<String>) {
        self.agent.steer_meta(id, text);
    }

    pub fn steer_meta_parts(&self, id: impl Into<String>, content: Vec<ContentPart>) {
        self.agent.steer_meta_parts(id, content);
    }

    pub fn drain_steering(&self) -> Vec<(String, Vec<ContentPart>)> {
        self.agent.drain_steering()
    }

    pub fn follow_up(&self, id: impl Into<String>, text: impl Into<String>) {
        self.agent.follow_up(id, text);
    }

    pub fn follow_up_parts(&self, id: impl Into<String>, parts: Vec<ContentPart>, is_meta: bool) {
        if is_meta {
            self.agent.follow_up_meta_parts(id, parts);
        } else {
            self.agent.follow_up_with_parts(id, parts);
        }
    }

    pub fn follow_up_meta(&self, id: impl Into<String>, text: impl Into<String>) {
        self.agent.follow_up_meta(id, text);
    }

    pub fn follow_up_meta_parts(&self, id: impl Into<String>, content: Vec<ContentPart>) {
        self.agent.follow_up_meta_parts(id, content);
    }

    pub fn clear_all_queues(&self) {
        self.agent.clear_all_queues();
    }

    pub fn abort_now(&self) {
        self.inbox.abort_prompt();
        self.agent.abort_now();
    }

    /// Cancel any active run and await complete teardown so immediate subsequent calls never
    /// spuriously fail with `Busy`.
    pub async fn abort(&self) {
        let (rx, driver) = self.inbox.begin_abort();
        self.agent.abort_now();
        if let Some(driver) = driver {
            driver.drive().await;
        }
        self.agent.abort().await;
        if let Some(rx) = rx {
            let _ = rx.await;
        }
    }

    pub async fn wait_for_idle(&self) {
        let (rx, driver) = self.inbox.begin_wait_for_idle();
        if let Some(driver) = driver {
            driver.drive().await;
        }
        self.agent.wait_for_idle().await;
        if let Some(rx) = rx {
            let _ = rx.await;
        }
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
        let user = Message::User(UserMessage::parts(content, now_ms()));
        self.prompt_agent_message(user.into()).await
    }

    pub async fn prompt_meta(
        &self,
        text: impl Into<String>,
    ) -> Result<Vec<AgentMessage>, KitError> {
        let user = Message::User(UserMessage::meta_text(text, now_ms()));
        self.prompt_agent_message(user.into()).await
    }

    pub async fn prompt_meta_parts(
        &self,
        content: Vec<ContentPart>,
    ) -> Result<Vec<AgentMessage>, KitError> {
        let user = Message::User(UserMessage::meta_parts(content, now_ms()));
        self.prompt_agent_message(user.into()).await
    }

    async fn prompt_agent_message(
        &self,
        user_msg: AgentMessage,
    ) -> Result<Vec<AgentMessage>, KitError> {
        if self.agent.is_running() {
            return Err(KitError::Busy);
        }
        let (tx, rx) = oneshot::channel();
        let runtime = self.runtime();
        let cancel_agent = Arc::clone(&self.agent);
        let on_cancel: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let _ = cancel_agent.drain_steering();
        });
        let driver = self
            .inbox
            .begin_stream(
                Activity::Prompt(KitTurnOrigin::Foreground),
                self.pause_background_on_error,
                Some(on_cancel),
                move |activity| {
                    SharedKitDriver::new(Box::pin(async move {
                        let result = runtime.drive_prompt(Some(user_msg), activity, None).await;
                        let _ = tx.send(result);
                    }))
                },
            )?
            .ok_or(KitError::Busy)?;
        driver.drive().await;
        rx.await.unwrap_or_else(|_| Ok(Vec::new()))
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
        self.begin_prompt_stream(None, origin)
    }

    /// Run a text prompt as a stream of agent and session events.
    pub fn prompt_stream(&self, text: impl Into<String>) -> Result<KitEventStream<'_>, KitError> {
        self.prompt_user_stream(UserMessage::text(text, now_ms()))
    }

    pub fn prompt_parts_stream(
        &self,
        content: Vec<ContentPart>,
    ) -> Result<KitEventStream<'_>, KitError> {
        self.prompt_user_stream(UserMessage::parts(content, now_ms()))
    }

    pub fn prompt_meta_stream(
        &self,
        text: impl Into<String>,
    ) -> Result<KitEventStream<'_>, KitError> {
        self.prompt_user_stream(UserMessage::meta_text(text, now_ms()))
    }

    pub fn prompt_meta_parts_stream(
        &self,
        content: Vec<ContentPart>,
    ) -> Result<KitEventStream<'_>, KitError> {
        self.prompt_user_stream(UserMessage::meta_parts(content, now_ms()))
    }

    /// Start a prompt stream detached from the `&self` borrow (for spawned driver tasks).
    pub fn prompt_parts_stream_owned(
        &self,
        content: Vec<ContentPart>,
        is_meta: bool,
    ) -> Result<KitEventStream<'static>, KitError> {
        let user = UserMessage::parts(content, now_ms()).with_meta(is_meta);
        let origin = KitTurnOrigin::Foreground;
        self.begin_prompt_stream(Some(Message::User(user).into()), origin)?
            .ok_or(KitError::Busy)
    }

    fn prompt_user_stream(&self, user: UserMessage) -> Result<KitEventStream<'_>, KitError> {
        let origin = KitTurnOrigin::Foreground;
        self.begin_prompt_stream(Some(Message::User(user).into()), origin)?
            .ok_or(KitError::Busy)
    }

    fn runtime(&self) -> KitRuntime {
        KitRuntime {
            provider: Arc::clone(&self.provider),
            agent: Arc::clone(&self.agent),
            session: Arc::clone(&self.session),
            deferred_registry: Arc::clone(&self.deferred_registry),
            skills: Arc::clone(&self.skills),
            skill_discovery: self.skill_discovery.clone(),
            auto_reload_skills: self.auto_reload_skills,
            known_skills: Arc::clone(&self.known_skills),
            compacting: Arc::clone(&self.compacting),
            api_key: self.api_key.clone(),
            get_api_key: self.get_api_key.clone(),
            cache_retention: self.cache_retention,
            headers: self.headers.clone(),
        }
    }

    fn begin_prompt_stream<'a>(
        &self,
        user_msg: Option<AgentMessage>,
        origin: KitTurnOrigin,
    ) -> Result<Option<KitEventStream<'a>>, KitError> {
        if self.agent.is_running() {
            return Err(KitError::Busy);
        }
        let (tx, rx) = mpsc::unbounded();
        let runtime = self.runtime();
        let cancel_agent = Arc::clone(&self.agent);
        let on_cancel: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let _ = cancel_agent.drain_steering();
        });
        let driver = self.inbox.begin_stream(
            Activity::Prompt(origin),
            self.pause_background_on_error,
            Some(on_cancel),
            move |activity| {
                SharedKitDriver::new(Box::pin(async move {
                    if let Err(err) = runtime.drive_prompt(user_msg, activity, Some(&tx)).await {
                        let _ = tx.unbounded_send(KitEvent::Error(err.to_string()));
                    }
                }))
            },
        )?;
        Ok(driver.map(|driver| KitEventStream {
            rx,
            driver,
            origin,
            _session: PhantomData,
        }))
    }
}

impl KitRuntime {
    fn session_id(&self) -> String {
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .session_id()
            .to_string()
    }

    fn reload_skills(&self) {
        let mut skills = self.skills.lock().unwrap_or_else(|e| e.into_inner());
        let discovered = SkillCatalog::discover_with_config(&self.skill_discovery);
        *skills = discovered;
    }

    async fn drive_prompt(
        &self,
        user_msg: Option<AgentMessage>,
        mut activity: ActivityGuard,
        tx: Option<&mpsc::UnboundedSender<KitEvent>>,
    ) -> Result<Vec<AgentMessage>, KitError> {
        let res = self.drive_prompt_inner(user_msg, &mut activity, tx).await;
        let aborted = activity.aborted();
        let succeeded = res.as_ref().is_ok_and(|(_, failed)| !*failed) && !aborted;
        if !succeeded {
            // Unified teardown: drain any steering queued before/during context preparation,
            // turn execution, persistence error, auto-compaction failure, or abort BEFORE
            // dropping `activity` and releasing idle so stale steering never leaks into the
            // next independent prompt.
            let _ = self.agent.drain_steering();
        }
        activity.complete(succeeded);
        res.map(|(messages, _)| messages)
    }

    async fn drive_prompt_inner(
        &self,
        user_msg: Option<AgentMessage>,
        activity: &mut ActivityGuard,
        tx: Option<&mpsc::UnboundedSender<KitEvent>>,
    ) -> Result<(Vec<AgentMessage>, bool), KitError> {
        if activity.aborted() {
            return Ok((Vec::new(), true));
        }

        let cancel = activity.cancel_token();
        let origin = if user_msg.is_some() {
            KitTurnOrigin::Foreground
        } else {
            KitTurnOrigin::Background
        };
        let background = activity.start(origin);
        let user_msg = user_msg.unwrap_or_else(|| {
            Message::User(UserMessage::meta_text(
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
        let mut new_messages = Vec::new();
        let mut persistence_error: Option<String> = None;
        let mut next_stream = Some((
            self.agent.prompt_messages_stream(prompt_messages)?,
            flushed_count,
        ));

        while let Some((mut agent_stream, skip_ends)) = next_stream.take() {
            if activity.aborted() {
                self.agent.abort_now();
            }
            let mut seen_ends = 0usize;
            while let Some(mut event) = agent_stream.next().await {
                match &mut event {
                    AgentEvent::MessageEnd { message } => {
                        if seen_ends >= skip_ends && persistence_error.is_none() {
                            let persist_res = self
                                .session
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .append_message(message.clone());
                            match persist_res {
                                Ok(_) => {
                                    if seen_ends == skip_ends
                                        && matches!(message, AgentMessage::Llm(Message::User(_)))
                                    {
                                        activity.delivered();
                                    }
                                }
                                Err(err) => {
                                    persistence_error =
                                        Some(format!("failed to persist session message: {err}"));
                                    failed = true;
                                    self.agent.abort_now();
                                }
                            }
                        }
                        if let Some(err_msg) = persistence_error.clone()
                            && let AgentMessage::Llm(Message::Assistant(a)) = message
                        {
                            a.stop_reason = StopReason::Error;
                            a.error_message = Some(err_msg);
                        }
                        seen_ends += 1;
                    }
                    AgentEvent::TurnEnd { message, .. } => {
                        if let Some(err_msg) = persistence_error.clone() {
                            message.stop_reason = StopReason::Error;
                            message.error_message = Some(err_msg);
                        }
                        failed |=
                            matches!(message.stop_reason, StopReason::Aborted | StopReason::Error);
                    }
                    AgentEvent::AgentEnd { messages, .. } if tx.is_none() => {
                        new_messages.extend(messages.iter().cloned());
                    }
                    _ => {}
                }
                if let Some(tx) = tx {
                    let _ = tx.unbounded_send(KitEvent::Agent(event));
                }
            }
            drop(agent_stream);
            if let Some(err_msg) = persistence_error {
                // Re-align the in-memory Agent transcript with the last committed disk branch so
                // failed-to-persist messages never diverge from durable session history.
                let committed_messages = self
                    .session
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .reconstruct_context()
                    .messages;
                let _ = self.agent.replace_messages(committed_messages);
                return Err(KitError::Tool(err_msg));
            }
            if !failed && !activity.aborted() {
                self.run_compaction_internal(true, None, tx, None, false, cancel.clone())
                    .await?;
            }
            if !failed && !activity.aborted() && self.agent.has_queued_messages() {
                next_stream = self.agent.continue_stream().ok().map(|s| (s, 0));
            }
        }
        Ok((new_messages, failed))
    }

    fn prepare_turn_prompt_messages_with_events(
        &self,
        user_msg: AgentMessage,
        origin: KitTurnOrigin,
    ) -> Result<(Vec<AgentMessage>, usize, Vec<SessionEvent>), KitError> {
        if self.auto_reload_skills {
            self.reload_skills();
        }
        let mut session = self.session.lock().unwrap_or_else(|e| e.into_inner());
        let skills = self.skills.lock().unwrap_or_else(|e| e.into_inner());
        let mut known = self.known_skills.lock().unwrap_or_else(|e| e.into_inner());
        let content = match &user_msg {
            AgentMessage::Llm(Message::User(user))
                if origin == KitTurnOrigin::Foreground && !user.is_meta =>
            {
                Some(user.content.as_slice())
            }
            _ => None,
        };
        let flushed = match known.flush_for_prompt(&mut session, &skills, content) {
            Ok(flushed) => flushed,
            Err(err) => {
                self.agent
                    .replace_messages(session.reconstruct_context().messages)?;
                return Err(KitError::Tool(format!(
                    "failed to persist session message: {err}"
                )));
            }
        };

        Ok(prepare_flushed_prompt(flushed, user_msg))
    }

    async fn resolve_compaction_options(
        &self,
        model: &Model,
        cancel: CancelToken,
    ) -> SimpleStreamOptions {
        let api_key = match &self.get_api_key {
            Some(resolver) => {
                let key_fut = resolver(&model.provider);
                let cancel_fut = cancel.cancelled();
                futures_util::pin_mut!(key_fut, cancel_fut);
                match futures_util::future::select(key_fut, cancel_fut).await {
                    futures_util::future::Either::Left((resolved, _)) => {
                        resolved.or_else(|| self.api_key.clone())
                    }
                    futures_util::future::Either::Right(((), _)) => None,
                }
            }
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
            cancel,
        }
    }

    async fn run_compaction_internal(
        &self,
        auto_only: bool,
        custom_instructions: Option<String>,
        stream_tx: Option<&mpsc::UnboundedSender<KitEvent>>,
        on_event: Option<&(dyn Fn(SessionEvent) + Send + Sync)>,
        error_on_cancel: bool,
        cancel: CancelToken,
    ) -> Result<Option<CompactionResult>, KitError> {
        if cancel.is_cancelled() {
            return if error_on_cancel {
                Err(KitError::Aborted)
            } else {
                Ok(None)
            };
        }
        let model = self.agent.model();
        let options = self
            .resolve_compaction_options(&model, cancel.clone())
            .await;
        if cancel.is_cancelled() {
            return if error_on_cancel {
                Err(KitError::Aborted)
            } else {
                Ok(None)
            };
        }

        let mut prepared = {
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

        if let Some(extra) = custom_instructions {
            prepared.input.config.custom_instructions = Some(extra);
        }

        let tokens_before = prepared.input.tokens_before;
        let expected_leaf = prepared.expected_leaf_id.clone();
        let _compacting_guard = CompactingFlagGuard::enter(&self.compacting);
        let start_event = SessionEvent::CompactionStart { tokens_before };
        if let Some(tx) = stream_tx {
            let _ = tx.unbounded_send(KitEvent::Session(start_event.clone()));
        }
        if let Some(cb) = on_event {
            cb(start_event);
        }

        let output =
            match SessionManager::execute_prepared_compaction(&*self.provider, prepared).await {
                Ok(out) => {
                    if cancel.is_cancelled() {
                        return if error_on_cancel {
                            Err(KitError::Aborted)
                        } else {
                            Ok(None)
                        };
                    }
                    out
                }
                Err(_) if cancel.is_cancelled() => {
                    return if error_on_cancel {
                        Err(KitError::Aborted)
                    } else {
                        Ok(None)
                    };
                }
                Err(err) => return Err(err.into()),
            };

        if cancel.is_cancelled() {
            return if error_on_cancel {
                Err(KitError::Aborted)
            } else {
                Ok(None)
            };
        }

        let (maybe_result, reconstructed) = {
            let mut session = self.session.lock().unwrap_or_else(|e| e.into_inner());
            if cancel.is_cancelled() {
                return if error_on_cancel {
                    Err(KitError::Aborted)
                } else {
                    Ok(None)
                };
            }
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

        let end_event = SessionEvent::CompactionEnd {
            summary: result.summary.clone(),
            tokens_before: result.tokens_before,
            first_kept_entry_id: result.first_kept_entry_id.clone(),
            usage: result.usage.clone(),
        };
        if let Some(tx) = stream_tx {
            let _ = tx.unbounded_send(KitEvent::Session(end_event.clone()));
        }
        if let Some(cb) = on_event {
            cb(end_event);
        }

        Ok(Some(result))
    }
}
