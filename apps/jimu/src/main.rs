mod args;
mod framing;
mod protocol;
mod runtime;

use args::{CliAction, CliMode, help_text, parse_args, version_text};
use framing::{JsonlReader, JsonlWriterGuard};
use protocol::{RpcResponse, parse_command_line};
use runtime::RpcRuntime;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let action = match parse_args(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::from(2);
        }
    };

    match action {
        CliAction::Help => {
            print!("{}", help_text());
            ExitCode::SUCCESS
        }
        CliAction::Version => {
            println!("{}", version_text());
            ExitCode::SUCCESS
        }
        CliAction::Login(cli) => run_auth(*cli, true).await,
        CliAction::Logout(cli) => run_auth(*cli, false).await,
        CliAction::Run(cli) => match cli.mode {
            CliMode::Tui => run_tui(*cli).await,
            CliMode::Rpc => run_rpc(*cli).await,
            CliMode::Acp => run_acp(*cli).await,
            CliMode::Json | CliMode::Text => run_oneshot(*cli).await,
        },
    }
}

fn build_host_config(cli: &args::CliArgs) -> jimu::host::HostConfig {
    let defaults = jimu::host::HostConfig::default();
    let jimu_home = cli.jimu_home.clone().unwrap_or(defaults.jimu_home);
    let cwd = defaults.cwd.join(&cli.cwd);
    let saved = jimu::settings::UserSettings::load(&jimu_home);
    jimu::host::HostConfig {
        cwd,
        jimu_home,
        provider: cli.provider.clone(),
        model: cli.model.clone(),
        api: cli.api.clone(),
        base_url: cli.base_url.clone(),
        api_key_env: cli.api_key_env.clone(),
        persist_sessions: !cli.no_session,
        skill_dirs: cli.skill_dirs.clone(),
        skill_walk_boundary: cli.skill_walk_boundary,
        instruction_discovery: Some(cli.instruction_discovery.clone()),
        extensions: cli.extensions.clone(),
        tools: cli.tools.clone(),
        enable_codemode: cli.enable_codemode,
        compact_tokens: cli.compact_tokens,
        context_window: cli.context_window,
        max_output_tokens: cli.max_output_tokens,
        thinking: cli
            .thinking
            .or(saved.default_effort)
            .unwrap_or(defaults.thinking),
        fast: cli.fast.or(saved.fast).unwrap_or(defaults.fast),
        initial_mode: cli
            .permission_mode
            .clone()
            .or(saved.default_permission_mode)
            .unwrap_or(defaults.initial_mode),
        system_prompt: cli.system_prompt.clone(),
        mcp_configs: cli.mcp_configs.clone(),
        mcp_servers: cli.mcp_servers.clone(),
        ..defaults
    }
}

