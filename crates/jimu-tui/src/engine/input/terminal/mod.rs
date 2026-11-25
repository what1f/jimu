//! Terminal facts shared by input, themes, hyperlinks and frame rendering.
use std::collections::HashMap;
use std::sync::OnceLock;
mod embedded_editor;
pub mod host;
pub mod hyperlinks;
pub mod keyboard;
pub(crate) mod probe;
mod tmux;
pub use embedded_editor::{EmbeddedEditor, embedded_editor_from_env};
use host::HostOs;
pub use keyboard::{KeyboardCapabilities, ModifierDelivery};
pub use tmux::*;
/// Known terminal emulator categories.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TerminalName {
    /// Apple Terminal (Terminal.app).
    AppleTerminal,
    Ghostty,

    Iterm2,

    WarpTerminal,
    /// VS Code integrated terminal.
    VsCode,
    /// Integrated terminal for the `Cursor` brand (VS Code family).
    Cursor,
    /// Integrated terminal for the `Windsurf` brand (VS Code family).
    Windsurf,
    /// Zed editor integrated terminal.
    Zed,
    WezTerm,

    Kitty,
    Alacritty,
    Rio,
    /// foot terminal emulator (Wayland-native, Linux-only), detected via TERM.
    /// It has full native Kitty keyboard protocol support.
    Foot,
    /// JetBrains IDE integrated terminal (JediTerm: IntelliJ, PhpStorm, etc.).
    /// No runtime capability probing is possible (no TERM_FEATURES, no XTVERSION, DA1 is bare VT102).
    /// The Classic and Reworked 2025 engines are indistinguishable, and all capabilities are conservative/Unknown.
    JetBrains,
    /// Grok Desktop (Electron app).
    GrokDesktop,
    /// VTE-based terminal (GNOME Terminal, kgx/GNOME Console, Tilix, etc.).
    Vte,
    /// Terminator terminal emulator (Python/GTK, VTE-based).
    /// It is detected via the `TERMINATOR_UUID` env var it exports on every child process, or `TERM_PROGRAM=terminator`.
    Terminator,
    /// Windows Terminal (wt, the default terminal on Windows 11+).
    WindowsTerminal,
    /// Otty (otty.sh). Wraps macOS IME commits in bracketed paste.
    Otty,
    #[default]
    Unknown,
}

/// The kind of terminal multiplexer wrapping the session.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MultiplexerKind {
    /// tmux (including Byobu-on-tmux).
    Tmux,
    /// GNU screen (including Byobu-on-screen).
    Screen,
    /// Zellij.
    Zellij,
    /// cmux (Ghostty-backed macOS terminal multiplexer).
    Cmux,
    /// Embedded emulator answers CSI itself, so it counts as CSI-intercepting. XTVERSION then describes herdr's engine, not the host.
    Herdr,
    /// No recognized multiplexer detected (does not rule out unknown ones).
    #[default]
    Undetected,
}

impl MultiplexerKind {
    /// Whether this multiplexer intercepts CSI queries (e.g. XTVERSION) instead of passing them through to the outer terminal.
    /// See [`Self::Herdr`] for the version signal herdr gives up by being here.
    pub fn intercepts_csi_queries(self) -> bool {
        matches!(self, Self::Tmux | Self::Screen | Self::Zellij | Self::Herdr)
    }
}

/// The Byobu backend, when Byobu markers are present.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ByobuBackend {
    /// Unknown backend (default; never produced by detection).
    #[default]
    Unknown,
    /// Byobu wrapping tmux.
    Tmux,
    /// Byobu wrapping GNU screen.
    Screen,
}

#[derive(Clone, Debug, Default)]
pub struct TerminalContext {
    pub brand: TerminalName,
    pub env_brand: TerminalName,
    pub multiplexer: MultiplexerKind,
    pub embedded_editor: Option<EmbeddedEditor>,
    pub tmux_version: Option<String>,
    pub is_ssh: bool,
    pub is_official_vscode_remote: bool,
    pub term_features: Option<String>,
}
impl TerminalContext {
    /// Returns `true` if the session is inside any tmux-backed environment (plain tmux or Byobu-on-tmux).
    pub fn is_tmux_backed(&self) -> bool {
        self.multiplexer == MultiplexerKind::Tmux
    }

