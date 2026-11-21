use crate::args::{CliArgs, CliMode};
use crate::framing::JsonlWriter;
use crate::protocol::{
    AgentSettledEvent, CompactionResponseData, ExtensionErrorEvent, RpcCommand, RpcImageInput,
    RpcResponse, RpcStateData, StreamingBehavior,
};
use futures_util::StreamExt;
use pikra_agent::{
    Agent, AgentConfig, AgentEvent, AgentMessage, AgentMiddleware, AgentTool, ApiKeyResolver,
    QueueMode, ToolExecutionMode,
};
use pikra_ai::{
    ApiKind, AssistantMessageEvent, CacheRetention, CancelToken, Client, ContentPart, Message,
    Model, ModelCost, SimpleStreamOptions, StopReason, StreamProvider, TextContent, ThinkingLevel,
    ToolDefinition, UserMessage,
};
use pikra_codemode::CodemodeSandboxOptions;
use pikra_http::ReqwestTransport;
use pikra_js::{JsExtensionConfig, JsExtensionRuntime};
use pikra_kit::{
    CODEMODE_EXEC_TOOL_NAME, CodemodeExecTool, DeferredToolRegistry, InstructionContextState,
    InstructionDiscoveryConfig, SkillCatalog, SkillDiscoveryConfig,
};
use pikra_session::{
    CompactionConfig, CompactionResult, JsonlSessionStore, MemorySessionStore, SessionConfig,
    SessionEntryPayload, SessionEvent, SessionManager, SessionStore, prepare_flushed_prompt,
};
use pikra_tools::{
    BuiltinToolSet, SleepTool, WakeGate, WebFetchConfig, WebFetchTool, WorkspaceTools,
    WorkspaceToolsConfig,
};
#[cfg(all(unix, not(target_os = "ios")))]
use pikra_tools::{
    ExecCommandTool, MonitorManager, MonitorTool, ProcessSessionManager, WriteStdinTool,
};
use serde_json::{Map, json};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex as AsyncMutex, oneshot};
use tokio::task::JoinHandle;
use uuid::Uuid;

const SKILLS_CONTEXT_NAME: &str = "skills";
const RPC_MODEL_CUSTOM_TYPE: &str = "pikra.rpc.model";
const DEFAULT_CONTEXT_WINDOW: u32 = 200_000;
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 16_384;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PersistedRpcModel {
    provider: String,
    model_id: String,
    api: ApiKind,
    base_url: String,
    context_window: u32,
    max_output_tokens: u32,
    supports_images: bool,
    supports_reasoning: bool,
}

impl From<&Model> for PersistedRpcModel {
    fn from(model: &Model) -> Self {
        Self {
            provider: model.provider.clone(),
            model_id: model.id.clone(),
            api: model.api.clone(),
            base_url: model.base_url.clone(),
            context_window: model.context_window,
            max_output_tokens: model.max_output_tokens,
            supports_images: model.supports_images,
            supports_reasoning: model.supports_reasoning,
        }
    }
}

impl From<PersistedRpcModel> for Model {
    fn from(pm: PersistedRpcModel) -> Self {
        Self {
            name: pm.model_id.clone(),
            id: pm.model_id,
            provider: pm.provider,
            api: pm.api,
            base_url: pm.base_url,
            context_window: pm.context_window,
            max_output_tokens: pm.max_output_tokens,
            supports_images: pm.supports_images,
            supports_reasoning: pm.supports_reasoning,
            cost: ModelCost::default(),
            headers: Vec::new(),
            compat: None,
        }
    }
}

