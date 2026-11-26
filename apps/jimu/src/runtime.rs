use crate::args::{CliArgs, CliMode};
use crate::framing::JsonlWriter;
use crate::protocol::{
    AgentSettledEvent, CompactionResponseData, ExtensionErrorEvent, RpcCommand, RpcImageInput,
    RpcResponse, RpcStateData, StreamingBehavior,
};
use futures_util::StreamExt;
use jimu::host::{
    BuiltWorkspaceTools, HostConfig, SessionFileLock, apply_session_model_on_open,
    attach_extensions_to_config, build_auth_provider, build_http_transport,
    build_instruction_discovery, build_skill_discovery, build_workspace_tools,
    canonical_jsonl_path, has_existing_session_content, load_js_extensions,
    make_background_callback, paths_same_dir, reserved_extension_tool_names,
    resolve_existing_session_cwd, resolve_model_metadata, resolve_system_prompt, set_model_atomic,
    should_enable_deferred_gateway, validate_base_url,
};
use jimu::mcp::{McpHub, McpServerEntry, has_cwd_dependent_stdio};
use jimu::providers::ProviderStore;
use jimu_agent::{AgentEvent, AgentMessage, QueueMode};
use jimu_ai::{
    ApiKind, AssistantMessageEvent, CancelToken, ContentPart, HttpTransport, ImageContent, Message,
    Model, StopReason, StreamProvider, TextContent,
};
use jimu_js::JsExtensionRuntime;
use jimu_kit::{
    BackgroundEventSender, KitError, KitEvent, KitSession, KitSessionConfig, KitToolSpec,
};
use jimu_session::{
    CompactionConfig, JsonlSessionStore, MemorySessionStore, SessionEvent, SessionStore,
};
use serde_json::json;
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;
use uuid::Uuid;

fn find_active_branch_model(
    session: &KitSession,
    provider: &str,
    model_id: &str,
    store: &ProviderStore,
) -> Option<Model> {
    let mut matched = session
        .header()
        .initial_model
        .filter(|m| m.provider == provider && m.model_id == model_id);
    for entry in session.active_branch() {
        if let jimu_session::SessionEntryPayload::ModelChange {
            provider: p,
            model_id: id,
            api,
            base_url,
            context_window,
            max_output_tokens,
            supports_images,
            supports_reasoning,
        } = entry.payload
            && p == provider
            && id == model_id
        {
            matched = Some(jimu_session::SessionModelMetadata {
                provider: p,
                model_id: id,
                api,
                base_url,
                context_window,
                max_output_tokens,
                supports_images,
                supports_reasoning,
            });
        }
    }
    resolve_model_metadata(matched.as_ref(), store)
}

#[allow(clippy::too_many_arguments)]
fn build_model(
    store: &ProviderStore,
    provider: &str,
    model_id: &str,
    api_override: Option<ApiKind>,
    base_url_override: Option<String>,
    context_window: Option<u32>,
    max_output_tokens: Option<u32>,
    supports_images: Option<bool>,
    supports_reasoning: Option<bool>,
) -> Model {
    let mut model = store.model(provider, model_id);
    if let Some(api) = api_override {
        model.api = api;
    }
    if let Some(url) = base_url_override {
        model.base_url = url;
    }
    if let Some(cw) = context_window {
        model.context_window = cw;
    }
    if let Some(mo) = max_output_tokens {
        model.max_output_tokens = mo;
    }
    if let Some(si) = supports_images {
        model.supports_images = si;
    }
    if let Some(sr) = supports_reasoning {
        model.supports_reasoning = sr;
    }
    model
}

fn build_content_parts(message: String, images: Option<Vec<RpcImageInput>>) -> Vec<ContentPart> {
    let mut parts = Vec::new();
    if !message.is_empty() || images.as_ref().is_none_or(Vec::is_empty) {
        parts.push(ContentPart::Text(TextContent::plain(message)));
    }
    if let Some(imgs) = images {
        for img in imgs {
            parts.push(ContentPart::Image(ImageContent::from(img)));
        }
    }
    parts
}

fn flush_js_errors(js_runtime: Option<&Arc<JsExtensionRuntime>>, writer: &JsonlWriter) {
    if let Some(rt) = js_runtime {
        for err in rt.take_errors() {
            writer.send_json(&ExtensionErrorEvent::new(err.to_string()));
        }
    }
}

