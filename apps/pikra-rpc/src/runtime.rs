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
use pikra_kit::{CodemodeExecTool, DeferredToolRegistry, SkillCatalog, CODEMODE_EXEC_TOOL_NAME};
use pikra_session::{
    context_block_to_agent_message, CompactionConfig, CompactionResult, JsonlSessionStore,
    MemorySessionStore, SessionConfig, SessionEntryPayload, SessionEvent, SessionManager,
    SessionStore,
};
use pikra_tools::{WorkspaceTools, WorkspaceToolsConfig};
use serde_json::{json, Map};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use tokio::task::JoinHandle;
use uuid::Uuid;

const SKILLS_CONTEXT_NAME: &str = "skills";
const DEFAULT_CONTEXT_WINDOW: u32 = 200_000;
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 16_384;

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
            "claude-sonnet-4-20250514",
        ),
        "google" | "gemini" => (
            ApiKind::GoogleGenerativeAi,
            "https://generativelanguage.googleapis.com/v1beta",
            "gemini-2.5-pro",
        ),
        "deepseek" => (
            ApiKind::OpenaiCompletions,
            "https://api.deepseek.com/v1",
            "deepseek-chat",
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

struct SessionRuntimeState {
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
    skill_roots: Vec<PathBuf>,
    no_session: bool,
    compaction_config: Mutex<CompactionConfig>,
    state: Mutex<SessionRuntimeState>,
    queued_steering: Arc<Mutex<VecDeque<(String, String)>>>,
    queued_follow_up: Arc<Mutex<VecDeque<(String, String)>>>,
    active_run: AsyncMutex<Option<ActiveRunTask>>,
    is_compacting: Arc<AtomicBool>,
    compaction_cancel: Arc<Mutex<Option<CancelToken>>>,
    writer: JsonlWriter,
}

impl RpcRuntime {
    pub fn new(args: &CliArgs, writer: JsonlWriter) -> Result<Self, String> {
        let transport = ReqwestTransport::new()
            .map_err(|err| format!("failed to initialize HTTP transport: {err}"))?;
        let provider: Arc<dyn StreamProvider> = Arc::new(Client::new(Arc::new(transport)));

        let explicit_env = args.api_key_env.clone();
        let api_key_resolver: ApiKeyResolver = Arc::new(move |provider_id: &str| {
            let provider_owned = provider_id.to_string();
            let env_owned = explicit_env.clone();
            Box::pin(async move {
                resolve_api_key_for_provider(&provider_owned, env_owned.as_deref())
            })
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

        if !args.no_tools {
            resident.extend(WorkspaceTools::build(WorkspaceToolsConfig::new(&args.cwd)));
        }

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

        let resident_tools_handle = Arc::new(Mutex::new(resident.clone()));
        if args.enable_codemode
            && !resident
                .iter()
                .any(|t| t.definition().name == CODEMODE_EXEC_TOOL_NAME)
        {
            let exec_tool: Arc<dyn AgentTool> = Arc::new(CodemodeExecTool::new(
                Arc::clone(&resident_tools_handle),
                Arc::clone(&deferred_registry),
                CodemodeSandboxOptions::default(),
            ));
            resident.push(exec_tool);
            *resident_tools_handle
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = resident.clone();
        }

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

        let (session_manager, agent) = build_session_and_agent(
            Arc::clone(&provider),
            api_key_resolver.clone(),
            store,
            &args.cwd,
            &base_system_prompt,
            &resident,
            js_runtime.as_ref(),
            &args.skill_dirs,
            compaction.clone(),
            initial_model.clone(),
            args.thinking,
            args.fast,
            QueueMode::OneAtATime,
            QueueMode::OneAtATime,
            None,
        )?;

        Ok(Self {
            provider,
            api_key_resolver,
            models: Mutex::new(vec![initial_model]),
            cwd: args.cwd.clone(),
            base_system_prompt,
            resident_tools: resident_tools_handle,
            deferred_registry,
            js_runtime,
            skill_roots: args.skill_dirs.clone(),
            no_session: args.no_session,
            compaction_config: Mutex::new(compaction),
            state: Mutex::new(SessionRuntimeState {
                session: Arc::new(Mutex::new(session_manager)),
                agent: Arc::new(agent),
                session_file,
            }),
            queued_steering: Arc::new(Mutex::new(VecDeque::new())),
            queued_follow_up: Arc::new(Mutex::new(VecDeque::new())),
            active_run: AsyncMutex::new(None),
            is_compacting: Arc::new(AtomicBool::new(false)),
            compaction_cancel: Arc::new(Mutex::new(None)),
            writer,
        })
    }

    fn current_agent(&self) -> Arc<Agent> {
        Arc::clone(&self.state.lock().unwrap_or_else(|e| e.into_inner()).agent)
    }

    fn current_session(&self) -> Arc<Mutex<SessionManager>> {
        Arc::clone(
            &self
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .session,
        )
    }

    async fn reap_finished_run_locked(&self, guard: &mut Option<ActiveRunTask>) {
        if let Some(task) = guard.as_ref()
            && !task.running.load(Ordering::Acquire)
            && let Some(finished) = guard.take()
        {
            let _ = finished.handle.await;
        }
    }

    async fn is_busy(&self) -> bool {
        let mut guard = self.active_run.lock().await;
        self.reap_finished_run_locked(&mut guard).await;
        guard.is_some() || self.is_compacting.load(Ordering::Acquire)
    }

    pub async fn handle_command(&self, id: Option<String>, cmd: RpcCommand) {
        let command_name = cmd.command_name();
        match cmd {
            RpcCommand::Prompt {
                message,
                images,
                streaming_behavior,
            } => {
                self.handle_prompt(id, message, images, streaming_behavior)
                    .await;
            }
            RpcCommand::Steer { message, images } => {
                let agent = self.current_agent();
                let queue_id = Uuid::now_v7().to_string();
                let parts = build_content_parts(message.clone(), images);
                self.queued_steering
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back((queue_id.clone(), message));
                agent.steer_with_parts(queue_id, parts);
                self.writer.send_json(&RpcResponse::ok(
                    id,
                    command_name,
                    Some(json!({ "disposition": "queued" })),
                ));
            }
            RpcCommand::FollowUp { message, images } => {
                let agent = self.current_agent();
                let queue_id = Uuid::now_v7().to_string();
                let parts = build_content_parts(message.clone(), images);
                self.queued_follow_up
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back((queue_id.clone(), message));
                agent.follow_up_with_parts(queue_id, parts);
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
                let is_streaming = {
                    let mut guard = self.active_run.lock().await;
                    self.reap_finished_run_locked(&mut guard).await;
                    guard.is_some()
                };
                let data = self.snapshot_state(is_streaming);
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
            RpcCommand::Compact {
                custom_instructions,
            } => {
                if self.is_busy().await {
                    self.writer.send_json(&RpcResponse::error(
                        id,
                        command_name,
                        "Cannot compact while session is busy",
                    ));
                    return;
                }
                let res = self.handle_manual_compact(custom_instructions).await;
                self.send_result(id, command_name, res);
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
            RpcCommand::GetMessages => {
                let messages = self.current_agent().messages();
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
                    Some(rt) => rt.reload().map(|()| {
                        json!({ "extensions": rt.loaded_extensions() })
                    }).map_err(|e| e.to_string()),
                    None => Ok(json!({ "extensions": Vec::<String>::new() })),
                };
                self.send_result(id, command_name, res);
            }
        }
    }

    async fn handle_prompt(
        &self,
        id: Option<String>,
        message: String,
        images: Option<Vec<RpcImageInput>>,
        streaming_behavior: Option<StreamingBehavior>,
    ) {
        let mut active_guard = self.active_run.lock().await;
        self.reap_finished_run_locked(&mut active_guard).await;

        if active_guard.is_some() {
            let agent = self.current_agent();
            match streaming_behavior {
                Some(StreamingBehavior::Steer) => {
                    let queue_id = Uuid::now_v7().to_string();
                    let parts = build_content_parts(message.clone(), images);
                    self.queued_steering
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push_back((queue_id.clone(), message));
                    agent.steer_with_parts(queue_id, parts);
                    self.writer.send_json(&RpcResponse::ok(
                        id,
                        "prompt",
                        Some(json!({ "disposition": "queued" })),
                    ));
                }
                Some(StreamingBehavior::FollowUp) => {
                    let queue_id = Uuid::now_v7().to_string();
                    let parts = build_content_parts(message.clone(), images);
                    self.queued_follow_up
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push_back((queue_id.clone(), message));
                    agent.follow_up_with_parts(queue_id, parts);
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

        if self.is_compacting.load(Ordering::Acquire) {
            self.writer.send_json(&RpcResponse::error(
                id,
                "prompt",
                "Session is currently compacting",
            ));
            return;
        }

        let content = build_content_parts(message, images);
        let user_msg = AgentMessage::Llm(Message::User(UserMessage {
            id: None,
            content,
            timestamp: now_ms(),
        }));

        let session = self.current_session();
        let agent = self.current_agent();

        let (prompt_messages, flushed_count, flushed_events) = {
            let mut sess = session.lock().unwrap_or_else(|e| e.into_inner());
            let flushed = match sess.flush_pending_context() {
                Ok(f) => f,
                Err(err) => {
                    self.writer.send_json(&RpcResponse::error(
                        id,
                        "prompt",
                        format!("Failed to flush pending context: {err}"),
                    ));
                    return;
                }
            };
            let mut msgs = Vec::with_capacity(flushed.len() + 1);
            let mut events = Vec::with_capacity(flushed.len());
            for entry in &flushed {
                if let SessionEntryPayload::ContextBlock { name, text } = &entry.payload {
                    msgs.push(context_block_to_agent_message(name, text, entry.timestamp));
                    events.push(SessionEvent::ContextAppended {
                        name: name.clone(),
                        text: text.clone(),
                    });
                }
            }
            let count = msgs.len();
            msgs.push(user_msg);
            (msgs, count, events)
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
            let mut next_stream = Some((initial_stream, flushed_count));

            while let Some((mut stream, skip_ends)) = next_stream.take() {
                let mut seen_ends = 0usize;
                while let Some(ev) = stream.next().await {
                    match &ev {
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
                                let mut sess =
                                    task_session.lock().unwrap_or_else(|e| e.into_inner());
                                let _ = sess.append_message(message.clone());
                            }
                            seen_ends += 1;
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

            let final_aborted = saw_abort || task_aborted.load(Ordering::Acquire);
            task_running.store(false, Ordering::Release);
            task_writer.send_json(&AgentSettledEvent::new(final_aborted));
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

    pub async fn abort_active(&self) {
        let mut active_guard = self.active_run.lock().await;
        if let Some(cancel) = self
            .compaction_cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            cancel.cancel();
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
        self.current_session()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .append_model_change(resolved.provider.clone(), resolved.id.clone())
            .map_err(|e| e.to_string())?;

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

    async fn handle_manual_compact(
        &self,
        custom_instructions: Option<String>,
    ) -> Result<serde_json::Value, String> {
        let session = self.current_session();
        let agent = self.current_agent();
        let result = run_compaction_task(
            &self.provider,
            &self.api_key_resolver,
            &session,
            &agent,
            &self.deferred_registry,
            &self.is_compacting,
            &self.compaction_cancel,
            &self.writer,
            false,
            custom_instructions,
        )
        .await?;
        let dto = CompactionResponseData::from(result);
        serde_json::to_value(dto).map_err(|e| e.to_string())
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

        let (session_manager, agent) = build_session_and_agent(
            Arc::clone(&self.provider),
            self.api_key_resolver.clone(),
            store,
            &self.cwd,
            &self.base_system_prompt,
            &tools,
            self.js_runtime.as_ref(),
            &self.skill_roots,
            compaction,
            current_model,
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

        let (session_manager, agent) = build_session_and_agent(
            Arc::clone(&self.provider),
            self.api_key_resolver.clone(),
            store,
            &self.cwd,
            &self.base_system_prompt,
            &tools,
            self.js_runtime.as_ref(),
            &self.skill_roots,
            compaction,
            current_model,
            current_thinking,
            current_fast,
            steer_mode,
            follow_mode,
            None,
        )?;

        // If the reopened session recorded a ModelChange, restore that model from catalog if known.
        let reconstructed = session_manager.reconstruct_context();
        if let Some((rec_provider, rec_model_id)) = reconstructed.model {
            let maybe_known = self
                .models
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .find(|m| m.provider == rec_provider && m.id == rec_model_id)
                .cloned();
            if let Some(m) = maybe_known {
                let _ = agent.set_model(m);
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
        let session = self.current_session();
        let agent = self.current_agent();
        let (from_leaf_id, reconstructed) = {
            let mut sess = session.lock().unwrap_or_else(|e| e.into_inner());
            let from = sess.leaf_id().map(ToString::to_string);
            sess.fork(entry_id).map_err(|e| e.to_string())?;
            let recon = sess.reconstruct_context();
            (from, recon)
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

    fn snapshot_state(&self, is_streaming: bool) -> RpcStateData {
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
            is_compacting: self.is_compacting.load(Ordering::Acquire),
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
            let mut msgs = Vec::with_capacity(flushed.len() + 1);
            let mut events = Vec::with_capacity(flushed.len());
            for entry in &flushed {
                if let SessionEntryPayload::ContextBlock { name, text } = &entry.payload {
                    msgs.push(context_block_to_agent_message(name, text, entry.timestamp));
                    events.push(SessionEvent::ContextAppended {
                        name: name.clone(),
                        text: text.clone(),
                    });
                }
            }
            let count = msgs.len();
            msgs.push(user_msg);
            (msgs, count, events)
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

        while let Some(ev) = stream.next().await {
            if let AgentEvent::MessageEnd { message } = &ev {
                if let AgentMessage::Llm(Message::Assistant(a)) = message
                    && a.stop_reason == StopReason::Aborted
                {
                    saw_abort = true;
                }
                if seen_ends >= flushed_count {
                    let mut sess = session.lock().unwrap_or_else(|e| e.into_inner());
                    let _ = sess.append_message(message.clone());
                }
                seen_ends += 1;
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
                CliMode::Rpc => {}
            }
        }

        drop(stream);

        if mode == CliMode::Text && printed_any_text {
            use std::io::Write;
            let mut stdout = std::io::stdout().lock();
            let _ = stdout.write_all(b"\n");
            let _ = stdout.flush();
        } else if mode == CliMode::Json {
            self.writer.send_json(&AgentSettledEvent::new(saw_abort));
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
    skill_roots: &[PathBuf],
    compaction: CompactionConfig,
    model: Model,
    thinking_level: ThinkingLevel,
    fast: bool,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
    parent_session: Option<String>,
) -> Result<(SessionManager, Agent), String> {
    let resident_defs: Vec<ToolDefinition> =
        resident_tools.iter().map(|t| t.definition()).collect();
    let mut session_cfg = SessionConfig::new(base_system_prompt, resident_defs)
        .with_cwd(cwd.display().to_string())
        .with_compaction(compaction);
    session_cfg.parent_session = parent_session;

    let mut session_manager = SessionManager::open(store, session_cfg)
        .map_err(|err| format!("failed to open session: {err}"))?;
    let reconstructed = session_manager.reconstruct_context();
    let session_id = session_manager.session_id().to_string();

    let effective_thinking = reconstructed
        .thinking
        .map(|(lvl, _)| lvl)
        .unwrap_or(thinking_level);
    let effective_fast = reconstructed.thinking.map(|(_, f)| f).unwrap_or(fast);

    let skills = SkillCatalog::discover(skill_roots);
    if let Some(skills_xml) = skills.format_for_context() {
        session_manager.set_context(SKILLS_CONTEXT_NAME, skills_xml);
    }

    let mut middlewares: Vec<Arc<dyn AgentMiddleware>> = Vec::new();
    if let Some(rt) = js_runtime {
        middlewares.push(Arc::clone(rt) as Arc<dyn AgentMiddleware>);
    }

    let mut agent_cfg = AgentConfig::new(model)
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
    Ok((session_manager, agent))
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
    writer: &JsonlWriter,
    auto_only: bool,
    custom_instructions: Option<String>,
) -> Result<Option<CompactionResult>, String> {
    let model = agent.model();
    let cancel = CancelToken::new();
    let api_key = api_key_resolver(&model.provider).await;
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
            return Ok(None);
        }
        let Some(prep) = sess.prepare_compaction(&model, options) else {
            return Ok(None);
        };
        sess.notify_compaction_start(prep.input.tokens_before);
        prep
    };

    if let Some(extra) = custom_instructions {
        prepared.input.config.custom_instructions = Some(extra);
    }

    let tokens_before = prepared.input.tokens_before;
    is_compacting.store(true, Ordering::Release);
    *compaction_cancel
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(cancel);

    writer.send_json(&SessionEvent::CompactionStart { tokens_before });

    let exec_result = SessionManager::execute_prepared_compaction(&**provider, prepared).await;

    *compaction_cancel
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
    is_compacting.store(false, Ordering::Release);

    let output = exec_result.map_err(|e| e.to_string())?;

    let (result, reconstructed) = {
        let mut sess = session.lock().unwrap_or_else(|e| e.into_inner());
        let res = sess
            .commit_compaction(output, tokens_before)
            .map_err(|e| e.to_string())?;
        let recon = sess.reconstruct_context();
        (res, recon)
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
