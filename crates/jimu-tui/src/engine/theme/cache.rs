//! In-memory theme selection and automatic appearance mappings.
use super::{ThemeKind, ThemeOverrides, system_appearance};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};

static CURRENT: AtomicU8 = AtomicU8::new(ThemeKind::Dark as u8);
static AUTO_MODE: AtomicBool = AtomicBool::new(false);
static TERMINAL_NATIVE_LOCK: AtomicBool = AtomicBool::new(false);
static TERMINAL_THEME_ENABLED: AtomicBool = AtomicBool::new(true);
static REVISION: AtomicU64 = AtomicU64::new(1);
static OVERRIDES: RwLock<Option<ThemeOverrides>> = RwLock::new(None);
static AUTO_THEME_CONFIG: Mutex<AutoThemeConfig> = Mutex::new(AutoThemeConfig {
    dark_theme: None,
    light_theme: None,
});
fn theme_kind_from_u8(byte: u8) -> ThemeKind {
    match byte {
        x if x == ThemeKind::Dark as u8 => ThemeKind::Dark,
        x if x == ThemeKind::Light as u8 => ThemeKind::Light,
        x if x == ThemeKind::TokyoNight as u8 => ThemeKind::TokyoNight,
        x if x == ThemeKind::RosePineMoon as u8 => ThemeKind::RosePineMoon,
        x if x == ThemeKind::OscuraMidnight as u8 => ThemeKind::OscuraMidnight,
        x if x == ThemeKind::Terminal as u8 => ThemeKind::Terminal,
        x if x == ThemeKind::Auto as u8 => ThemeKind::Auto,
        _ => ThemeKind::Dark,
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AutoThemeConfig {
    pub dark_theme: Option<ThemeKind>,
    pub light_theme: Option<ThemeKind>,
}

pub fn current_kind() -> ThemeKind {
    if terminal_native_locked() {
        ThemeKind::Dark
    } else {
        selected_kind()
    }
}

pub fn selected_kind() -> ThemeKind {
    theme_kind_from_u8(CURRENT.load(Ordering::Relaxed))
}

/// Store the resolved palette. The host can refresh it while Auto remains active.
pub fn set(kind: ThemeKind) {
    let kind = if kind.is_auto() {
        set_auto_mode(true);
        resolve_auto()
    } else {
        kind
    };
    let kind = if kind.is_terminal_native() && !terminal_theme_enabled() {
        ThemeKind::Dark
    } else {
        kind
    };
    CURRENT.store(kind as u8, Ordering::Relaxed);
    REVISION.fetch_add(1, Ordering::Relaxed);
    sync_markdown_polarity();
}

/// Monotonic revision bumped whenever the active theme kind, terminal-native lock, or overrides change.
#[must_use]
pub fn revision() -> u64 {
    REVISION.load(Ordering::Relaxed)
}

/// Replace active theme overrides (pass `None` to clear).
pub fn set_overrides(overrides: Option<ThemeOverrides>) {
    *OVERRIDES.write().unwrap_or_else(|e| e.into_inner()) = overrides;
    REVISION.fetch_add(1, Ordering::Relaxed);
    sync_markdown_polarity();
}

/// Clone the currently active theme overrides, if any.
#[must_use]
pub fn current_overrides() -> Option<ThemeOverrides> {
    OVERRIDES.read().unwrap_or_else(|e| e.into_inner()).clone()
}

#[must_use]
pub fn terminal_native_locked() -> bool {
    TERMINAL_NATIVE_LOCK.load(Ordering::Relaxed)
}

/// Whether rendering uses the native terminal palette.
#[must_use]
pub fn terminal_native_active() -> bool {
    terminal_native_locked()
        || theme_kind_from_u8(CURRENT.load(Ordering::Relaxed)).is_terminal_native()
}

/// Engage or clear the terminal-native theme lock.
pub fn set_terminal_native_lock(locked: bool) {
    TERMINAL_NATIVE_LOCK.store(locked, Ordering::Relaxed);
    REVISION.fetch_add(1, Ordering::Relaxed);
    sync_markdown_polarity();
}

/// Read the `terminal` theme rollout gate (see [`TERMINAL_THEME_ENABLED`]).
#[must_use]
pub fn terminal_theme_enabled() -> bool {
    TERMINAL_THEME_ENABLED.load(Ordering::Relaxed)
}

pub fn set_terminal_theme_enabled(enabled: bool) {
    TERMINAL_THEME_ENABLED.store(enabled, Ordering::Relaxed);
    if !enabled && selected_kind().is_terminal_native() {
        set(ThemeKind::Dark);
    }
}

#[must_use]
pub fn is_auto_mode() -> bool {
    AUTO_MODE.load(Ordering::Relaxed)
}

pub fn set_auto_mode(enabled: bool) {
    AUTO_MODE.store(enabled, Ordering::Relaxed);
}

pub fn auto_theme_config() -> AutoThemeConfig {
    let mut config = *AUTO_THEME_CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    let allowed =
        |k: &ThemeKind| !k.is_auto() && (!k.is_terminal_native() || terminal_theme_enabled());
    config.dark_theme = config.dark_theme.filter(allowed);
    config.light_theme = config.light_theme.filter(allowed);
    config
}

pub fn set_auto_theme_config(config: AutoThemeConfig) {
    *AUTO_THEME_CONFIG.lock().unwrap_or_else(|e| e.into_inner()) = config;
}

/// Concrete kind, never `Auto`. Env (`TUI_THEME` / `LC_TUI_THEME`), then the in-memory selection (initially Dark).
#[must_use]
pub fn resolve_initial_theme() -> ThemeKind {
    resolve_initial_theme_from(
        env_theme_name().as_deref(),
        Some(if is_auto_mode() {
            ThemeKind::Auto
        } else {
            selected_kind()
        }),
        true,
    )
}

/// Variant of [`resolve_initial_theme`] without the OSC 11 startup fallback, for resolution after the terminal is initialized.
#[must_use]
pub fn resolve_initial_theme_no_osc11() -> ThemeKind {
    resolve_initial_theme_from(
        env_theme_name().as_deref(),
        Some(if is_auto_mode() {
            ThemeKind::Auto
        } else {
            selected_kind()
        }),
        false,
    )
}

fn env_theme_name() -> Option<String> {
    env_theme_name_from(&crate::engine::input::terminal::host::collect_unicode_env())
        .map(str::to_owned)
}

fn env_theme_name_from(env: &HashMap<String, String>) -> Option<&str> {
    for key in ["TUI_THEME", "LC_TUI_THEME"] {
        let Some(raw) = env
            .get(key)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        if ThemeKind::from_name(raw).is_some() {
            return Some(raw);
        }
    }
    None
}

fn resolve_initial_theme_from(
    env_theme: Option<&str>,
    config_theme: Option<ThemeKind>,
    osc11_fallback: bool,
) -> ThemeKind {
    if let Some(kind) = env_theme.and_then(ThemeKind::from_name) {
        return resolve_from_config(Some(kind), osc11_fallback);
    }
    resolve_from_config(config_theme, osc11_fallback)
}

/// Inner resolution logic, factored out for testability.
fn resolve_from_config(config_theme: Option<ThemeKind>, osc11_fallback: bool) -> ThemeKind {
    if let Some(kind) = config_theme {
        if kind.is_auto() {
            set_auto_mode(true);
            let appearance = if osc11_fallback {
                system_appearance::detect_with_osc11_fallback()
            } else {
                system_appearance::detect()
            };
            return resolve_from_appearance(appearance);
        }
        return kind;
    }

    ThemeKind::Dark
}

fn resolve_from_appearance(appearance: Option<system_appearance::SystemAppearance>) -> ThemeKind {
    let config = auto_theme_config();
    appearance
        .map(|a| system_appearance::to_theme_kind(a, config.dark_theme, config.light_theme))
        .unwrap_or(ThemeKind::Dark)
}

/// Desktop APIs and env hints only (no OSC 11), so it is safe while `EventStream` is active. Detection failure is `Dark`.
#[must_use]
pub fn resolve_auto() -> ThemeKind {
    resolve_from_appearance(system_appearance::detect())
}

/// Terminal-native: cap at ANSI-16 and use the dual-polarity accent map. Night pastels otherwise collapse to White and vanish on light profiles.
fn sync_markdown_polarity() {
    let native = terminal_native_active();
    crate::engine::markdown::set_color_level_cap(if native {
        crate::engine::markdown::ColorLevel::Basic
    } else {
        crate::engine::markdown::ColorLevel::TrueColor
    });
    crate::engine::markdown::set_polarity_safe_syntax(native);
}