struct ActiveRunTask {
    running: Arc<AtomicBool>,
    aborted: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

struct ActiveCompactionTask {
    running: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

struct SessionRuntimeState {
    session: Arc<KitSession>,
    built_tools: Arc<BuiltWorkspaceTools>,
    mcp_hub: Arc<McpHub>,
    session_file: Option<PathBuf>,
    _file_lock: Option<SessionFileLock>,
    cwd: PathBuf,
}

pub struct RpcRuntime {
    host: HostConfig,
    transport: Arc<dyn HttpTransport>,
    provider_store: Arc<ProviderStore>,
    provider: Arc<dyn StreamProvider>,
    models: Mutex<Vec<Model>>,
    js_runtime: Option<Arc<JsExtensionRuntime>>,
    extension_tool_names: Mutex<Vec<String>>,
    mcp_entries: Vec<McpServerEntry>,
    no_session: bool,
    compaction_config: Mutex<CompactionConfig>,
    state: Mutex<SessionRuntimeState>,
    queued_steering: Arc<Mutex<VecDeque<(String, String)>>>,
    queued_follow_up: Arc<Mutex<VecDeque<(String, String)>>>,
    active_run: AsyncMutex<Option<ActiveRunTask>>,
    active_compaction: AsyncMutex<Option<ActiveCompactionTask>>,
    writer: JsonlWriter,
}

impl RpcRuntime {
    pub async fn new(
        host: HostConfig,
        args: &CliArgs,
        writer: JsonlWriter,
        startup_cancel: CancelToken,
    ) -> Result<Self, String> {
        let transport = build_http_transport(host.transport_override.clone())?;
        let provider_store = Arc::new(ProviderStore::new(host.provider_store_config()));
        for warning in provider_store.warnings() {
            eprintln!("warning: {warning}");
        }

        let provider = build_auth_provider(
            Arc::clone(&provider_store),
            Arc::clone(&transport),
            host.provider_override.clone(),
            true,
            true,
        );

        let (store, session_file, file_lock, session_cwd): (
            Arc<dyn SessionStore>,
            Option<PathBuf>,
            Option<SessionFileLock>,
            PathBuf,
        ) = if args.no_session {
            (
                Arc::new(MemorySessionStore::new()),
                None,
                None,
                host.cwd.clone(),
            )
        } else if let Some(path) = &args.session {
            let resolved_path = canonical_jsonl_path(path);
            let lock = SessionFileLock::try_acquire(&resolved_path)?;
            let resolved_cwd = resolve_existing_session_cwd(&resolved_path, &host.cwd)?;
            (
                Arc::new(JsonlSessionStore::new(resolved_path.clone())),
                Some(resolved_path),
                Some(lock),
                resolved_cwd,
            )
        } else {
            (
                Arc::new(MemorySessionStore::new()),
                None,
                None,
                host.cwd.clone(),
            )
        };

        let (mcp_entries, mcp_settings_warnings) = host.resolve_mcp_entries(&[])?;
        for warning in mcp_settings_warnings {
            eprintln!("warning: {warning}");
        }
        let (mcp_hub, mcp_warnings) = McpHub::connect(
            mcp_entries.clone(),
            &session_cwd,
            Arc::clone(&transport),
            startup_cancel.clone(),
        )
        .await
        .map_err(|err| err.to_string())?;
        for warning in mcp_warnings {
            eprintln!("warning: {warning}");
        }
        let mcp_hub = Arc::new(mcp_hub);

        // Load QuickJS extensions once so hooks and reload_extensions share one runtime.
        let extension_paths = host.resolve_extension_paths(&session_cwd);
        let loaded_extensions = match load_js_extensions(&extension_paths, &HashSet::new()) {
            Ok(ext) => ext,
            Err(err) => {
                mcp_hub.shutdown().await;
                return Err(err);
            }
        };
        let extension_tool_names: Vec<String> = loaded_extensions
            .as_ref()
            .map(|ext| ext.tools.iter().map(|t| t.definition().name).collect())
            .unwrap_or_default();
        let js_runtime = loaded_extensions.map(|ext| ext.runtime);

        let initial_model = provider_store
            .default_model()
            .unwrap_or_else(|| provider_store.unconfigured_placeholder_model());
        let mut known_models = provider_store.models();

        let mut compaction = CompactionConfig::default();
        if let Some(reserve) = host.compact_tokens {
            compaction.compaction_reserve_tokens = Some(reserve);
        }

        let explicit_model_override = host.provider.is_some() || host.model.is_some();
        let (session, built_tools) = match build_kit_session(
            &host,
            &session_cwd,
            &transport,
            &provider_store,
            Arc::clone(&provider),
            js_runtime.as_ref(),
            &mcp_hub,
            store,
            compaction.clone(),
            initial_model,
            !explicit_model_override,
            host.api.clone(),
            host.base_url.clone(),
            host.context_window,
            host.thinking,
            host.fast,
            QueueMode::OneAtATime,
            QueueMode::OneAtATime,
            None,
            startup_cancel,
        )
        .await
        {
            Ok(built) => built,
            Err(err) => {
                mcp_hub.shutdown().await;
                return Err(err);
            }
        };

        let effective_model = session.model();
        if !effective_model.id.is_empty()
            && !known_models
                .iter()
                .any(|m| m.provider == effective_model.provider && m.id == effective_model.id)
        {
            known_models.insert(0, effective_model);
        }

        Ok(Self {
            host,
            transport,
            provider_store,
            provider,
            models: Mutex::new(known_models),
            js_runtime,
            extension_tool_names: Mutex::new(extension_tool_names),
            mcp_entries,
            no_session: args.no_session,
            compaction_config: Mutex::new(compaction),
            state: Mutex::new(SessionRuntimeState {
                session,
                built_tools: Arc::new(built_tools),
                mcp_hub,
                session_file,
                _file_lock: file_lock,
                cwd: session_cwd,
            }),
            queued_steering: Arc::new(Mutex::new(VecDeque::new())),
            queued_follow_up: Arc::new(Mutex::new(VecDeque::new())),
            active_run: AsyncMutex::new(None),
            active_compaction: AsyncMutex::new(None),
            writer,
        })
    }

    fn current_session(&self) -> Arc<KitSession> {
        Arc::clone(&self.state.lock().unwrap_or_else(|e| e.into_inner()).session)
    }

    async fn reap_finished_run_locked(&self, guard: &mut Option<ActiveRunTask>) {
        if guard
            .as_ref()
            .is_some_and(|task| !task.running.load(Ordering::Acquire))
            && let Some(task) = guard.take()
        {
            let _ = task.handle.await;
        }
    }

