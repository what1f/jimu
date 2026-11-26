use jimu::acp::args::{parse_instruction_max_bytes, parse_walk_boundary};
use jimu::mcp::{McpServerConfig, parse_cli_mcp_server};
use jimu::tools::ToolSelection;
use jimu_ai::{ApiKind, ThinkingLevel};
use jimu_kit::{InstructionDiscoveryConfig, WalkBoundary};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliMode {
    Tui,
    Rpc,
    Acp,
    Json,
    Text,
}

#[derive(Debug, Clone)]
pub struct CliArgs {
    pub mode: CliMode,
    pub cwd: PathBuf,
    pub jimu_home: Option<PathBuf>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api: Option<ApiKind>,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub session: Option<PathBuf>,
    pub no_session: bool,
    pub continue_last: bool,
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    pub open_resume_picker: bool,
    pub skill_dirs: Vec<PathBuf>,
    pub skill_walk_boundary: WalkBoundary,
    pub instruction_discovery: InstructionDiscoveryConfig,
    pub extensions: Vec<PathBuf>,
    pub mcp_configs: Vec<PathBuf>,
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
    pub tools: ToolSelection,
    pub enable_codemode: bool,
    pub compact_tokens: Option<u32>,
    pub context_window: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub thinking: Option<ThinkingLevel>,
    pub fast: Option<bool>,
    pub permission_mode: Option<String>,
    pub system_prompt: Option<String>,
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    pub theme: Option<String>,
    pub prompt: Option<String>,
}

#[derive(Debug, Clone)]
pub enum CliAction {
    Help,
    Version,
    Login(Box<CliArgs>),
    Logout(Box<CliArgs>),
    Run(Box<CliArgs>),
}

pub fn help_text() -> &'static str {
    "\
jimu - Interactive Jimu coding agent, non-interactive CLI, JSONL RPC, and ACP v1 server

USAGE:
  jimu [OPTIONS] [PROMPT]           Start interactive TUI (default)
  jimu -p [OPTIONS] <PROMPT>        Non-interactive one-shot mode (text or --mode json)
  jimu rpc [OPTIONS]                Start JSONL RPC server (or `jimu --mode rpc`)
  jimu acp [OPTIONS]                Start ACP v1 editor server
  jimu login [OPTIONS]              Sign in with ChatGPT / Codex OAuth
  jimu logout [OPTIONS]             Remove stored authentication credentials

MODES:
  -p, --print               Non-interactive mode: run prompt and exit
  --mode <text|json|rpc>    Non-interactive output or protocol mode

MODEL & PROVIDER OPTIONS:
  --provider <id>           Provider id (e.g. openai, anthropic, google, deepseek)
  -m, --model <id>          Model id (e.g. gpt-5.4, claude-sonnet-5-5, deepseek-flash)
  --api <kind>              Wire protocol: openai-responses | openai-completions |
                            anthropic-messages | google-generative-ai
  --base-url <url>          Override provider API base URL
  --api-key-env <ENV_VAR>   Environment variable name holding the API key
  --thinking <level>        Thinking level: off | minimal | low | medium | high | xhigh | max
  --fast                    Enable fast mode
  --context-window <u32>    Override model context window size in tokens
  --max-output-tokens <u32> Override model max output tokens (ACP mode)

SESSION & WORKSPACE OPTIONS:
  -C, --cwd <dir>           Workspace directory (default: current directory)
  -c, --continue            Continue the most recent session for the workspace
  -r, --resume              Open the session picker on startup (TUI)
  --session <jsonl_path>    Persist/resume append-only JSONL session file
  --no-session              Run with an ephemeral in-memory session
  --jimu-home <dir>        Override the data directory
  --skill-dir <dir>         Skill directory to discover (repeatable)
  --walk-boundary <project-root|filesystem-root|cwd-only>
  --instruction-max-bytes <bytes|none>  Maximum instruction bytes (default: 32768)
  --instruction-file <path> Include an instruction file (repeatable)
  --no-user-instructions    Skip user instructions
  --no-instructions         Disable instruction discovery
  --extension <js_path>     QuickJS extension file or directory (repeatable)
  --mcp-config <json_path>  Load MCP servers from a JSON file (repeatable)
  --mcp-server <name>=<spec>
                            Attach an MCP server (URL, command, or JSON object; repeatable)
  --tools <names>           Enable only the listed comma-separated built-in tools (repeatable)
  --disable-tools <names>   Disable the listed comma-separated built-in tools (repeatable)
  --no-tools                Disable all built-in workspace tools
  --enable-codemode         Enable QuickJS codemode `exec` tool
  --compact-tokens <u32>    Output reserve tokens for auto-compaction threshold
  --permission-mode <mode>  Initial permission mode: default | acceptEdits | bypassPermissions
  --system-prompt <text>    Override base system instructions
  --theme <name>            TUI color theme (built-in or custom JSON theme in ~/.jimu/themes/)

