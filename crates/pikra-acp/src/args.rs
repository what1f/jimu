use crate::{AcpConfig, auth, serve_stdio};
use pikra_ai::ApiKind;
use pikra_kit::WalkBoundary;
use std::{collections::HashMap, path::PathBuf};

const HELP: &str = "Usage: pikra-acp [options] [login|logout]\n\
  --cwd <dir>  --provider <id>  --model <id>  --api <kind>\n\
  --base-url <url>  --api-key-env <ENV>  --pikra-home <dir>\n\
  --no-session  --skill-dir <dir>  --extension <path>\n\
  --walk-boundary <project-root|filesystem-root|cwd-only>\n\
  --instruction-max-bytes <bytes|none>  --instruction-file <path> (repeatable)\n\
  --no-user-instructions  --no-instructions\n\
  --no-tools  --enable-codemode  --compact-tokens <u32>  --context-window <u32>\n\
  --max-output-tokens <u32>  --thinking <level>  --fast\n\
  --mode <default|acceptEdits|plan|bypassPermissions>\n\
  --system-prompt <text>  -h, --help  -V, --version\n";

pub async fn run(args: impl IntoIterator<Item = String>) -> Result<(), String> {
    let mut args = args.into_iter();
    let mut cfg = AcpConfig::default();
    let mut instructions = cfg.instruction_discovery.take().unwrap_or_default();
    let mut instructions_enabled = true;
    let mut action = None;
    while let Some(arg) = args.next() {
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg.as_str(), None), |(key, value)| (key, Some(value)));
        let mut value = || {
            inline
                .map(str::to_owned)
                .or_else(|| args.next())
                .ok_or_else(|| format!("Missing value for {flag}."))
        };
        match flag {
            "login" | "logout" if inline.is_none() && action.is_none() => {
                action = Some(arg.clone())
            }
            "--help" | "-h" if inline.is_none() => {
                print!("{HELP}");
                return Ok(());
            }
            "--version" | "-V" if inline.is_none() => {
                println!("pikra-acp {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--cwd" => cfg.cwd = PathBuf::from(value()?),
            "--provider" => cfg.provider = Some(value()?),
            "--model" => cfg.model = Some(value()?),
            "--api" => cfg.api = Some(parse_api(&value()?)?),
            "--base-url" => cfg.base_url = Some(value()?),
            "--api-key-env" => cfg.api_key_env = Some(value()?),
            "--pikra-home" => cfg.pikra_home = PathBuf::from(value()?),
            "--skill-dir" => cfg.skill_dirs.push(PathBuf::from(value()?)),
            "--walk-boundary" => {
                let boundary = parse_walk_boundary(&value()?)?;
                cfg.skill_walk_boundary = boundary;
                instructions.walk_boundary = boundary;
            }
            "--instruction-max-bytes" => {
                instructions.max_bytes = parse_instruction_max_bytes(&value()?)?;
            }
            "--instruction-file" => instructions.extra_files.push(PathBuf::from(value()?)),
            "--no-user-instructions" if inline.is_none() => {
                instructions.include_user_instructions = false
            }
            "--no-instructions" if inline.is_none() => instructions_enabled = false,
            "--extension" => cfg.extensions.push(PathBuf::from(value()?)),
            "--compact-tokens" => {
                cfg.compact_tokens = Some(
                    value()?
                        .parse()
                        .map_err(|_| "Expected a number for --compact-tokens.")?,
                )
            }
            "--context-window" => {
                cfg.context_window = Some(
                    value()?
                        .parse()
                        .map_err(|_| "Expected a number for --context-window.")?,
                )
            }
            "--max-output-tokens" => {
                cfg.max_output_tokens = Some(
                    value()?
                        .parse()
                        .map_err(|_| "Expected a number for --max-output-tokens.")?,
                )
            }
            "--thinking" => cfg.thinking = value()?.parse()?,
            "--mode" => cfg.initial_mode = value()?,
            "--system-prompt" => cfg.system_prompt = Some(value()?),
            "--fast" if inline.is_none() => cfg.fast = true,
            "--no-session" if inline.is_none() => cfg.persist_sessions = false,
            "--no-tools" if inline.is_none() => cfg.enable_tools = false,
            "--enable-codemode" if inline.is_none() => cfg.enable_codemode = true,
            _ => {
                return Err(format!(
                    "Unknown argument {flag}. Run pikra-acp --help for usage."
                ));
            }
        }
    }
    if cfg.cwd.is_relative() {
        cfg.cwd = std::env::current_dir()
            .map_err(|_| "Cannot find the current directory.")?
            .join(cfg.cwd);
    }
    if !instructions_enabled {
        instructions.max_bytes = Some(0);
    }
    cfg.instruction_discovery = Some(instructions);
    match action.as_deref() {
        Some("login") => auth::login(&cfg.pikra_home).await,
        Some("logout") => logout(&cfg).await,
        _ => serve_stdio(cfg).await,
    }
}

pub fn parse_walk_boundary(value: &str) -> Result<WalkBoundary, String> {
    match value {
        "project-root" => Ok(WalkBoundary::ProjectRoot),
        "filesystem-root" => Ok(WalkBoundary::FilesystemRoot),
        "cwd-only" => Ok(WalkBoundary::CurrentDirOnly),
        _ => Err("Use project-root, filesystem-root, or cwd-only for --walk-boundary.".into()),
    }
}

pub fn parse_instruction_max_bytes(value: &str) -> Result<Option<usize>, String> {
    match value {
        "none" | "unlimited" => Ok(None),
        _ => value.parse().map(Some).map_err(|_| {
            "Use a nonnegative byte count or none for --instruction-max-bytes.".into()
        }),
    }
}

pub async fn login(config: &AcpConfig) -> Result<(), String> {
    auth::login(&config.pikra_home).await
}
pub async fn logout(config: &AcpConfig) -> Result<(), String> {
    auth::Auth::new(config.clone())
        .logout()
        .await
        .map_err(|error| error.message)?;
    eprintln!("Signed out of ChatGPT.");
    Ok(())
}

pub(crate) fn parse_api(value: &str) -> Result<ApiKind, String> {
    match value {
        "openai-responses" => Ok(ApiKind::OpenaiResponses),
        "openai-codex-responses" => Ok(ApiKind::OpenaiCodexResponses),
        "openai-completions" => Ok(ApiKind::OpenaiCompletions),
        "anthropic-messages" => Ok(ApiKind::AnthropicMessages),
        "google-generative-ai" => Ok(ApiKind::GoogleGenerativeAi),
        _ => Err("Unknown API protocol.".into()),
    }
}

pub(crate) fn auth_env(config: &AcpConfig) -> HashMap<String, String> {
    HashMap::from([(
        "PIKRA_HOME".into(),
        config.pikra_home.to_string_lossy().into_owned(),
    )])
}
