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
        CliAction::Run(cli) => match cli.mode {
            CliMode::Rpc => run_rpc(*cli).await,
            CliMode::Acp => run_acp(*cli).await,
            CliMode::Tui => run_tui(*cli).await,
            CliMode::Json | CliMode::Text => run_oneshot(*cli).await,
        },
    }
}

fn build_acp_config(cli: &args::CliArgs) -> pikra::acp::AcpConfig {
    let defaults = pikra::acp::AcpConfig::default();
    pikra::acp::AcpConfig {
        cwd: defaults.cwd.join(&cli.cwd),
        provider: cli.provider.clone(),
        model: cli.model.clone(),
        api: cli.api.clone(),
        base_url: cli.base_url.clone(),
        api_key_env: cli.api_key_env.clone(),
        pikra_home: cli.pikra_home.clone().unwrap_or(defaults.pikra_home),
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
        thinking: cli.thinking,
        fast: cli.fast,
        initial_mode: cli.permission_mode.clone().unwrap_or(defaults.initial_mode),
        system_prompt: cli.system_prompt.clone(),
        ..defaults
    }
}

async fn run_acp(cli: args::CliArgs) -> ExitCode {
    let config = build_acp_config(&cli);
    let result = match cli.prompt.as_deref() {
        Some("login") => pikra::acp::args::login(&config).await,
        Some("logout") => pikra::acp::args::logout(&config).await,
        _ => pikra::acp::serve_stdio(config).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(feature = "tui")]
async fn run_tui(cli: args::CliArgs) -> ExitCode {
    let acp = build_acp_config(&cli);
    let config = pikra::tui::TuiLaunchConfig {
        acp,
        session_file: cli.session,
        theme: cli.theme,
    };
    match pikra::tui::run(config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(feature = "tui"))]
async fn run_tui(_cli: args::CliArgs) -> ExitCode {
    eprintln!("error: pikra was built without the `tui` feature (rebuild with `--features tui`)");
    ExitCode::from(2)
}

async fn run_oneshot(cli: args::CliArgs) -> ExitCode {
    let prompt = cli.prompt.clone().unwrap_or_default();
    let mode = cli.mode;
    let writer_guard = JsonlWriterGuard::spawn_stdout();
    let runtime = match RpcRuntime::new(&cli, writer_guard.writer()) {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("error: {err}");
            writer_guard.finish().await;
            return ExitCode::FAILURE;
        }
    };

    let res = runtime.run_oneshot(prompt, mode).await;
    runtime.shutdown_tools().await;
    drop(runtime);
    writer_guard.finish().await;

    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run_rpc(cli: args::CliArgs) -> ExitCode {
    let writer_guard = JsonlWriterGuard::spawn_stdout();
    let writer = writer_guard.writer();
    let runtime = match RpcRuntime::new(&cli, writer.clone()) {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("error: {err}");
            drop(writer);
            writer_guard.finish().await;
            return ExitCode::FAILURE;
        }
    };

    let mut reader = JsonlReader::new(tokio::io::stdin());
    let mut signals = ShutdownSignals::new();
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
                        eprintln!("[pikra-rpc] stdin read error: {io_err}");
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