GENERAL:
  -h, --help                Show this help message
  -V, --version             Show version
"
}

pub fn version_text() -> String {
    format!("jimu {}", env!("CARGO_PKG_VERSION"))
}

pub fn parse_api_kind(raw: &str) -> Result<ApiKind, String> {
    match raw.trim() {
        "openai-responses" => Ok(ApiKind::OpenaiResponses),
        "openai-codex-responses" => Ok(ApiKind::OpenaiCodexResponses),
        "openai-completions" => Ok(ApiKind::OpenaiCompletions),
        "anthropic-messages" => Ok(ApiKind::AnthropicMessages),
        "google-generative-ai" => Ok(ApiKind::GoogleGenerativeAi),
        other => Err(format!(
            "unsupported --api `{other}` (expected openai-responses, openai-codex-responses, openai-completions, anthropic-messages, or google-generative-ai)"
        )),
    }
}

pub fn parse_thinking_level(raw: &str) -> Result<ThinkingLevel, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "off" => Ok(ThinkingLevel::Off),
        "none" | "disable" => Ok(ThinkingLevel::None),
        "minimal" => Ok(ThinkingLevel::Minimal),
        "low" => Ok(ThinkingLevel::Low),
        "medium" => Ok(ThinkingLevel::Medium),
        "high" => Ok(ThinkingLevel::High),
        "xhigh" => Ok(ThinkingLevel::Xhigh),
        "max" => Ok(ThinkingLevel::Max),
        other => Err(format!(
            "unsupported --thinking `{other}` (expected off, none, minimal, low, medium, high, xhigh, or max)"
        )),
    }
}

