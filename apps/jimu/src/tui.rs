//! Interactive terminal UI host (`--features tui`), built on the shared [`HostConfig`]
//! assembly pipeline used across TUI, ACP, RPC, and one-shot (`-p`) modes.

use crate::{
    host::{
        BuiltWorkspaceTools, HostConfig, LoadedExtensions, SessionFileLock,
        apply_session_model_on_open, build_auth_provider, build_http_transport,
        build_instruction_discovery, build_skill_discovery, build_workspace_tools,
        canonical_jsonl_path, discover_extension_paths, load_js_extensions,
        make_background_callback, paths_same_dir, reserved_extension_tool_names,
        resolve_existing_session_cwd, resolve_system_prompt, should_enable_deferred_gateway,
        validate_extension_tools,
    },
    mcp::McpHub,
    providers::{ProviderStore, model_key},
    settings::{UserSettings, discover_custom_themes},
    status_line::{StatusLineRunner, StatusLineSnapshot},
    ui_mod::UiModBridge,
};
use jimu_agent::{
    AfterToolCallCtx, AfterToolCallResult, AgentContextSnapshot, AgentMessage, AgentMiddleware,
    BeforeToolCallCtx, BeforeToolCallResult, FinishTurnCtx, QueueMode, RequestOverride,
    TurnDecision,
};
use jimu_ai::{BoxFuture, CancelToken, ContentPart, HttpTransport, Message, Model, StreamProvider};
use jimu_js::UiComponent;
#[cfg(not(target_os = "ios"))]
use jimu_kit::SkillWatcher;
use jimu_kit::{
    BackgroundEventSender, CALL_TOOL_NAME, CODEMODE_EXEC_TOOL_NAME, KitSession, KitSessionConfig,
    KitToolSpec,
};
use jimu_session::{JsonlSessionStore, SessionEntryPayload, SessionStore};
#[cfg(not(target_os = "ios"))]
use jimu_tools::{MonitorManager, ProcessSessionManager};
use jimu_tui::{
    app::{ScreenMode, TuiApp, TuiAppConfig},
    engine::theme::{Theme, ThemeKind, ThemeOverrides},
    harness::{
        BoxFuture as TuiBoxFuture, ChromeConfig, ChromeSlots, ChromeState, CompletionItem, McpCap,
        McpServerSummary, NoticeLevel, PermissionModeCap, PermissionModeOption, SessionOption,
        SlashCommand, SlashContext, SurfaceDecoration, ToolRowView, TuiController,
        render_default_status_line_with_spinner,
    },
    jimu::{JimuApprovalMiddleware, JimuHarness, JimuInteractionSink, JimuSessionController},
};
use ratatui::text::{Line, Span};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, RwLock},
    time::{Instant, UNIX_EPOCH},
};
use uuid::Uuid;

/// Launch configuration for interactive TUI mode, embedding [`HostConfig`] so TUI, ACP, RPC,
/// and `-p` share one model, auth, tool, MCP, skill, instruction, and session pipeline.
#[derive(Clone)]
pub struct TuiLaunchConfig {
    pub host: HostConfig,
    pub session_file: Option<PathBuf>,
    pub continue_last: bool,
    pub open_resume_picker: bool,
    pub theme: Option<String>,
    pub initial_prompt: Option<String>,
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

struct SharedMcpCap {
    hub: Arc<McpHub>,
}

impl McpCap for SharedMcpCap {
    fn list_mcp_servers(&self) -> Vec<McpServerSummary> {
        self.hub
            .summaries()
            .into_iter()
            .map(|(name, tool_count)| McpServerSummary {
                name,
                status: "●".into(),
                tool_count,
            })
            .collect()
    }
}

struct SharedPermissionMode {
    mode: Arc<Mutex<PermissionMode>>,
    jimu_home: PathBuf,
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
        let _ = UserSettings::update(&self.jimu_home, |settings| {
            settings.default_permission_mode = Some(parsed.label().to_owned());
        });
        Ok(())
    }
}

/// Per-session permission middleware supporting live `/permissions` and `Shift+Tab` switching
/// while keeping "always allow" grants scoped to the active session.
struct DynamicPermissionMiddleware {
    approval: JimuApprovalMiddleware,
}

