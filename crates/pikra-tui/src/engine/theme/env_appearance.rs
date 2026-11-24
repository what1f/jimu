//! Process-environment appearance hints when desktop APIs are unavailable.
//!
//! `LC_GROK_APPEARANCE` is the SSH-surviving alias (`AcceptEnv LC_*`).
//! `COLORFGBG` is a terminal polarity hint stamped once at shell start and then inherited unchanged: a guess, not a live reading.
//!
//! Startup (`detect_with_osc11_fallback`) checks desktop, then explicit wrap/SSH stamps, then OSC 11, then `COLORFGBG`.
//! The runtime watcher (`detect`) checks desktop, then explicit stamps, then the cached startup OSC 11, then `COLORFGBG`.
//! It sends no new OSC 11 probe once crossterm owns stdin.

use std::collections::HashMap;

use super::system_appearance::SystemAppearance;

/// Runtime watcher path. Startup orders explicit hints, then OSC 11, then `COLORFGBG`.
#[must_use]
pub fn detect() -> Option<SystemAppearance> {
    detect_from_env_map(&crate::engine::input::terminal::host::collect_unicode_env())
}

/// Ordered lookup: `GROK_APPEARANCE`, then `LC_GROK_APPEARANCE`, then `COLORFGBG`.
#[must_use]
pub fn detect_from_env_map(env: &HashMap<String, String>) -> Option<SystemAppearance> {
    detect_explicit_from_env_map(env).or_else(|| detect_colorfgbg_from_env_map(env))
}

/// Deliberate wrap/SSH stamps only, no inherited `COLORFGBG` guess.
#[must_use]
pub fn detect_explicit_from_env_map(env: &HashMap<String, String>) -> Option<SystemAppearance> {
    parse_appearance_var(env_nonempty(env, "GROK_APPEARANCE"))
        .or_else(|| parse_appearance_var(env_nonempty(env, "LC_GROK_APPEARANCE")))
}

/// Inherited `COLORFGBG` polarity guess.
#[must_use]
pub fn detect_colorfgbg_from_env_map(env: &HashMap<String, String>) -> Option<SystemAppearance> {
    parse_colorfgbg(env_nonempty(env, "COLORFGBG"))
}

fn env_nonempty<'a>(env: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    env.get(key)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
}

/// Parse `dark`/`light` (plus `night`/`day` aliases). Unknown values are ignored.
#[must_use]
pub fn parse_appearance_var(raw: Option<&str>) -> Option<SystemAppearance> {
    match raw?.trim().to_ascii_lowercase().as_str() {
        "dark" | "night" => Some(SystemAppearance::Dark),
        "light" | "day" => Some(SystemAppearance::Light),
        _ => None,
    }
}

/// Parse `COLORFGBG`. Vim/Neovim heuristic: bg `0-6` and `8` are dark; `7` and `9-15` are light.
/// Non-ANSI indexes and a non-numeric last field (`default`) yield `None`.
#[must_use]
pub fn parse_colorfgbg(raw: Option<&str>) -> Option<SystemAppearance> {
    // Last field is bg (`fg;bg` or `fg;default;bg`)
    // A trailing `default` means "unknown polarity", not "skip and use an earlier number"
    let bg = raw?.split(';').next_back()?.trim().parse::<u8>().ok()?;
    match bg {
        0..=6 | 8 => Some(SystemAppearance::Dark),
        7 | 9..=15 => Some(SystemAppearance::Light),
        _ => None,
    }
}
