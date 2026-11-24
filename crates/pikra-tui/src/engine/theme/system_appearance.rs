//! System appearance detection for automatic day/night theming.
//!
//! Detection chain (each step only runs when the previous returns nothing):
//! 1. `dark-light` desktop APIs: macOS `AppleInterfaceStyle`, Linux XDG portal `org.freedesktop.appearance.color-scheme`, Windows registry
//! 2. Explicit env stamps, `GROK_APPEARANCE` / `LC_GROK_APPEARANCE` (SSH with tmux, wrap, headless).
//!    See [`super::env_appearance`].
//! 3. OSC 11 terminal background query, **startup-only**; see [`detect_with_osc11_fallback`].
//!    The result is cached so runtime `detect()` / `resolve_auto` cannot be overwritten by stale `COLORFGBG`.
//! 4. Inherited `COLORFGBG` guess, the last resort.
//!
//! Detection returns `None` when every step fails.

use super::ThemeKind;
use std::sync::OnceLock;
use std::time::Duration;

/// Startup OSC 11 result (`Some`/`None` after a probe); unset until [`detect_with_osc11_fallback`] runs.
/// Runtime `detect` reuses it so a live OSC 11 polarity is not replaced by inherited `COLORFGBG`.
static OSC11_STARTUP: OnceLock<Option<SystemAppearance>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemAppearance {
    Light,
    Dark,
}

impl SystemAppearance {
    /// Canonical env-var payload (`dark` / `light`), ASCII so it is safe in `LC_*` values forwarded by OpenSSH.
    #[must_use]
    pub const fn as_env_value(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }
}

/// Same chain as startup, but OSC 11 reuses the cached result instead of probing.
#[must_use]
pub fn detect() -> Option<SystemAppearance> {
    let env = crate::engine::input::terminal::host::collect_unicode_env();
    resolve_appearance_chain(
        detect_desktop(),
        super::env_appearance::detect_explicit_from_env_map(&env),
        OSC11_STARTUP.get().copied().flatten(),
        super::env_appearance::detect_colorfgbg_from_env_map(&env),
    )
}

/// Startup-only: OSC 11 needs raw-mode stdin and must not run once `EventStream` is active.
/// The watcher uses [`detect`] and still prefers a cached OSC 11 hit over `COLORFGBG`.
#[must_use]
pub fn detect_with_osc11_fallback() -> Option<SystemAppearance> {
    if let Some(appearance) = detect_desktop() {
        return Some(appearance);
    }
    let env = crate::engine::input::terminal::host::collect_unicode_env();
    let explicit = super::env_appearance::detect_explicit_from_env_map(&env);
    if explicit.is_some() {
        return explicit;
    }
    let osc11 = super::osc11::detect_via_osc11();
    let _ = OSC11_STARTUP.set(osc11);
    osc11.or_else(|| super::env_appearance::detect_colorfgbg_from_env_map(&env))
}

fn resolve_appearance_chain(
    desktop: Option<SystemAppearance>,
    explicit: Option<SystemAppearance>,
    osc11: Option<SystemAppearance>,
    colorfgbg: Option<SystemAppearance>,
) -> Option<SystemAppearance> {
    desktop.or(explicit).or(osc11).or(colorfgbg)
}

/// Desktop APIs only. `grok wrap` stamps the local OS theme before SSH; env hints may be a previous hop's snapshot.
#[must_use]
#[cfg(feature = "system-appearance")]
pub fn detect_desktop() -> Option<SystemAppearance> {
    match dark_light::detect() {
        Ok(dark_light::Mode::Dark) => Some(SystemAppearance::Dark),
        Ok(dark_light::Mode::Light) => Some(SystemAppearance::Light),
        // Mode::Unspecified or Err: no system preference detected
        _ => None,
    }
}

/// `[ui].auto_dark_theme` / `auto_light_theme`; `None` defaults to `Dark` / `Light`.
#[must_use]
pub fn to_theme_kind(
    appearance: SystemAppearance,
    dark_theme: Option<ThemeKind>,
    light_theme: Option<ThemeKind>,
) -> ThemeKind {
    match appearance {
        SystemAppearance::Light => light_theme.unwrap_or(ThemeKind::Light),
        SystemAppearance::Dark => dark_theme.unwrap_or(ThemeKind::Dark),
    }
}

#[cfg(not(feature = "system-appearance"))]
pub fn detect_desktop() -> Option<SystemAppearance> {
    None
}

/// Watches for changes without consuming terminal input.
pub struct SystemAppearanceWatcher {
    current: std::sync::Arc<std::sync::Mutex<Option<SystemAppearance>>>,
    changes: futures_channel::mpsc::Receiver<()>,
    stop: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl SystemAppearanceWatcher {
    pub fn start_if_auto(is_auto: bool) -> Option<Self> {
        if !is_auto {
            return None;
        }
        let current = std::sync::Arc::new(std::sync::Mutex::new(detect()));
        let latest = current.clone();
        let (mut tx, changes) = futures_channel::mpsc::channel(1);
        let (stop, rx) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("appearance".into())
            .spawn(move || {
                while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
                    rx.recv_timeout(Duration::from_secs(5))
                {
                    let detected = detect();
                    let mut value = latest.lock().unwrap_or_else(|e| e.into_inner());
                    if *value != detected {
                        *value = detected;
                        let _ = tx.try_send(());
                    }
                }
            })
            .ok()?;
        Some(Self {
            current,
            changes,
            stop: Some(stop),
            thread: Some(thread),
        })
    }
    pub async fn changed(&mut self) -> Option<()> {
        use futures_util::StreamExt;
        self.changes.next().await
    }
    pub fn current(&self) -> Option<SystemAppearance> {
        *self.current.lock().unwrap_or_else(|e| e.into_inner())
    }
}
impl Drop for SystemAppearanceWatcher {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