async fn run_auth(cli: args::CliArgs, is_login: bool) -> ExitCode {
    let config = jimu::acp::AcpConfig::from(build_host_config(&cli));
    let result = if is_login {
        jimu::acp::args::login(&config).await
    } else {
        jimu::acp::args::logout(&config).await
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

async fn run_acp(cli: args::CliArgs) -> ExitCode {
    let config = jimu::acp::AcpConfig::from(build_host_config(&cli));
    match jimu::acp::serve_stdio(config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(feature = "tui")]
async fn run_tui(cli: args::CliArgs) -> ExitCode {
    let host = build_host_config(&cli);
    let config = jimu::tui::TuiLaunchConfig {
        host,
        session_file: cli.session,
        continue_last: cli.continue_last,
        open_resume_picker: cli.open_resume_picker,
        theme: cli.theme,
        initial_prompt: cli.prompt,
    };
    match jimu::tui::run(config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(feature = "tui"))]
async fn run_tui(_cli: args::CliArgs) -> ExitCode {
    eprintln!(
        "error: jimu was built without the `tui` feature (rebuild with `--features tui`, or use `jimu -p <prompt>`, `jimu rpc`, or `jimu acp`)"
    );
    ExitCode::from(2)
}

fn resolve_continue_session(cli: &mut args::CliArgs) {
    if cli.continue_last && cli.session.is_none() && !cli.no_session {
        let home = cli
            .jimu_home
            .clone()
            .unwrap_or_else(|| jimu::host::HostConfig::default().jimu_home);
        if let Some((_id, path)) =
            jimu::find_latest_session_for_cwd(&home.join("sessions"), &cli.cwd)
        {
            cli.session = Some(path);
        }
    }
}

async fn run_oneshot(mut cli: args::CliArgs) -> ExitCode {
    resolve_continue_session(&mut cli);
    let prompt = cli.prompt.clone().unwrap_or_default();
    let mode = cli.mode;
    let host = build_host_config(&cli);
    let mut signals = ShutdownSignals::new();
    let startup_cancel = jimu_ai::CancelToken::new();
    let writer_guard = JsonlWriterGuard::spawn_stdout();

    let runtime = {
        let init_fut = RpcRuntime::new(host, &cli, writer_guard.writer(), startup_cancel.clone());
        tokio::pin!(init_fut);
        let init_res = tokio::select! {
            biased;
            res = &mut init_fut => res,
            _ = signals.recv() => {
                startup_cancel.cancel();
                init_fut.await
            }
        };
        match init_res {
            Ok(rt) => rt,
            Err(err) => {
                if !startup_cancel.is_cancelled() {
                    eprintln!("error: {err}");
                }
                writer_guard.finish().await;
                return ExitCode::FAILURE;
            }
        }
    };

    let mut triggered_by_signal = false;
    let res = {
        let run_fut = runtime.run_oneshot(prompt, mode);
        tokio::pin!(run_fut);
        tokio::select! {
            biased;
            res = &mut run_fut => res,
            _ = signals.recv() => {
                triggered_by_signal = true;
                runtime.abort_now_sync();
                let _ = run_fut.await;
                Err("Operation aborted".to_string())
            }
        }
    };

    let shutdown_fut = async {
        if triggered_by_signal {
            runtime.abort_active().await;
        }
        runtime.shutdown_tools().await;
    };
    tokio::select! {
        biased;
        () = shutdown_fut => {}
        _ = signals.recv() => {
            runtime.abort_now_sync();
        }
    }
    drop(runtime);
    writer_guard.finish().await;

    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            if !triggered_by_signal {
                eprintln!("error: {err}");
            }
            ExitCode::FAILURE
        }
    }
}

async fn run_rpc(mut cli: args::CliArgs) -> ExitCode {
    resolve_continue_session(&mut cli);
    let host = build_host_config(&cli);
    let mut signals = ShutdownSignals::new();
    let startup_cancel = jimu_ai::CancelToken::new();
    let writer_guard = JsonlWriterGuard::spawn_stdout();
    let writer = writer_guard.writer();

    let runtime = {
        let init_fut = RpcRuntime::new(host, &cli, writer.clone(), startup_cancel.clone());
        tokio::pin!(init_fut);
        let init_res = tokio::select! {
            biased;
            res = &mut init_fut => res,
            _ = signals.recv() => {
                startup_cancel.cancel();
                init_fut.await
            }
        };
        match init_res {
            Ok(rt) => rt,
            Err(err) => {
                if !startup_cancel.is_cancelled() {
                    eprintln!("error: {err}");
                }
                drop(writer);
                writer_guard.finish().await;
                return ExitCode::FAILURE;
            }
        }
    };

    let mut reader = JsonlReader::new(tokio::io::stdin());
    let mut triggered_by_signal = false;

    loop {
        tokio::select! {
            biased;
            _ = signals.recv() => {
                triggered_by_signal = true;
                break;
            }
            next = reader.next_record() => {
                match next {
                    Ok(None) => {
                        // Stdin EOF: orderly shutdown.
                        break;
                    }
                    Ok(Some(Err(utf8_err))) => {
                        writer.send_json(&RpcResponse::error(None, "parse", utf8_err));
                    }
                    Ok(Some(Ok(line))) => {
                        match parse_command_line(&line) {
                            Ok(parsed) => {
                                runtime.handle_command(parsed.id, parsed.command).await;
                            }
                            Err(err_resp) => {
                                writer.send_json(&err_resp);
                            }
                        }
                    }
                    Err(io_err) => {
                        eprintln!("[jimu-rpc] stdin read error: {io_err}");
                        break;
                    }
                }
            }
        }
    }

    // Orderly shutdown phase: wait for active work (on EOF) or abort active work (on signal).
    // A second termination signal forces immediate exit without hanging.
    let shutdown_fut = async {
        if triggered_by_signal {
            runtime.abort_active().await;
        } else {
            runtime.wait_for_active_run().await;
        }
        runtime.shutdown_tools().await;
    };

    tokio::select! {
        biased;
        () = shutdown_fut => {}
        _ = signals.recv() => {
            runtime.abort_now_sync();
        }
    }

    // Drop runtime (tears down QuickJS worker thread) and flush every buffered JSONL stdout line.
    drop(runtime);
    drop(writer);
    writer_guard.finish().await;

    ExitCode::SUCCESS
}

struct ShutdownSignals {
    #[cfg(unix)]
    sigterm: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    sigint: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    sighup: Option<tokio::signal::unix::Signal>,
}

impl ShutdownSignals {
    fn new() -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Self {
                sigterm: signal(SignalKind::terminate()).ok(),
                sigint: signal(SignalKind::interrupt()).ok(),
                sighup: signal(SignalKind::hangup()).ok(),
            }
        }
        #[cfg(not(unix))]
        {
            Self {}
        }
    }

    async fn recv(&mut self) {
        #[cfg(unix)]
        {
            let term = async {
                if let Some(s) = self.sigterm.as_mut() {
                    let _ = s.recv().await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            let int = async {
                if let Some(s) = self.sigint.as_mut() {
                    let _ = s.recv().await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            let hup = async {
                if let Some(s) = self.sighup.as_mut() {
                    let _ = s.recv().await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            tokio::select! {
                () = term => {}
                () = int => {}
                () = hup => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}