    async fn reap_finished_compaction_locked(&self, guard: &mut Option<ActiveCompactionTask>) {
        if guard
            .as_ref()
            .is_some_and(|task| !task.running.load(Ordering::Acquire))
            && let Some(task) = guard.take()
        {
            let _ = task.handle.await;
        }
    }

    async fn is_busy(&self) -> bool {
        let mut run_guard = self.active_run.lock().await;
        self.reap_finished_run_locked(&mut run_guard).await;
        let mut comp_guard = self.active_compaction.lock().await;
        self.reap_finished_compaction_locked(&mut comp_guard).await;
        run_guard.is_some() || comp_guard.is_some() || self.current_session().is_running()
    }

    pub async fn handle_command(&self, id: Option<String>, cmd: RpcCommand) {
        let command_name = cmd.command_name();
        match cmd {
            RpcCommand::Prompt {
                message,
                is_meta,
                images,
                streaming_behavior,
            } => {
                self.handle_prompt(id, message, images, streaming_behavior, is_meta)
                    .await;
            }
            RpcCommand::Steer {
                message,
                is_meta,
                images,
            } => {
                self.queue_steer(message, images, is_meta).await;
                self.writer.send_json(&RpcResponse::ok(
                    id,
                    command_name,
                    Some(json!({ "disposition": "queued" })),
                ));
            }
            RpcCommand::FollowUp {
                message,
                is_meta,
                images,
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
                self.current_session().clear_all_queues();
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
                    let session = self.current_session();
                    (
                        run_guard.is_some(),
                        comp_guard.is_some() || session.is_compacting(),
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
                let models = self.available_models_for_active_branch();
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
                let res = self
                    .current_session()
                    .set_thinking_level(level)
                    .map(|()| None)
                    .map_err(|e| e.to_string());
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
                let res = self
                    .current_session()
                    .set_fast(fast)
                    .map(|()| None)
                    .map_err(|e| e.to_string());
                self.send_result_opt(id, command_name, res);
            }
            RpcCommand::SetContext { name, text } => {
                let changed = self.current_session().set_context(name, text);
                self.writer.send_json(&RpcResponse::ok(
                    id,
                    command_name,
                    Some(json!({ "changed": changed })),
                ));
            }
            RpcCommand::AppendContext { name, text } => {
                self.current_session()
                    .append_named_context(name.as_deref().unwrap_or("context"), text);
                self.writer
                    .send_json(&RpcResponse::ok(id, command_name, None));
            }
            RpcCommand::AppendMeta { text, images } => {
                self.current_session()
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
                self.current_session().set_auto_compaction(enabled);
                self.writer.send_json(&RpcResponse::ok(
                    id,
                    command_name,
                    Some(json!({ "enabled": enabled })),
                ));
            }
            RpcCommand::SetSteeringMode { mode } => {
                self.current_session().set_steering_mode(mode);
                self.writer
                    .send_json(&RpcResponse::ok(id, command_name, None));
            }
            RpcCommand::SetFollowUpMode { mode } => {
                self.current_session().set_follow_up_mode(mode);
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
                let res = self.handle_new_session(session_path, parent_session).await;
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
                let res = self.handle_switch_session(session_path).await;
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
                let all_entries = session.entries();
                let entries = match since.as_deref() {
                    Some(since_id) => match all_entries.iter().position(|e| e.id == since_id) {
                        Some(idx) => all_entries[idx + 1..].to_vec(),
                        None => Vec::new(),
                    },
                    None => all_entries,
                };
                let leaf_id = session.leaf_id();
                self.writer.send_json(&RpcResponse::ok(
                    id,
                    command_name,
                    Some(json!({ "entries": entries, "leaf_id": leaf_id })),
                ));
            }
            RpcCommand::GetTree => {
                let session = self.current_session();
                let tree = session.tree();
                let leaf_id = session.leaf_id();
                self.writer.send_json(&RpcResponse::ok(
                    id,
                    command_name,
                    Some(json!({ "tree": tree, "leaf_id": leaf_id })),
                ));
            }
            RpcCommand::GetMessages { include_meta } => {
                let session = self.current_session();
                let messages = if include_meta.unwrap_or(true) {
                    session.messages()
                } else {
                    session.visible_messages()
                };
                self.writer.send_json(&RpcResponse::ok(
                    id,
                    command_name,
                    Some(json!({ "messages": messages })),
                ));
            }
            RpcCommand::GetLastAssistantText => {
                let text = self
                    .current_session()
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
                let res = self.handle_reload_extensions();
                self.send_result(id, command_name, res);
            }
        }
    }

    async fn queue_steer(
        &self,
        message: String,
        images: Option<Vec<RpcImageInput>>,
        is_meta: bool,
    ) {
        let queue_id = Uuid::now_v7().to_string();
        let parts = build_content_parts(message.clone(), images);
        if !is_meta {
            self.queued_steering
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back((queue_id.clone(), message));
        }
        let _ = self
            .current_session()
            .steer_parts(queue_id, parts, is_meta)
            .await;
    }

    fn queue_follow_up(&self, message: String, images: Option<Vec<RpcImageInput>>, is_meta: bool) {
        let queue_id = Uuid::now_v7().to_string();
        let parts = build_content_parts(message.clone(), images);
        if !is_meta {
            self.queued_follow_up
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back((queue_id.clone(), message));
        }
        self.current_session()
            .follow_up_parts(queue_id, parts, is_meta);
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
                    self.queue_steer(message, images, is_meta).await;
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
            if comp_guard.is_some() || self.current_session().is_compacting() {
                self.writer.send_json(&RpcResponse::error(
                    id,
                    "prompt",
                    "Session is currently compacting",
                ));
                return;
            }
        }

        let session = self.current_session();
        if let Some(bg_prompt) = session.drain_background_prompt() {
            session.append_meta(bg_prompt);
        }

        let content = build_content_parts(message, images);
        let mut stream = match session.prompt_parts_stream_owned(content, is_meta) {
            Ok(stream) => stream,
            Err(err) => {
                self.writer
                    .send_json(&RpcResponse::error(id, "prompt", err.to_string()));
                return;
            }
        };

        // Enqueue prompt acceptance before spawning stream forwarding so stdout ordering is strict.
        self.writer.send_json(&RpcResponse::ok(
            id,
            "prompt",
            Some(json!({ "disposition": "started" })),
        ));

        let running = Arc::new(AtomicBool::new(true));
        let aborted = Arc::new(AtomicBool::new(false));
        let task_js = self.js_runtime.clone();
        let task_steering = Arc::clone(&self.queued_steering);
        let task_follow_up = Arc::clone(&self.queued_follow_up);
        let task_running = Arc::clone(&running);
        let task_aborted = Arc::clone(&aborted);
        let task_writer = self.writer.clone();

        let handle = tokio::spawn(async move {
            let mut saw_abort = false;
            let mut stream_error: Option<String> = None;

            while let Some(ev) = stream.next().await {
                match ev {
                    KitEvent::Session(session_ev) => {
                        task_writer.send_json(&session_ev);
                    }
                    KitEvent::Agent(agent_ev) => {
                        match &agent_ev {
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
                            AgentEvent::MessageEnd {
                                message: AgentMessage::Llm(Message::Assistant(assistant)),
                            } => {
                                if assistant.stop_reason == StopReason::Aborted {
                                    saw_abort = true;
                                }
                            }
                            AgentEvent::AgentEnd { .. } => {
                                task_steering
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .clear();
                            }
                            _ => {}
                        }
                        task_writer.send_json(&agent_ev);
                        flush_js_errors(task_js.as_ref(), &task_writer);
                    }
                    KitEvent::Error(err) => {
                        if stream_error.is_none() {
                            stream_error = Some(err);
                        }
                    }
                }
            }

            let final_aborted = if stream_error.is_some() {
                task_aborted.load(Ordering::Acquire)
            } else {
                saw_abort || task_aborted.load(Ordering::Acquire)
            };
            // Always clear tracked steering once the stream settles (normal end, error such as
            // failed auto-compaction, or abort) so `pending_message_count` matches Kit's drained
            // agent queue and stale steering never leaks into the next turn.
            task_steering
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            task_running.store(false, Ordering::Release);
            task_writer.send_json(&AgentSettledEvent::with_error(final_aborted, stream_error));
        });

        *active_guard = Some(ActiveRunTask {
            running,
            aborted,
            handle,
        });
    }

