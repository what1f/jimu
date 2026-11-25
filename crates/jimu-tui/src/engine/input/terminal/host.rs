//! Host platform and display server classification.

use std::collections::HashMap;
use std::ffi::OsString;
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

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
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