impl DynamicPermissionMiddleware {
    fn new(
        mode: Arc<Mutex<PermissionMode>>,
        enabled_builtins: Arc<HashSet<String>>,
        sink: JimuInteractionSink,
    ) -> Self {
        let approval = JimuApprovalMiddleware::new(sink, move |ctx| {
            let current = *mode.lock().unwrap_or_else(|e| e.into_inner());
            is_approval_required(current, ctx.tool_call.name.as_str(), &enabled_builtins)
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
    transcript_path: Arc<RwLock<Option<PathBuf>>>,
    project_dir: PathBuf,
    started_at: Instant,
    status_line_runner: Option<Arc<StatusLineRunner>>,
    ui_mod: Arc<UiModBridge>,
}

impl DynamicChromeSlots {
    fn build_status_line_snapshot(&self, state: &ChromeState<'_>) -> StatusLineSnapshot {
        let session = self
            .active_session
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let model = session.model();
        let transcript_path = self
            .transcript_path
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (context_window_size, used_percentage) = match state.context {
            Some(ctx) => {
                let cap = u64::from(ctx.context_window);
                let used = ctx.tokens.unwrap_or(ctx.estimated_tokens);
                let pct = ctx.percent.unwrap_or_else(|| {
                    if cap > 0 {
                        (used as f64 / cap as f64) * 100.0
                    } else {
                        0.0
                    }
                });
                (cap, pct)
            }
            None => (u64::from(model.context_window), 0.0),
        };
        StatusLineSnapshot {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            session_id: state.session_id.unwrap_or_default().to_owned(),
            transcript_path,
            model_id: model.id.clone(),
            model_display_name: state.model_name.unwrap_or(&model.name).to_owned(),
            current_dir: state.cwd.to_string_lossy().into_owned(),
            project_dir: self.project_dir.to_string_lossy().into_owned(),
            git_branch: state.git_branch.unwrap_or_default().to_owned(),
            context_window_size,
            used_percentage,
            input_tokens: state.usage.input_tokens,
            output_tokens: state.usage.output_tokens,
            cache_read_input_tokens: state.usage.cache_read_tokens,
            cache_creation_input_tokens: state.usage.cache_write_tokens,
            total_cost_usd: state.usage.cost_usd,
            total_duration_ms: self.started_at.elapsed().as_millis() as u64,
            effort: state.effort.unwrap_or_default().to_owned(),
            fast_mode: state.fast_mode || session.fast(),
            permission_mode: state.permission_mode.unwrap_or_default().to_owned(),
            running: state.running,
        }
    }
}

impl ChromeSlots for DynamicChromeSlots {
    fn header(&self, state: &ChromeState<'_>) -> Vec<Line<'static>> {
        self.ui_mod
            .render_chrome_slot(UiComponent::Header, state, &[])
            .unwrap_or_default()
    }

    fn above_prompt(&self, state: &ChromeState<'_>) -> Vec<Line<'static>> {
        self.ui_mod
            .render_chrome_slot(UiComponent::AbovePrompt, state, &[])
            .unwrap_or_default()
    }

    fn footer(&self, state: &ChromeState<'_>) -> Vec<Line<'static>> {
        self.ui_mod
            .render_chrome_slot(UiComponent::Footer, state, &[])
            .unwrap_or_default()
    }

    fn status_line(&self, state: &ChromeState<'_>) -> Option<Vec<Line<'static>>> {
        let shell_lines = if let Some(runner) = &self.status_line_runner {
            runner.update_snapshot(self.build_status_line_snapshot(state));
            runner.current_lines()
        } else {
            None
        };

        if self.ui_mod.is_active() {
            let spinner_override = self.ui_mod.spinner_override(state);
            let native_lines = match &shell_lines {
                Some(lines) if !lines.is_empty() => lines.clone(),
                _ => vec![render_default_status_line_with_spinner(
                    state,
                    spinner_override,
                )],
            };
            if let Some(lines) =
                self.ui_mod
                    .render_chrome_slot(UiComponent::StatusLine, state, &native_lines)
            {
                return Some(lines);
            }
        }

        shell_lines
    }

    fn status_flags(&self, state: &ChromeState<'_>) -> Vec<String> {
        if state.fast_mode {
            return vec!["fast".into()];
        }
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

    fn decorate_user_message(
        &self,
        text: &str,
        image_count: usize,
        width: u16,
        theme: &Theme,
    ) -> Option<SurfaceDecoration> {
        self.ui_mod
            .user_message_decoration(text, image_count, width, theme)
    }

    fn decorate_tool_row(&self, row: &ToolRowView<'_>, theme: &Theme) -> Option<SurfaceDecoration> {
        self.ui_mod.tool_row_decoration(row, theme)
    }

    fn spinner_override(&self, state: &ChromeState<'_>) -> Option<Span<'static>> {
        self.ui_mod.spinner_override(state)
    }
}

/// Delegates agent lifecycle hooks to whichever [`JsExtensionRuntime`] is currently loaded so
/// `/reload` can activate newly added extensions or swap runtimes without restarting the session.
struct DynamicJsMiddleware {
    extensions: Arc<RwLock<Option<Arc<LoadedExtensions>>>>,
}

impl DynamicJsMiddleware {
    fn current_runtime(&self) -> Option<Arc<jimu_js::JsExtensionRuntime>> {
        self.extensions
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|ext| Arc::clone(&ext.runtime))
    }
}

impl AgentMiddleware for DynamicJsMiddleware {
    fn prepare_request<'a>(
        &'a self,
        snapshot: &'a AgentContextSnapshot,
        cancel: CancelToken,
    ) -> BoxFuture<'a, Option<RequestOverride>> {
        let Some(rt) = self.current_runtime() else {
            return Box::pin(async { None });
        };
        Box::pin(async move { rt.prepare_request(snapshot, cancel).await })
    }

    fn transform_context<'a>(
        &'a self,
        messages: Vec<AgentMessage>,
        cancel: CancelToken,
    ) -> BoxFuture<'a, Vec<AgentMessage>> {
        let Some(rt) = self.current_runtime() else {
            return Box::pin(async move { messages });
        };
        Box::pin(async move { rt.transform_context(messages, cancel).await })
    }

    fn before_tool_call<'a>(
        &'a self,
        ctx: BeforeToolCallCtx<'a>,
        cancel: CancelToken,
    ) -> BoxFuture<'a, Option<BeforeToolCallResult>> {
        let Some(rt) = self.current_runtime() else {
            return Box::pin(async { None });
        };
        Box::pin(async move { rt.before_tool_call(ctx, cancel).await })
    }

    fn after_tool_call<'a>(
        &'a self,
        ctx: AfterToolCallCtx<'a>,
        cancel: CancelToken,
    ) -> BoxFuture<'a, Option<AfterToolCallResult>> {
        let Some(rt) = self.current_runtime() else {
            return Box::pin(async { None });
        };
        Box::pin(async move { rt.after_tool_call(ctx, cancel).await })
    }

    fn finish_turn<'a>(
        &'a self,
        ctx: FinishTurnCtx<'a>,
        cancel: CancelToken,
    ) -> BoxFuture<'a, Option<TurnDecision>> {
        let Some(rt) = self.current_runtime() else {
            return Box::pin(async { None });
        };
        Box::pin(async move { rt.finish_turn(ctx, cancel).await })
    }
}

struct ReloadCommand {
    active_session: Arc<RwLock<Arc<KitSession>>>,
    extensions: Arc<RwLock<Option<Arc<LoadedExtensions>>>>,
    loaded_extension_paths: Arc<Mutex<Vec<PathBuf>>>,
    extension_tool_names: Arc<Mutex<Vec<String>>>,
    ui_mod: Arc<UiModBridge>,
    jimu_home: PathBuf,
    cwd: PathBuf,
    cli_extensions: Vec<PathBuf>,
}