    /// Per-terminal keyboard capabilities for this brand on the current host.
    pub fn keyboard_capabilities(&self) -> KeyboardCapabilities {
        keyboard::keyboard_capabilities(self.brand)
    }
    /// Returns the reason inline images / terminal graphics protocols are disabled, or `None` if the environment is compatible.
    pub fn graphics_protocol_skip_reason(&self) -> Option<&'static str> {
        if self.is_tmux_backed() {
            return Some("tmux");
        }
        None
    }
    /// Returns `true` if the detected tmux version is `major`.`minor` or later.
    pub fn is_tmux_version_or_later(&self, major: u32, minor: u32) -> bool {
        match self
            .tmux_version
            .as_deref()
            .and_then(parse_tmux_major_minor)
        {
            Some((maj, min)) => (maj, min) >= (major, minor),
            // Unknown version, so conservatively assume old
            None => false,
        }
    }
}

pub fn terminal_context() -> &'static TerminalContext {
    static CONTEXT: OnceLock<TerminalContext> = OnceLock::new();
    CONTEXT.get_or_init(|| {
        let mut ctx = build_terminal_context_from_env(&host::collect_unicode_env());
        if ctx.is_tmux_backed() && ctx.tmux_version.is_none() {
            ctx.tmux_version = tmux::query_version();
        }
        ctx
    })
}

pub fn build_terminal_context_from_env(env: &HashMap<String, String>) -> TerminalContext {
    let env_brand = detect_terminal_brand_from_env(env);
    let multiplexer = detect_multiplexer_from_env(env);
    let tmux_version = env_get(env, "TERM_PROGRAM")
        .filter(|p| p.eq_ignore_ascii_case("tmux") && multiplexer == MultiplexerKind::Tmux)
        .and_then(|_| env_get(env, "TERM_PROGRAM_VERSION"))
        .map(|version| format!("tmux {version}"));
    let is_ssh = env_get(env, "SSH_CONNECTION").is_some()
        || env_get(env, "SSH_TTY").is_some()
        || env_get(env, "SSH_CLIENT").is_some();
    let is_official_vscode_remote = is_ssh
        && env_get(env, "VSCODE_GIT_ASKPASS_MAIN").is_some_and(is_official_vscode_remote_askpass);

    TerminalContext {
        brand: refine_unknown_brand_for_host(env_brand, HostOs::current()),
        env_brand,
        multiplexer,
        tmux_version,
        is_ssh,
        is_official_vscode_remote,
        embedded_editor: embedded_editor_from_env(env),
        term_features: env_get(env, "TERM_FEATURES").map(str::to_owned),
    }
}
/// Look up a key in the env map, returning `Some` for non-empty values.
fn env_get<'a>(env: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    env.get(key).map(|v| v.as_str()).filter(|v| !v.is_empty())
}

fn is_official_vscode_remote_askpass(path: &str) -> bool {
    std::path::Path::new(path).components().any(|component| {
        matches!(
            component,
            std::path::Component::Normal(name)
                if name == ".vscode-server" || name == ".vscode-server-insiders"
        )
    })
}

