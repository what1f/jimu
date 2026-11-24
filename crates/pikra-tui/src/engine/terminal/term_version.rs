//! Terminal version capture from environment variables.
//!
//! **A version variable is trusted only when the environment corroborates the brand it belongs to.**
//! These variables cross process, SSH and multiplexer boundaries.
//! An uncorroborated version is as likely to describe another program as the terminal drawing our output.
//! `WEZTERM_VERSION` and `VTE_VERSION` are themselves brand markers.
//! For them, corroboration means no stronger marker outranked them in `detect_terminal_brand_from_env`.
//!
//! `ZELLIJ_VERSION` is never read: it would make an Alacritty pane inside Zellij report Zellij's number as its own.
//! `KONSOLE_VERSION` is never read either: it has no `TerminalName::Konsole` to attach to.

use std::collections::HashMap;

use super::{TerminalName, terminal_name_from_term_program};

/// Which source produced a [`TermVersion`].
///
/// The rendered labels are stable telemetry values; do not rename them.
#[derive(Clone, Copy, Debug, Eq, PartialEq, strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum TermVersionSource {
    None,
    /// The runtime [`crate::engine::terminal::da2`] probe, the only non-env source.
    Da2,
    /// `TERM_PROGRAM_VERSION`, or its SSH-surviving `LC_TERMINAL_VERSION` mirror (iTerm2 only).
    TermProgram,
    #[strum(serialize = "wezterm")]
    WezTerm,
    Vte,
}

/// A terminal version together with the source that reported it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TermVersion {
    /// Raw, exactly as the source reported it (trimmed).
    /// Formats vary by terminal: `"3.5.6"`, `"20240203-110809-5046fc22"`, `"7402"`.
    pub version: String,
    pub source: TermVersionSource,
}

impl TermVersion {
    fn new(version: &str, source: TermVersionSource) -> Self {
        Self {
            version: version.to_owned(),
            source,
        }
    }
}

/// Identity, widened one way for VS Code forks: they export `TERM_PROGRAM=vscode` from the host that draws our output.
/// A leaked marker cannot borrow another brand's version. Zed is not an xterm.js host.
fn corroborates(named: TerminalName, env_brand: TerminalName) -> bool {
    named == env_brand
        || (named == TerminalName::VsCode
            && matches!(
                env_brand,
                TerminalName::VsCode | TerminalName::Cursor | TerminalName::Windsurf
            ))
}

/// Pick the best available version: a runtime probe outranks the environment.
/// A live self-report cannot be inherited across a process, SSH or multiplexer boundary, nor go stale.
/// XTVERSION has no arm: its payload is a name-and-version string, carried in `TerminalTelemetry::xtversion` instead.
pub(super) fn best_term_version(
    da2: Option<&str>,
    env_version: Option<&TermVersion>,
) -> (String, TermVersionSource) {
    da2.map(|version| (version.to_owned(), TermVersionSource::Da2))
        .or_else(|| env_version.map(|v| (v.version.clone(), v.source)))
        .unwrap_or_else(|| (String::new(), TermVersionSource::None))
}

/// Look up an env value, trimmed; `env_get` alone would pass whitespace.
fn env_trimmed<'a>(env: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    let value = super::env_get(env, key)?.trim();
    (!value.is_empty()).then_some(value)
}

/// First env var corroborated by the pre-refinement brand. The Windows `Unknown -> WindowsTerminal` guess must not attribute a version.
pub(super) fn detect_env_term_version(
    env: &HashMap<String, String>,
    env_brand: TerminalName,
) -> Option<TermVersion> {
    // tmux >= 3.2 exports TERM_PROGRAM=tmux and its own TERM_PROGRAM_VERSION
    // Ungated, that version would land on whichever brand marker survived inside the tmux server environment
    let named_brand = env_trimmed(env, "TERM_PROGRAM").and_then(terminal_name_from_term_program);
    if let Some(version) = env_trimmed(env, "TERM_PROGRAM_VERSION")
        && named_brand.is_some_and(|named| corroborates(named, env_brand))
    {
        return Some(TermVersion::new(version, TermVersionSource::TermProgram));
    }

    // LC_TERMINAL_VERSION survives SSH where TERM_PROGRAM_VERSION does not, but only iTerm2 sets the pair
    if let Some(version) = env_trimmed(env, "LC_TERMINAL_VERSION")
        && env_trimmed(env, "LC_TERMINAL").is_some_and(|v| v.eq_ignore_ascii_case("iterm2"))
        && env_brand == TerminalName::Iterm2
    {
        return Some(TermVersion::new(version, TermVersionSource::TermProgram));
    }

    if let Some(version) = env_trimmed(env, "WEZTERM_VERSION")
        && env_brand == TerminalName::WezTerm
    {
        return Some(TermVersion::new(version, TermVersionSource::WezTerm));
    }

    // Brand-only: `TerminalContext::is_vte_based()` also accepts a present `vte_version`, which is the candidate here
    // Routing the gate through it would make it vacuous
    if let Some(version) = env_trimmed(env, "VTE_VERSION")
        && env_brand.is_vte_based()
    {
        return Some(TermVersion::new(version, TermVersionSource::Vte));
    }

    None
}