impl SlashCommand for ReloadCommand {
    fn name(&self) -> &str {
        "reload"
    }

    fn description(&self) -> &str {
        "Hot-reload JS extensions, UI mods, and custom themes"
    }

    fn run(&self, ctx: SlashContext) -> TuiBoxFuture<'_, ()> {
        Box::pin(async move {
            let session = self
                .active_session
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();

            // Rescan custom JSON themes off the UI event loop.
            let theme_home = self.jimu_home.clone();
            let theme_cwd = self.cwd.clone();
            let (themes, theme_warnings) = match tokio::task::spawn_blocking(move || {
                discover_custom_themes(&theme_home, &theme_cwd)
            })
            .await
            {
                Ok(res) => res,
                Err(err) => {
                    ctx.controller.push_notice(
                        NoticeLevel::Error,
                        format!("Failed to reload custom themes: {err}"),
                    );
                    return;
                }
            };
            let theme_count = themes.len();
            ctx.controller.replace_custom_themes(themes);
            for warning in theme_warnings {
                ctx.controller.push_notice(NoticeLevel::Warning, warning);
            }

            if session.is_running() {
                ctx.controller.push_notice(
                    NoticeLevel::Warning,
                    "Cannot reload extensions while a turn is running",
                );
                ctx.controller.invalidate_history_layout();
                return;
            }

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
            let prev_paths = self
                .loaded_extension_paths
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let prev_runtime = self
                .extensions
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(|ext| Arc::clone(&ext.runtime));

            let ext_home = self.jimu_home.clone();
            let ext_cwd = self.cwd.clone();
            let cli_extensions = self.cli_extensions.clone();
            let reload_result = tokio::task::spawn_blocking(move || {
                let saved = UserSettings::load(&ext_home);
                let new_paths = discover_extension_paths(
                    &ext_home,
                    &ext_cwd,
                    &saved.extensions,
                    &cli_extensions,
                );
                if new_paths.is_empty() {
                    return Ok::<_, String>((new_paths, None));
                }
                if new_paths == prev_paths
                    && let Some(rt) = prev_runtime
                {
                    rt.reload_excluding(forbidden)
                        .map_err(|err| format!("Failed to reload JS extensions: {err}"))?;
                    let tools = rt.tools();
                    return Ok((
                        new_paths,
                        Some(Arc::new(LoadedExtensions { runtime: rt, tools })),
                    ));
                }
                let loaded = load_js_extensions(&new_paths, &non_extension_names)?;
                Ok((new_paths, loaded.map(Arc::new)))
            })
            .await;

            let (new_paths, new_loaded) = match reload_result {
                Ok(Ok(pair)) => pair,
                Ok(Err(err)) => {
                    ctx.controller.push_notice(NoticeLevel::Error, err);
                    ctx.controller.invalidate_history_layout();
                    return;
                }
                Err(err) => {
                    ctx.controller.push_notice(
                        NoticeLevel::Error,
                        format!("Failed to reload JS extensions: {err}"),
                    );
                    ctx.controller.invalidate_history_layout();
                    return;
                }
            };

            let new_tools = new_loaded
                .as_ref()
                .map(|loaded| loaded.tools.clone())
                .unwrap_or_default();
            let has_gateway = session
                .resident_tool_definitions()
                .into_iter()
                .any(|def| def.name == CALL_TOOL_NAME || def.name == CODEMODE_EXEC_TOOL_NAME);

            let old_refs: Vec<&str> = old_names.iter().map(String::as_str).collect();
            if !old_refs.is_empty()
                && let Err(err) = session.remove_tools(&old_refs)
            {
                ctx.controller.push_notice(
                    NoticeLevel::Error,
                    format!("Failed to detach old extension tools: {err}"),
                );
                return;
            }

            let next_names = if new_tools.is_empty() {
                Vec::new()
            } else if has_gateway {
                let names: Vec<String> = new_tools.iter().map(|t| t.definition().name).collect();
                if let Err(err) = session.upsert_tools(new_tools) {
                    ctx.controller.push_notice(
                        NoticeLevel::Error,
                        format!("Failed to attach reloaded extension tools: {err}"),
                    );
                    return;
                }
                names
            } else {
                ctx.controller.push_notice(
                    NoticeLevel::Info,
                    "New extension tools will activate on /new (current session has no tool gateway)",
                );
                Vec::new()
            };

            *self
                .extension_tool_names
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = next_names;
            *self
                .loaded_extension_paths
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = new_paths;
            *self.extensions.write().unwrap_or_else(|e| e.into_inner()) = new_loaded.clone();

            let js_overrides = new_loaded
                .as_ref()
                .map(|loaded| loaded.runtime.ui_theme_overrides())
                .filter(|colors| !colors.is_empty())
                .map(|colors| ThemeOverrides { base: None, colors });
            ctx.controller.set_theme_overrides(js_overrides);

            self.ui_mod.set_runtime(
                new_loaded
                    .as_ref()
                    .map(|loaded| Arc::clone(&loaded.runtime)),
            );

            let ext_count = new_loaded
                .as_ref()
                .map(|loaded| loaded.runtime.loaded_extensions().len())
                .unwrap_or(0);
            ctx.controller.show_toast(format!(
                "Reloaded {ext_count} extension(s), {theme_count} theme(s)"
            ));
            ctx.controller.invalidate_history_layout();
        })
    }
}

struct FastCommand {
    active_session: Arc<RwLock<Arc<KitSession>>>,
    jimu_home: PathBuf,
}

impl SlashCommand for FastCommand {
    fn name(&self) -> &str {
        "fast"
    }

    fn description(&self) -> &str {
        "Toggle fast mode (/fast [on|off])"
    }