/// A new marker must also go into the PTY harness strip list so the host terminal cannot leak into tests.
pub fn detect_terminal_brand_from_env(env: &HashMap<String, String>) -> TerminalName {
    // Fork-specific markers first: they set TERM_PROGRAM=vscode, and they survive SSH/tmux where TERM_PROGRAM does not.
    if env_get(env, "CURSOR_TRACE_ID").is_some() {
        return TerminalName::Cursor;
    }
    if let Some(askpass) = env_get(env, "VSCODE_GIT_ASKPASS_MAIN") {
        let askpass_lower = askpass.to_ascii_lowercase();
        if askpass_lower.contains("cursor") {
            return TerminalName::Cursor;
        }
        if askpass_lower.contains("windsurf") {
            return TerminalName::Windsurf;
        }
        // Pure VS Code remote agent injects this even without TERM_PROGRAM.
        return TerminalName::VsCode;
    }

    // TERM_PROGRAM is the most reliable signal.
    if let Some(term_program) = env_get(env, "TERM_PROGRAM")
        && let Some(name) = terminal_name_from_term_program(term_program)
    {
        return name;
    }

    // Both JediTerm engines share this marker; no capability query is safe (XTVERSION leaks). Must run before TERM_SESSION_ID or it false-positives as Apple Terminal.
    if let Some(te) = env_get(env, "TERMINAL_EMULATOR") {
        let te_lower = te.to_ascii_lowercase();
        if te_lower.contains("jetbrains") || te_lower.contains("jediterm") {
            return TerminalName::JetBrains;
        }
    }

    // WezTerm-specific variable.
    if env_get(env, "WEZTERM_VERSION").is_some() {
        return TerminalName::WezTerm;
    }

    // iTerm2. LC_TERMINAL=iTerm2 survives SSH (SendEnv/AcceptEnv LC_*) where ITERM_SESSION_ID / TERM_PROGRAM do not.
    if env_get(env, "ITERM_SESSION_ID").is_some()
        || env_get(env, "ITERM_PROFILE").is_some()
        || env_get(env, "LC_TERMINAL").is_some_and(|v| v.eq_ignore_ascii_case("iterm2"))
    {
        return TerminalName::Iterm2;
    }

    // Apple Terminal.
    if env_get(env, "TERM_SESSION_ID").is_some() {
        return TerminalName::AppleTerminal;
    }

    // Kitty.
    if env_get(env, "KITTY_WINDOW_ID").is_some() {
        return TerminalName::Kitty;
    }
    if let Some(term) = env_get(env, "TERM")
        && term.contains("kitty")
    {
        return TerminalName::Kitty;
    }

    // Alacritty.
    if env_get(env, "ALACRITTY_SOCKET").is_some() {
        return TerminalName::Alacritty;
    }
    if let Some(term) = env_get(env, "TERM")
        && term == "alacritty"
    {
        return TerminalName::Alacritty;
    }

    // Rio.
    if let Some(term) = env_get(env, "TERM")
        && term == "rio"
    {
        return TerminalName::Rio;
    }

    // foot (Wayland-native, Linux-only). foot sets no unique env var, only TERM.
    // Match its exact terminfo names to avoid over-matching
    if let Some(term) = env_get(env, "TERM")
        && matches!(term, "foot" | "foot-extra" | "foot-direct")
    {
        return TerminalName::Foot;
    }

    // Terminator (Python/GTK, VTE-based) exports TERMINATOR_UUID on every child process
    // Check before the generic VTE_VERSION fallback so it is identified specifically rather than as a generic VTE terminal (it sets both)
    if env_get(env, "TERMINATOR_UUID").is_some() {
        return TerminalName::Terminator;
    }

    // VTE-based terminals (GNOME Terminal, kgx, Tilix, etc.).
    if env_get(env, "VTE_VERSION").is_some() {
        return TerminalName::Vte;
    }

    // Windows Terminal sets WT_SESSION (a GUID) on every child process.
    if env_get(env, "WT_SESSION").is_some() {
        return TerminalName::WindowsTerminal;
    }

    TerminalName::Unknown
}

/// WT's DefTerm handoff omits WT_SESSION/TERM_PROGRAM. Raw detection stays in `env_brand`. WSL is unaffected.
fn refine_unknown_brand_for_host(brand: TerminalName, host: HostOs) -> TerminalName {
    if brand == TerminalName::Unknown && host == HostOs::Windows {
        TerminalName::WindowsTerminal
    } else {
        brand
    }
}
/// `BYOBU_BACKEND` when set; otherwise infer tmux vs screen from other Byobu markers plus `TMUX`/`STY`.
pub fn detect_byobu_from_env(env: &HashMap<String, String>) -> Option<ByobuBackend> {
    let has_byobu_backend = env_get(env, "BYOBU_BACKEND").is_some();
    let has_byobu_config = env_get(env, "BYOBU_CONFIG_DIR").is_some();
    let has_byobu_distro = env_get(env, "BYOBU_DISTRO").is_some();

    if !has_byobu_backend && !has_byobu_config && !has_byobu_distro {
        return None;
    }

    // Explicit backend marker takes precedence.
    if let Some(backend) = env_get(env, "BYOBU_BACKEND") {
        return match backend.to_ascii_lowercase().as_str() {
            "tmux" => Some(ByobuBackend::Tmux),
            "screen" => Some(ByobuBackend::Screen),
            // Unknown backend string, so fall through to inference
            _ => infer_byobu_backend_from_mux_markers(env),
        };
    }

    // No explicit BYOBU_BACKEND but other markers are present, so infer from the mux markers
    infer_byobu_backend_from_mux_markers(env)
}

