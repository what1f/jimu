use crate::tools::{FfiAgentTool, JimuToolCallbacks, SendToolCallbacks};
use crate::transport::{FfiHttpTransport, JimuHttpCallbacks, SendHttpCallbacks};
use crate::types::{
    FfiCompactionResultJson, FfiEvent, FfiPromptRequest, FfiSessionConfig, FfiSessionInfo,
    FfiToolSpec, JimuStatus,
};
use futures_channel::oneshot;
use futures_executor::ThreadPool;
use futures_util::StreamExt;
use jimu_agent::{Agent, AgentConfig, AgentEvent, AgentMessage, AgentMiddleware, AgentTool};
use jimu_ai::{
    AssistantMessageEvent, CacheRetention, CancelToken, Client, Message, Model,
    SimpleStreamOptions, StopReason, ThinkingLevel, ToolDefinition, UserMessage,
};
use jimu_js::{JsExtensionConfig, JsExtensionRuntime};
use jimu_kit::{
    CALL_TOOL_NAME, CODEMODE_EXEC_TOOL_NAME, CallToolGateway, CodemodeExecTool,
    DeferredToolRegistry, InstructionContextState, SKILLS_CONTEXT_NAME, SkillCatalog,
    SkillContextState, SkillDiscoveryConfig, TOOL_SEARCH_NAME, ToolSchemaBudget, ToolSearchTool,
};
use jimu_session::{
    CompactionConfig, CompactionResult, JsonlSessionStore, MemorySessionStore, SessionConfig,
    SessionEntry, SessionEntryPayload, SessionEvent, SessionManager, SessionStore,
    prepare_flushed_prompt,
};
use serde_json::Map;
use std::collections::{HashMap, HashSet};
use std::ffi::{CString, c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const DEFERRED_TOOLS_CONTEXT_NAME: &str = "deferred_tools";

pub type JimuEventFn = extern "C" fn(ctx: *mut c_void, event_json: *const c_char);
pub type JimuStreamDoneFn =
    extern "C" fn(ctx: *mut c_void, status: JimuStatus, error_message: *const c_char);
pub type JimuVoidDoneFn = extern "C" fn(ctx: *mut c_void);
pub type JimuResultDoneFn = extern "C" fn(
    ctx: *mut c_void,
    status: JimuStatus,
    result_json: *const c_char,
    error_message: *const c_char,
);

#[derive(Clone, Copy)]
pub(crate) struct SendCallbackCtx(pub *mut c_void);

impl SendCallbackCtx {
    #[inline]
    pub(crate) fn as_ptr(self) -> *mut c_void {
        self.0
    }
}

// SAFETY: Host guarantees callback context pointers are thread-safe until completion fires.
unsafe impl Send for SendCallbackCtx {}
unsafe impl Sync for SendCallbackCtx {}

pub(crate) fn global_pool() -> &'static ThreadPool {
    static POOL: OnceLock<ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| ThreadPool::new().expect("failed to initialize Jimu FFI ThreadPool"))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn persisted_baseline_tool_names(store: &dyn SessionStore) -> Result<Option<Vec<String>>, String> {
    let (_, entries) = store
        .load()
        .map_err(|e| format!("failed to open session: {e}"))?;
    if entries.is_empty() {
        return Ok(None);
    }
    let by_id: HashMap<&str, usize> = entries
        .iter()
        .enumerate()
        .map(|(idx, entry)| (entry.id.as_str(), idx))
        .collect();
    let mut cursor = entries.last().map(|entry| entry.id.as_str());
    let mut visited = 0usize;
    while let Some(id) = cursor {
        if visited >= entries.len() {
            break;
        }
        visited += 1;
        let Some(&idx) = by_id.get(id) else {
            break;
        };
        if let SessionEntryPayload::SystemBaseline { tools, .. } = &entries[idx].payload {
            return Ok(Some(tools.iter().map(|t| t.name.clone()).collect()));
        }
        cursor = entries[idx].parent_id.as_deref();
    }
    Ok(None)
}

struct ActivePromptRun {
    cancel: CancelToken,
    idle_waiters: Vec<oneshot::Sender<()>>,
}

struct SessionRunState {
    closed: bool,
    active: Option<ActivePromptRun>,
}

pub struct FfiSession {
    provider: Arc<Client>,
    agent: Agent,
    session: Arc<Mutex<SessionManager>>,
    deferred_registry: Arc<DeferredToolRegistry>,
    skills: Arc<Mutex<SkillCatalog>>,
    skill_discovery: Option<SkillDiscoveryConfig>,
    auto_reload_skills: bool,
    known_skills: Mutex<SkillContextState>,
    instructions: Mutex<InstructionContextState>,
    had_deferred_catalog: Mutex<bool>,
    tool_callbacks: SendToolCallbacks,
    next_tool_call_id: Arc<AtomicU64>,
    js_runtime: Option<JsExtensionRuntime>,
    api_key: Mutex<Option<String>>,
    cache_retention: CacheRetention,
    headers: Vec<(String, Option<String>)>,
    rollback_first_turn_on_failure: bool,
    run_state: Mutex<SessionRunState>,
}