    fn completions(&self, query: &str) -> Vec<CompletionItem> {
        let q = query.to_ascii_lowercase();
        let active = self
            .active_session
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .fast();
        [("on", active), ("off", !active)]
            .into_iter()
            .filter(|(label, _)| label.contains(&q))
            .map(|(label, is_cur)| CompletionItem {
                token: label.to_owned(),
                description: is_cur.then(|| "●".to_owned()),
            })
            .collect()
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
                    let _ = UserSettings::update(&self.jimu_home, |settings| {
                        settings.fast = Some(next);
                    });
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

struct BuiltTuiSession {
    session: Arc<KitSession>,
    built_tools: Arc<BuiltWorkspaceTools>,
    attached_extension_tools: Vec<String>,
    warnings: Vec<String>,
}

struct SessionTemplate {
    host: HostConfig,
    transport: Arc<dyn HttpTransport>,
    provider_store: Arc<ProviderStore>,
    provider: Arc<dyn StreamProvider>,
    active_model: Model,
    explicit_model_override: bool,
    shared_mode: Arc<Mutex<PermissionMode>>,
    sink: JimuInteractionSink,
    extensions: Arc<RwLock<Option<Arc<LoadedExtensions>>>>,
    mcp_hub: Arc<McpHub>,
}

impl SessionTemplate {
    async fn build(
        &self,
        store: Option<Arc<dyn SessionStore>>,
        restore_model: bool,
    ) -> Result<BuiltTuiSession, String> {
        let host = &self.host;
        let cwd = &host.cwd;

        let mut opts = KitSessionConfig::new(self.active_model.clone());
        opts.store = store;
        opts.cwd = Some(cwd.to_string_lossy().into_owned());
        opts.thinking_level = host.thinking;
        opts.fast = host.fast;
        opts.enable_codemode = host.enable_codemode;
        opts.steering_mode = QueueMode::All;
        opts.compaction.compaction_reserve_tokens = host.compact_tokens;
        opts.skill_discovery = build_skill_discovery(
            cwd,
            &host.jimu_home,
            host.skill_walk_boundary,
            &host.skill_dirs,
            &[],
        );
        opts.instruction_discovery = build_instruction_discovery(
            host.instruction_discovery.as_ref(),
            cwd,
            &host.jimu_home,
            &[],
        );

        // Each session gets its own dedicated background sender and workspace process/monitor
        // managers so switching sessions never leaks events or background tasks across sessions.
        let bg_sender: Arc<OnceLock<BackgroundEventSender>> = Arc::new(OnceLock::new());
        let bg_slot = Arc::clone(&bg_sender);
        let bg_callback = make_background_callback(move || bg_slot.get().cloned());
        let built_tools = build_workspace_tools(
            cwd,
            &self.transport,
            &host.tools,
            true,
            Some(bg_callback),
            None,
        );
        let enabled_builtins: Arc<HashSet<String>> = Arc::new(
            built_tools
                .tools
                .iter()
                .map(|tool| tool.definition().name)
                .collect(),
        );
        opts.tools
            .extend(built_tools.tools.iter().cloned().map(KitToolSpec::resident));

        let current_extensions = self
            .extensions
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let attached_extension_tools = if let Some(loaded) = current_extensions.as_ref() {
            let existing_names: HashSet<String> = opts
                .tools
                .iter()
                .map(|spec| spec.tool.definition().name)
                .collect();
            if let Err(err) = validate_extension_tools(&loaded.tools, &existing_names) {
                built_tools.shutdown().await;
                return Err(err);
            }
            let mut names = Vec::with_capacity(loaded.tools.len());
            for tool in &loaded.tools {
                names.push(tool.definition().name);
                opts.tools.push(KitToolSpec::deferred(Arc::clone(tool)));
            }
            names
        } else {
            Vec::new()
        };
        let has_extension_tools = !attached_extension_tools.is_empty();

        // Install dynamic JS middleware BEFORE approval middleware so /reload activates hooks
        // seamlessly and user approval always evaluates the final post-hook arguments (matching
        // ACP middleware ordering). Each session receives its own JimuApprovalMiddleware instance
        // so "always allow" grants never leak across sessions.
        opts.middlewares.push(Arc::new(DynamicJsMiddleware {
            extensions: Arc::clone(&self.extensions),
        }));
        opts.middlewares
            .push(Arc::new(DynamicPermissionMiddleware::new(
                Arc::clone(&self.shared_mode),
                enabled_builtins,
                self.sink.clone(),
            )));

        let has_mcp = !self.mcp_hub.is_empty();
        opts.enable_deferred_gateway = should_enable_deferred_gateway(
            &host.tools,
            host.enable_codemode,
            has_extension_tools,
            has_mcp,
        );
        let has_tools = !opts.tools.is_empty() || host.enable_codemode || has_mcp;
        opts.system_prompt = resolve_system_prompt(
            host.system_prompt.as_deref(),
            cwd,
            &host.jimu_home,
            &host.tools,
            has_tools,
        );

        let requested_model = opts.model.clone();
        let session = match KitSession::new(Arc::clone(&self.provider), opts) {
            Ok(session) => Arc::new(session),
            Err(err) => {
                built_tools.shutdown().await;
                return Err(format!("failed to initialize session: {err}"));
            }
        };
        let _ = bg_sender.set(session.background_sender());

        if let Err(err) = apply_session_model_on_open(
            &session,
            &self.provider_store,
            &requested_model,
            restore_model && !self.explicit_model_override,
            self.host.api.clone(),
            self.host.base_url.clone(),
            self.host.context_window,
        ) {
            built_tools.shutdown().await;
            return Err(err);
        }

        let warnings = if !self.mcp_hub.is_empty() {
            match self
                .mcp_hub
                .attach_to_session(&session, CancelToken::new())
                .await
            {
                Ok(warnings) => warnings,
                Err(err) => {
                    built_tools.shutdown().await;
                    return Err(err.to_string());
                }
            }
        } else {
            Vec::new()
        };

        Ok(BuiltTuiSession {
            session,
            built_tools: Arc::new(built_tools),
            attached_extension_tools,
            warnings,
        })
    }
}

struct ActiveSessionState {
    id: String,
    path: Option<PathBuf>,
    auto_created: bool,
    _file_lock: Option<SessionFileLock>,
    built_tools: Arc<BuiltWorkspaceTools>,
}

struct TuiSessionManager {
    template: SessionTemplate,
    sessions_dir: Option<PathBuf>,
    cwd: PathBuf,
    active_session: Arc<RwLock<Arc<KitSession>>>,
    transcript_path: Arc<RwLock<Option<PathBuf>>>,
    extension_tool_names: Arc<Mutex<Vec<String>>>,
    #[cfg(not(target_os = "ios"))]
    skill_watcher: Mutex<Option<SkillWatcher>>,
    state: Mutex<ActiveSessionState>,
    switch_lock: tokio::sync::Mutex<()>,
}

impl TuiSessionManager {
    fn cleanup_empty_session(session: &KitSession, state: &ActiveSessionState) {
        if !state.auto_created {
            return;
        }
        let Some(path) = &state.path else {
            return;
        };
        // Check the entire session tree rather than `visible_messages()` so rewinding before the
        // first user turn never deletes a file containing sibling branches. Keep the advisory
        // `.lock` file stable on disk so concurrent Unix `flock` callers never race on inodes.
        let has_any_message = session
            .entries()
            .iter()
            .any(|entry| matches!(entry.payload, SessionEntryPayload::Message { .. }));
        if !has_any_message {
            let _ = fs::remove_file(path);
        }
    }

    async fn shutdown_current(&self) {
        let _guard = self.switch_lock.lock().await;
        let session = self
            .active_session
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let tools = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            Self::cleanup_empty_session(&session, &state);
            Arc::clone(&state.built_tools)
        };
        tools.shutdown().await;
    }

    async fn activate(
        &self,
        next_session: Arc<KitSession>,
        next_state: ActiveSessionState,
        next_extension_tools: Vec<String>,
    ) {
        // Shut down the outgoing session's background tools BEFORE mutating active state so
        // cancelling the switch future at the await point never leaves active_session, file lock,
        // and harness diverged. The actual swap below is synchronous with zero await points.
        let prev_tools = Arc::clone(
            &self
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .built_tools,
        );
        prev_tools.shutdown().await;

        let prev_session = {
            let mut slot = self
                .active_session
                .write()
                .unwrap_or_else(|e| e.into_inner());
            std::mem::replace(&mut *slot, Arc::clone(&next_session))
        };
        let next_path = next_state.path.clone();
        let prev_state = {
            let mut slot = self.state.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::replace(&mut *slot, next_state)
        };
        *self
            .transcript_path
            .write()
            .unwrap_or_else(|e| e.into_inner()) = next_path;
        *self
            .extension_tool_names
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = next_extension_tools;
        #[cfg(not(target_os = "ios"))]
        {
            *self.skill_watcher.lock().unwrap_or_else(|e| e.into_inner()) =
                next_session.watch_skills(|_| {}).ok();
        }
        Self::cleanup_empty_session(&prev_session, &prev_state);
    }
}

impl JimuSessionController for TuiSessionManager {
    fn current_session_id(&self) -> Option<String> {
        Some(
            self.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .id
                .clone(),
        )
    }

