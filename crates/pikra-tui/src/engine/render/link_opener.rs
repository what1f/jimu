//! Shared URL-opening and scheme validation utilities.
//!
//! Keyboard navigation, mouse clicks, and action dispatch all open links through these helpers.

use std::collections::HashMap;

use crate::engine::input::terminal::hyperlinks::SchemeFilter;

/// Outcome of attempting to open a URL in the system browser/handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenUrlResult {
    /// Opener was launched.
    Opened,
    /// Scheme was rejected by the safety filter.
    RejectedScheme,
    /// Browser cannot run here (headless / no display) or the opener failed to spawn.
    /// Callers should show the URL so the user can open it manually.
    BrowserUnavailable,
}

/// Linux/BSD needs non-empty `DISPLAY`, `WAYLAND_DISPLAY`, or `BROWSER`; macOS/Windows always return true.
/// Pure for a fixed env map. A true result is not a successful spawn.
pub fn browser_open_likely_available_from_env(env: &HashMap<String, String>) -> bool {
    if cfg!(any(target_os = "macos", target_os = "windows")) {
        return true;
    }
    // Explicit BROWSER override: allow even without a display server so scripted/headless setups that point at a CLI browser still try
    if env.get("BROWSER").is_some_and(|v| !v.is_empty()) {
        return true;
    }
    env.get("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
        || env.get("DISPLAY").is_some_and(|v| !v.is_empty())
}

/// Whether this process likely has a GUI browser available right now.
pub fn browser_open_likely_available() -> bool {
    let env = crate::engine::input::terminal::host::collect_unicode_env();
    browser_open_likely_available_from_env(&env)
}

/// Host-localized labels for browser launch failures.
pub struct BrowserUnavailableText<'a> {
    pub notice: &'a str,
    pub copied: &'a str,
}

/// Present the destination on its own line so the user can select it.
pub fn browser_unavailable_message(url: &str, text: &BrowserUnavailableText<'_>) -> String {
    format!("{}:\n{url}", text.notice)
}

/// Keep the destination at the start of a compact notice.
pub fn browser_unavailable_line(
    url: &str,
    copied: bool,
    text: &BrowserUnavailableText<'_>,
) -> String {
    if copied {
        format!("{url} · {} ({})", text.notice, text.copied)
    } else {
        format!("{url} · {}", text.notice)
    }
}

/// Native opener with detached stdio so a GUI helper cannot block the pager.
/// `false` on headless or spawn failure — show the URL. Untrusted input must use [`is_safe_to_open`] or [`try_open_url`].
pub fn open_url(url: &str) -> bool {
    if !browser_open_likely_available() {
        return false;
    }
    spawn_url_opener(url)
}
/// Hand the URL to the default browser via `ShellExecuteW`, which takes it as a single argument.
/// The URL must never pass through `cmd.exe`: `start` splits on `&` and expands `%VAR%`.
/// A server-supplied URL such as `https://example.com/&calc.exe` would execute a command.
#[cfg(target_os = "windows")]
fn spawn_url_opener(url: &str) -> bool {
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn ShellExecuteW(
            hwnd: *mut std::ffi::c_void,
            verb: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show: i32,
        ) -> isize;
    }
    let verb: Vec<u16> = "open".encode_utf16().chain(Some(0)).collect();
    let target: Vec<u16> = url.encode_utf16().chain(Some(0)).collect();
    // Both arguments are NUL-terminated and stay alive across the call.
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
        ) > 32
    }
}
#[cfg(not(target_os = "windows"))]
#[allow(clippy::disallowed_methods)] // fire and forget; the child is reaped when this process exits
fn spawn_url_opener(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(not(target_os = "macos"))]
    let cmd = "xdg-open";

    let mut command = std::process::Command::new(cmd);
    command
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    crate::engine::tty::detach_command(&mut command);
    launch(command)
}
/// `detach_std_command` keeps the GUI helper off the TUI's `/dev/tty`. Path is one argv, never a shell string.
/// Windows uses [`reveal_in_explorer`]. Split out so tests can inspect the command without spawning.
#[cfg(not(target_os = "windows"))]
fn build_open_path_command(path: &std::path::Path) -> std::process::Command {
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(not(target_os = "macos"))]
    let mut command = std::process::Command::new("xdg-open");
    command
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    crate::engine::tty::detach_command(&mut command);
    command
}

