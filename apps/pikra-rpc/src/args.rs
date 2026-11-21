use pikra_ai::{ApiKind, ThinkingLevel};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliMode {
    Rpc,
    Json,
    Text,
}

#[derive(Debug, Clone)]
pub struct CliArgs {
    pub mode: CliMode,
    pub cwd: PathBuf,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api: Option<ApiKind>,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub session: Option<PathBuf>,
    pub no_session: bool,
    pub skill_dirs: Vec<PathBuf>,
    pub extensions: Vec<PathBuf>,
    pub no_tools: bool,
    pub enable_codemode: bool,
    pub compact_tokens: Option<u32>,
    pub context_window: Option<u32>,
    pub thinking: ThinkingLevel,
    pub fast: bool,
    pub system_prompt: Option<String>,
    pub prompt: Option<String>,
}

#[derive(Debug, Clone)]
pub enum CliAction {
    Help,
    Version,
    Run(Box<CliArgs>),
}

pub fn help_text() -> &'static str {
    "\
pikra - Headless Pikra agent CLI & JSONL RPC server

USAGE:
  pikra --mode rpc [OPTIONS]
  pikra ask [OPTIONS] <PROMPT>

MODES:
  --mode <rpc|json|text>    Execution mode (default: rpc; text for `ask`)
  --rpc                     Shorthand for --mode rpc
  --json                    Shorthand for --mode json

MODEL & PROVIDER OPTIONS:
  --provider <id>           Provider id (e.g. openai, anthropic, google, deepseek)
  --model <id>              Model id (e.g. gpt-5.4, deepseek-chat)
  --api <kind>              Wire protocol: openai-responses | openai-completions |
                            anthropic-messages | google-generative-ai
  --base-url <url>          Override provider API base URL
  --api-key-env <ENV_VAR>   Environment variable name holding the API key
  --thinking <level>        Thinking level: off | minimal | low | medium | high | xhigh | max
  --fast                    Enable fast mode
  --context-window <u32>    Override model context window size in tokens

SESSION & WORKSPACE OPTIONS:
  --cwd <dir>               Workspace directory (default: current directory)
  --session <jsonl_path>    Persist/resume append-only JSONL session file
  --no-session              Run with an ephemeral in-memory session
  --skill-dir <dir>         Skill directory to discover (repeatable)
  --extension <js_path>     QuickJS extension file or directory (repeatable)
  --no-tools                Disable built-in workspace tools
  --enable-codemode         Enable QuickJS codemode `exec` tool
  --compact-tokens <u32>    Output reserve tokens for auto-compaction threshold
  --system-prompt <text>    Base system instructions

GENERAL:
  -h, --help                Show this help message
  -V, --version             Show version
"
}

pub fn version_text() -> String {
    format!("pikra {}", env!("CARGO_PKG_VERSION"))
}

pub fn parse_api_kind(raw: &str) -> Result<ApiKind, String> {
    match raw.trim() {
        "openai-responses" => Ok(ApiKind::OpenaiResponses),
        "openai-completions" => Ok(ApiKind::OpenaiCompletions),
        "anthropic-messages" => Ok(ApiKind::AnthropicMessages),
        "google-generative-ai" => Ok(ApiKind::GoogleGenerativeAi),
        other => Err(format!(
            "unsupported --api `{other}` (expected openai-responses, openai-completions, anthropic-messages, or google-generative-ai)"
        )),
    }
}

pub fn parse_thinking_level(raw: &str) -> Result<ThinkingLevel, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "off" => Ok(ThinkingLevel::Off),
        "minimal" => Ok(ThinkingLevel::Minimal),
        "low" => Ok(ThinkingLevel::Low),
        "medium" => Ok(ThinkingLevel::Medium),
        "high" => Ok(ThinkingLevel::High),
        "xhigh" => Ok(ThinkingLevel::Xhigh),
        "max" => Ok(ThinkingLevel::Max),
        other => Err(format!(
            "unsupported --thinking `{other}` (expected off, minimal, low, medium, high, xhigh, or max)"
        )),
    }
}