    pub async fn shutdown_tools(&self) {
        let (tools, mcp_hub) = {
            let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (Arc::clone(&guard.built_tools), Arc::clone(&guard.mcp_hub))
        };
        tools.shutdown().await;
        mcp_hub.shutdown().await;
    }

    pub async fn abort_active(&self) {
        let mut active_guard = self.active_run.lock().await;
        let mut comp_guard = self.active_compaction.lock().await;
        let session = self.current_session();

        if let Some(comp_task) = comp_guard.take() {
            session.abort_now();
            let _ = comp_task.handle.await;
        }
        if let Some(task) = active_guard.take() {
            task.aborted.store(true, Ordering::SeqCst);
            session.abort_now();
            let _ = task.handle.await;
        }
        session.abort().await;
        let _ = session.drain_steering();
        self.queued_steering
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
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
        self.current_session().abort_now();
        if let Ok(mut guard) = self.active_run.try_lock()
            && let Some(task) = guard.take()
        {
            task.aborted.store(true, Ordering::SeqCst);
            task.handle.abort();
        }
        if let Ok(mut comp_guard) = self.active_compaction.try_lock()
            && let Some(comp_task) = comp_guard.take()
        {
            comp_task.handle.abort();
        }
    }

    fn available_models_for_active_branch(&self) -> Vec<Model> {
        let mut models = self
            .models
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let upsert = |list: &mut Vec<Model>, model: Model| {
            if let Some(slot) = list
                .iter_mut()
                .find(|m| m.provider == model.provider && m.id == model.id)
            {
                *slot = model;
            } else {
                list.push(model);
            }
        };
        let session = self.current_session();
        if let Some(hdr_model) = resolve_model_metadata(
            session.header().initial_model.as_ref(),
            &self.provider_store,
        ) {
            upsert(&mut models, hdr_model);
        }
        for entry in session.active_branch() {
            if let jimu_session::SessionEntryPayload::ModelChange {
                provider,
                model_id,
                api,
                base_url,
                context_window,
                max_output_tokens,
                supports_images,
                supports_reasoning,
            } = entry.payload
            {
                let meta = jimu_session::SessionModelMetadata {
                    provider,
                    model_id,
                    api,
                    base_url,
                    context_window,
                    max_output_tokens,
                    supports_images,
                    supports_reasoning,
                };
                if let Some(branch_model) =
                    resolve_model_metadata(Some(&meta), &self.provider_store)
                {
                    upsert(&mut models, branch_model);
                }
            }
        }
        let current = session.model();
        if !current.id.trim().is_empty() {
            upsert(&mut models, current);
        }
        models
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
        let session = self.current_session();
        let current = session.model();
        if let Some(url) = base_url.as_deref() {
            validate_base_url(url)?;
        }

        let catalog = self.models.lock().unwrap_or_else(|e| e.into_inner());
        let apply_overrides = |mut base: Model| {
            if let Some(a) = api.clone() {
                base.api = a;
            }
            if let Some(u) = base_url.clone() {
                base.base_url = u;
            }
            if let Some(cw) = context_window {
                base.context_window = cw;
            }
            if let Some(mo) = max_output_tokens {
                base.max_output_tokens = mo;
            }
            if let Some(si) = supports_images {
                base.supports_images = si;
            }
            if let Some(sr) = supports_reasoning {
                base.supports_reasoning = sr;
            }
            base
        };
        let resolved = if provider == current.provider && model_id == current.id {
            apply_overrides(current)
        } else if let Some(branch_model) =
            find_active_branch_model(&session, &provider, &model_id, &self.provider_store)
        {
            apply_overrides(branch_model)
        } else if let Some(existing) = catalog
            .iter()
            .find(|m| m.provider == provider && m.id == model_id)
            .cloned()
        {
            apply_overrides(existing)
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
                &self.provider_store,
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
        drop(catalog);

        set_model_atomic(&session, resolved.clone())?;

        serde_json::to_value(resolved).map_err(|e| e.to_string())
    }

    fn handle_reload_extensions(&self) -> Result<serde_json::Value, String> {
        let Some(rt) = &self.js_runtime else {
            return Ok(json!({ "extensions": Vec::<String>::new() }));
        };
        let session = self.current_session();
        let old_names: Vec<String> = self
            .extension_tool_names
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let old_name_set: HashSet<&str> = old_names.iter().map(String::as_str).collect();

        let mut non_extension_names: HashSet<String> = session
            .resident_tool_definitions()
            .into_iter()
            .map(|def| def.name)
            .collect();
        for def in session.deferred_tools() {
            if !old_name_set.contains(def.name.as_str()) {
                non_extension_names.insert(def.name);
            }
        }
        let forbidden = reserved_extension_tool_names(&non_extension_names);
        rt.reload_excluding(forbidden).map_err(|e| e.to_string())?;

        let new_tools = rt.tools();
        let old_refs: Vec<&str> = old_names.iter().map(String::as_str).collect();
        if !old_refs.is_empty() {
            session.remove_tools(&old_refs).map_err(|e| e.to_string())?;
        }
        let next_names: Vec<String> = new_tools.iter().map(|t| t.definition().name).collect();
        if !new_tools.is_empty() {
            session.upsert_tools(new_tools).map_err(|e| e.to_string())?;
        }
        *self
            .extension_tool_names
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = next_names;

        Ok(json!({ "extensions": rt.loaded_extensions() }))
    }

    async fn handle_manual_compact(&self, id: Option<String>, custom_instructions: Option<String>) {
        let mut run_guard = self.active_run.lock().await;
        self.reap_finished_run_locked(&mut run_guard).await;
        let mut comp_guard = self.active_compaction.lock().await;
        self.reap_finished_compaction_locked(&mut comp_guard).await;

        let session = self.current_session();
        if run_guard.is_some() || comp_guard.is_some() || session.is_running() {
            self.writer.send_json(&RpcResponse::error(
                id,
                "compact",
                "Cannot compact while session is busy",
            ));
            return;
        }

        // Synchronously claim Activity::Compacting BEFORE spawning so concurrent `abort` or
        // `get_state` never races ahead of compaction registration.
        let event_writer = self.writer.clone();
        let compact_fut =
            match session.start_compact_with_instructions(custom_instructions, move |ev| {
                event_writer.send_json(&ev);
            }) {
                Ok(fut) => fut,
                Err(KitError::Busy) => {
                    self.writer.send_json(&RpcResponse::error(
                        id,
                        "compact",
                        "Cannot compact while session is busy",
                    ));
                    return;
                }
                Err(err) => {
                    self.writer
                        .send_json(&RpcResponse::error(id, "compact", err.to_string()));
                    return;
                }
            };

        let running = Arc::new(AtomicBool::new(true));
        let task_writer = self.writer.clone();
        let task_running = Arc::clone(&running);

        let handle = tokio::spawn(async move {
            let outcome = match compact_fut.await {
                Ok(res) => {
                    let dto = CompactionResponseData::from(res);
                    serde_json::to_value(dto).map_err(|e| e.to_string())
                }
                Err(KitError::Aborted) => Err("Compaction aborted".to_string()),
                Err(err) => Err(err.to_string()),
            };

            task_running.store(false, Ordering::Release);

            match outcome {
                Ok(data) => task_writer.send_json(&RpcResponse::ok(id, "compact", Some(data))),
                Err(err) => task_writer.send_json(&RpcResponse::error(id, "compact", err)),
            }
        });

        *comp_guard = Some(ActiveCompactionTask { running, handle });
    }

    async fn handle_new_session(
        &self,
        session_path: Option<String>,
        parent_session: Option<String>,
    ) -> Result<serde_json::Value, String> {
        let (
            current_model,
            current_thinking,
            current_fast,
            steer_mode,
            follow_mode,
            current_file,
            current_cwd,
            current_hub,
        ) = {
            let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (
                guard.session.model(),
                guard.session.thinking_level(),
                guard.session.fast(),
                guard.session.steering_mode(),
                guard.session.follow_up_mode(),
                guard.session_file.clone(),
                guard.cwd.clone(),
                Arc::clone(&guard.mcp_hub),
            )
        };

        let (store, new_file): (Arc<dyn SessionStore>, Option<PathBuf>) =
            if let Some(path_str) = session_path {
                let p = canonical_jsonl_path(std::path::Path::new(&path_str));
                (Arc::new(JsonlSessionStore::new(p.clone())), Some(p))
            } else if self.no_session {
                (Arc::new(MemorySessionStore::new()), None)
            } else if let Some(prev) = current_file.as_ref() {
                let dir = prev
                    .parent()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| self.host.cwd.clone());
                let fresh =
                    canonical_jsonl_path(&dir.join(format!("session-{}.jsonl", Uuid::now_v7())));
                (Arc::new(JsonlSessionStore::new(fresh.clone())), Some(fresh))
            } else {
                (Arc::new(MemorySessionStore::new()), None)
            };

        let same_as_current = match (&new_file, &current_file) {
            (Some(a), Some(b)) => same_file_path(a, b),
            _ => false,
        };
        let next_lock = match (&new_file, same_as_current) {
            (Some(path), false) => Some(SessionFileLock::try_acquire(path)?),
            _ => None,
        };

        // Inspect target file content and cwd only after acquiring the exclusive session file lock
        // so another process cannot finish writing between inspection and lock acquisition.
        let has_existing = match &new_file {
            Some(path) => has_existing_session_content(path)?,
            None => false,
        };
        let next_cwd = match &new_file {
            Some(path) => resolve_existing_session_cwd(path, &self.host.cwd)?,
            None => self.host.cwd.clone(),
        };

        let reconnect_mcp =
            !paths_same_dir(&current_cwd, &next_cwd) && has_cwd_dependent_stdio(&self.mcp_entries);
        let (target_hub, replaced_hub) = if reconnect_mcp {
            let (hub, warnings) = McpHub::connect(
                self.mcp_entries.clone(),
                &next_cwd,
                Arc::clone(&self.transport),
                CancelToken::new(),
            )
            .await
            .map_err(|err| err.to_string())?;
            for warning in warnings {
                eprintln!("warning: {warning}");
            }
            (Arc::new(hub), true)
        } else {
            (current_hub, false)
        };

        let compaction = self
            .compaction_config
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        let (next_session, next_tools) = match build_kit_session(
            &self.host,
            &next_cwd,
            &self.transport,
            &self.provider_store,
            Arc::clone(&self.provider),
            self.js_runtime.as_ref(),
            &target_hub,
            store,
            compaction,
            current_model,
            has_existing,
            None,
            None,
            None,
            current_thinking,
            current_fast,
            steer_mode,
            follow_mode,
            parent_session,
            CancelToken::new(),
        )
        .await
        {
            Ok(built) => built,
            Err(err) => {
                if replaced_hub {
                    target_hub.shutdown().await;
                }
                return Err(err);
            }
        };

        let (prev_tools, prev_hub) = {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            guard.session = Arc::clone(&next_session);
            guard.session_file = new_file.clone();
            guard.cwd = next_cwd;
            if !same_as_current {
                guard._file_lock = next_lock;
            }
            let prev_hub = if replaced_hub {
                Some(std::mem::replace(&mut guard.mcp_hub, target_hub))
            } else {
                None
            };
            let prev_tools = std::mem::replace(&mut guard.built_tools, Arc::new(next_tools));
            (prev_tools, prev_hub)
        };
        prev_tools.shutdown().await;
        if let Some(prev_hub) = prev_hub {
            prev_hub.shutdown().await;
        }

        self.queued_steering
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.queued_follow_up
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();

        Ok(json!({
            "cancelled": false,
            "session_id": next_session.session_id(),
            "session_file": new_file.map(|p| p.display().to_string()),
        }))
    }