pub fn parse_args<I>(iter: I) -> Result<CliAction, String>
where
    I: IntoIterator<Item = String>,
{
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Subcommand {
        Acp,
        Rpc,
        Login,
        Logout,
    }

    let raw: Vec<String> = iter.into_iter().collect();
    let mut subcommand: Option<Subcommand> = None;
    let mut mode: Option<CliMode> = None;
    let mut print_mode = false;
    let mut cwd: Option<PathBuf> = None;
    let mut jimu_home = None;
    let mut provider: Option<String> = None;
    let mut model: Option<String> = None;
    let mut api: Option<ApiKind> = None;
    let mut base_url: Option<String> = None;
    let mut api_key_env: Option<String> = None;
    let mut session: Option<PathBuf> = None;
    let mut no_session = false;
    let mut continue_last = false;
    let mut open_resume_picker = false;
    let mut skill_dirs: Vec<PathBuf> = Vec::new();
    let mut instructions = InstructionDiscoveryConfig::default();
    let mut instructions_enabled = true;
    let mut extensions: Vec<PathBuf> = Vec::new();
    let mut mcp_configs: Vec<PathBuf> = Vec::new();
    let mut mcp_servers: BTreeMap<String, McpServerConfig> = BTreeMap::new();
    let mut no_tools = false;
    let mut allow_tools: Vec<String> = Vec::new();
    let mut disable_tools: Vec<String> = Vec::new();
    let mut enable_codemode = false;
    let mut compact_tokens: Option<u32> = None;
    let mut context_window: Option<u32> = None;
    let mut max_output_tokens: Option<u32> = None;
    let mut thinking: Option<ThinkingLevel> = None;
    let mut fast: Option<bool> = None;
    let mut permission_mode: Option<String> = None;
    let mut system_prompt: Option<String> = None;
    let mut theme: Option<String> = None;
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

        if i == 0 {
            match arg.as_str() {
                "acp" => {
                    subcommand = Some(Subcommand::Acp);
                    i += 1;
                    continue;
                }
                "rpc" => {
                    subcommand = Some(Subcommand::Rpc);
                    i += 1;
                    continue;
                }
                "login" => {
                    subcommand = Some(Subcommand::Login);
                    i += 1;
                    continue;
                }
                "logout" => {
                    subcommand = Some(Subcommand::Logout);
                    i += 1;
                    continue;
                }
                _ => {}
            }
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
            "-p" | "--print" => {
                if inline_val.is_some() {
                    return Err(format!("`{flag}` does not take a value"));
                }
                print_mode = true;
            }
            "--mode" => {
                let val = take_value(flag, inline_val, &mut i, &raw)?;
                mode = Some(match val.as_str() {
                    "text" => CliMode::Text,
                    "json" => CliMode::Json,
                    "rpc" => CliMode::Rpc,
                    other => {
                        return Err(format!(
                            "unsupported --mode `{other}` (expected text, json, or rpc)"
                        ));
                    }
                });
            }
            "-C" | "--cwd" => {
                let val = take_value(flag, inline_val, &mut i, &raw)?;
                cwd = Some(PathBuf::from(val));
            }
            "--jimu-home" => {
                jimu_home = Some(PathBuf::from(take_value(flag, inline_val, &mut i, &raw)?));
            }
            "--provider" => {
                provider = Some(take_value(flag, inline_val, &mut i, &raw)?);
            }
            "-m" | "--model" => {
                model = Some(take_value(flag, inline_val, &mut i, &raw)?);
            }
            "--api" => {
                let val = take_value(flag, inline_val, &mut i, &raw)?;
                api = Some(parse_api_kind(&val)?);
            }
            "--base-url" => {
                let val = take_value(flag, inline_val, &mut i, &raw)?;
                if jimu::providers::parse_base_url(&val).is_none() {
                    return Err(
                        "invalid --base-url: must be an http/https URL without credentials, query string, or fragment"
                            .to_string(),
                    );
                }
                base_url = Some(val);
            }
            "--api-key-env" => {
                api_key_env = Some(take_value(flag, inline_val, &mut i, &raw)?);
            }
            "-c" | "--continue" => {
                if inline_val.is_some() {
                    return Err(format!("`{flag}` does not take a value"));
                }
                continue_last = true;
            }
            "-r" | "--resume" => {
                if inline_val.is_some() {
                    return Err(format!("`{flag}` does not take a value"));
                }
                open_resume_picker = true;
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
            "--walk-boundary" => {
                instructions.walk_boundary =
                    parse_walk_boundary(&take_value(flag, inline_val, &mut i, &raw)?)?;
            }
            "--instruction-max-bytes" => {
                instructions.max_bytes =
                    parse_instruction_max_bytes(&take_value(flag, inline_val, &mut i, &raw)?)?;
            }
            "--instruction-file" => {
                instructions
                    .extra_files
                    .push(PathBuf::from(take_value(flag, inline_val, &mut i, &raw)?));
            }
            "--no-user-instructions" | "--no-instructions" => {
                if inline_val.is_some() {
                    return Err(format!("`{flag}` does not take a value"));
                }
                if flag == "--no-user-instructions" {
                    instructions.include_user_instructions = false;
                } else {
                    instructions_enabled = false;
                }
            }
            "--extension" => {
                let val = take_value(flag, inline_val, &mut i, &raw)?;
                extensions.push(PathBuf::from(val));
            }
            "--mcp-config" => {
                let val = take_value(flag, inline_val, &mut i, &raw)?;
                mcp_configs.push(PathBuf::from(val));
            }
            "--mcp-server" => {
                let val = take_value(flag, inline_val, &mut i, &raw)?;
                let (name, cfg) = parse_cli_mcp_server(&val)?;
                mcp_servers.insert(name, cfg);
            }
            "--tools" => {
                allow_tools.push(take_value(flag, inline_val, &mut i, &raw)?);
            }
            "--disable-tools" => {
                disable_tools.push(take_value(flag, inline_val, &mut i, &raw)?);
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
            "--max-output-tokens" => {
                let val = take_value(flag, inline_val, &mut i, &raw)?;
                let parsed: u32 = val
                    .parse()
                    .map_err(|_| format!("invalid --max-output-tokens `{val}` (expected u32)"))?;
                max_output_tokens = Some(parsed);
            }
            "--thinking" => {
                let val = take_value(flag, inline_val, &mut i, &raw)?;
                thinking = Some(parse_thinking_level(&val)?);
            }
            "--fast" => {
                if inline_val.is_some() {
                    return Err("`--fast` does not take a value".to_string());
                }
                fast = Some(true);
            }
            "--permission-mode" => {
                permission_mode = Some(take_value(flag, inline_val, &mut i, &raw)?);
            }
            "--system-prompt" => {
                system_prompt = Some(take_value(flag, inline_val, &mut i, &raw)?);
            }
            "--theme" => {
                theme = Some(take_value(flag, inline_val, &mut i, &raw)?);
            }
            unknown => {
                return Err(format!("unknown flag `{unknown}` (run `jimu --help`)"));
            }
        }

        i += 1;
    }

    if no_session && session.is_some() {
        return Err("cannot specify both `--session` and `--no-session`".to_string());
    }
    if continue_last && (session.is_some() || no_session) {
        return Err("cannot combine `--continue` with `--session` or `--no-session`".to_string());
    }
    if open_resume_picker && (session.is_some() || no_session || continue_last) {
        return Err(
            "cannot combine `--resume` with `--session`, `--no-session`, or `--continue`"
                .to_string(),
        );
    }

    let tools = ToolSelection::from_cli(no_tools, &allow_tools, &disable_tools)?;

    let resolved_cwd = match cwd {
        Some(path) => path,
        None => std::env::current_dir()
            .map_err(|err| format!("failed to determine current directory: {err}"))?,
    };

    if !instructions_enabled {
        instructions.max_bytes = Some(0);
    }

    let has_session = session.is_some();
    let explicit_permission_mode = permission_mode.clone();

    let build_args = |effective_mode: CliMode, prompt: Option<String>| {
        Box::new(CliArgs {
            mode: effective_mode,
            cwd: resolved_cwd,
            jimu_home,
            provider,
            model,
            api,
            base_url,
            api_key_env,
            session,
            no_session,
            continue_last,
            open_resume_picker,
            skill_dirs,
            skill_walk_boundary: instructions.walk_boundary,
            instruction_discovery: instructions,
            extensions,
            mcp_configs,
            mcp_servers,
            tools,
            enable_codemode,
            compact_tokens,
            context_window,
            max_output_tokens,
            thinking,
            fast,
            permission_mode,
            system_prompt,
            theme,
            prompt,
        })
    };

    if let Some(auth_cmd @ (Subcommand::Login | Subcommand::Logout)) = subcommand {
        if print_mode || mode.is_some() || !positionals.is_empty() {
            return Err(
                "`login`/`logout` does not accept `--print`, `--mode`, or positional arguments"
                    .into(),
            );
        }
        let args = build_args(CliMode::Acp, None);
        return Ok(match auth_cmd {
            Subcommand::Login => CliAction::Login(args),
            Subcommand::Logout => CliAction::Logout(args),
            _ => unreachable!(),
        });
    }

    let effective_mode = match subcommand {
        Some(Subcommand::Acp) => {
            if print_mode || mode.is_some() {
                return Err("`jimu acp` cannot be combined with `--print` or `--mode`".into());
            }
            CliMode::Acp
        }
        Some(Subcommand::Rpc) => {
            if print_mode || matches!(mode, Some(CliMode::Text | CliMode::Json)) {
                return Err(
                    "`jimu rpc` cannot be combined with `--print` or `--mode text|json`".into(),
                );
            }
            CliMode::Rpc
        }
        None => {
            if print_mode {
                match mode {
                    None => CliMode::Text,
                    Some(CliMode::Text | CliMode::Json) => mode.unwrap(),
                    Some(CliMode::Rpc) => {
                        return Err("cannot combine `--print` with `--mode rpc`".into());
                    }
                    Some(CliMode::Tui | CliMode::Acp) => unreachable!(),
                }
            } else {
                mode.unwrap_or(CliMode::Tui)
            }
        }
        Some(Subcommand::Login | Subcommand::Logout) => unreachable!(),
    };

    if effective_mode == CliMode::Rpc && !positionals.is_empty() {
        return Err(
            "RPC mode does not accept positional prompt arguments; send `prompt` commands over stdin"
                .to_string(),
        );
    }

    if effective_mode == CliMode::Acp && !positionals.is_empty() {
        return Err("ACP mode does not accept positional arguments.".into());
    }
    if effective_mode == CliMode::Acp && (has_session || continue_last || open_resume_picker) {
        return Err("Use --jimu-home or --no-session in ACP mode.".into());
    }
    if effective_mode != CliMode::Tui && open_resume_picker {
        return Err("`--resume` is only supported in interactive TUI mode.".into());
    }
    if matches!(effective_mode, CliMode::Rpc | CliMode::Text | CliMode::Json)
        && let Some(ref pm) = explicit_permission_mode
        && pm.trim() != "bypassPermissions"
    {
        return Err(format!(
            "`--permission-mode {}` requires an interactive host (TUI or ACP); in headless mode omit it, pass `bypassPermissions`, or restrict tools with `--tools`/`--disable-tools`",
            pm.trim()
        ));
    }

    let prompt = if positionals.is_empty() {
        None
    } else {
        Some(positionals.join(" "))
    };

    if matches!(effective_mode, CliMode::Json | CliMode::Text) && prompt.is_none() {
        return Err(
            "one-shot mode requires a prompt argument (e.g. `jimu -p \"Hello\"`)".to_string(),
        );
    }

    Ok(CliAction::Run(build_args(effective_mode, prompt)))
}