fn restore_persisted_model(session: &SessionManager, known_models: &[Model]) -> Option<Model> {
    enum LatestModel {
        Ref(String, String),
        Full(PersistedRpcModel),
    }

    let mut latest: Option<LatestModel> = None;
    for entry in session.active_branch() {
        match entry.payload {
            SessionEntryPayload::ModelChange { provider, model_id } => {
                latest = Some(LatestModel::Ref(provider, model_id));
            }
            SessionEntryPayload::Custom { custom_type, data }
                if custom_type == RPC_MODEL_CUSTOM_TYPE =>
            {
                if let Ok(pm) = serde_json::from_value::<PersistedRpcModel>(data) {
                    latest = Some(LatestModel::Full(pm));
                }
            }
            _ => {}
        }
    }

    match latest {
        Some(LatestModel::Full(pm)) => Some(pm.into()),
        Some(LatestModel::Ref(provider, model_id)) => known_models
            .iter()
            .find(|m| m.provider == provider && m.id == model_id)
            .cloned()
            .or_else(|| {
                Some(build_model(
                    &provider, &model_id, None, None, None, None, None, None,
                ))
            }),
        None => None,
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn default_provider_spec(provider: &str) -> (ApiKind, &'static str, &'static str) {
    match provider.to_ascii_lowercase().as_str() {
        "anthropic" => (
            ApiKind::AnthropicMessages,
            "https://api.anthropic.com",
            "claude-sonnet-5-5",
        ),
        "google" | "gemini" => (
            ApiKind::GoogleGenerativeAi,
            "https://generativelanguage.googleapis.com/v1beta",
            "gemini-2.5-pro",
        ),
        "deepseek" => (
            ApiKind::OpenaiCompletions,
            "https://api.deepseek.com/v1",
            "deepseek-flash",
        ),
        "openrouter" => (
            ApiKind::OpenaiCompletions,
            "https://openrouter.ai/api/v1",
            "openai/gpt-4o-mini",
        ),
        _ => (
            ApiKind::OpenaiResponses,
            "https://api.openai.com/v1",
            "gpt-5.4",
        ),
    }
}

fn resolve_api_key_for_provider(provider: &str, explicit_env: Option<&str>) -> Option<String> {
    if let Some(env_name) = explicit_env {
        return std::env::var(env_name).ok().filter(|v| !v.is_empty());
    }

    let candidates: &[&str] = match provider.to_ascii_lowercase().as_str() {
        "openai" => &["OPENAI_API_KEY"],
        "anthropic" => &["ANTHROPIC_API_KEY"],
        "google" | "gemini" => &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        "deepseek" => &["DEEPSEEK_API_KEY"],
        "openrouter" => &["OPENROUTER_API_KEY"],
        _ => &[],
    };

    for var in candidates {
        if let Ok(val) = std::env::var(var)
            && !val.is_empty()
        {
            return Some(val);
        }
    }

    let dynamic = format!(
        "{}_API_KEY",
        provider
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect::<String>()
    );
    std::env::var(dynamic).ok().filter(|v| !v.is_empty())
}

#[allow(clippy::too_many_arguments)]
fn build_model(
    provider: &str,
    model_id: &str,
    api_override: Option<ApiKind>,
    base_url_override: Option<String>,
    context_window: Option<u32>,
    max_output_tokens: Option<u32>,
    supports_images: Option<bool>,
    supports_reasoning: Option<bool>,
) -> Model {
    let (default_api, default_url, _) = default_provider_spec(provider);
    let id = model_id.to_string();
    Model {
        name: id.clone(),
        id,
        provider: provider.to_string(),
        api: api_override.unwrap_or(default_api),
        base_url: base_url_override.unwrap_or_else(|| default_url.to_string()),
        context_window: context_window.unwrap_or(DEFAULT_CONTEXT_WINDOW),
        max_output_tokens: max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
        supports_images: supports_images.unwrap_or(true),
        supports_reasoning: supports_reasoning.unwrap_or(true),
        cost: ModelCost::default(),
        headers: Vec::new(),
        compat: None,
    }
}

fn build_content_parts(message: String, images: Option<Vec<RpcImageInput>>) -> Vec<ContentPart> {
    let mut parts = Vec::new();
    if !message.is_empty() || images.as_ref().is_none_or(Vec::is_empty) {
        parts.push(ContentPart::Text(TextContent::plain(message)));
    }
    if let Some(imgs) = images {
        for img in imgs {
            parts.push(ContentPart::Image(img.into()));
        }
    }
    parts
}

struct ActiveRunTask {
    running: Arc<AtomicBool>,
    aborted: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

struct ActiveCompactionTask {
    running: Arc<AtomicBool>,
    cancel: CancelToken,
    handle: JoinHandle<()>,
}

struct SessionRuntimeState {
    instructions: InstructionContextState,
    session: Arc<Mutex<SessionManager>>,
    agent: Arc<Agent>,
    session_file: Option<PathBuf>,
}

pub struct RpcRuntime {
    provider: Arc<dyn StreamProvider>,
    api_key_resolver: ApiKeyResolver,
    models: Mutex<Vec<Model>>,
    cwd: PathBuf,
    base_system_prompt: String,
    resident_tools: Arc<Mutex<Vec<Arc<dyn AgentTool>>>>,
    deferred_registry: Arc<DeferredToolRegistry>,
    js_runtime: Option<Arc<JsExtensionRuntime>>,
    skill_discovery: SkillDiscoveryConfig,
    instruction_discovery: InstructionDiscoveryConfig,
    #[cfg(all(unix, not(target_os = "ios")))]
    background_tools: Option<(ProcessSessionManager, MonitorManager)>,
    no_session: bool,
    compaction_config: Mutex<CompactionConfig>,
    state: Mutex<SessionRuntimeState>,
    queued_steering: Arc<Mutex<VecDeque<(String, String)>>>,
    queued_follow_up: Arc<Mutex<VecDeque<(String, String)>>>,
    active_run: AsyncMutex<Option<ActiveRunTask>>,
    active_compaction: AsyncMutex<Option<ActiveCompactionTask>>,
    is_compacting: Arc<AtomicBool>,
    compaction_cancel: Arc<Mutex<Option<CancelToken>>>,
    writer: JsonlWriter,
}

impl RpcRuntime {
    pub fn new(args: &CliArgs, writer: JsonlWriter) -> Result<Self, String> {
        let transport = ReqwestTransport::new()
            .map_err(|err| format!("failed to initialize HTTP transport: {err}"))?;
        let transport = Arc::new(transport);
        let provider: Arc<dyn StreamProvider> = Arc::new(Client::new(transport.clone()));

        let explicit_env = args.api_key_env.clone();
        let api_key_resolver: ApiKeyResolver = Arc::new(move |provider_id: &str| {
            let provider_owned = provider_id.to_string();
            let env_owned = explicit_env.clone();
            Box::pin(
                async move { resolve_api_key_for_provider(&provider_owned, env_owned.as_deref()) },
            )
        });

        let provider_name = args
            .provider
            .clone()
            .unwrap_or_else(|| "openai".to_string());
        let (_, _, default_model_id) = default_provider_spec(&provider_name);
        let model_id = args
            .model
            .clone()
            .unwrap_or_else(|| default_model_id.to_string());

        let initial_model = build_model(
            &provider_name,
            &model_id,
            args.api.clone(),
            args.base_url.clone(),
            args.context_window,
            None,
            None,
            None,
        );

        let deferred_registry = Arc::new(DeferredToolRegistry::new());
        let mut resident: Vec<Arc<dyn AgentTool>> = Vec::new();

        #[cfg(all(unix, not(target_os = "ios")))]
        let mut background_tools = None;
        if !args.no_tools {
            let workspace =
                WorkspaceToolsConfig::new(&args.cwd).with_enabled_tools(BuiltinToolSet::all());
            let wake_gate = WakeGate::new();
            resident.extend(WorkspaceTools::build(workspace.clone()));
            #[cfg(all(unix, not(target_os = "ios")))]
            {
                let workspace = Arc::new(workspace);
                let processes =
                    ProcessSessionManager::new(workspace.clone(), wake_gate.clone(), None);
                let monitors = MonitorManager::new(workspace, wake_gate.clone(), None);
                resident.push(Arc::new(ExecCommandTool::new(processes.clone())));
                resident.push(Arc::new(WriteStdinTool::new(processes.clone())));
                resident.push(Arc::new(MonitorTool::new(monitors.clone())));
                background_tools = Some((processes, monitors));
            }
            resident.push(Arc::new(SleepTool::new(wake_gate)));
            resident.push(Arc::new(WebFetchTool::new(
                transport,
                WebFetchConfig::default(),
            )));
        }

        let pikra_home = args
            .pikra_home
            .clone()
            .unwrap_or_else(|| pikra_acp::AcpConfig::default().pikra_home);
        let skill_discovery = SkillDiscoveryConfig {
            cwd: Some(args.cwd.clone()),
            walk_boundary: args.skill_walk_boundary,
            project_skill_dirs: vec![".pikra/skills".into()],
            extra_roots: args.skill_dirs.clone(),
            user_skill_dirs: vec![pikra_home.join("skills")],
            ..SkillDiscoveryConfig::default()
        };
        let mut instruction_discovery = args.instruction_discovery.clone();
        instruction_discovery.user_dirs.push(pikra_home);

        let js_runtime = if args.extensions.is_empty() {
            None
        } else {
            let mut js_cfg = JsExtensionConfig::new();
            for ext_path in &args.extensions {
                js_cfg = js_cfg.with_file(ext_path);
            }
            let rt = JsExtensionRuntime::new(js_cfg)
                .map_err(|err| format!("failed to load JS extensions: {err}"))?;
            let rt = Arc::new(rt);
            resident.extend(rt.tools());
            Some(rt)
        };

        if args.enable_codemode
            && !resident
                .iter()
                .any(|t| t.definition().name == CODEMODE_EXEC_TOOL_NAME)
        {
            // Snapshot resident tools BEFORE pushing `exec` so `CodemodeExecTool` never holds
            // a strong Arc cycle back to itself.
            let codemode_resident_handle = Arc::new(Mutex::new(resident.clone()));
            let exec_tool: Arc<dyn AgentTool> = Arc::new(CodemodeExecTool::new(
                codemode_resident_handle,
                Arc::clone(&deferred_registry),
                CodemodeSandboxOptions::default(),
            ));
            resident.push(exec_tool);
        }
        let resident_tools_handle = Arc::new(Mutex::new(resident.clone()));

        let mut compaction = CompactionConfig::default();
        if let Some(reserve) = args.compact_tokens {
            compaction.compaction_reserve_tokens = Some(reserve);
        }

        let base_system_prompt = args.system_prompt.clone().unwrap_or_default();

        let (store, session_file): (Arc<dyn SessionStore>, Option<PathBuf>) = if args.no_session {
            (Arc::new(MemorySessionStore::new()), None)
        } else if let Some(path) = &args.session {
            (
                Arc::new(JsonlSessionStore::new(path.clone())),
                Some(path.clone()),
            )
        } else {
            (Arc::new(MemorySessionStore::new()), None)
        };

        let explicit_model_override = args.provider.is_some() || args.model.is_some();
        let (session_manager, agent, instructions) = build_session_and_agent(
            Arc::clone(&provider),
            api_key_resolver.clone(),
            store,
            &args.cwd,
            &base_system_prompt,
            &resident,
            js_runtime.as_ref(),
            &skill_discovery,
            &instruction_discovery,
            compaction.clone(),
            initial_model,
            !explicit_model_override,
            &[],
            args.api.clone(),
            args.base_url.clone(),
            args.context_window,
            args.thinking,
            args.fast,
            QueueMode::OneAtATime,
            QueueMode::OneAtATime,
            None,
        )?;
        let effective_model = agent.model();

        Ok(Self {
            provider,
            api_key_resolver,
            models: Mutex::new(vec![effective_model]),
            cwd: args.cwd.clone(),
            base_system_prompt,
            resident_tools: resident_tools_handle,
            deferred_registry,
            js_runtime,
            skill_discovery,
            instruction_discovery,
            #[cfg(all(unix, not(target_os = "ios")))]
            background_tools,
            no_session: args.no_session,
            compaction_config: Mutex::new(compaction),
            state: Mutex::new(SessionRuntimeState {
                instructions,
                session: Arc::new(Mutex::new(session_manager)),
                agent: Arc::new(agent),
                session_file,
            }),
            queued_steering: Arc::new(Mutex::new(VecDeque::new())),
            queued_follow_up: Arc::new(Mutex::new(VecDeque::new())),
            active_run: AsyncMutex::new(None),
            active_compaction: AsyncMutex::new(None),
            is_compacting: Arc::new(AtomicBool::new(false)),
            compaction_cancel: Arc::new(Mutex::new(None)),
            writer,
        })
    }

    fn current_agent(&self) -> Arc<Agent> {
        Arc::clone(&self.state.lock().unwrap_or_else(|e| e.into_inner()).agent)
    }

    fn current_session(&self) -> Arc<Mutex<SessionManager>> {
        Arc::clone(&self.state.lock().unwrap_or_else(|e| e.into_inner()).session)
    }

    async fn reap_finished_run_locked(&self, guard: &mut Option<ActiveRunTask>) {
        if let Some(task) = guard.as_ref()
            && !task.running.load(Ordering::Acquire)
            && let Some(finished) = guard.take()
        {
            let _ = finished.handle.await;
        }
    }

    async fn reap_finished_compaction_locked(&self, guard: &mut Option<ActiveCompactionTask>) {
        if let Some(task) = guard.as_ref()
            && !task.running.load(Ordering::Acquire)
            && let Some(finished) = guard.take()
        {
            let _ = finished.handle.await;
        }
    }

    async fn is_busy(&self) -> bool {
        let mut run_guard = self.active_run.lock().await;
        self.reap_finished_run_locked(&mut run_guard).await;
        let mut comp_guard = self.active_compaction.lock().await;
        self.reap_finished_compaction_locked(&mut comp_guard).await;
        run_guard.is_some() || comp_guard.is_some() || self.is_compacting.load(Ordering::Acquire)
    }

    pub async fn handle_command(&self, id: Option<String>, cmd: RpcCommand) {
        let command_name = cmd.command_name();
        match cmd {
            RpcCommand::Prompt {
                message,
                images,
                streaming_behavior,
                is_meta,
            } => {
                self.handle_prompt(id, message, images, streaming_behavior, is_meta)
                    .await;
            }
            RpcCommand::Steer {
                message,
                images,
                is_meta,
            } => {
                self.queue_steer(message, images, is_meta);
                self.writer.send_json(&RpcResponse::ok(
                    id,
                    command_name,
                    Some(json!({ "disposition": "queued" })),
                ));
            }
            RpcCommand::FollowUp {
                message,
                images,
                is_meta,
            } => {
                self.queue_follow_up(message, images, is_meta);
                self.writer.send_json(&RpcResponse::ok(
                    id,
                    command_name,
                    Some(json!({ "disposition": "queued" })),
                ));
            }
            RpcCommand::Abort => {
                self.abort_active().await;
                self.writer
                    .send_json(&RpcResponse::ok(id, command_name, None));
            }
            RpcCommand::ClearQueue => {
                let agent = self.current_agent();
                agent.clear_all_queues();
                let steering: Vec<String> = self
                    .queued_steering
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .drain(..)
                    .map(|(_, text)| text)
                    .collect();
                let follow_up: Vec<String> = self
                    .queued_follow_up
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .drain(..)
                    .map(|(_, text)| text)
                    .collect();
                self.writer.send_json(&RpcResponse::ok(
                    id,
                    command_name,
                    Some(json!({
                        "steering": steering,
                        "follow_up": follow_up,
                    })),
                ));
            }
            RpcCommand::GetState => {
                let (is_streaming, is_compacting) = {
                    let mut run_guard = self.active_run.lock().await;
                    self.reap_finished_run_locked(&mut run_guard).await;
                    let mut comp_guard = self.active_compaction.lock().await;
                    self.reap_finished_compaction_locked(&mut comp_guard).await;
                    (
                        run_guard.is_some(),
                        comp_guard.is_some() || self.is_compacting.load(Ordering::Acquire),
                    )
                };
                let data = self.snapshot_state(is_streaming, is_compacting);
                self.writer.send_json(&RpcResponse::ok(
                    id,
                    command_name,
                    serde_json::to_value(data).ok(),
                ));
            }
            RpcCommand::SetModel {
                provider,
                model_id,
                api,
                base_url,
                context_window,
                max_output_tokens,
                supports_images,
                supports_reasoning,
            } => {
                if self.is_busy().await {
                    self.writer.send_json(&RpcResponse::error(
                        id,
                        command_name,
                        "Cannot change model while session is busy",
                    ));
                    return;
                }
                let res = self.handle_set_model(
                    provider,
                    model_id,
                    api,
                    base_url,
                    context_window,
                    max_output_tokens,
                    supports_images,
                    supports_reasoning,
                );
                self.send_result(id, command_name, res);
            }
            RpcCommand::GetAvailableModels => {
                let models = self
                    .models
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                self.writer.send_json(&RpcResponse::ok(
                    id,
                    command_name,
                    Some(json!({ "models": models })),
                ));
            }
            RpcCommand::SetThinkingLevel { level } => {
                if self.is_busy().await {
                    self.writer.send_json(&RpcResponse::error(
                        id,
                        command_name,
                        "Cannot change thinking level while session is busy",
                    ));
                    return;
                }
                let agent = self.current_agent();
                let session = self.current_session();
                let res = agent
                    .set_thinking_level(level)
                    .map_err(|e| e.to_string())
                    .and_then(|()| {
                        let fast = agent.fast();
                        session
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .append_thinking_change(level, fast)
                            .map(|_| None)
                            .map_err(|e| e.to_string())
                    });
                self.send_result_opt(id, command_name, res);
            }
            RpcCommand::SetFast { fast } => {
                if self.is_busy().await {
                    self.writer.send_json(&RpcResponse::error(
                        id,
                        command_name,
                        "Cannot change fast mode while session is busy",
                    ));
                    return;
                }
                let agent = self.current_agent();
                let session = self.current_session();
                let res = agent
                    .set_fast(fast)
                    .map_err(|e| e.to_string())
                    .and_then(|()| {
                        let level = agent.thinking_level();
                        session
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .append_thinking_change(level, fast)
                            .map(|_| None)
                            .map_err(|e| e.to_string())
                    });
                self.send_result_opt(id, command_name, res);
            }
            RpcCommand::SetContext { name, text } => {
                let changed = self
                    .current_session()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .set_context(name, text);
                self.writer.send_json(&RpcResponse::ok(
                    id,
                    command_name,
                    Some(json!({ "changed": changed })),
                ));
            }
            RpcCommand::AppendContext { name, text } => {
                self.current_session()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .append_named_context(name.as_deref().unwrap_or("context"), text);
                self.writer
                    .send_json(&RpcResponse::ok(id, command_name, None));
            }
            RpcCommand::AppendMeta { text, images } => {
                self.current_session()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .append_meta_parts(build_content_parts(text, images));
                self.writer
                    .send_json(&RpcResponse::ok(id, command_name, None));
            }
            RpcCommand::Compact {
                custom_instructions,
            } => {
                self.handle_manual_compact(id, custom_instructions).await;
            }
            RpcCommand::SetAutoCompaction { enabled } => {
                {
                    let mut cfg = self
                        .compaction_config
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    cfg.enabled = enabled;
                }
                {
                    let session = self.current_session();
                    let mut guard = session.lock().unwrap_or_else(|e| e.into_inner());
                    let mut cfg = guard.compaction_config().clone();
                    cfg.enabled = enabled;
                    guard.set_compaction_config(cfg);
                }
                self.writer.send_json(&RpcResponse::ok(
                    id,
                    command_name,
                    Some(json!({ "enabled": enabled })),
                ));
            }
            RpcCommand::SetSteeringMode { mode } => {
                self.current_agent().set_steering_mode(mode);
                self.writer
                    .send_json(&RpcResponse::ok(id, command_name, None));
            }
            RpcCommand::SetFollowUpMode { mode } => {
                self.current_agent().set_follow_up_mode(mode);
                self.writer
                    .send_json(&RpcResponse::ok(id, command_name, None));
            }
            RpcCommand::NewSession {
                session_path,
                parent_session,
            } => {
                if self.is_busy().await {
                    self.writer.send_json(&RpcResponse::error(
                        id,
                        command_name,
                        "Cannot start a new session while busy",
                    ));
                    return;
                }
                let res = self.handle_new_session(session_path, parent_session);
                self.send_result(id, command_name, res);
            }
            RpcCommand::SwitchSession { session_path } => {
                if self.is_busy().await {
                    self.writer.send_json(&RpcResponse::error(
                        id,
                        command_name,
                        "Cannot switch session while busy",
                    ));
                    return;
                }
                let res = self.handle_switch_session(session_path);
                self.send_result(id, command_name, res);
            }
            RpcCommand::Fork { entry_id } => {
                if self.is_busy().await {
                    self.writer.send_json(&RpcResponse::error(
                        id,
                        command_name,
                        "Cannot fork session while busy",
                    ));
                    return;
                }
                let res = self.handle_fork(&entry_id);
                self.send_result(id, command_name, res);
            }
            RpcCommand::GetEntries { since } => {
                let session = self.current_session();
                let guard = session.lock().unwrap_or_else(|e| e.into_inner());
                let all_entries = guard.entries();
                let entries = match since.as_deref() {
                    Some(since_id) => match all_entries.iter().position(|e| e.id == since_id) {
                        Some(idx) => all_entries[idx + 1..].to_vec(),
                        None => Vec::new(),
                    },
                    None => all_entries.to_vec(),
                };
                let leaf_id = guard.leaf_id().map(ToString::to_string);
                self.writer.send_json(&RpcResponse::ok(
                    id,
                    command_name,
                    Some(json!({ "entries": entries, "leaf_id": leaf_id })),
                ));
            }
            RpcCommand::GetTree => {
                let session = self.current_session();
                let guard = session.lock().unwrap_or_else(|e| e.into_inner());
                let tree = guard.tree();
                let leaf_id = guard.leaf_id().map(ToString::to_string);
                self.writer.send_json(&RpcResponse::ok(
                    id,
                    command_name,
                    Some(json!({ "tree": tree, "leaf_id": leaf_id })),
                ));
            }
            RpcCommand::GetMessages { include_meta } => {
                let agent = self.current_agent();
                let messages = if include_meta.unwrap_or(true) {
                    agent.messages()
                } else {
                    agent.visible_messages()
                };
                self.writer.send_json(&RpcResponse::ok(
                    id,
                    command_name,
                    Some(json!({ "messages": messages })),
                ));
            }
            RpcCommand::GetLastAssistantText => {
                let text = self
                    .current_agent()
                    .messages()
                    .into_iter()
                    .rev()
                    .find_map(|msg| match msg {
                        AgentMessage::Llm(Message::Assistant(assistant)) => {
                            Some(assistant.text_output())
                        }
                        _ => None,
                    });
                self.writer.send_json(&RpcResponse::ok(
                    id,
                    command_name,
                    Some(json!({ "text": text })),
                ));
            }
            RpcCommand::ReloadExtensions => {
                if self.is_busy().await {
                    self.writer.send_json(&RpcResponse::error(
                        id,
                        command_name,
                        "Cannot reload extensions while session is busy",
                    ));
                    return;
                }
                let res = match &self.js_runtime {
                    Some(rt) => rt
                        .reload()
                        .map(|()| json!({ "extensions": rt.loaded_extensions() }))
                        .map_err(|e| e.to_string()),
                    None => Ok(json!({ "extensions": Vec::<String>::new() })),
                };
                self.send_result(id, command_name, res);
            }
        }
    }

    fn queue_steer(&self, message: String, images: Option<Vec<RpcImageInput>>, is_meta: bool) {
        let agent = self.current_agent();
        let queue_id = Uuid::now_v7().to_string();
        if is_meta {
            agent.steer_meta_parts(queue_id, build_content_parts(message, images));
        } else {
            let parts = build_content_parts(message.clone(), images);
            self.queued_steering
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back((queue_id.clone(), message));
            agent.steer_with_parts(queue_id, parts);
        }
    }

    fn queue_follow_up(&self, message: String, images: Option<Vec<RpcImageInput>>, is_meta: bool) {
        let agent = self.current_agent();
        let queue_id = Uuid::now_v7().to_string();
        let parts = build_content_parts(message.clone(), images);
        self.queued_follow_up
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back((queue_id.clone(), message));
        if is_meta {
            agent.follow_up_meta_parts(queue_id, parts);
        } else {
            agent.follow_up_with_parts(queue_id, parts);
        }
    }

    async fn handle_prompt(
        &self,
        id: Option<String>,
        message: String,
        images: Option<Vec<RpcImageInput>>,
        streaming_behavior: Option<StreamingBehavior>,
        is_meta: bool,
    ) {
        let mut active_guard = self.active_run.lock().await;
        self.reap_finished_run_locked(&mut active_guard).await;

        if active_guard.is_some() {
            match streaming_behavior {
                Some(StreamingBehavior::Steer) => {
                    self.queue_steer(message, images, is_meta);
                    self.writer.send_json(&RpcResponse::ok(
                        id,
                        "prompt",
                        Some(json!({ "disposition": "queued" })),
                    ));
                }
                Some(StreamingBehavior::FollowUp) => {
                    self.queue_follow_up(message, images, is_meta);
                    self.writer.send_json(&RpcResponse::ok(
                        id,
                        "prompt",
                        Some(json!({ "disposition": "queued" })),
                    ));
                }
                None => {
                    self.writer.send_json(&RpcResponse::error(
                        id,
                        "prompt",
                        "Agent is already streaming; specify `streaming_behavior` (\"steer\" or \"followUp\")",
                    ));
                }
            }
            return;
        }

        {
            let mut comp_guard = self.active_compaction.lock().await;
            self.reap_finished_compaction_locked(&mut comp_guard).await;
            if comp_guard.is_some() || self.is_compacting.load(Ordering::Acquire) {
                self.writer.send_json(&RpcResponse::error(
                    id,
                    "prompt",
                    "Session is currently compacting",
                ));
                return;
            }
        }

        let content = build_content_parts(message, images);
        let user_msg =
            Message::User(UserMessage::parts(content, now_ms()).with_meta(is_meta)).into();

        let session = self.current_session();
        let agent = self.current_agent();

        let (prompt_messages, flushed_count, flushed_events) = {
            let mut sess = session.lock().unwrap_or_else(|e| e.into_inner());
            let flushed = match sess.flush_pending_context() {
                Ok(f) => f,
                Err(err) => {
                    let _ = agent.replace_messages(sess.reconstruct_context().messages);
                    self.writer.send_json(&RpcResponse::ok(
                        id,
                        "prompt",
                        Some(json!({"disposition": "started"})),
                    ));
                    self.writer.send_json(&AgentSettledEvent::with_error(
                        false,
                        Some(format!("failed to persist session message: {err}")),
                    ));
                    return;
                }
            };
            prepare_flushed_prompt(flushed, user_msg)
        };

        let running = Arc::new(AtomicBool::new(true));
        let aborted = Arc::new(AtomicBool::new(false));
        let (started_tx, started_rx) = oneshot::channel::<Result<(), String>>();
        let (go_tx, go_rx) = oneshot::channel::<()>();

        let task_provider = Arc::clone(&self.provider);
        let task_resolver = self.api_key_resolver.clone();
        let task_session = Arc::clone(&session);
        let task_agent = Arc::clone(&agent);
        let task_deferred = Arc::clone(&self.deferred_registry);
        let task_js = self.js_runtime.clone();
        let task_steering = Arc::clone(&self.queued_steering);
        let task_follow_up = Arc::clone(&self.queued_follow_up);
        let task_running = Arc::clone(&running);
        let task_aborted = Arc::clone(&aborted);
        let task_compacting = Arc::clone(&self.is_compacting);
        let task_compaction_cancel = Arc::clone(&self.compaction_cancel);
        let task_writer = self.writer.clone();

        let handle = tokio::spawn(async move {
            let initial_stream = match task_agent.prompt_messages_stream(prompt_messages) {
                Ok(s) => {
                    let _ = started_tx.send(Ok(()));
                    s
                }
                Err(err) => {
                    task_running.store(false, Ordering::Release);
                    let _ = started_tx.send(Err(err.to_string()));
                    return;
                }
            };

            // Wait until the prompt acceptance response has been enqueued on stdout.
            let _ = go_rx.await;

            for ev in flushed_events {
                task_writer.send_json(&ev);
            }

            let mut saw_abort = false;
            let persistence_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
            let mut next_stream = Some((initial_stream, flushed_count));

            while let Some((mut stream, skip_ends)) = next_stream.take() {
                let mut seen_ends = 0usize;
                while let Some(mut ev) = stream.next().await {
                    match &mut ev {
                        AgentEvent::Steered { id: steer_id, .. } => {
                            let mut q = task_steering.lock().unwrap_or_else(|e| e.into_inner());
                            if let Some(pos) = q.iter().position(|(qid, _)| qid == steer_id) {
                                q.remove(pos);
                            }
                        }
                        AgentEvent::MessageStart {
                            message: AgentMessage::Llm(Message::User(user)),
                        } => {
                            if let Some(uid) = &user.id {
                                let mut q =
                                    task_follow_up.lock().unwrap_or_else(|e| e.into_inner());
                                if let Some(pos) = q.iter().position(|(qid, _)| qid == uid) {
                                    q.remove(pos);
                                }
                            }
                        }
                        AgentEvent::MessageEnd { message } => {
                            if let AgentMessage::Llm(Message::Assistant(a)) = message
                                && a.stop_reason == StopReason::Aborted
                            {
                                saw_abort = true;
                            }
                            if seen_ends >= skip_ends {
                                let persist_res = {
                                    let mut sess =
                                        task_session.lock().unwrap_or_else(|e| e.into_inner());
                                    sess.append_message(message.clone())
                                };
                                if let Err(err) = persist_res {
                                    let mut slot =
                                        persistence_error.lock().unwrap_or_else(|e| e.into_inner());
                                    if slot.is_none() {
                                        *slot = Some(format!(
                                            "failed to persist session message: {err}"
                                        ));
                                    }
                                    drop(slot);
                                    task_agent.abort_now();
                                }
                            }
                            if let Some(err_msg) = persistence_error
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .clone()
                                && let AgentMessage::Llm(Message::Assistant(a)) = message
                            {
                                a.stop_reason = StopReason::Error;
                                a.error_message = Some(err_msg);
                            }
                            seen_ends += 1;
                        }
                        AgentEvent::TurnEnd { message, .. } => {
                            if let Some(err_msg) = persistence_error
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .clone()
                            {
                                message.stop_reason = StopReason::Error;
                                message.error_message = Some(err_msg);
                            }
                        }
                        AgentEvent::AgentEnd { .. } => {
                            // Unread steering items were discarded by Agent on AgentEnd.
                            task_steering
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .clear();
                        }
                        _ => {}
                    }

                    task_writer.send_json(&ev);
                    flush_js_errors(task_js.as_ref(), &task_writer);
                }

                drop(stream);

                if persistence_error
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_some()
                {
                    break;
                }

                if task_aborted.load(Ordering::Acquire) {
                    saw_abort = true;
                    break;
                }

                // Auto-compaction after turn settles (unless aborted).
                if let Err(err) = run_compaction_task(
                    &task_provider,
                    &task_resolver,
                    &task_session,
                    &task_agent,
                    &task_deferred,
                    &task_compacting,
                    &task_compaction_cancel,
                    CancelToken::new(),
                    &task_writer,
                    true,
                    None,
                )
                .await
                {
                    if task_aborted.load(Ordering::Acquire) {
                        saw_abort = true;
                        break;
                    }
                    eprintln!("[pikra-rpc] auto-compaction error: {err}");
                }

                if task_aborted.load(Ordering::Acquire) {
                    saw_abort = true;
                    break;
                }

                // If a follow-up or steering message arrived during compaction, continue automatically.
                if task_agent.has_queued_messages() {
                    match task_agent.continue_stream() {
                        Ok(cont_stream) => {
                            next_stream = Some((cont_stream, 0));
                        }
                        Err(_) => break,
                    }
                }
            }

            let persist_err = persistence_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let final_aborted = if persist_err.is_some() {
                task_aborted.load(Ordering::Acquire)
            } else {
                saw_abort || task_aborted.load(Ordering::Acquire)
            };
            task_running.store(false, Ordering::Release);
            task_writer.send_json(&AgentSettledEvent::with_error(final_aborted, persist_err));
        });

        match started_rx.await {
            Ok(Ok(())) => {
                *active_guard = Some(ActiveRunTask {
                    running,
                    aborted,
                    handle,
                });
                self.writer.send_json(&RpcResponse::ok(
                    id,
                    "prompt",
                    Some(json!({ "disposition": "started" })),
                ));
                let _ = go_tx.send(());
            }
            Ok(Err(err)) => {
                let _ = handle.await;
                self.writer
                    .send_json(&RpcResponse::error(id, "prompt", err));
            }
            Err(_) => {
                let _ = handle.await;
                self.writer.send_json(&RpcResponse::error(
                    id,
                    "prompt",
                    "Prompt worker terminated unexpectedly before starting",
                ));
            }
        }
    }

    pub async fn shutdown_tools(&self) {
        #[cfg(all(unix, not(target_os = "ios")))]
        if let Some((processes, monitors)) = &self.background_tools {
            tokio::join!(processes.shutdown(), monitors.shutdown());
        }
    }

    pub async fn abort_active(&self) {
        let mut active_guard = self.active_run.lock().await;
        let mut comp_guard = self.active_compaction.lock().await;
        if let Some(cancel) = self
            .compaction_cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            cancel.cancel();
        }
        if let Some(comp_task) = comp_guard.take() {
            comp_task.cancel.cancel();
            let _ = comp_task.handle.await;
        }
        if let Some(task) = active_guard.take() {
            task.aborted.store(true, Ordering::SeqCst);
            self.current_agent().abort_now();
            let _ = task.handle.await;
        } else {
            self.current_agent().abort().await;
        }
    }

    pub async fn wait_for_active_run(&self) {
        let mut active_guard = self.active_run.lock().await;
        if let Some(task) = active_guard.take() {
            let _ = task.handle.await;
        }
        let mut comp_guard = self.active_compaction.lock().await;
        if let Some(comp_task) = comp_guard.take() {
            let _ = comp_task.handle.await;
        }
    }

    pub fn abort_now_sync(&self) {
        if let Some(cancel) = self
            .compaction_cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            cancel.cancel();
        }
        self.current_agent().abort_now();
        if let Ok(mut guard) = self.active_run.try_lock()
            && let Some(task) = guard.take()
        {
            task.handle.abort();
        }
        if let Ok(mut comp_guard) = self.active_compaction.try_lock()
            && let Some(comp_task) = comp_guard.take()
        {
            comp_task.cancel.cancel();
            comp_task.handle.abort();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_set_model(
        &self,
        provider: String,
        model_id: String,
        api: Option<ApiKind>,
        base_url: Option<String>,
        context_window: Option<u32>,
        max_output_tokens: Option<u32>,
        supports_images: Option<bool>,
        supports_reasoning: Option<bool>,
    ) -> Result<serde_json::Value, String> {
        let agent = self.current_agent();
        let current = agent.model();

        let mut models = self.models.lock().unwrap_or_else(|e| e.into_inner());
        let resolved = if let Some(existing) = models
            .iter()
            .find(|m| m.provider == provider && m.id == model_id)
            .cloned()
        {
            let mut updated = existing;
            if let Some(a) = api {
                updated.api = a;
            }
            if let Some(u) = base_url {
                updated.base_url = u;
            }
            if let Some(cw) = context_window {
                updated.context_window = cw;
            }
            if let Some(mo) = max_output_tokens {
                updated.max_output_tokens = mo;
            }
            if let Some(si) = supports_images {
                updated.supports_images = si;
            }
            if let Some(sr) = supports_reasoning {
                updated.supports_reasoning = sr;
            }
            updated
        } else if provider == current.provider {
            Model {
                id: model_id.clone(),
                name: model_id.clone(),
                provider: provider.clone(),
                api: api.unwrap_or(current.api),
                base_url: base_url.unwrap_or(current.base_url),
                context_window: context_window.unwrap_or(current.context_window),
                max_output_tokens: max_output_tokens.unwrap_or(current.max_output_tokens),
                supports_images: supports_images.unwrap_or(current.supports_images),
                supports_reasoning: supports_reasoning.unwrap_or(current.supports_reasoning),
                cost: current.cost,
                headers: current.headers,
                compat: current.compat,
            }
        } else {
            build_model(
                &provider,
                &model_id,
                api,
                base_url,
                context_window,
                max_output_tokens,
                supports_images,
                supports_reasoning,
            )
        };

        agent
            .set_model(resolved.clone())
            .map_err(|e| e.to_string())?;
        let persisted_model_json =
            serde_json::to_value(PersistedRpcModel::from(&resolved)).map_err(|e| e.to_string())?;
        {
            let session = self.current_session();
            let mut sess = session.lock().unwrap_or_else(|e| e.into_inner());
            sess.append_model_change(resolved.provider.clone(), resolved.id.clone())
                .map_err(|e| e.to_string())?;
            sess.append_custom(RPC_MODEL_CUSTOM_TYPE, persisted_model_json)
                .map_err(|e| e.to_string())?;
        }

        if let Some(slot) = models
            .iter_mut()
            .find(|m| m.provider == resolved.provider && m.id == resolved.id)
        {
            *slot = resolved.clone();
        } else {
            models.push(resolved.clone());
        }

        serde_json::to_value(resolved).map_err(|e| e.to_string())
    }

    async fn handle_manual_compact(&self, id: Option<String>, custom_instructions: Option<String>) {
        let mut run_guard = self.active_run.lock().await;
        self.reap_finished_run_locked(&mut run_guard).await;
        let mut comp_guard = self.active_compaction.lock().await;
        self.reap_finished_compaction_locked(&mut comp_guard).await;

        if run_guard.is_some() || comp_guard.is_some() || self.is_compacting.load(Ordering::Acquire)
        {
            self.writer.send_json(&RpcResponse::error(
                id,
                "compact",
                "Cannot compact while session is busy",
            ));
            return;
        }

        let session = self.current_session();
        let agent = self.current_agent();
        let cancel = CancelToken::new();
        let running = Arc::new(AtomicBool::new(true));

        self.is_compacting.store(true, Ordering::Release);
        *self
            .compaction_cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(cancel.clone());

        let task_provider = Arc::clone(&self.provider);
        let task_resolver = self.api_key_resolver.clone();
        let task_deferred = Arc::clone(&self.deferred_registry);
        let task_compacting = Arc::clone(&self.is_compacting);
        let task_compaction_cancel = Arc::clone(&self.compaction_cancel);
        let task_writer = self.writer.clone();
        let task_running = Arc::clone(&running);
        let task_cancel = cancel.clone();

        let handle = tokio::spawn(async move {
            let outcome = run_compaction_task(
                &task_provider,
                &task_resolver,
                &session,
                &agent,
                &task_deferred,
                &task_compacting,
                &task_compaction_cancel,
                task_cancel,
                &task_writer,
                false,
                custom_instructions,
            )
            .await
            .and_then(|res| {
                let dto = CompactionResponseData::from(res);
                serde_json::to_value(dto).map_err(|e| e.to_string())
            });

            *task_compaction_cancel
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
            task_compacting.store(false, Ordering::Release);
            task_running.store(false, Ordering::Release);

            match outcome {
                Ok(data) => task_writer.send_json(&RpcResponse::ok(id, "compact", Some(data))),
                Err(err) => task_writer.send_json(&RpcResponse::error(id, "compact", err)),
            }
        });

        *comp_guard = Some(ActiveCompactionTask {
            running,
            cancel,
            handle,
        });
    }

    fn handle_new_session(
        &self,
        session_path: Option<String>,
        parent_session: Option<String>,
    ) -> Result<serde_json::Value, String> {
        let (current_model, current_thinking, current_fast, steer_mode, follow_mode, current_file) = {
            let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (
                guard.agent.model(),
                guard.agent.thinking_level(),
                guard.agent.fast(),
                guard.agent.steering_mode(),
                guard.agent.follow_up_mode(),
                guard.session_file.clone(),
            )
        };

        let (store, new_file): (Arc<dyn SessionStore>, Option<PathBuf>) =
            if let Some(path_str) = session_path {
                let p = PathBuf::from(path_str);
                (Arc::new(JsonlSessionStore::new(p.clone())), Some(p))
            } else if self.no_session {
                (Arc::new(MemorySessionStore::new()), None)
            } else if let Some(prev) = current_file {
                let dir = prev
                    .parent()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| self.cwd.clone());
                let fresh = dir.join(format!("session-{}.jsonl", Uuid::now_v7()));
                (Arc::new(JsonlSessionStore::new(fresh.clone())), Some(fresh))
            } else {
                (Arc::new(MemorySessionStore::new()), None)
            };

        let tools = self
            .resident_tools
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let compaction = self
            .compaction_config
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        let (session_manager, agent, instructions) = build_session_and_agent(
            Arc::clone(&self.provider),
            self.api_key_resolver.clone(),
            store,
            &self.cwd,
            &self.base_system_prompt,
            &tools,
            self.js_runtime.as_ref(),
            &self.skill_discovery,
            &self.instruction_discovery,
            compaction,
            current_model,
            false,
            &[],
            None,
            None,
            None,
            current_thinking,
            current_fast,
            steer_mode,
            follow_mode,
            parent_session,
        )?;

        let session_id = session_manager.session_id().to_string();
        self.deferred_registry.clear_seen_schemas();
        self.queued_steering
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.queued_follow_up
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();

        {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            guard.session = Arc::new(Mutex::new(session_manager));
            guard.agent = Arc::new(agent);
            guard.instructions = instructions;
            guard.session_file = new_file.clone();
        }

        Ok(json!({
            "cancelled": false,
            "session_id": session_id,
            "session_file": new_file.map(|p| p.display().to_string()),
        }))
    }

    fn handle_switch_session(&self, session_path: String) -> Result<serde_json::Value, String> {
        let path = PathBuf::from(&session_path);
        let store: Arc<dyn SessionStore> = Arc::new(JsonlSessionStore::new(path.clone()));

        let (current_model, current_thinking, current_fast, steer_mode, follow_mode) = {
            let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (
                guard.agent.model(),
                guard.agent.thinking_level(),
                guard.agent.fast(),
                guard.agent.steering_mode(),
                guard.agent.follow_up_mode(),
            )
        };

        let tools = self
            .resident_tools
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let compaction = self
            .compaction_config
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        let known_models = self
            .models
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        let (session_manager, agent, instructions) = build_session_and_agent(
            Arc::clone(&self.provider),
            self.api_key_resolver.clone(),
            store,
            &self.cwd,
            &self.base_system_prompt,
            &tools,
            self.js_runtime.as_ref(),
            &self.skill_discovery,
            &self.instruction_discovery,
            compaction,
            current_model,
            true,
            &known_models,
            None,
            None,
            None,
            current_thinking,
            current_fast,
            steer_mode,
            follow_mode,
            None,
        )?;

        let restored_model = agent.model();
        {
            let mut models = self.models.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(slot) = models
                .iter_mut()
                .find(|m| m.provider == restored_model.provider && m.id == restored_model.id)
            {
                *slot = restored_model;
            } else {
                models.push(restored_model);
            }
        }

        let session_id = session_manager.session_id().to_string();
        let leaf_id = session_manager.leaf_id().map(ToString::to_string);

        self.deferred_registry.clear_seen_schemas();
        self.queued_steering
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.queued_follow_up
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();

        {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            guard.session = Arc::new(Mutex::new(session_manager));
            guard.agent = Arc::new(agent);
            guard.instructions = instructions;
            guard.session_file = Some(path.clone());
        }

        Ok(json!({
            "cancelled": false,
            "session_id": session_id,
            "session_file": path.display().to_string(),
            "leaf_id": leaf_id,
        }))
    }

    fn handle_fork(&self, entry_id: &str) -> Result<serde_json::Value, String> {
        let (from_leaf_id, reconstructed, agent) = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let SessionRuntimeState {
                session,
                agent,
                instructions,
                ..
            } = &mut *state;
            let mut sess = session.lock().unwrap_or_else(|e| e.into_inner());
            let from = sess.leaf_id().map(ToString::to_string);
            sess.fork(entry_id).map_err(|e| e.to_string())?;
            let recon = sess.reconstruct_context();
            instructions.sync_branch(&mut sess, &recon);
            (from, recon, Arc::clone(agent))
        };
        self.deferred_registry.clear_seen_schemas();
        agent
            .replace_messages(reconstructed.messages)
            .map_err(|e| e.to_string())?;
        self.writer.send_json(&SessionEvent::Forked {
            from_leaf_id,
            to_leaf_id: entry_id.to_string(),
        });
        Ok(json!({
            "cancelled": false,
            "leaf_id": entry_id,
        }))
    }

    fn snapshot_state(&self, is_streaming: bool, is_compacting: bool) -> RpcStateData {
        let (session, agent, session_file) = {
            let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (
                Arc::clone(&guard.session),
                Arc::clone(&guard.agent),
                guard.session_file.clone(),
            )
        };
        let (session_id, leaf_id, auto_compaction_enabled) = {
            let sess = session.lock().unwrap_or_else(|e| e.into_inner());
            (
                sess.session_id().to_string(),
                sess.leaf_id().map(ToString::to_string),
                sess.compaction_config().enabled,
            )
        };
        let pending_message_count = self
            .queued_steering
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
            + self
                .queued_follow_up
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .len();
        let tools = agent
            .tool_definitions()
            .into_iter()
            .map(|t| t.name)
            .collect();
        let extensions = self
            .js_runtime
            .as_ref()
            .map(|rt| rt.loaded_extensions())
            .unwrap_or_default();

        RpcStateData {
            model: agent.model(),
            thinking_level: agent.thinking_level(),
            fast: agent.fast(),
            is_streaming,
            is_compacting,
            steering_mode: agent.steering_mode(),
            follow_up_mode: agent.follow_up_mode(),
            session_file: session_file.map(|p| p.display().to_string()),
            session_id,
            leaf_id,
            cwd: self.cwd.display().to_string(),
            auto_compaction_enabled,
            message_count: agent.messages().len(),
            pending_message_count,
            tools,
            extensions,
        }
    }

    fn send_result(
        &self,
        id: Option<String>,
        command: &str,
        res: Result<serde_json::Value, String>,
    ) {
        match res {
            Ok(data) => self
                .writer
                .send_json(&RpcResponse::ok(id, command, Some(data))),
            Err(err) => self.writer.send_json(&RpcResponse::error(id, command, err)),
        }
    }

    fn send_result_opt(
        &self,
        id: Option<String>,
        command: &str,
        res: Result<Option<serde_json::Value>, String>,
    ) {
        match res {
            Ok(data) => self.writer.send_json(&RpcResponse::ok(id, command, data)),
            Err(err) => self.writer.send_json(&RpcResponse::error(id, command, err)),
        }
    }

    /// Run a single one-shot prompt (used by `pikra ask` / `--mode text` / `--mode json`).
    pub async fn run_oneshot(&self, prompt: String, mode: CliMode) -> Result<(), String> {
        let session = self.current_session();
        let agent = self.current_agent();
        let user_msg = AgentMessage::Llm(Message::User(UserMessage::text(prompt, now_ms())));

        let (prompt_messages, flushed_count, flushed_events) = {
            let mut sess = session.lock().unwrap_or_else(|e| e.into_inner());
            let flushed = sess
                .flush_pending_context()
                .map_err(|e| format!("failed to flush pending context: {e}"))?;
            prepare_flushed_prompt(flushed, user_msg)
        };

        if mode == CliMode::Json {
            for ev in flushed_events {
                self.writer.send_json(&ev);
            }
        }

        let mut stream = agent
            .prompt_messages_stream(prompt_messages)
            .map_err(|e| e.to_string())?;

        let mut seen_ends = 0usize;
        let mut printed_any_text = false;
        let mut saw_abort = false;
        let mut last_stop_reason: Option<StopReason> = None;
        let mut last_error_message: Option<String> = None;
        let persistence_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        while let Some(mut ev) = stream.next().await {
            match &mut ev {
                AgentEvent::MessageEnd { message } => {
                    if let AgentMessage::Llm(Message::Assistant(a)) = message {
                        if a.stop_reason == StopReason::Aborted {
                            saw_abort = true;
                        }
                        last_stop_reason = Some(a.stop_reason);
                        if a.error_message.is_some() {
                            last_error_message = a.error_message.clone();
                        }
                    }
                    if seen_ends >= flushed_count {
                        let persist_res = {
                            let mut sess = session.lock().unwrap_or_else(|e| e.into_inner());
                            sess.append_message(message.clone())
                        };
                        if let Err(err) = persist_res {
                            let mut slot =
                                persistence_error.lock().unwrap_or_else(|e| e.into_inner());
                            if slot.is_none() {
                                *slot = Some(format!("failed to persist session message: {err}"));
                            }
                            drop(slot);
                            agent.abort_now();
                        }
                    }
                    if let Some(err_msg) = persistence_error
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone()
                        && let AgentMessage::Llm(Message::Assistant(a)) = message
                    {
                        a.stop_reason = StopReason::Error;
                        a.error_message = Some(err_msg);
                    }
                    seen_ends += 1;
                }
                AgentEvent::TurnEnd { message, .. } => {
                    if let Some(err_msg) = persistence_error
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone()
                    {
                        message.stop_reason = StopReason::Error;
                        message.error_message = Some(err_msg);
                    }
                    last_stop_reason = Some(message.stop_reason);
                    last_error_message = message.error_message.clone();
                }
                _ => {}
            }

            match mode {
                CliMode::Json => {
                    self.writer.send_json(&ev);
                    flush_js_errors(self.js_runtime.as_ref(), &self.writer);
                }
                CliMode::Text => {
                    if let AgentEvent::MessageUpdate {
                        assistant_message_event: AssistantMessageEvent::TextDelta { delta, .. },
                        ..
                    } = &ev
                    {
                        use std::io::Write;
                        let mut stdout = std::io::stdout().lock();
                        let _ = stdout.write_all(delta.as_bytes());
                        let _ = stdout.flush();
                        printed_any_text = true;
                    }
                    if let Some(rt) = &self.js_runtime {
                        for err in rt.take_errors() {
                            eprintln!("[pikra] extension error: {err}");
                        }
                    }
                }
                CliMode::Rpc | CliMode::Acp => {}
            }
        }

        drop(stream);

        let persist_err = persistence_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        if mode == CliMode::Text && printed_any_text {
            use std::io::Write;
            let mut stdout = std::io::stdout().lock();
            let _ = stdout.write_all(b"\n");
            let _ = stdout.flush();
        } else if mode == CliMode::Json {
            let settled_aborted = if persist_err.is_some() {
                false
            } else {
                saw_abort
            };
            self.writer.send_json(&AgentSettledEvent::with_error(
                settled_aborted,
                persist_err.clone(),
            ));
        }

        if let Some(err_msg) = persist_err {
            return Err(err_msg);
        }
        if last_stop_reason == Some(StopReason::Error) {
            return Err(last_error_message.unwrap_or_else(|| "Model request failed".to_string()));
        }
        if saw_abort || last_stop_reason == Some(StopReason::Aborted) {
            return Err(last_error_message.unwrap_or_else(|| "Operation aborted".to_string()));
        }

        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn build_session_and_agent(
    provider: Arc<dyn StreamProvider>,
    api_key_resolver: ApiKeyResolver,
    store: Arc<dyn SessionStore>,
    cwd: &std::path::Path,
    base_system_prompt: &str,
    resident_tools: &[Arc<dyn AgentTool>],
    js_runtime: Option<&Arc<JsExtensionRuntime>>,
    skill_discovery: &SkillDiscoveryConfig,
    instruction_discovery: &InstructionDiscoveryConfig,
    compaction: CompactionConfig,
    fallback_model: Model,
    should_restore_model: bool,
    known_models: &[Model],
    cli_api_override: Option<ApiKind>,
    cli_base_url_override: Option<String>,
    cli_context_window_override: Option<u32>,
    thinking_level: ThinkingLevel,
    fast: bool,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
    parent_session: Option<String>,
) -> Result<(SessionManager, Agent, InstructionContextState), String> {
    let resident_defs: Vec<ToolDefinition> =
        resident_tools.iter().map(|t| t.definition()).collect();
    let mut session_cfg = SessionConfig::new(base_system_prompt, resident_defs)
        .with_cwd(cwd.display().to_string())
        .with_compaction(compaction);
    session_cfg.parent_session = parent_session;

    let mut session_manager = SessionManager::open(store, session_cfg)
        .map_err(|err| format!("failed to open session: {err}"))?;
    let reconstructed = session_manager.reconstruct_context();
    let instructions = InstructionContextState::new(
        Some(instruction_discovery.clone()),
        Some(cwd),
        &mut session_manager,
        &reconstructed,
    );
    let session_id = session_manager.session_id().to_string();

    let effective_model = if should_restore_model {
        match restore_persisted_model(&session_manager, known_models) {
            Some(mut restored) => {
                if let Some(api) = cli_api_override {
                    restored.api = api;
                }
                if let Some(url) = cli_base_url_override {
                    restored.base_url = url;
                }
                if let Some(cw) = cli_context_window_override {
                    restored.context_window = cw;
                }
                restored
            }
            None => fallback_model,
        }
    } else {
        fallback_model
    };

    let effective_thinking = reconstructed
        .thinking
        .map(|(lvl, _)| lvl)
        .unwrap_or(thinking_level);
    let effective_fast = reconstructed.thinking.map(|(_, f)| f).unwrap_or(fast);

    let skills = SkillCatalog::discover_with_config(skill_discovery);
    if let Some(skills_xml) = skills.format_for_context() {
        session_manager.set_context(SKILLS_CONTEXT_NAME, skills_xml);
    }

    let mut middlewares: Vec<Arc<dyn AgentMiddleware>> = Vec::new();
    if let Some(rt) = js_runtime {
        middlewares.push(Arc::clone(rt) as Arc<dyn AgentMiddleware>);
    }

    let mut agent_cfg = AgentConfig::new(effective_model)
        .with_system_prompt(reconstructed.system_prompt)
        .with_tools(resident_tools.to_vec())
        .with_thinking_level(effective_thinking)
        .with_fast(effective_fast)
        .with_tool_execution_mode(ToolExecutionMode::Parallel)
        .with_steering_mode(steering_mode)
        .with_follow_up_mode(follow_up_mode);
    agent_cfg.middlewares = middlewares;
    agent_cfg.get_api_key = Some(api_key_resolver);
    agent_cfg.cache_retention = CacheRetention::default();
    agent_cfg.session_id = Some(session_id);
    agent_cfg.initial_messages = reconstructed.messages;

    let agent = Agent::new(provider, agent_cfg);
    Ok((session_manager, agent, instructions))
}

fn flush_js_errors(js_runtime: Option<&Arc<JsExtensionRuntime>>, writer: &JsonlWriter) {
    if let Some(rt) = js_runtime {
        for err in rt.take_errors() {
            writer.send_json(&ExtensionErrorEvent::new(err.to_string()));
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_compaction_task(
    provider: &Arc<dyn StreamProvider>,
    api_key_resolver: &ApiKeyResolver,
    session: &Arc<Mutex<SessionManager>>,
    agent: &Arc<Agent>,
    deferred_registry: &Arc<DeferredToolRegistry>,
    is_compacting: &Arc<AtomicBool>,
    compaction_cancel: &Arc<Mutex<Option<CancelToken>>>,
    cancel: CancelToken,
    writer: &JsonlWriter,
    auto_only: bool,
    custom_instructions: Option<String>,
) -> Result<Option<CompactionResult>, String> {
    if cancel.is_cancelled() {
        *compaction_cancel.lock().unwrap_or_else(|e| e.into_inner()) = None;
        is_compacting.store(false, Ordering::Release);
        return Err("Compaction aborted".to_string());
    }

    let model = agent.model();
    let api_key = api_key_resolver(&model.provider).await;
    if cancel.is_cancelled() {
        *compaction_cancel.lock().unwrap_or_else(|e| e.into_inner()) = None;
        is_compacting.store(false, Ordering::Release);
        return Err("Compaction aborted".to_string());
    }

    let session_id = session
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .session_id()
        .to_string();

    let options = SimpleStreamOptions {
        api_key,
        reasoning: Some(agent.thinking_level()),
        fast: Some(agent.fast()),
        temperature: None,
        max_tokens: None,
        cache_retention: CacheRetention::default(),
        session_id: Some(session_id),
        headers: Vec::new(),
        sampling_params: Map::new(),
        cancel: cancel.clone(),
    };

    let mut prepared = {
        let sess = session.lock().unwrap_or_else(|e| e.into_inner());
        if auto_only && !sess.needs_compaction(&model) {
            *compaction_cancel.lock().unwrap_or_else(|e| e.into_inner()) = None;
            is_compacting.store(false, Ordering::Release);
            return Ok(None);
        }
        let Some(prep) = sess.prepare_compaction(&model, options) else {
            *compaction_cancel.lock().unwrap_or_else(|e| e.into_inner()) = None;
            is_compacting.store(false, Ordering::Release);
            return Ok(None);
        };
        sess.notify_compaction_start(prep.input.tokens_before);
        prep
    };

    if let Some(extra) = custom_instructions {
        prepared.input.config.custom_instructions = Some(extra);
    }

    let tokens_before = prepared.input.tokens_before;
    let expected_leaf = prepared.expected_leaf_id.clone();
    is_compacting.store(true, Ordering::Release);
    *compaction_cancel.lock().unwrap_or_else(|e| e.into_inner()) = Some(cancel.clone());

    writer.send_json(&SessionEvent::CompactionStart { tokens_before });

    let exec_result = SessionManager::execute_prepared_compaction(&**provider, prepared).await;

    *compaction_cancel.lock().unwrap_or_else(|e| e.into_inner()) = None;
    is_compacting.store(false, Ordering::Release);

    if cancel.is_cancelled() {
        return Err("Compaction aborted".to_string());
    }

    let output = exec_result.map_err(|e| e.to_string())?;

    let (maybe_result, reconstructed) = {
        let mut sess = session.lock().unwrap_or_else(|e| e.into_inner());
        let res = sess
            .commit_compaction_if_leaf_matches(output, tokens_before, expected_leaf.as_deref())
            .map_err(|e| e.to_string())?;
        let recon = sess.reconstruct_context();
        (res, recon)
    };

    let Some(result) = maybe_result else {
        return Ok(None);
    };

    deferred_registry.clear_seen_schemas();
    agent
        .replace_messages(reconstructed.messages)
        .map_err(|e| e.to_string())?;

    writer.send_json(&SessionEvent::CompactionEnd {
        summary: result.summary.clone(),
        tokens_before: result.tokens_before,
        first_kept_entry_id: result.first_kept_entry_id.clone(),
        usage: result.usage.clone(),
    });

    Ok(Some(result))
}