    async fn handle_switch_session(
        &self,
        session_path: String,
    ) -> Result<serde_json::Value, String> {
        let path = canonical_jsonl_path(std::path::Path::new(&session_path));
        let store: Arc<dyn SessionStore> = Arc::new(JsonlSessionStore::new(path.clone()));

        let (
            current_model,
            current_thinking,
            current_fast,
            steer_mode,
            follow_mode,
            current_file,
            current_cwd,
            current_hub,
        ) = {
            let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (
                guard.session.model(),
                guard.session.thinking_level(),
                guard.session.fast(),
                guard.session.steering_mode(),
                guard.session.follow_up_mode(),
                guard.session_file.clone(),
                guard.cwd.clone(),
                Arc::clone(&guard.mcp_hub),
            )
        };

        let same_as_current = current_file
            .as_ref()
            .is_some_and(|cur| same_file_path(cur, &path));
        let next_lock = if same_as_current {
            None
        } else {
            Some(SessionFileLock::try_acquire(&path)?)
        };

        let target_cwd = resolve_existing_session_cwd(&path, &self.host.cwd)?;

        let reconnect_mcp = !paths_same_dir(&current_cwd, &target_cwd)
            && has_cwd_dependent_stdio(&self.mcp_entries);
        let (target_hub, replaced_hub) = if reconnect_mcp {
            let (hub, warnings) = McpHub::connect(
                self.mcp_entries.clone(),
                &target_cwd,
                Arc::clone(&self.transport),
                CancelToken::new(),
            )
            .await
            .map_err(|err| err.to_string())?;
            for warning in warnings {
                eprintln!("warning: {warning}");
            }
            (Arc::new(hub), true)
        } else {
            (current_hub, false)
        };

        let compaction = self
            .compaction_config
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        let (next_session, next_tools) = match build_kit_session(
            &self.host,
            &target_cwd,
            &self.transport,
            &self.provider_store,
            Arc::clone(&self.provider),
            self.js_runtime.as_ref(),
            &target_hub,
            store,
            compaction,
            current_model,
            true,
            None,
            None,
            None,
            current_thinking,
            current_fast,
            steer_mode,
            follow_mode,
            None,
            CancelToken::new(),
        )
        .await
        {
            Ok(built) => built,
            Err(err) => {
                if replaced_hub {
                    target_hub.shutdown().await;
                }
                return Err(err);
            }
        };

        let (prev_tools, prev_hub) = {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            guard.session = Arc::clone(&next_session);
            guard.session_file = Some(path.clone());
            guard.cwd = target_cwd;
            if !same_as_current {
                guard._file_lock = next_lock;
            }
            let prev_hub = if replaced_hub {
                Some(std::mem::replace(&mut guard.mcp_hub, target_hub))
            } else {
                None
            };
            let prev_tools = std::mem::replace(&mut guard.built_tools, Arc::new(next_tools));
            (prev_tools, prev_hub)
        };
        prev_tools.shutdown().await;
        if let Some(prev_hub) = prev_hub {
            prev_hub.shutdown().await;
        }

        self.queued_steering
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.queued_follow_up
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();

        Ok(json!({
            "cancelled": false,
            "session_id": next_session.session_id(),
            "session_file": path.display().to_string(),
            "leaf_id": next_session.leaf_id(),
        }))
    }