    #[cfg(not(target_os = "ios"))]
    fn active_process_manager(&self) -> Option<ProcessSessionManager> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .built_tools
            .processes
            .clone()
    }

    #[cfg(not(target_os = "ios"))]
    fn active_monitor_manager(&self) -> Option<MonitorManager> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .built_tools
            .monitors
            .clone()
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
            Ok(scan_sessions(dir, &current_id, &self.cwd))
        })
    }

    fn open_session(&self, session_id: &str) -> TuiBoxFuture<'_, Result<Arc<KitSession>, String>> {
        let id = session_id.to_owned();
        Box::pin(async move {
            let _guard = self.switch_lock.lock().await;
            let dir = self
                .sessions_dir
                .as_ref()
                .ok_or_else(|| "session persistence is disabled".to_owned())?;
            let raw_path = dir.join(format!("{id}.jsonl"));
            let path = canonical_jsonl_path(&raw_path);
            if !path.is_file() {
                return Err(format!("session `{id}` not found"));
            }
            // If reopening the already-active session, return it without re-acquiring the lock.
            {
                let cur = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if cur.id == id {
                    return Ok(self
                        .active_session
                        .read()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone());
                }
            }
            let file_lock = SessionFileLock::try_acquire(&path)?;
            let recorded_cwd = resolve_existing_session_cwd(&path, &self.cwd)?;
            if !paths_same_dir(&recorded_cwd, &self.cwd) {
                return Err(format!(
                    "session `{id}` belongs to a different directory (`{}`); reopen jimu with `-C`",
                    recorded_cwd.display()
                ));
            }
            let store: Arc<dyn SessionStore> = Arc::new(JsonlSessionStore::new(&path));
            let built = self.template.build(Some(store), true).await?;
            for warning in &built.warnings {
                eprintln!("warning: {warning}");
            }
            self.activate(
                Arc::clone(&built.session),
                ActiveSessionState {
                    id,
                    path: Some(path),
                    auto_created: false,
                    _file_lock: Some(file_lock),
                    built_tools: built.built_tools,
                },
                built.attached_extension_tools,
            )
            .await;
            Ok(built.session)
        })
    }

    fn create_session(&self) -> TuiBoxFuture<'_, Result<Arc<KitSession>, String>> {
        Box::pin(async move {
            let _guard = self.switch_lock.lock().await;
            let id = Uuid::now_v7().to_string();
            let (store, path, auto_created, file_lock) = if let Some(dir) = &self.sessions_dir {
                fs::create_dir_all(dir)
                    .map_err(|err| format!("failed to open session directory: {err}"))?;
                let file = dir.join(format!("{id}.jsonl"));
                let lock = SessionFileLock::try_acquire(&file)?;
                (
                    Some(Arc::new(JsonlSessionStore::new(&file)) as Arc<dyn SessionStore>),
                    Some(file),
                    true,
                    Some(lock),
                )
            } else {
                (None, None, false, None)
            };
            let built = self.template.build(store, false).await?;
            for warning in &built.warnings {
                eprintln!("warning: {warning}");
            }
            self.activate(
                Arc::clone(&built.session),
                ActiveSessionState {
                    id,
                    path,
                    auto_created,
                    _file_lock: file_lock,
                    built_tools: built.built_tools,
                },
                built.attached_extension_tools,
            )
            .await;
            Ok(built.session)
        })
    }
}

