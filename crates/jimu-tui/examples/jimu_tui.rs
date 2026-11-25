use jimu_ai::{ApiKind, Client, Model};
use jimu_http::ReqwestTransport;
use jimu_kit::{
    BackgroundEventSender, BackgroundNotification, KitSession, KitSessionConfig, KitToolSpec,
};
use jimu_tools::{
    BackgroundEventCallback, BackgroundToolEvent, BuiltinToolKind, BuiltinToolSet, ExecCommandTool,
    MonitorManager, MonitorTool, ProcessSessionManager, SleepTool, WakeGate, WebFetchConfig,
    WebFetchTool, WorkspaceTools, WorkspaceToolsConfig, WriteStdinTool,
    format_background_events_xml,
};
use jimu_tui::{
    app::{ScreenMode, TuiApp, TuiAppConfig},
    engine::theme::ThemeKind,
    harness::ChromeConfig,
    jimu::{JimuApprovalMiddleware, JimuHarness, JimuInteractionSink},
};
use std::{
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

struct CliArgs {
    cwd: PathBuf,
    model_id: String,
    theme_kind: ThemeKind,
    screen_mode: ScreenMode,
    yolo: bool,
}

fn parse_args() -> Result<CliArgs, String> {
    let mut cwd: Option<PathBuf> = None;
    let mut model_id = std::env::var("JIMU_MODEL").unwrap_or_else(|_| "deepseek-flash".into());
    let mut theme_kind = std::env::var("JIMU_THEME")
        .ok()
        .and_then(|s| ThemeKind::from_name(&s))
        .unwrap_or(ThemeKind::Auto);
    let mut screen_mode = match std::env::var("JIMU_MODE").ok().as_deref() {
        Some("inline") => ScreenMode::Inline,
        _ => ScreenMode::Fullscreen,
    };
    let mut yolo = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!(
                    "jimu_tui [DIR] [--model deepseek-flash|deepseek-v4-pro] [--theme auto|dark|light|tokyonight|rosepine|oscura|terminal] [--yolo]"
                );
                std::process::exit(0);
            }
            "--model" => {
                model_id = args.next().ok_or("--model requires a value")?;
            }
            "--theme" => {
                let raw = args.next().ok_or("--theme requires a value")?;
                theme_kind =
                    ThemeKind::from_name(&raw).ok_or_else(|| format!("unknown theme: {raw}"))?;
            }
            "--mode" => {
                let raw = args.next().ok_or("--mode requires a value")?;
                screen_mode = match raw.as_str() {
                    "inline" => ScreenMode::Inline,
                    "fullscreen" => ScreenMode::Fullscreen,
                    _ => return Err(format!("unknown mode: {raw} (use fullscreen or inline)")),
                };
            }
            "--yolo" => yolo = true,
            other if other.starts_with('-') => return Err(format!("unknown flag: {other}")),
            dir => cwd = Some(PathBuf::from(dir)),
        }
    }

    let cwd = match cwd {
        Some(dir) => dir,
        None => std::env::current_dir().map_err(|e| e.to_string())?,
    };
    Ok(CliArgs {
        cwd,
        model_id,
        theme_kind,
        screen_mode,
        yolo,
    })
}