    fn handle_fork(&self, entry_id: &str) -> Result<serde_json::Value, String> {
        let session = self.current_session();
        let from_leaf_id = session.leaf_id();
        session
            .fork_with_model_resolver(entry_id, |meta| {
                resolve_model_metadata(meta, &self.provider_store)
            })
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
        let (session, session_file, cwd) = {
            let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (
                Arc::clone(&guard.session),
                guard.session_file.clone(),
                guard.cwd.clone(),
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
        let tools = session
            .resident_tool_definitions()
            .into_iter()
            .map(|t| t.name)
            .collect();
        let extensions = self
            .js_runtime
            .as_ref()
            .map(|rt| rt.loaded_extensions())
            .unwrap_or_default();

        RpcStateData {
            model: session.model(),
            thinking_level: session.thinking_level(),
            fast: session.fast(),
            is_streaming,
            is_compacting,
            steering_mode: session.steering_mode(),
            follow_up_mode: session.follow_up_mode(),
            session_file: session_file.map(|p| p.display().to_string()),
            session_id: session.session_id(),
            leaf_id: session.leaf_id(),
            cwd: cwd.display().to_string(),
            auto_compaction_enabled: session.compaction_config().enabled,
            message_count: session.messages().len(),
            pending_message_count,
            tools,
            extensions,
        }
    }

    fn send_result(
        &self,
        id: Option<String>,
        command: &'static str,
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
        command: &'static str,
        res: Result<Option<serde_json::Value>, String>,
    ) {
        match res {
            Ok(data) => self.writer.send_json(&RpcResponse::ok(id, command, data)),
            Err(err) => self.writer.send_json(&RpcResponse::error(id, command, err)),
        }
    }

    /// Run a single one-shot prompt (used by `jimu -p` / `--mode text` / `--mode json`).
    pub async fn run_oneshot(&self, prompt: String, mode: CliMode) -> Result<(), String> {
        let session = self.current_session();
        if session.model().id.trim().is_empty() {
            return Err(
                "No model configured. Pass --provider <id> -m <model>, or add models to ~/.jimu/providers.json."
                    .into(),
            );
        }
        if let Some(bg_prompt) = session.drain_background_prompt() {
            session.append_meta(bg_prompt);
        }

        let mut stream = session
            .prompt_parts_stream_owned(vec![ContentPart::Text(TextContent::plain(prompt))], false)
            .map_err(|e| e.to_string())?;

        let mut printed_any_text = false;
        let mut saw_abort = false;
        let mut last_stop_reason: Option<StopReason> = None;
        let mut last_error_message: Option<String> = None;
        let mut stream_error: Option<String> = None;

        while let Some(ev) = stream.next().await {
            match ev {
                KitEvent::Session(session_ev) => {
                    if mode == CliMode::Json {
                        self.writer.send_json(&session_ev);
                    }
                }
                KitEvent::Agent(agent_ev) => {
                    match &agent_ev {
                        AgentEvent::MessageEnd {
                            message: AgentMessage::Llm(Message::Assistant(a)),
                        } => {
                            if a.stop_reason == StopReason::Aborted {
                                saw_abort = true;
                            }
                            last_stop_reason = Some(a.stop_reason);
                            if a.error_message.is_some() {
                                last_error_message = a.error_message.clone();
                            }
                        }
                        AgentEvent::TurnEnd { message, .. } => {
                            last_stop_reason = Some(message.stop_reason);
                            last_error_message = message.error_message.clone();
                        }
                        _ => {}
                    }

                    match mode {
                        CliMode::Json => {
                            self.writer.send_json(&agent_ev);
                            flush_js_errors(self.js_runtime.as_ref(), &self.writer);
                        }
                        CliMode::Text => {
                            if let AgentEvent::MessageUpdate {
                                assistant_message_event:
                                    AssistantMessageEvent::TextDelta { delta, .. },
                                ..
                            } = &agent_ev
                            {
                                use std::io::Write;
                                let mut stdout = std::io::stdout().lock();
                                let _ = stdout.write_all(delta.as_bytes());
                                let _ = stdout.flush();
                                printed_any_text = true;
                            }
                            if let Some(rt) = &self.js_runtime {
                                for err in rt.take_errors() {
                                    eprintln!("[jimu] extension error: {err}");
                                }
                            }
                        }
                        CliMode::Rpc | CliMode::Acp | CliMode::Tui => {}
                    }
                }
                KitEvent::Error(err) => {
                    if stream_error.is_none() {
                        stream_error = Some(err);
                    }
                }
            }
        }

        if mode == CliMode::Text && printed_any_text {
            use std::io::Write;
            let mut stdout = std::io::stdout().lock();
            let _ = stdout.write_all(b"\n");
            let _ = stdout.flush();
        } else if mode == CliMode::Json {
            let settled_aborted = if stream_error.is_some() {
                false
            } else {
                saw_abort
            };
            self.writer.send_json(&AgentSettledEvent::with_error(
                settled_aborted,
                stream_error.clone(),
            ));
        }

        if let Some(err_msg) = stream_error {
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

fn same_file_path(a: &std::path::Path, b: &std::path::Path) -> bool {
    canonical_jsonl_path(a) == canonical_jsonl_path(b)
}

#[allow(clippy::too_many_arguments)]
async fn build_kit_session(
    host: &HostConfig,
    cwd: &std::path::Path,
    transport: &Arc<dyn HttpTransport>,
    provider_store: &ProviderStore,
    provider: Arc<dyn StreamProvider>,
    js_runtime: Option<&Arc<JsExtensionRuntime>>,
    mcp_hub: &McpHub,
    store: Arc<dyn SessionStore>,
    compaction: CompactionConfig,
    fallback_model: Model,
    should_restore_model: bool,
    cli_api_override: Option<ApiKind>,
    cli_base_url_override: Option<String>,
    cli_context_window_override: Option<u32>,
    thinking_level: jimu_ai::ThinkingLevel,
    fast: bool,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
    parent_session: Option<String>,
    cancel: CancelToken,
) -> Result<(Arc<KitSession>, BuiltWorkspaceTools), String> {
    let bg_sender: Arc<OnceLock<BackgroundEventSender>> = Arc::new(OnceLock::new());
    let bg_slot = Arc::clone(&bg_sender);
    let bg_callback = make_background_callback(move || bg_slot.get().cloned());

    let built_tools =
        build_workspace_tools(cwd, transport, &host.tools, true, Some(bg_callback), None);

    let mut cfg = KitSessionConfig::new(fallback_model);
    cfg.store = Some(store);
    cfg.cwd = Some(cwd.to_string_lossy().into_owned());
    cfg.parent_session = parent_session;
    cfg.thinking_level = thinking_level;
    cfg.fast = fast;
    cfg.enable_codemode = host.enable_codemode;
    cfg.compaction = compaction;
    cfg.steering_mode = steering_mode;
    cfg.follow_up_mode = follow_up_mode;
    cfg.skill_discovery = build_skill_discovery(
        cwd,
        &host.jimu_home,
        host.skill_walk_boundary,
        &host.skill_dirs,
        &[],
    );
    cfg.instruction_discovery = build_instruction_discovery(
        host.instruction_discovery.as_ref(),
        cwd,
        &host.jimu_home,
        &[],
    );
    cfg.tools
        .extend(built_tools.tools.iter().cloned().map(KitToolSpec::resident));

    let has_extension_tools = if let Some(rt) = js_runtime {
        let loaded = jimu::host::LoadedExtensions {
            runtime: Arc::clone(rt),
            tools: rt.tools(),
        };
        attach_extensions_to_config(&mut cfg, &loaded)?;
        !loaded.tools.is_empty()
    } else {
        false
    };

    let has_mcp = !mcp_hub.is_empty();
    cfg.enable_deferred_gateway = should_enable_deferred_gateway(
        &host.tools,
        host.enable_codemode,
        has_extension_tools,
        has_mcp,
    );
    let has_tools = !cfg.tools.is_empty() || host.enable_codemode || has_mcp;
    cfg.system_prompt = resolve_system_prompt(
        host.system_prompt.as_deref(),
        cwd,
        &host.jimu_home,
        &host.tools,
        has_tools,
    );

    let requested_model = cfg.model.clone();
    let session = match KitSession::new(provider, cfg) {
        Ok(session) => Arc::new(session),
        Err(err) => {
            built_tools.shutdown().await;
            return Err(format!("failed to open session: {err}"));
        }
    };
    let _ = bg_sender.set(session.background_sender());

    if let Err(err) = apply_session_model_on_open(
        &session,
        provider_store,
        &requested_model,
        should_restore_model,
        cli_api_override,
        cli_base_url_override,
        cli_context_window_override,
    ) {
        built_tools.shutdown().await;
        return Err(err);
    }

    if !mcp_hub.is_empty() {
        match mcp_hub.attach_to_session(&session, cancel).await {
            Ok(warnings) => {
                for warning in warnings {
                    eprintln!("warning: {warning}");
                }
            }
            Err(err) => {
                built_tools.shutdown().await;
                return Err(err.to_string());
            }
        }
    }

    Ok((session, built_tools))
}