/// Trusted filesystem path (no scheme check, unlike [`open_url`]).
/// Windows avoids `cmd /c start`: `%VAR%` expansion corrupts percent-encoded session paths.
#[allow(clippy::disallowed_methods)] // fire and forget; the child is reaped when this process exits
pub fn open_path(path: &std::path::Path) -> bool {
    #[cfg(target_os = "windows")]
    {
        reveal_in_explorer(path)
    }
    #[cfg(not(target_os = "windows"))]
    {
        launch(build_open_path_command(path))
    }
}
/// `raw_arg` keeps Explorer's `/select,"<path>"` quoting; no `cmd`, so `%` in urlencoded paths is not expanded.
/// A missing file opens the parent folder (no `/select`) instead of Home.
#[cfg(target_os = "windows")]
#[allow(clippy::disallowed_methods)] // fire and forget; the child is reaped when this process exits
fn reveal_in_explorer(path: &std::path::Path) -> bool {
    use std::os::windows::process::CommandExt;

    // Prefer the real on-disk location (absolute)
    // Fall back to the parent when the file was deleted so Explorer does not dump the user in Home
    let target = if path.is_file() || path.is_dir() {
        path.to_path_buf()
    } else if let Some(parent) = path.parent().filter(|p| p.is_dir()) {
        parent.to_path_buf()
    } else {
        path.to_path_buf()
    };

    let select_file = target.is_file();
    let mut command = std::process::Command::new("explorer");
    // Escape embedded double-quotes in the path so the `/select,"<path>"` quoting does not break
    // Windows file-system paths cannot legally contain `"`, but percent-decoded display paths or future user-chosen filenames could, so be defensive
    let escaped = target.display().to_string().replace('"', "\"\"");
    if select_file {
        command.raw_arg(format!("/select,\"{}\"", escaped));
    } else {
        // Open the folder itself (no /select); this works for dirs and as a fallback when we only have a parent path
        command.raw_arg(format!("\"{}\"", escaped));
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    crate::engine::tty::detach_command(&mut command);
    // explorer.exe returns exit code 1 even on success, so a successful spawn is the best signal we have
    launch(command)
}
/// Check if a URL's scheme is safe to open.
///
/// Uses the `url` crate for scheme extraction, falling back to prefix matching for non-standard URLs that `url::Url::parse` rejects.
pub fn is_safe_to_open(url: &str, filter: SchemeFilter) -> bool {
    let url = url.trim();
    if let Ok(parsed) = url::Url::parse(url) {
        return filter.allows(parsed.scheme());
    }
    // Fallback: check for scheme via "://" prefix, lowercasing for case-insensitive comparison (SchemeFilter matches lowercase literals)
    if let Some((scheme, _)) = url.split_once("://") {
        return filter.allows(&scheme.to_ascii_lowercase());
    }
    // Defensive: url::Url::parse handles well-formed mailto, but guard against edge cases where the parser rejects a mailto-like string
    if let Some((scheme, _)) = url.split_once(':')
        && scheme.eq_ignore_ascii_case("mailto")
    {
        return filter.allows(&scheme.to_ascii_lowercase());
    }
    false
}

/// `true` only when the scheme is allowed and the opener launched.
/// Use [`try_open_url`] to tell scheme rejection from a missing browser.
pub fn open_url_if_safe(url: &str, filter: SchemeFilter) -> bool {
    matches!(try_open_url(url, filter), OpenUrlResult::Opened)
}

/// Validate scheme and attempt to open.
/// Prefer this when the caller needs to show the URL for manual opening on [`OpenUrlResult::BrowserUnavailable`].
pub fn try_open_url(url: &str, filter: SchemeFilter) -> OpenUrlResult {
    if !is_safe_to_open(url, filter) {
        return OpenUrlResult::RejectedScheme;
    }
    if open_url(url) {
        OpenUrlResult::Opened
    } else {
        OpenUrlResult::BrowserUnavailable
    }
}

fn launch(mut command: std::process::Command) -> bool {
    match command.spawn() {
        Ok(child) => {
            match crate::engine::tty::spawn_child_reaper(
                "link-opener",
                child,
                None::<std::sync::Arc<()>>,
            ) {
                Ok(()) => true,
                Err((_, mut child, _)) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    false
                }
            }
        }
        Err(_) => false,
    }
}

/// Leave an existing parameter untouched; on parse failure return the original so untrusted opener input stays safe.
pub fn ensure_query_param(url: &str, key: &str, value: &str) -> String {
    let Ok(mut parsed) = url::Url::parse(url) else {
        return url.to_string();
    };
    let already_present = parsed.query_pairs().any(|(k, _)| k == key);
    if already_present {
        return parsed.to_string();
    }
    parsed.query_pairs_mut().append_pair(key, value);
    parsed.to_string()
}