fn deepseek_model(id: &str, supports_reasoning: bool) -> Model {
    let mut m = Model::new(
        id,
        "deepseek",
        ApiKind::OpenaiCompletions,
        "https://api.deepseek.com/v1",
    );
    m.context_window = 128_000;
    m.supports_reasoning = supports_reasoning;
    m
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn make_bg_callback(sender_slot: Arc<OnceLock<BackgroundEventSender>>) -> BackgroundEventCallback {
    Arc::new(move |event: BackgroundToolEvent| {
        let Some(sender) = sender_slot.get() else {
            return;
        };
        let (source_id, kind, summary) = match &event {
            BackgroundToolEvent::ShellExited {
                session_id,
                command,
                exit_code,
                ..
            } => (
                format!("shell:{session_id}"),
                "shell_exited".to_string(),
                format!("shell #{session_id} exited ({exit_code:?}): {command}"),
            ),
            BackgroundToolEvent::MonitorLines { id, name, .. } => (
                format!("monitor:{id}"),
                "monitor_lines".to_string(),
                format!("monitor {name} emitted new lines"),
            ),
            BackgroundToolEvent::MonitorExited {
                id,
                name,
                exit_code,
                ..
            } => (
                format!("monitor:{id}"),
                "monitor_exited".to_string(),
                format!("monitor {name} exited ({exit_code:?})"),
            ),
        };
        let xml_payload = format_background_events_xml(&[event]);
        sender.send(BackgroundNotification {
            source_id,
            kind,
            summary,
            xml_payload,
            timestamp_ms: now_ms(),
        });
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = parse_args().map_err(|e| {
        eprintln!("error: {e}");
        e
    })?;

    let api_key = std::env::var("TEST_DEEPSEEK_API_KEY")
        .or_else(|_| std::env::var("DEEPSEEK_API_KEY"))
        .map_err(|_| "请先设置 TEST_DEEPSEEK_API_KEY 或 DEEPSEEK_API_KEY 环境变量")?;

    let mut models = vec![
        deepseek_model("deepseek-flash", false),
        deepseek_model("deepseek-v4-pro", true),
    ];
    let active_model = if let Some(found) = models.iter().find(|m| m.id == cli.model_id).cloned() {
        found
    } else {
        let custom = deepseek_model(&cli.model_id, false);
        models.insert(0, custom.clone());
        custom
    };

    let transport: Arc<dyn jimu_ai::HttpTransport> = Arc::new(ReqwestTransport::new()?);
    let provider = Arc::new(Client::new(Arc::clone(&transport)));

    let sink = JimuInteractionSink::new();
    let wake_gate = WakeGate::new();
    let bg_sender_slot: Arc<OnceLock<BackgroundEventSender>> = Arc::new(OnceLock::new());
    let bg_cb = make_bg_callback(Arc::clone(&bg_sender_slot));

    let ws_cfg = Arc::new(
        WorkspaceToolsConfig::new(cli.cwd.clone())
            .with_enabled_tools(BuiltinToolSet::default().without(BuiltinToolKind::ExecCommand)),
    );
    let process_manager = ProcessSessionManager::new(
        Arc::clone(&ws_cfg),
        wake_gate.clone(),
        Some(Arc::clone(&bg_cb)),
    );
    let monitor_manager = MonitorManager::new(Arc::clone(&ws_cfg), wake_gate.clone(), Some(bg_cb));

    let mut kit_cfg = KitSessionConfig::new(active_model);
    kit_cfg.api_key = Some(api_key);
    kit_cfg.cwd = Some(cli.cwd.display().to_string());
    kit_cfg.system_prompt = "You are a concise, helpful coding assistant running inside Jimu TUI. Use workspace tools when appropriate and reply in the user's language.".into();

    if !cli.yolo {
        let approval = JimuApprovalMiddleware::new(sink.clone(), |ctx| {
            matches!(
                ctx.tool_call.name.as_str(),
                "write" | "edit" | "apply_patch" | "exec_command"
            )
        });
        kit_cfg.middlewares.push(Arc::new(approval));
    }

    for tool in WorkspaceTools::build((*ws_cfg).clone()) {
        kit_cfg.tools.push(KitToolSpec::resident(tool));
    }
    kit_cfg
        .tools
        .push(KitToolSpec::resident(Arc::new(ExecCommandTool::new(
            process_manager.clone(),
        ))));
    kit_cfg
        .tools
        .push(KitToolSpec::resident(Arc::new(WriteStdinTool::new(
            process_manager.clone(),
        ))));
    kit_cfg
        .tools
        .push(KitToolSpec::resident(Arc::new(MonitorTool::new(
            monitor_manager.clone(),
        ))));
    kit_cfg
        .tools
        .push(KitToolSpec::resident(Arc::new(SleepTool::new(wake_gate))));
    kit_cfg
        .tools
        .push(KitToolSpec::resident(Arc::new(WebFetchTool::new(
            transport,
            WebFetchConfig::default(),
        ))));

    let session = Arc::new(KitSession::new(provider, kit_cfg)?);
    let _ = bg_sender_slot.set(session.background_sender());

    let harness = Arc::new(
        JimuHarness::with_interaction_sink(session, sink)
            .with_process_manager(process_manager)
            .with_monitor_manager(monitor_manager)
            .with_models(models),
    );

    let tui_cfg = TuiAppConfig {
        screen_mode: cli.screen_mode,
        theme_kind: cli.theme_kind,
        directory: Some(cli.cwd),
        chrome: ChromeConfig {
            app_name: Some("Jimu".into()),
            app_version: Some(env!("CARGO_PKG_VERSION").into()),
            footer_hint: Some("Signed in with DeepSeek API key".into()),
            status_flags: vec![if cli.yolo { "yolo" } else { "approve" }.into()],
            osc7501_enabled: true,
            ..Default::default()
        },
        ..Default::default()
    };

    TuiApp::new(harness, tui_cfg).run().await?;
    Ok(())
}