/// Opaque C handle wrapping `Arc<FfiSession>`.
pub struct JimuSession {
    pub(crate) inner: Arc<FfiSession>,
}

impl FfiSession {
    pub(crate) fn create(
        config: FfiSessionConfig,
        http_callbacks: JimuHttpCallbacks,
        tool_callbacks: JimuToolCallbacks,
    ) -> Result<Arc<Self>, String> {
        let http_cb = SendHttpCallbacks::new(http_callbacks)?;
        let tool_cb = SendToolCallbacks::new(tool_callbacks, Some(Arc::clone(&http_cb)));
        let next_tool_call_id = Arc::new(AtomicU64::new(1));

        let transport = Arc::new(FfiHttpTransport::new(http_cb.clone()));
        let provider = Arc::new(Client::new(transport));

        let store: Arc<dyn SessionStore> = match &config.persistence_path {
            Some(path) if !path.is_empty() => Arc::new(JsonlSessionStore::new(path)),
            _ => Arc::new(MemorySessionStore::new()),
        };
        let baseline_tool_names = persisted_baseline_tool_names(&*store)?;

        let deferred_registry = Arc::new(DeferredToolRegistry::new());
        let mut resident: Vec<Arc<dyn AgentTool>> = Vec::new();
        let mut deferred: Vec<Arc<dyn AgentTool>> = Vec::new();

        let auto_defer = config
            .auto_defer_threshold_bytes
            .filter(|_| config.enable_deferred_gateway);
        let reserved = |name: &str| {
            [TOOL_SEARCH_NAME, CALL_TOOL_NAME, CODEMODE_EXEC_TOOL_NAME].contains(&name)
        };

        if let Some(baseline_names) = baseline_tool_names {
            let baseline_set: HashSet<&str> = baseline_names.iter().map(String::as_str).collect();
            let mut by_name: HashMap<&str, Arc<dyn AgentTool>> = HashMap::new();
            let mut non_baseline: Vec<(&FfiToolSpec, Arc<dyn AgentTool>)> = Vec::new();
            for spec in &config.tools {
                let tool =
                    FfiAgentTool::from_spec(spec, tool_cb.clone(), Arc::clone(&next_tool_call_id));
                if baseline_set.contains(spec.name.as_str()) {
                    by_name.entry(spec.name.as_str()).or_insert(tool);
                } else {
                    non_baseline.push((spec, tool));
                }
            }
            for name in &baseline_names {
                if let Some(tool) = by_name.remove(name.as_str()) {
                    resident.push(tool);
                }
            }
            let pinned = resident
                .iter()
                .map(|tool| tool.definition())
                .chain(
                    non_baseline
                        .iter()
                        .filter(|(spec, _)| !spec.deferred && reserved(&spec.name))
                        .map(|(spec, _)| spec.definition()),
                )
                .collect::<Vec<_>>();
            let mut budget = ToolSchemaBudget::new(auto_defer, &pinned);
            for (spec, tool) in non_baseline {
                if spec.deferred
                    || (auto_defer.is_some()
                        && !reserved(&spec.name)
                        && !budget.admit(&tool.definition()))
                {
                    deferred.push(tool);
                } else {
                    resident.push(tool);
                }
            }
        } else {
            let pinned = config
                .tools
                .iter()
                .filter(|spec| !spec.deferred && reserved(&spec.name))
                .map(FfiToolSpec::definition)
                .collect::<Vec<_>>();
            let mut budget = ToolSchemaBudget::new(auto_defer, &pinned);
            for spec in &config.tools {
                let tool =
                    FfiAgentTool::from_spec(spec, tool_cb.clone(), Arc::clone(&next_tool_call_id));
                if spec.deferred
                    || (auto_defer.is_some()
                        && !reserved(&spec.name)
                        && !budget.admit(&tool.definition()))
                {
                    deferred.push(tool);
                } else {
                    resident.push(tool);
                }
            }
        }

        let skill_discovery = if let Some(mut discovery) = config.skill_discovery {
            if discovery.cwd.is_none() {
                discovery.cwd = config.cwd.as_ref().map(PathBuf::from);
            }
            discovery
                .extra_roots
                .extend(config.skill_roots.into_iter().map(PathBuf::from));
            Some(discovery)
        } else if !config.skill_roots.is_empty() {
            Some(SkillDiscoveryConfig {
                extra_roots: config.skill_roots.into_iter().map(PathBuf::from).collect(),
                include_user_skills: false,
                ..SkillDiscoveryConfig::default()
            })
        } else {
            None
        };
        let skills = Arc::new(Mutex::new(
            skill_discovery
                .as_ref()
                .map(SkillCatalog::discover_with_config)
                .unwrap_or_default(),
        ));

        // Load optional QuickJS extensions (`jimu-js`).
        let mut middlewares: Vec<Arc<dyn AgentMiddleware>> = Vec::new();
        let mut js_runtime: Option<JsExtensionRuntime> = None;
        if let Some(js_cfg) = config.js_extensions
            && !js_cfg.sources.is_empty()
        {
            let rt_cfg: JsExtensionConfig = js_cfg.into();
            let rt = JsExtensionRuntime::new(rt_cfg)
                .map_err(|e| format!("failed to load JS extensions: {e}"))?;
            for tool in rt.tools() {
                if !resident
                    .iter()
                    .any(|t| t.definition().name == tool.definition().name)
                {
                    resident.push(tool);
                }
            }
            middlewares.push(Arc::new(rt.clone()));
            js_runtime = Some(rt);
        }

        deferred_registry.upsert(deferred);

        // Keep deferred gateway resident when enabled so dynamic deferred tool updates
        // NEVER mutate the top-level resident `tools` array (prompt cache stability).
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
                        .with_middlewares(middlewares.clone()),
                ));
            }
        }

        if config.enable_codemode
            && !resident
                .iter()
                .any(|t| t.definition().name == CODEMODE_EXEC_TOOL_NAME)
        {
            // Snapshot resident tools BEFORE pushing `exec` so `CodemodeExecTool` never holds
            // a strong Arc cycle back to itself.
            let resident_tools_handle = Arc::new(Mutex::new(resident.clone()));
            let cm_opts = config.codemode_options.map(Into::into).unwrap_or_default();
            let exec_tool: Arc<dyn AgentTool> = Arc::new(
                CodemodeExecTool::new(
                    resident_tools_handle,
                    Arc::clone(&deferred_registry),
                    cm_opts,
                )
                .with_middlewares(middlewares.clone()),
            );
            resident.push(exec_tool);
        }

        let resident_defs: Vec<ToolDefinition> = resident.iter().map(|t| t.definition()).collect();
        let compaction: CompactionConfig = config.compaction.map(Into::into).unwrap_or_default();

        let mut session_cfg =
            SessionConfig::new(&config.system_prompt, resident_defs).with_compaction(compaction);
        if let Some(cwd) = &config.cwd {
            session_cfg = session_cfg.with_cwd(cwd.clone());
        }

        let mut session_manager = SessionManager::open(store, session_cfg)
            .map_err(|e| format!("failed to open session: {e}"))?;
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
            .with_follow_up_mode(config.follow_up_mode)
            .with_rollback_first_turn_on_failure(config.rollback_first_turn_on_failure);
        agent_cfg.middlewares = middlewares;
        agent_cfg.api_key = config.api_key.clone();
        agent_cfg.temperature = config.temperature;
        agent_cfg.max_tokens = config.max_tokens;
        agent_cfg.cache_retention = config.cache_retention;
        agent_cfg.session_id = Some(session_id);
        agent_cfg.headers = config.headers.clone();
        agent_cfg.sampling_params = config.sampling_params;
        agent_cfg.initial_messages = reconstructed.messages;

        let agent = Agent::new(provider.clone(), agent_cfg);

        http_cb.take_ownership();
        Ok(Arc::new(Self {
            provider,
            agent,
            session: Arc::new(Mutex::new(session_manager)),
            deferred_registry,
            skills,
            skill_discovery,
            auto_reload_skills: config.auto_reload_skills,
            known_skills: Mutex::new(known_skills),
            instructions: Mutex::new(instructions),
            had_deferred_catalog: Mutex::new(had_deferred_catalog),
            tool_callbacks: tool_cb,
            next_tool_call_id,
            js_runtime,
            api_key: Mutex::new(config.api_key),
            cache_retention: config.cache_retention,
            headers: config.headers,
            rollback_first_turn_on_failure: config.rollback_first_turn_on_failure,
            run_state: Mutex::new(SessionRunState {
                closed: false,
                active: None,
            }),
        }))
    }

    fn is_busy(&self) -> bool {
        let state = self.run_state.lock().unwrap_or_else(|e| e.into_inner());
        state.active.is_some() || self.agent.is_running()
    }

    fn check_open_and_idle(&self) -> Result<(), JimuStatus> {
        let state = self.run_state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return Err(JimuStatus::ErrClosed);
        }
        if state.active.is_some() || self.agent.is_running() {
            return Err(JimuStatus::ErrBusy);
        }
        Ok(())
    }

    fn finish_active_run(&self) -> Vec<oneshot::Sender<()>> {
        let mut state = self.run_state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .active
            .take()
            .map(|r| r.idle_waiters)
            .unwrap_or_default()
    }

    pub(crate) fn start_prompt(
        self: &Arc<Self>,
        req: FfiPromptRequest,
        ctx: SendCallbackCtx,
        on_event: JimuEventFn,
        on_done: JimuStreamDoneFn,
    ) -> Result<(), (JimuStatus, String)> {
        if req.content.is_empty() {
            return Err((
                JimuStatus::ErrInvalidArgument,
                "prompt content must not be empty".to_string(),
            ));
        }

        let prompt_cancel = CancelToken::new();
        {
            let mut state = self.run_state.lock().unwrap_or_else(|e| e.into_inner());
            if state.closed {
                return Err((JimuStatus::ErrClosed, "session is closed".to_string()));
            }
            if state.active.is_some() || self.agent.is_running() {
                return Err((JimuStatus::ErrBusy, "session is currently busy".to_string()));
            }
            state.active = Some(ActivePromptRun {
                cancel: prompt_cancel.clone(),
                idle_waiters: Vec::new(),
            });
        }

        let user_msg = Message::User(
            UserMessage::parts(req.content, now_ms())
                .with_id(req.id)
                .with_meta(req.is_meta),
        )
        .into();

        let (prompt_messages, flushed_count, flushed_events) =
            match self.prepare_turn_prompt_messages(user_msg) {
                Ok(v) => v,
                Err(err) => {
                    let waiters = self.finish_active_run();
                    for w in waiters {
                        let _ = w.send(());
                    }
                    return Err((JimuStatus::ErrIo, err));
                }
            };

        let this = Arc::clone(self);
        global_pool().spawn_ok(async move {
            let emit = move |event: &FfiEvent| {
                if let Ok(json) = serde_json::to_string(event)
                    && let Ok(cstr) = CString::new(json)
                {
                    on_event(ctx.as_ptr(), cstr.as_ptr());
                }
            };

            for ev in flushed_events {
                if let SessionEvent::ContextAppended { name, text } = ev {
                    emit(&FfiEvent::ContextAppended { name, text });
                }
            }

            if prompt_cancel.is_cancelled() || this.is_closed() {
                this.restore_persisted_messages();
                emit(&FfiEvent::TurnEnded {
                    stop_reason: StopReason::Aborted,
                    user_message_kept: false,
                    error_message: Some("Operation aborted".to_string()),
                });
                let waiters = this.finish_active_run();
                on_done(ctx.as_ptr(), JimuStatus::Ok, std::ptr::null());
                for w in waiters {
                    let _ = w.send(());
                }
                return;
            }

            let stream_res = this.agent.prompt_messages_stream(prompt_messages);
            let mut agent_stream = match stream_res {
                Ok(s) => s,
                Err(err) => {
                    let waiters = this.finish_active_run();
                    let err_c = CString::new(err.to_string()).ok();
                    on_done(
                        ctx.as_ptr(),
                        JimuStatus::ErrInternal,
                        err_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
                    );
                    for w in waiters {
                        let _ = w.send(());
                    }
                    return;
                }
            };
            if prompt_cancel.is_cancelled() || this.is_closed() {
                this.agent.abort_now();
            }

            let mut seen_ends = 0usize;
            let mut deferred_persist: Vec<AgentMessage> = Vec::new();
            let persistence_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
            let mut last_stop_reason = StopReason::Stop;
            let mut last_error_message: Option<String> = None;
            let mut user_message_kept = true;

            while let Some(ev) = agent_stream.next().await {
                match &ev {
                    AgentEvent::MessageEnd { message } => {
                        if seen_ends >= flushed_count {
                            if this.rollback_first_turn_on_failure {
                                deferred_persist.push(message.clone());
                            } else {
                                let persist_res = {
                                    let mut session =
                                        this.session.lock().unwrap_or_else(|e| e.into_inner());
                                    session.append_message(message.clone())
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
                                    this.agent.abort_now();
                                }
                            }
                        }
                        seen_ends += 1;
                    }
                    AgentEvent::TurnStart => {
                        emit(&FfiEvent::TurnStarted);
                    }
                    AgentEvent::MessageUpdate {
                        assistant_message_event,
                        ..
                    } => match assistant_message_event {
                        AssistantMessageEvent::TextDelta {
                            content_index,
                            delta,
                        } => {
                            emit(&FfiEvent::TextDelta {
                                content_index: *content_index,
                                delta: delta.clone(),
                            });
                        }
                        AssistantMessageEvent::ThinkingDelta {
                            content_index,
                            delta,
                        } => {
                            emit(&FfiEvent::ThinkingDelta {
                                content_index: *content_index,
                                delta: delta.clone(),
                            });
                        }
                        _ => {}
                    },
                    AgentEvent::ToolExecutionStart {
                        tool_call_id,
                        tool_name,
                        args,
                    } => {
                        emit(&FfiEvent::ToolStarted {
                            tool_call_id: tool_call_id.clone(),
                            tool_name: tool_name.clone(),
                            args: args.clone(),
                        });
                    }
                    AgentEvent::ToolExecutionUpdate {
                        tool_call_id,
                        tool_name,
                        args,
                        partial_result,
                    } => {
                        emit(&FfiEvent::ToolUpdated {
                            tool_call_id: tool_call_id.clone(),
                            tool_name: tool_name.clone(),
                            args: args.clone(),
                            partial_result: partial_result.clone(),
                        });
                    }
                    AgentEvent::ToolExecutionEnd {
                        tool_call_id,
                        tool_name,
                        result,
                        is_error,
                    } => {
                        emit(&FfiEvent::ToolFinished {
                            tool_call_id: tool_call_id.clone(),
                            tool_name: tool_name.clone(),
                            result: result.clone(),
                            is_error: *is_error,
                        });
                    }
                    AgentEvent::RequestUsage {
                        model,
                        provider,
                        usage,
                    } => {
                        emit(&FfiEvent::RequestUsage {
                            model: model.clone(),
                            provider: provider.clone(),
                            usage: usage.clone(),
                        });
                    }
                    AgentEvent::Steered { id, text } => {
                        emit(&FfiEvent::Steered {
                            id: id.clone(),
                            text: text.clone(),
                        });
                    }
                    AgentEvent::TurnEnd { message, .. } => {
                        last_stop_reason = message.stop_reason;
                        last_error_message = message.error_message.clone();
                    }
                    AgentEvent::AgentEnd {
                        user_message_kept: kept,
                        ..
                    } => {
                        user_message_kept = *kept;
                    }
                    AgentEvent::AgentStart | AgentEvent::MessageStart { .. } => {}
                }
            }

            if this.rollback_first_turn_on_failure && user_message_kept {
                let mut session = this.session.lock().unwrap_or_else(|e| e.into_inner());
                for msg in deferred_persist {
                    if let Err(err) = session.append_message(msg) {
                        let mut slot = persistence_error.lock().unwrap_or_else(|e| e.into_inner());
                        if slot.is_none() {
                            *slot = Some(format!("failed to persist session message: {err}"));
                        }
                        break;
                    }
                }
            }

            // Ensure Agent's internal active_run is fully released before auto-compaction.
            this.agent.wait_for_idle().await;

            let persist_err = persistence_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();

            if (flushed_count > 0 && !user_message_kept) || persist_err.is_some() {
                this.restore_persisted_messages();
            }

            // Run auto-compaction if needed, not cancelled, and persistence succeeded.
            if persist_err.is_none() && !prompt_cancel.is_cancelled() && !this.is_closed() {
                let _ = this
                    .run_compaction_internal(true, Some(&emit), prompt_cancel.clone())
                    .await;
            }

            if let Some(err_msg) = persist_err {
                last_stop_reason = StopReason::Error;
                last_error_message = Some(err_msg);
            }

            // Release busy state BEFORE emitting `.turnEnded` and `on_done` so any Swift consumer
            // reacting to `.turnEnded` or exiting `for try await` can immediately call `prompt`/`updateTools`
            // with zero spurious `busy` errors.
            let waiters = this.finish_active_run();

            emit(&FfiEvent::TurnEnded {
                stop_reason: last_stop_reason,
                user_message_kept,
                error_message: last_error_message,
            });

            on_done(ctx.as_ptr(), JimuStatus::Ok, std::ptr::null());
            for w in waiters {
                let _ = w.send(());
            }
        });

        Ok(())
    }

    fn is_closed(&self) -> bool {
        self.run_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .closed
    }

    fn prepare_turn_prompt_messages(
        &self,
        user_msg: AgentMessage,
    ) -> Result<(Vec<AgentMessage>, usize, Vec<SessionEvent>), String> {
        if self.auto_reload_skills
            && let Some(config) = &self.skill_discovery
        {
            let discovered = SkillCatalog::discover_with_config(config);
            *self.skills.lock().unwrap_or_else(|e| e.into_inner()) = discovered;
        }
        let flushed = {
            let mut session = self.session.lock().unwrap_or_else(|e| e.into_inner());
            if self.skill_discovery.is_some() {
                let skills = self.skills.lock().unwrap_or_else(|e| e.into_inner());
                let mut known = self.known_skills.lock().unwrap_or_else(|e| e.into_inner());
                let content = match &user_msg {
                    AgentMessage::Llm(Message::User(user)) if !user.is_meta => {
                        Some(user.content.as_slice())
                    }
                    _ => None,
                };
                known.flush_for_prompt(&mut session, &skills, content)
            } else {
                session.flush_pending_context()
            }
        };
        match flushed {
            Ok(flushed) => Ok(prepare_flushed_prompt(flushed, user_msg)),
            Err(err) => {
                self.restore_persisted_messages();
                Err(format!("failed to flush pending context: {err}"))
            }
        }
    }

    fn restore_persisted_messages(&self) {
        let reconstructed = self
            .session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reconstruct_context();
        let _ = self.agent.replace_messages(reconstructed.messages);
    }

    pub(crate) fn steer(&self, id: String, text: String, is_meta: bool) -> bool {
        let state = self.run_state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed || (state.active.is_none() && !self.agent.is_running()) {
            return false;
        }
        if is_meta {
            self.agent.steer_meta(id, text);
        } else {
            self.agent.steer(id, text);
        }
        true
    }

    pub(crate) fn follow_up(
        &self,
        id: String,
        text: String,
        is_meta: bool,
    ) -> Result<(), (JimuStatus, String)> {
        if self.is_closed() {
            return Err((JimuStatus::ErrClosed, "session is closed".to_string()));
        }
        if is_meta {
            self.agent.follow_up_meta(id, text);
        } else {
            self.agent.follow_up(id, text);
        }
        Ok(())
    }

    pub(crate) fn interrupt_async(self: &Arc<Self>, ctx: SendCallbackCtx, on_done: JimuVoidDoneFn) {
        let waiter = {
            let mut state = self.run_state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(active) = state.active.as_mut() {
                active.cancel.cancel();
                self.agent.abort_now();
                let (tx, rx) = oneshot::channel();
                active.idle_waiters.push(tx);
                Some(rx)
            } else {
                None
            }
        };

        global_pool().spawn_ok(async move {
            if let Some(rx) = waiter {
                let _ = rx.await;
            }
            on_done(ctx.as_ptr());
        });
    }

    pub(crate) fn close_async(self: &Arc<Self>, ctx: SendCallbackCtx, on_done: JimuVoidDoneFn) {
        let waiter = {
            let mut state = self.run_state.lock().unwrap_or_else(|e| e.into_inner());
            state.closed = true;
            if let Some(active) = state.active.as_mut() {
                active.cancel.cancel();
                self.agent.abort_now();
                let (tx, rx) = oneshot::channel();
                active.idle_waiters.push(tx);
                Some(rx)
            } else {
                None
            }
        };

        global_pool().spawn_ok(async move {
            if let Some(rx) = waiter {
                let _ = rx.await;
            }
            on_done(ctx.as_ptr());
        });
    }

    pub(crate) fn cancel_for_destroy(&self) {
        let mut state = self.run_state.lock().unwrap_or_else(|e| e.into_inner());
        state.closed = true;
        if let Some(active) = state.active.as_ref() {
            active.cancel.cancel();
        }
        drop(state);
        self.agent.abort_now();
    }

    pub(crate) fn set_context(
        &self,
        name: String,
        text: String,
    ) -> Result<bool, (JimuStatus, String)> {
        if self.is_closed() {
            return Err((JimuStatus::ErrClosed, "session is closed".to_string()));
        }
        let changed = self
            .session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_context(name, text);
        Ok(changed)
    }

    pub(crate) fn append_named_context(
        &self,
        name: String,
        text: String,
    ) -> Result<(), (JimuStatus, String)> {
        self.queue_context(|session| session.append_named_context(name, text))
    }

    pub(crate) fn append_meta(&self, text: String) -> Result<(), (JimuStatus, String)> {
        self.queue_context(|session| session.append_meta(text))
    }

    fn queue_context(
        &self,
        append: impl FnOnce(&mut SessionManager),
    ) -> Result<(), (JimuStatus, String)> {
        if self.is_closed() {
            return Err((JimuStatus::ErrClosed, "session is closed".to_string()));
        }
        append(&mut self.session.lock().unwrap_or_else(|e| e.into_inner()));
        Ok(())
    }

    pub(crate) fn set_model(&self, model: Model) -> Result<(), (JimuStatus, String)> {
        self.check_open_and_idle()
            .map_err(|s| (s, status_message(s)))?;
        self.agent
            .set_model(model.clone())
            .map_err(|e| (JimuStatus::ErrBusy, e.to_string()))?;
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .append_model_change(model.provider, model.id)
            .map_err(|e| (JimuStatus::ErrIo, e.to_string()))?;
        Ok(())
    }

    pub(crate) fn set_thinking_level(
        &self,
        level: ThinkingLevel,
    ) -> Result<(), (JimuStatus, String)> {
        self.check_open_and_idle()
            .map_err(|s| (s, status_message(s)))?;
        self.agent
            .set_thinking_level(level)
            .map_err(|e| (JimuStatus::ErrBusy, e.to_string()))?;
        let fast = self.agent.fast();
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .append_thinking_change(level, fast)
            .map_err(|e| (JimuStatus::ErrIo, e.to_string()))?;
        Ok(())
    }

    pub(crate) fn set_fast(&self, fast: bool) -> Result<(), (JimuStatus, String)> {
        self.check_open_and_idle()
            .map_err(|s| (s, status_message(s)))?;
        self.agent
            .set_fast(fast)
            .map_err(|e| (JimuStatus::ErrBusy, e.to_string()))?;
        let level = self.agent.thinking_level();
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .append_thinking_change(level, fast)
            .map_err(|e| (JimuStatus::ErrIo, e.to_string()))?;
        Ok(())
    }

    pub(crate) fn set_api_key(&self, api_key: Option<String>) -> Result<(), (JimuStatus, String)> {
        self.check_open_and_idle()
            .map_err(|s| (s, status_message(s)))?;
        self.agent
            .set_api_key(api_key.clone())
            .map_err(|e| (JimuStatus::ErrBusy, e.to_string()))?;
        *self.api_key.lock().unwrap_or_else(|e| e.into_inner()) = api_key;
        Ok(())
    }

    pub(crate) fn upsert_tools(&self, specs: Vec<FfiToolSpec>) -> Result<(), (JimuStatus, String)> {
        self.check_open_and_idle()
            .map_err(|s| (s, status_message(s)))?;
        let resident_defs = self.agent.tool_definitions();
        for spec in &specs {
            if resident_defs.iter().any(|t| t.name == spec.name) {
                return Err((
                    JimuStatus::ErrInvalidArgument,
                    format!(
                        "cannot upsert tool `{}`: collides with a resident tool",
                        spec.name
                    ),
                ));
            }
        }
        let tools: Vec<Arc<dyn AgentTool>> = specs
            .iter()
            .map(|spec| {
                FfiAgentTool::from_spec(
                    spec,
                    self.tool_callbacks.clone(),
                    Arc::clone(&self.next_tool_call_id),
                )
            })
            .collect();
        self.deferred_registry.upsert(tools);
        self.sync_deferred_tools_context();
        Ok(())
    }

    pub(crate) fn remove_tools(&self, names: &[String]) -> Result<usize, (JimuStatus, String)> {
        self.check_open_and_idle()
            .map_err(|s| (s, status_message(s)))?;
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let removed = self.deferred_registry.remove(&refs);
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

    pub(crate) fn compact_async(self: &Arc<Self>, ctx: SendCallbackCtx, on_done: JimuResultDoneFn) {
        let cancel = CancelToken::new();
        {
            let mut state = self.run_state.lock().unwrap_or_else(|e| e.into_inner());
            if state.closed {
                let err_c = CString::new("session is closed").ok();
                on_done(
                    ctx.as_ptr(),
                    JimuStatus::ErrClosed,
                    std::ptr::null(),
                    err_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
                );
                return;
            }
            if state.active.is_some() || self.agent.is_running() {
                let err_c = CString::new("session is currently busy").ok();
                on_done(
                    ctx.as_ptr(),
                    JimuStatus::ErrBusy,
                    std::ptr::null(),
                    err_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
                );
                return;
            }
            state.active = Some(ActivePromptRun {
                cancel: cancel.clone(),
                idle_waiters: Vec::new(),
            });
        }

        let this = Arc::clone(self);
        global_pool().spawn_ok(async move {
            let outcome = this.run_compaction_internal(false, None, cancel).await;
            let waiters = this.finish_active_run();
            match outcome {
                Ok(Some(res)) => {
                    let payload = FfiCompactionResultJson {
                        entry_id: res.entry_id,
                        summary: res.summary,
                        first_kept_entry_id: res.first_kept_entry_id,
                        tokens_before: res.tokens_before,
                        usage: res.usage,
                    };
                    match serde_json::to_string(&payload)
                        .ok()
                        .and_then(|s| CString::new(s).ok())
                    {
                        Some(json_c) => on_done(
                            ctx.as_ptr(),
                            JimuStatus::Ok,
                            json_c.as_ptr(),
                            std::ptr::null(),
                        ),
                        None => {
                            let err_c = CString::new("failed to serialize compaction result").ok();
                            on_done(
                                ctx.as_ptr(),
                                JimuStatus::ErrInternal,
                                std::ptr::null(),
                                err_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
                            );
                        }
                    }
                }
                Ok(None) => {
                    on_done(
                        ctx.as_ptr(),
                        JimuStatus::Ok,
                        std::ptr::null(),
                        std::ptr::null(),
                    );
                }
                Err(err) => {
                    let err_c = CString::new(err).ok();
                    on_done(
                        ctx.as_ptr(),
                        JimuStatus::ErrInternal,
                        std::ptr::null(),
                        err_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
                    );
                }
            }
            for w in waiters {
                let _ = w.send(());
            }
        });
    }

    async fn run_compaction_internal(
        &self,
        auto_only: bool,
        emit: Option<&(dyn Fn(&FfiEvent) + Send + Sync)>,
        cancel: CancelToken,
    ) -> Result<Option<CompactionResult>, String> {
        let model = self.agent.model();
        let level = self.agent.thinking_level();
        let current_api_key = self
            .api_key
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let options = SimpleStreamOptions {
            api_key: current_api_key,
            reasoning: (level != ThinkingLevel::Off).then_some(level),
            fast: Some(self.agent.fast()),
            temperature: None,
            max_tokens: None,
            cache_retention: self.cache_retention,
            session_id: Some(self.session_id()),
            headers: self.headers.clone(),
            sampling_params: Map::new(),
            cancel,
        };

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
        if let Some(cb) = emit {
            cb(&FfiEvent::CompactionStarted { tokens_before });
        }

        let output = SessionManager::execute_prepared_compaction(&*self.provider, prepared)
            .await
            .map_err(|e| e.to_string())?;

        let (maybe_result, reconstructed) = {
            let mut session = self.session.lock().unwrap_or_else(|e| e.into_inner());
            let res = session
                .commit_compaction_if_leaf_matches(output, tokens_before, expected_leaf.as_deref())
                .map_err(|e| e.to_string())?;
            let recon = session.reconstruct_context();
            (res, recon)
        };

        let Some(result) = maybe_result else {
            return Ok(None);
        };

        self.deferred_registry.clear_seen_schemas();
        if self.skill_discovery.is_some() {
            let _ = self.reload_skills();
            self.known_skills
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .reset_after_compaction();
        }
        self.agent
            .replace_messages(reconstructed.messages)
            .map_err(|e| e.to_string())?;

        if let Some(cb) = emit {
            cb(&FfiEvent::CompactionEnded {
                summary: result.summary.clone(),
                tokens_before: result.tokens_before,
                first_kept_entry_id: result.first_kept_entry_id.clone(),
                usage: result.usage.clone(),
            });
        }

        Ok(Some(result))
    }

    pub(crate) fn fork(&self, entry_id: &str) -> Result<(), (JimuStatus, String)> {
        self.check_open_and_idle()
            .map_err(|s| (s, status_message(s)))?;
        let reconstructed = {
            let mut session = self.session.lock().unwrap_or_else(|e| e.into_inner());
            session
                .fork(entry_id)
                .map_err(|e| (JimuStatus::ErrNotFound, e.to_string()))?;
            let reconstructed = session.reconstruct_context();
            self.instructions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .sync_branch(&mut session, &reconstructed);
            if self.skill_discovery.is_some() {
                self.known_skills
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .restore_fork(
                        &mut session,
                        reconstructed
                            .all_branch_context_blocks
                            .get(SKILLS_CONTEXT_NAME)
                            .map(String::as_str),
                    );
            }
            reconstructed
        };
        self.deferred_registry.clear_seen_schemas();
        self.agent
            .replace_messages(reconstructed.messages)
            .map_err(|e| (JimuStatus::ErrBusy, e.to_string()))?;
        Ok(())
    }

    pub(crate) fn entries(&self) -> Result<Vec<SessionEntry>, (JimuStatus, String)> {
        if self.is_closed() {
            return Err((JimuStatus::ErrClosed, "session is closed".to_string()));
        }
        Ok(self
            .session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries()
            .to_vec())
    }

    pub(crate) fn session_id(&self) -> String {
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .session_id()
            .to_string()
    }

    pub(crate) fn info(&self) -> Result<FfiSessionInfo, (JimuStatus, String)> {
        if self.is_closed() {
            return Err((JimuStatus::ErrClosed, "session is closed".to_string()));
        }
        let (session_id, leaf_id) = {
            let s = self.session.lock().unwrap_or_else(|e| e.into_inner());
            (
                s.session_id().to_string(),
                s.leaf_id().map(ToString::to_string),
            )
        };
        let deferred_tools = self
            .deferred_registry
            .list_tools()
            .into_iter()
            .map(|t| t.definition())
            .collect();
        Ok(FfiSessionInfo {
            session_id,
            leaf_id,
            model: self.agent.model(),
            thinking_level: self.agent.thinking_level(),
            fast: self.agent.fast(),
            is_running: self.is_busy(),
            resident_tools: self.agent.tool_definitions(),
            deferred_tools,
        })
    }

    pub(crate) fn reload_extensions(&self) -> Result<(), (JimuStatus, String)> {
        self.check_open_and_idle()
            .map_err(|s| (s, status_message(s)))?;
        if let Some(rt) = &self.js_runtime {
            rt.reload()
                .map_err(|e| (JimuStatus::ErrInternal, e.to_string()))?;
        }
        Ok(())
    }

    pub(crate) fn reload_skills(&self) -> Result<bool, (JimuStatus, String)> {
        if self.is_closed() {
            return Err((JimuStatus::ErrClosed, "session is closed".to_string()));
        }
        let Some(config) = &self.skill_discovery else {
            return Ok(false);
        };
        let mut skills = self.skills.lock().unwrap_or_else(|e| e.into_inner());
        let discovered = SkillCatalog::discover_with_config(config);
        let changed = !skills.has_same_visible_skills(&discovered);
        *skills = discovered;
        Ok(changed)
    }

    pub(crate) fn reload_instructions(&self) -> Result<bool, (JimuStatus, String)> {
        if self.is_closed() {
            return Err((JimuStatus::ErrClosed, "session is closed".into()));
        }
        let mut session = self.session.lock().unwrap_or_else(|e| e.into_inner());
        let changed = self
            .instructions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reload(&mut session);
        Ok(changed)
    }
}

fn status_message(status: JimuStatus) -> String {
    match status {
        JimuStatus::ErrClosed => "session is closed".to_string(),
        JimuStatus::ErrBusy => "session is currently busy".to_string(),
        _ => format!("error status {:?}", status),
    }
}