fn scan_sessions(dir: &Path, current_id: &str, cwd: &Path) -> Vec<SessionOption> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut rows: Vec<(u64, SessionOption)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(id) = name.strip_suffix(".jsonl") else {
            continue;
        };
        let store = JsonlSessionStore::new(&path);
        let Ok((header, items)) = store.load() else {
            continue;
        };
        if let Some(header_cwd) = header.as_ref().and_then(|h| h.cwd.as_deref())
            && !paths_same_dir(Path::new(header_cwd), cwd)
        {
            continue;
        }
        let message_count = items
            .iter()
            .filter(|item| matches!(item.payload, SessionEntryPayload::Message { .. }))
            .count();
        if message_count == 0 && id != current_id {
            continue;
        }
        let meta_title = read_meta_title(&dir.join(format!("{id}.meta.json")));
        let first_user = items.iter().find_map(|item| match &item.payload {
            SessionEntryPayload::Message {
                message: AgentMessage::Llm(Message::User(user)),
            } if !user.is_meta => {
                let text = user
                    .content
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text(t) if !t.text.trim().is_empty() => Some(t.text.as_str()),
                        ContentPart::Image(_) => Some("[image]"),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                let collapsed = collapse_preview(&text, 64);
                (!collapsed.is_empty()).then_some(collapsed)
            }
            _ => None,
        });
        let title = meta_title
            .or(first_user)
            .unwrap_or_else(|| id.chars().take(12).collect());
        let last_ts = items
            .iter()
            .map(|item| item.timestamp)
            .max()
            .or_else(|| header.map(|h| h.created_at))
            .unwrap_or(0);
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
        rows.push((
            sort_ms,
            SessionOption {
                id: id.to_owned(),
                title,
                updated_at: format_session_time(sort_ms),
                message_count: Some(message_count),
            },
        ));
    }
    rows.sort_by_key(|row| std::cmp::Reverse(row.0));
    rows.into_iter().map(|(_, opt)| opt).collect()
}

fn read_meta_title(path: &Path) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    let val: serde_json::Value = serde_json::from_str(&raw).ok()?;
    val.get("title")
        .and_then(|v| v.as_str())
        .map(|s| collapse_preview(s, 64))
        .filter(|s| !s.is_empty())
}

