//! Host platform and display server classification.

use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::OnceLock;

mod display_refresh;

pub use display_refresh::{DisplayRefreshProbeResult, DisplayRefreshSource, probe_display_refresh};

/// Process env as UTF-8. Skips non-Unicode entries (`vars()` panics on those).
pub fn collect_unicode_env() -> HashMap<String, String> {
    unicode_env_from_os(std::env::vars_os())
}

/// Pure helper: drop OsString pairs that are not valid Unicode.
pub fn unicode_env_from_os(
    iter: impl IntoIterator<Item = (OsString, OsString)>,
) -> HashMap<String, String> {
    iter.into_iter()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect()
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, strum::Display)]
#[strum(serialize_all = "snake_case")]
#[non_exhaustive]
pub enum HostOs {
    Macos,
    Linux,
    Windows,
    #[default]
    Other,
}

impl HostOs {
    /// Call on demand since cfg is compile-time constant.
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Macos
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Other
        }
    }
}

/// WSL detection.
/// The implementation lives in `xai-tty-utils` (the shared low-level crate) so crates that must not depend on this UI crate can reuse it.
/// The re-export keeps existing `host::is_wsl()` callers unchanged.
pub use crate::engine::tty::is_wsl;

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, strum::Display)]
#[strum(serialize_all = "snake_case")]
#[non_exhaustive]
pub enum DisplayServer {
    Quartz,
    Wayland,
    X11,
    Win32,
    #[default]
    Unknown,
}

impl DisplayServer {
    /// Detect the display server. Cached for process lifetime on Linux (env vars don't change); compile-time constant on macOS/Windows.
    pub fn current() -> Self {
        static CACHE: OnceLock<DisplayServer> = OnceLock::new();
        *CACHE.get_or_init(|| {
            let env = collect_unicode_env();
            Self::detect_from_env(&env)
        })
    }

    /// Pure helper so tests can drive env directly.
    fn detect_from_env(env: &HashMap<String, String>) -> Self {
        match HostOs::current() {
            HostOs::Macos => Self::Quartz,
            HostOs::Windows => Self::Win32,
            HostOs::Linux => {
                if env.get("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty()) {
                    Self::Wayland
                } else if env.get("DISPLAY").is_some_and(|v| !v.is_empty()) {
                    Self::X11
                } else {
                    Self::Unknown
                }
            }
            HostOs::Other => Self::Unknown,
        }
    }
}