/// When Byobu markers exist but `BYOBU_BACKEND` is absent or unrecognised, infer the backend from which multiplexer markers are set.
fn infer_byobu_backend_from_mux_markers(env: &HashMap<String, String>) -> Option<ByobuBackend> {
    let has_tmux = env_get(env, "TMUX").is_some();
    let has_sty = env_get(env, "STY").is_some();

    match (has_tmux, has_sty) {
        (true, _) => Some(ByobuBackend::Tmux),
        (false, true) => Some(ByobuBackend::Screen),
        // Byobu markers with neither TMUX nor STY, so the backend cannot be determined
        (false, false) => None,
    }
}

/// Explicit Byobu backend, then TMUX, then ZELLIJ, then STY. herdr/cmux only if no real mux won; `HERDR_ENV` beats inherited `CMUX_*`.
pub fn detect_multiplexer_from_env(env: &HashMap<String, String>) -> MultiplexerKind {
    let byobu = detect_byobu_from_env(env);

    // If Byobu is detected with an explicit backend, let that win.
    if let Some(backend) = byobu {
        match backend {
            ByobuBackend::Tmux => return MultiplexerKind::Tmux,
            ByobuBackend::Screen => return MultiplexerKind::Screen,
            ByobuBackend::Unknown => {} // fall through to standard markers
        }
    }

    // Nested real muxes win. A herdr daemon started from tmux freezes TMUX into every pane; classifying that as herdr wraps OSC 52 in DCS that herdr prints as text.
    if env_get(env, "TMUX").is_some() {
        return MultiplexerKind::Tmux;
    }
    if env_get(env, "ZELLIJ").is_some() || env_get(env, "ZELLIJ_SESSION_NAME").is_some() {
        return MultiplexerKind::Zellij;
    }
    if env_get(env, "STY").is_some() {
        return MultiplexerKind::Screen;
    }
    // herdr sets HERDR_ENV=1 in every pane, overrides TERM to xterm-256color and never sets TERM_PROGRAM
    // This marker is its only documented, stable signal
    if env_get(env, "HERDR_ENV").is_some() {
        return MultiplexerKind::Herdr;
    }
    // cmux sets non-empty CMUX_SOCKET_PATH / CMUX_PANEL_ID / CMUX_BUNDLE_ID; CMUX_SOCKET may be present but empty (env_get filters empties)
    if env_get(env, "CMUX_SOCKET_PATH").is_some()
        || env_get(env, "CMUX_PANEL_ID").is_some()
        || env_get(env, "CMUX_BUNDLE_ID").is_some()
    {
        return MultiplexerKind::Cmux;
    }

    MultiplexerKind::Undetected
}
fn terminal_name_from_term_program(value: &str) -> Option<TerminalName> {
    let normalized: String = value
        .trim()
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '_' | '.'))
        .map(|c| c.to_ascii_lowercase())
        .collect();

    match normalized.as_str() {
        "appleterminal" => Some(TerminalName::AppleTerminal),
        "ghostty" => Some(TerminalName::Ghostty),
        "iterm" | "iterm2" | "itermapp" => Some(TerminalName::Iterm2),
        "warp" | "warpterminal" => Some(TerminalName::WarpTerminal),
        "vscode" => Some(TerminalName::VsCode),
        "wezterm" => Some(TerminalName::WezTerm),
        "kitty" => Some(TerminalName::Kitty),
        "alacritty" => Some(TerminalName::Alacritty),
        "rio" => Some(TerminalName::Rio),
        "terminator" => Some(TerminalName::Terminator),
        "zed" => Some(TerminalName::Zed),
        "grokdesktop" => Some(TerminalName::GrokDesktop),
        "windowsterminal" => Some(TerminalName::WindowsTerminal),
        "otty" => Some(TerminalName::Otty),
        _ => None,
    }
}
/// Parse the major.minor version from a tmux version string like `"tmux 3.4"` or `"tmux 3.3a"`.
/// Returns `None` on unrecognized formats.
fn parse_tmux_major_minor(version: &str) -> Option<(u32, u32)> {
    let rest = version.strip_prefix("tmux ")?;
    let mut parts = rest.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor_str = parts.next()?;
    let minor_end = minor_str
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(minor_str.len());
    let minor: u32 = minor_str.get(..minor_end)?.parse().ok()?;
    Some((major, minor))
}