pub fn parse_args<I>(iter: I) -> Result<CliAction, String>
where
    I: IntoIterator<Item = String>,
{
    let raw: Vec<String> = iter.into_iter().collect();
    let mut mode: Option<CliMode> = None;
    let mut is_ask = false;
    let mut cwd: Option<PathBuf> = None;
    let mut provider: Option<String> = None;
    let mut model: Option<String> = None;
    let mut api: Option<ApiKind> = None;
    let mut base_url: Option<String> = None;
    let mut api_key_env: Option<String> = None;
    let mut session: Option<PathBuf> = None;
    let mut no_session = false;
    let mut skill_dirs: Vec<PathBuf> = Vec::new();
    let mut extensions: Vec<PathBuf> = Vec::new();
    let mut no_tools = false;
    let mut enable_codemode = false;
    let mut compact_tokens: Option<u32> = None;
    let mut context_window: Option<u32> = None;
    let mut thinking = ThinkingLevel::Off;
    let mut fast = false;
    let mut system_prompt: Option<String> = None;
    let mut positionals: Vec<String> = Vec::new();

    let mut i = 0usize;
    let mut after_double_dash = false;

    while i < raw.len() {
        let arg = &raw[i];

        if after_double_dash {
            positionals.push(arg.clone());
            i += 1;
            continue;
        }

        if arg == "--" {
            after_double_dash = true;
            i += 1;
            continue;
        }

        if i == 0 && arg == "ask" {
            is_ask = true;
            i += 1;
            continue;
        }

        if arg == "-h" || arg == "--help" {
            return Ok(CliAction::Help);
        }
        if arg == "-V" || arg == "--version" {
            return Ok(CliAction::Version);
        }

        if !arg.starts_with('-') {
            positionals.push(arg.clone());
            i += 1;
            continue;
        }

        let (flag, inline_val) = match arg.split_once('=') {
            Some((k, v)) => (k, Some(v)),
            None => (arg.as_str(), None),
        };

        let take_value = |flag: &str,
                          inline: Option<&str>,
                          idx: &mut usize,
                          all: &[String]|
         -> Result<String, String> {
            if let Some(v) = inline {
                Ok(v.to_string())
            } else if *idx + 1 < all.len() {
                *idx += 1;
                Ok(all[*idx].clone())
            } else {
                Err(format!("missing value for `{flag}`"))
            }
        };

        match flag {
            "--mode" => {
                let val = take_value(flag, inline_val, &mut i, &raw)?;
                mode = Some(match val.as_str() {
                    "rpc" => CliMode::Rpc,
                    "json" => CliMode::Json,
                    "text" => CliMode::Text,
                    other => {
                        return Err(format!(
                            "unsupported --mode `{other}` (expected rpc, json, or text)"
                        ));
                    }
                });
            }
            "--rpc" => {
                if inline_val.is_some() {
                    return Err("`--rpc` does not take a value".to_string());
                }
                mode = Some(CliMode::Rpc);
            }
            "--json" => {
                if inline_val.is_some() {
                    return Err("`--json` does not take a value".to_string());
                }
                mode = Some(CliMode::Json);
            }
            "--cwd" => {
                let val = take_value(flag, inline_val, &mut i, &raw)?;
                cwd = Some(PathBuf::from(val));
            }
            "--provider" => {
                provider = Some(take_value(flag, inline_val, &mut i, &raw)?);
            }
            "--model" => {
                model = Some(take_value(flag, inline_val, &mut i, &raw)?);
            }
            "--api" => {
                let val = take_value(flag, inline_val, &mut i, &raw)?;
                api = Some(parse_api_kind(&val)?);
            }
            "--base-url" => {
                base_url = Some(take_value(flag, inline_val, &mut i, &raw)?);
            }
            "--api-key-env" => {
                api_key_env = Some(take_value(flag, inline_val, &mut i, &raw)?);
            }
            "--session" => {
                let val = take_value(flag, inline_val, &mut i, &raw)?;
                session = Some(PathBuf::from(val));
            }
            "--no-session" => {
                if inline_val.is_some() {
                    return Err("`--no-session` does not take a value".to_string());
                }
                no_session = true;
            }
            "--skill-dir" => {
                let val = take_value(flag, inline_val, &mut i, &raw)?;
                skill_dirs.push(PathBuf::from(val));
            }
            "--extension" => {
                let val = take_value(flag, inline_val, &mut i, &raw)?;
                extensions.push(PathBuf::from(val));
            }
            "--no-tools" => {
                if inline_val.is_some() {
                    return Err("`--no-tools` does not take a value".to_string());
                }
                no_tools = true;
            }
            "--enable-codemode" => {
                if inline_val.is_some() {
                    return Err("`--enable-codemode` does not take a value".to_string());
                }
                enable_codemode = true;
            }
            "--compact-tokens" => {
                let val = take_value(flag, inline_val, &mut i, &raw)?;
                let parsed: u32 = val
                    .parse()
                    .map_err(|_| format!("invalid --compact-tokens `{val}` (expected u32)"))?;
                compact_tokens = Some(parsed);
            }
            "--context-window" => {
                let val = take_value(flag, inline_val, &mut i, &raw)?;
                let parsed: u32 = val
                    .parse()
                    .map_err(|_| format!("invalid --context-window `{val}` (expected u32)"))?;
                context_window = Some(parsed);
            }
            "--thinking" => {
                let val = take_value(flag, inline_val, &mut i, &raw)?;
                thinking = parse_thinking_level(&val)?;
            }
            "--fast" => {
                if inline_val.is_some() {
                    return Err("`--fast` does not take a value".to_string());
                }
                fast = true;
            }
            "--system-prompt" => {
                system_prompt = Some(take_value(flag, inline_val, &mut i, &raw)?);
            }
            unknown => {
                return Err(format!("unknown flag `{unknown}` (run `pikra --help`)"));
            }
        }

        i += 1;
    }

    if no_session && session.is_some() {
        return Err("cannot specify both `--session` and `--no-session`".to_string());
    }

    let effective_mode = match mode {
        Some(m) => m,
        None if is_ask || !positionals.is_empty() => CliMode::Text,
        None => CliMode::Rpc,
    };

    if effective_mode == CliMode::Rpc && !positionals.is_empty() {
        return Err(
            "RPC mode does not accept positional prompt arguments; send `prompt` commands over stdin"
                .to_string(),
        );
    }

    let prompt = if positionals.is_empty() {
        None
    } else {
        Some(positionals.join(" "))
    };

    if effective_mode != CliMode::Rpc && prompt.is_none() {
        return Err("one-shot mode requires a prompt argument (e.g. `pikra ask \"Hello\"`)".to_string());
    }

    let resolved_cwd = match cwd {
        Some(path) => path,
        None => std::env::current_dir()
            .map_err(|err| format!("failed to determine current directory: {err}"))?,
    };

    Ok(CliAction::Run(Box::new(CliArgs {
        mode: effective_mode,
        cwd: resolved_cwd,
        provider,
        model,
        api,
        base_url,
        api_key_env,
        session,
        no_session,
        skill_dirs,
        extensions,
        no_tools,
        enable_codemode,
        compact_tokens,
        context_window,
        thinking,
        fast,
        system_prompt,
        prompt,
    })))
}