fn collapse_preview(raw: &str, max_chars: usize) -> String {
    let collapsed: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    for (i, ch) in collapsed.chars().enumerate() {
        if i >= max_chars {
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

fn is_read_only_builtin(name: &str) -> bool {
    matches!(name, "read" | "find" | "grep" | "sleep")
}

fn is_delegating_gateway_tool(name: &str) -> bool {
    // Delegated inner tools (MCP, extension, or workspace tools) pass through middleware individually.
    matches!(name, "call_tool" | "exec")
}

fn is_edit_builtin(name: &str) -> bool {
    matches!(name, "write" | "edit" | "apply_patch")
}

fn is_approval_required(
    mode: PermissionMode,
    tool_name: &str,
    enabled_builtins: &HashSet<String>,
) -> bool {
    if tool_name == "tool_search" || is_delegating_gateway_tool(tool_name) {
        return false;
    }
    if enabled_builtins.contains(tool_name) && is_read_only_builtin(tool_name) {
        return false;
    }
    match mode {
        PermissionMode::Default => true,
        PermissionMode::AcceptEdits => {
            !(enabled_builtins.contains(tool_name) && is_edit_builtin(tool_name))
        }
        PermissionMode::BypassPermissions => false,
    }
}

fn find_custom_theme_name(
    custom_themes: &BTreeMap<String, ThemeOverrides>,
    raw: &str,
) -> Option<String> {
    let trimmed = raw.trim();
    if custom_themes.contains_key(trimmed) {
        return Some(trimmed.to_owned());
    }
    let lower = trimmed.to_ascii_lowercase();
    custom_themes
        .keys()
        .find(|k| k.to_ascii_lowercase() == lower)
        .cloned()
}

/// Run the interactive TUI with full shared host capabilities (tools, MCP, extensions, skills).
pub async fn run(config: TuiLaunchConfig) -> Result<(), String> {
    let host = config.host;
    let initial_mode = PermissionMode::parse(&host.initial_mode)?;
    let cwd = host.cwd.clone();

    let (saved_settings, settings_warnings) = UserSettings::load_with_warnings(&host.jimu_home);
    let (custom_themes, theme_warnings) = discover_custom_themes(&host.jimu_home, &cwd);

    let mut startup_warnings = settings_warnings;
    startup_warnings.extend(theme_warnings);

    let (theme_kind, initial_custom_theme) = if let Some(raw) = config.theme.as_deref() {
        if let Some(custom_key) = find_custom_theme_name(&custom_themes, raw) {
            (ThemeKind::Auto, Some(custom_key))
        } else if let Some(kind) = ThemeKind::from_name(raw) {
            (kind, None)
        } else {
            return Err(format!("unknown --theme `{raw}`"));
        }
    } else if let Some(raw) = saved_settings.theme.as_deref() {
        if let Some(custom_key) = find_custom_theme_name(&custom_themes, raw) {
            (ThemeKind::Auto, Some(custom_key))
        } else if let Some(kind) = ThemeKind::from_name(raw) {
            (kind, None)
        } else {
            startup_warnings.push(format!(
                "settings.json `theme` ignored: unknown theme `{raw}`"
            ));
            (ThemeKind::Auto, None)
        }
    } else {
        (ThemeKind::Auto, None)
    };

    let transport = build_http_transport(host.transport_override.clone())?;
    let override_provider = host.provider_override.is_some();

    let provider_store = Arc::new(ProviderStore::new(host.provider_store_config()));
    startup_warnings.extend(provider_store.warnings());
    let active_model = provider_store
        .default_model()
        .unwrap_or_else(|| provider_store.unconfigured_placeholder_model());
    let selectable_models = provider_store.models();
    let explicit_model_override = host.provider.is_some() || host.model.is_some();

    let provider = build_auth_provider(
        Arc::clone(&provider_store),
        Arc::clone(&transport),
        host.provider_override.clone(),
        override_provider,
        true,
    );

    let (mcp_entries, mcp_settings_warnings) = host.resolve_mcp_entries(&[])?;
    startup_warnings.extend(mcp_settings_warnings);
    let (mcp_hub, mcp_warnings) = McpHub::connect(
        mcp_entries,
        &cwd,
        Arc::clone(&transport),
        CancelToken::new(),
    )
    .await
    .map_err(|err| err.to_string())?;
    startup_warnings.extend(mcp_warnings);
    let mcp_hub = Arc::new(mcp_hub);

    let cli_extensions = host.extensions.clone();
    let initial_extension_paths = host.resolve_extension_paths(&cwd);
    let loaded_extensions = match load_js_extensions(&initial_extension_paths, &HashSet::new()) {
        Ok(loaded) => loaded.map(Arc::new),
        Err(err) => {
            mcp_hub.shutdown().await;
            return Err(err);
        }
    };
    let js_runtime = loaded_extensions
        .as_ref()
        .map(|ext| Arc::clone(&ext.runtime));
    let live_extensions: Arc<RwLock<Option<Arc<LoadedExtensions>>>> =
        Arc::new(RwLock::new(loaded_extensions));
    let loaded_extension_paths = Arc::new(Mutex::new(initial_extension_paths));

    let sink = JimuInteractionSink::new();
    let shared_mode = Arc::new(Mutex::new(initial_mode));
    let jimu_home = host.jimu_home.clone();
    let sessions_dir = host
        .persist_sessions
        .then(|| host.jimu_home.join("sessions"));

    let template = SessionTemplate {
        host,
        transport,
        provider_store: Arc::clone(&provider_store),
        provider,
        active_model: active_model.clone(),
        explicit_model_override,
        shared_mode: Arc::clone(&shared_mode),
        sink: sink.clone(),
        extensions: Arc::clone(&live_extensions),
        mcp_hub: Arc::clone(&mcp_hub),
    };

    let (
        initial_store,
        initial_id,
        initial_path,
        initial_auto_created,
        initial_lock,
        should_restore,
    ) = if let Some(explicit) = config.session_file.clone()
        && template.host.persist_sessions
    {
        let resolved_explicit = canonical_jsonl_path(&explicit);
        let lock = match SessionFileLock::try_acquire(&resolved_explicit) {
            Ok(lock) => lock,
            Err(err) => {
                mcp_hub.shutdown().await;
                return Err(err);
            }
        };
        let recorded_cwd = match resolve_existing_session_cwd(&resolved_explicit, &cwd) {
            Ok(dir) => dir,
            Err(err) => {
                mcp_hub.shutdown().await;
                return Err(err);
            }
        };
        if !paths_same_dir(&recorded_cwd, &cwd) {
            mcp_hub.shutdown().await;
            return Err(format!(
                "session `{}` belongs to `{}`; pass `-C {}` to open it safely",
                explicit.display(),
                recorded_cwd.display(),
                recorded_cwd.display()
            ));
        }
        let id = resolved_explicit
            .file_stem()
            .and_then(|s| s.to_str())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| Uuid::now_v7().to_string());
        (
            Some(Arc::new(JsonlSessionStore::new(&resolved_explicit)) as Arc<dyn SessionStore>),
            id,
            Some(resolved_explicit),
            false,
            Some(lock),
            true,
        )
    } else if config.continue_last
        && let Some(dir) = &sessions_dir
        && let Some((id, file)) = crate::find_latest_session_for_cwd(dir, &cwd)
    {
        let lock = match SessionFileLock::try_acquire(&file) {
            Ok(lock) => lock,
            Err(err) => {
                mcp_hub.shutdown().await;
                return Err(err);
            }
        };
        (
            Some(Arc::new(JsonlSessionStore::new(&file)) as Arc<dyn SessionStore>),
            id,
            Some(file),
            false,
            Some(lock),
            true,
        )
    } else if let Some(dir) = &sessions_dir {
        if let Err(err) = fs::create_dir_all(dir) {
            mcp_hub.shutdown().await;
            return Err(format!("failed to open session directory: {err}"));
        }
        let id = Uuid::now_v7().to_string();
        let file = dir.join(format!("{id}.jsonl"));
        let lock = match SessionFileLock::try_acquire(&file) {
            Ok(lock) => lock,
            Err(err) => {
                mcp_hub.shutdown().await;
                return Err(err);
            }
        };
        (
            Some(Arc::new(JsonlSessionStore::new(&file)) as Arc<dyn SessionStore>),
            id,
            Some(file),
            true,
            Some(lock),
            false,
        )
    } else {
        (None, Uuid::now_v7().to_string(), None, false, None, false)
    };

    let initial_built = match template.build(initial_store, should_restore).await {
        Ok(built) => built,
        Err(err) => {
            mcp_hub.shutdown().await;
            return Err(err);
        }
    };
    startup_warnings.extend(initial_built.warnings);

    let session = initial_built.session;
    #[cfg(not(target_os = "ios"))]
    let initial_watcher = session.watch_skills(|_| {}).ok();

    let active_session = Arc::new(RwLock::new(Arc::clone(&session)));
    let transcript_path = Arc::new(RwLock::new(initial_path.clone()));
    let extension_tool_names = Arc::new(Mutex::new(initial_built.attached_extension_tools));
    let session_manager = Arc::new(TuiSessionManager {
        template,
        sessions_dir,
        cwd: cwd.clone(),
        active_session: Arc::clone(&active_session),
        transcript_path: Arc::clone(&transcript_path),
        extension_tool_names: Arc::clone(&extension_tool_names),
        #[cfg(not(target_os = "ios"))]
        skill_watcher: Mutex::new(initial_watcher),
        state: Mutex::new(ActiveSessionState {
            id: initial_id,
            path: initial_path,
            auto_created: initial_auto_created,
            _file_lock: initial_lock,
            built_tools: initial_built.built_tools,
        }),
        switch_lock: tokio::sync::Mutex::new(()),
    });

    let catalog_store = Arc::clone(&provider_store);
    let resolver_store = Arc::clone(&provider_store);
    let levels_store = Arc::clone(&provider_store);
    let model_home = jimu_home.clone();
    let effort_home = jimu_home.clone();
    let theme_home = jimu_home.clone();
    let harness = JimuHarness::with_interaction_sink(Arc::clone(&session), sink)
        .with_models(selectable_models)
        .with_model_catalog(Arc::new(move || catalog_store.models()))
        .with_model_resolver(Arc::new(move |key, current_provider| {
            resolver_store.select_model(key, current_provider)
        }))
        .with_thinking_levels_resolver(Arc::new(move |provider, id| {
            levels_store.thinking_levels(provider, id)
        }))
        .with_on_model_change(Arc::new(move |model| {
            let key = model_key(model);
            let _ = UserSettings::update(&model_home, |settings| {
                settings.default_model = Some(key);
            });
        }))
        .with_on_thinking_change(Arc::new(move |level| {
            let _ = UserSettings::update(&effort_home, |settings| {
                settings.default_effort = Some(level);
            });
        }))
        .with_permission_mode(Arc::new(SharedPermissionMode {
            mode: Arc::clone(&shared_mode),
            jimu_home: jimu_home.clone(),
        }))
        .with_mcp(Arc::new(SharedMcpCap {
            hub: Arc::clone(&mcp_hub),
        }))
        .with_session_controller(Arc::clone(&session_manager) as Arc<dyn JimuSessionController>)
        .with_shell_cwd(cwd.clone());

    let effective_model = session.model();
    let auth_hint = if effective_model.id.is_empty() {
        Some(
            "No model configured — run `/model <provider/id>` or edit ~/.jimu/providers.json"
                .into(),
        )
    } else if provider_store.authenticated_for(&effective_model) {
        if effective_model.provider == "openai-codex" {
            Some("Signed in with ChatGPT / Codex".to_string())
        } else {
            Some(format!("Signed in ({})", effective_model.provider))
        }
    } else {
        Some(format!(
            "No credentials for {} — set API key or run `jimu login`",
            effective_model.provider
        ))
    };

    let redraw_handle: Arc<RwLock<Option<TuiController>>> = Arc::new(RwLock::new(None));
    let on_chrome_update: Arc<dyn Fn() + Send + Sync> = {
        let handle = Arc::clone(&redraw_handle);
        Arc::new(move || {
            if let Some(ctrl) = handle.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
                ctrl.request_redraw();
            }
        })
    };
    let on_history_update: Arc<dyn Fn() + Send + Sync> = {
        let handle = Arc::clone(&redraw_handle);
        Arc::new(move || {
            if let Some(ctrl) = handle.read().unwrap_or_else(|e| e.into_inner()).as_ref() {
                ctrl.invalidate_history_layout();
            }
        })
    };

    let status_line_runner = saved_settings.status_line.clone().map(|sl_cfg| {
        Arc::new(StatusLineRunner::spawn(
            sl_cfg,
            cwd.clone(),
            Arc::clone(&on_chrome_update),
        ))
    });

    let ui_mod = Arc::new(UiModBridge::with_callbacks(
        js_runtime.clone(),
        Arc::clone(&on_chrome_update),
        Arc::clone(&on_history_update),
    ));

    let js_theme_colors = js_runtime
        .as_ref()
        .map(|rt| rt.ui_theme_overrides())
        .unwrap_or_default();
    let theme_overrides = (!js_theme_colors.is_empty()).then_some(ThemeOverrides {
        base: None,
        colors: js_theme_colors,
    });

    let chrome = ChromeConfig {
        slots: Arc::new(DynamicChromeSlots {
            active_session: Arc::clone(&active_session),
            transcript_path,
            project_dir: cwd.clone(),
            started_at: Instant::now(),
            status_line_runner,
            ui_mod: Arc::clone(&ui_mod),
        }),
        show_header: saved_settings.show_header.unwrap_or(false),
        app_name: Some("jimu".into()),
        app_version: Some(env!("CARGO_PKG_VERSION").into()),
        footer_hint: auth_hint,
        osc7501_enabled: true,
        ..ChromeConfig::default()
    };

    let slash_commands: Vec<Arc<dyn SlashCommand>> = vec![
        Arc::new(FastCommand {
            active_session: Arc::clone(&active_session),
            jimu_home: jimu_home.clone(),
        }),
        Arc::new(ReloadCommand {
            active_session: Arc::clone(&active_session),
            extensions: live_extensions,
            loaded_extension_paths,
            extension_tool_names,
            ui_mod,
            jimu_home: jimu_home.clone(),
            cwd: cwd.clone(),
            cli_extensions,
        }),
    ];

    let initial_notices = startup_warnings
        .into_iter()
        .map(|msg| (NoticeLevel::Warning, msg))
        .collect();

    let tui_cfg = TuiAppConfig {
        screen_mode: ScreenMode::Fullscreen,
        theme_kind,
        chrome,
        slash_commands,
        directory: Some(cwd),
        prompt_history_path: Some(jimu_home.join("history.jsonl")),
        initial_prompt: config.initial_prompt,
        open_resume_picker: config.open_resume_picker,
        initial_notices,
        on_theme_name_change: Some(Arc::new(move |name| {
            let owned = name.to_owned();
            let _ = UserSettings::update(&theme_home, |settings| {
                settings.theme = Some(owned);
            });
        })),
        theme_overrides,
        custom_themes,
        initial_custom_theme,
        ..Default::default()
    };

    let app = TuiApp::new(Arc::new(harness), tui_cfg);
    *redraw_handle.write().unwrap_or_else(|e| e.into_inner()) = Some(app.controller());
    let result = app.run().await.map_err(|err| err.to_string());
    *redraw_handle.write().unwrap_or_else(|e| e.into_inner()) = None;

    session_manager.shutdown_current().await;
    mcp_hub.shutdown().await;

    result
}
