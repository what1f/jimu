//! All colors come from the `Theme` struct. No hardcoded colors elsewhere.
//! The default theme is Dark (neutral gray base with TokyoNight accents).
//!
//! ## Color support
//!
//! Dark is defined in `Color::Rgb` (truecolor).
//! At startup, [`Theme::current()`] quantizes every color to the terminal's detected capability level via [`Theme::quantized`].
//! Runtime-generated colors (syntax highlighting, blending) are also quantized via [`color_support::quantize`].

pub mod cache;
pub mod color_support;
mod dark;
pub mod env_appearance;
mod light;
pub mod md_style;
pub mod osc11;
mod oscura;
mod rosepine;
pub mod system_appearance;
mod terminal_default;
pub mod tokyonight;

pub use color_support::quantize;
pub use tokyonight::{Theme, pulse_brightness, wave_brightness};

use ratatui::style::Color;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::LazyLock;

/// Declarative color overrides layered on top of a built-in base theme.
///
/// Colors accept `#rrggbb`, `#rgb`, `rgb(r,g,b)`, `ansi(n)` / `indexed(n)`, `reset` / `default`,
/// and standard ANSI color names (`red`, `bright_blue`, `dark_gray`, etc.).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemeOverrides {
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub colors: BTreeMap<String, String>,
}

/// Parse a CSS-like hex color (`#rrggbb` / `#rgb`), `rgb(r,g,b)`, `ansi(n)`, or standard ANSI name.
pub fn parse_color(spec: &str) -> Option<Color> {
    let raw = spec.trim();
    if raw.is_empty() {
        return None;
    }
    if let Some(hex) = raw.strip_prefix('#') {
        if !hex.is_ascii() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        return match hex.len() {
            6 => {
                let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
                let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
                let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
                Some(Color::Rgb(r, g, b))
            }
            3 => {
                let r = u8::from_str_radix(&hex[0..1], 16).ok()?;
                let g = u8::from_str_radix(&hex[1..2], 16).ok()?;
                let b = u8::from_str_radix(&hex[2..3], 16).ok()?;
                Some(Color::Rgb(r * 17, g * 17, b * 17))
            }
            _ => None,
        };
    }
    let lower = raw.to_ascii_lowercase();
    if let Some(inner) = lower.strip_prefix("rgb(").and_then(|s| s.strip_suffix(')')) {
        let mut parts = inner.split(',').map(|p| p.trim().parse::<u8>().ok());
        let r = parts.next()??;
        let g = parts.next()??;
        let b = parts.next()??;
        if parts.next().is_none() {
            return Some(Color::Rgb(r, g, b));
        }
        return None;
    }
    if let Some(inner) = lower
        .strip_prefix("ansi(")
        .or_else(|| lower.strip_prefix("indexed("))
        .and_then(|s| s.strip_suffix(')'))
    {
        return inner.trim().parse::<u8>().ok().map(Color::Indexed);
    }
    let normalized = lower.replace(['-', ' '], "_");
    match normalized.as_str() {
        "reset" | "default" | "none" | "transparent" => Some(Color::Reset),
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" | "purple" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "gray" | "grey" => Some(Color::Gray),
        "dark_gray" | "dark_grey" | "darkgray" | "darkgrey" | "bright_black" => {
            Some(Color::DarkGray)
        }
        "light_red" | "bright_red" => Some(Color::LightRed),
        "light_green" | "bright_green" => Some(Color::LightGreen),
        "light_yellow" | "bright_yellow" => Some(Color::LightYellow),
        "light_blue" | "bright_blue" => Some(Color::LightBlue),
        "light_magenta" | "bright_magenta" | "light_purple" | "bright_purple" => {
            Some(Color::LightMagenta)
        }
        "light_cyan" | "bright_cyan" => Some(Color::LightCyan),
        "white" | "bright_white" | "light_white" => Some(Color::White),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThemeKind {
    Dark = 0,
    Light = 1,
    TokyoNight = 2,
    RosePineMoon = 3,
    OscuraMidnight = 5,
    /// Every bg is `Reset` so the terminal canvas shows through; legible on both polarities without appearance detection.
    /// Hidden and unparseable while `cache::terminal_theme_enabled()` is off.
    Terminal = 6,
    /// Follow system appearance. `cache::CURRENT` holds only the resolved concrete kind. Excluded from [`ALL`].
    Auto = 4,
}

impl ThemeKind {
    /// All theme kinds (including those that may not work on the current terminal).
    pub const ALL: &[ThemeKind] = &[
        ThemeKind::Dark,
        ThemeKind::Light,
        ThemeKind::TokyoNight,
        ThemeKind::RosePineMoon,
        ThemeKind::OscuraMidnight,
        ThemeKind::Terminal,
    ];

    /// [`ALL`] minus gated `terminal`. Ignores color capability ([`available()`] filters that). Derived from [`ALL`] so a new theme cannot be omitted.
    pub fn selectable() -> &'static [ThemeKind] {
        if cache::terminal_theme_enabled() {
            Self::ALL
        } else {
            static GATED: LazyLock<Vec<ThemeKind>> = LazyLock::new(|| {
                ThemeKind::ALL
                    .iter()
                    .copied()
                    .filter(|kind| !kind.is_terminal_native())
                    .collect()
            });
            &GATED
        }
    }

    /// Theme kinds available on the current terminal.
    ///
    /// [`selectable()`] minus themes that require truecolor when the terminal does not support it (e.g., macOS Terminal.app is 256-color).
    pub fn available() -> &'static [ThemeKind] {
        if color_support::detect().has_truecolor() {
            return Self::selectable();
        }
        if cache::terminal_theme_enabled() {
            static NO_TRUECOLOR: LazyLock<Vec<ThemeKind>> = LazyLock::new(|| {
                ThemeKind::ALL
                    .iter()
                    .copied()
                    .filter(|kind| !kind.requires_truecolor())
                    .collect()
            });
            &NO_TRUECOLOR
        } else {
            static NO_TRUECOLOR_GATED: LazyLock<Vec<ThemeKind>> = LazyLock::new(|| {
                ThemeKind::ALL
                    .iter()
                    .copied()
                    .filter(|kind| !kind.requires_truecolor() && !kind.is_terminal_native())
                    .collect()
            });
            &NO_TRUECOLOR_GATED
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::TokyoNight => "tokyonight",
            Self::Light => "light",
            Self::RosePineMoon => "rosepine-moon",
            Self::OscuraMidnight => "oscura-midnight",
            Self::Terminal => "terminal",
            Self::Auto => "auto",
        }
    }

    /// TokyoNight's blue-tinted backgrounds lose their character below truecolor; neutral grays survive quantization.
    pub fn requires_truecolor(self) -> bool {
        match self {
            Self::Dark => false,
            Self::TokyoNight => true,
            Self::Light => false,
            Self::RosePineMoon => true,
            Self::OscuraMidnight => true,
            // Reset plus named ANSI-16 entries only — nothing to quantize.
            Self::Terminal => false,
            // Auto is resolved to a concrete theme before rendering.
            Self::Auto => false,
        }
    }

    /// Whether this kind paints the terminal-native palette ([`Theme::terminal_default`]) instead of an RGB palette, and so needs the same polarity-safe rendering paths as minimal mode's lock.
    #[must_use]
    pub fn is_terminal_native(self) -> bool {
        self == Self::Terminal
    }

    /// Alternate lowercase spellings accepted by [`from_name`](Self::from_name), excluding [`display_name`](Self::display_name).
    pub fn aliases(self) -> &'static [&'static str] {
        match self {
            Self::Dark => &["night"],
            Self::TokyoNight => &["tokyo-night", "tokyo"],
            Self::Light => &["day"],
            Self::RosePineMoon => &["rosepine", "rose-pine", "rose-pine-moon"],
            Self::OscuraMidnight => &["oscura"],
            Self::Terminal => &["terminal-default", "transparent", "native"],
            Self::Auto => &["system"],
        }
    }

    /// Parse a theme name (case-insensitive) against [`display_name`](Self::display_name) and [`aliases`](Self::aliases).
    /// Every conversion from string to `ThemeKind` must go through this function.
    /// While the `terminal` rollout gate is off its names do not parse, so a configured or typed value falls back like any unknown name.
    pub fn from_name(name: &str) -> Option<Self> {
        let lower = name.to_lowercase();
        let kind = Self::ALL
            .iter()
            .chain(std::iter::once(&Self::Auto))
            .copied()
            .find(|kind| {
                kind.display_name() == lower || kind.aliases().contains(&lower.as_str())
            })?;
        if kind.is_terminal_native() && !cache::terminal_theme_enabled() {
            return None;
        }
        Some(kind)
    }

    /// Whether this is the meta "auto" variant (resolved at runtime).
    #[must_use]
    pub fn is_auto(self) -> bool {
        self == Self::Auto
    }
}

/// `FromStr` wrapper around [`ThemeKind::from_name`].
impl std::str::FromStr for ThemeKind {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_name(s).ok_or(())
    }
}

/// Resolve a theme string to its canonical `&'static str` name.
/// Used by both dispatch and registry layers.
pub fn canonical_name(value: &str) -> Option<&'static str> {
    ThemeKind::from_name(value).map(|k| k.display_name())
}

/// Human-friendly display name for a canonical theme value (e.g. `"dark"` becomes `"Dark"`).
/// Falls back to `value` verbatim.
pub fn display_name_for_canonical(value: &str) -> &str {
    match value {
        "auto" => "Auto",
        "dark" => "Dark",
        "light" => "Light",
        "tokyonight" => "Tokyo Night",
        "rosepine-moon" => "Rose Pine Moon",
        "oscura-midnight" => "Oscura Midnight",
        "terminal" => "Terminal",
        other => other,
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

impl Theme {
    /// Truecolor passes through; 256-color maps to nearest index; 16-color maps to ANSI names.
    pub fn quantized(self, level: color_support::ColorLevel) -> Self {
        use color_support::quantize_color;
        let q = |c: ratatui::style::Color| quantize_color(c, level);
        Self {
            bg_base: q(self.bg_base),
            bg_light: q(self.bg_light),
            bg_dark: q(self.bg_dark),
            bg_highlight: q(self.bg_highlight),
            bg_hover: q(self.bg_hover),
            bg_terminal: q(self.bg_terminal),

            accent_user: q(self.accent_user),
            accent_assistant: q(self.accent_assistant),
            accent_thinking: q(self.accent_thinking),
            accent_tool: q(self.accent_tool),
            accent_system: q(self.accent_system),
            accent_error: q(self.accent_error),
            accent_success: q(self.accent_success),
            accent_running: q(self.accent_running),
            accent_skill: q(self.accent_skill),

            text_primary: q(self.text_primary),
            text_secondary: q(self.text_secondary),

            gray_dim: q(self.gray_dim),
            gray: q(self.gray),
            gray_bright: q(self.gray_bright),

            command: q(self.command),
            path: q(self.path),
            running: q(self.running),
            warning: q(self.warning),

            fuzzy_accent: q(self.fuzzy_accent),

            accent_plan: q(self.accent_plan),

            accent_verify: q(self.accent_verify),

            accent_remember: q(self.accent_remember),

            selection_border: q(self.selection_border),
            hover_border: q(self.hover_border),
            prompt_border: q(self.prompt_border),
            prompt_border_active: q(self.prompt_border_active),

            accent_model: q(self.accent_model),

            scrollbar_bg: q(self.scrollbar_bg),
            scrollbar_fg: q(self.scrollbar_fg),

            diff_delete_bg: q(self.diff_delete_bg),
            diff_delete_fg: q(self.diff_delete_fg),
            diff_insert_bg: q(self.diff_insert_bg),
            diff_insert_fg: q(self.diff_insert_fg),
            diff_equal_fg: q(self.diff_equal_fg),
            diff_gutter_fg: q(self.diff_gutter_fg),

            bg_visual: q(self.bg_visual),

            paste_bg: q(self.paste_bg),
            paste_fg: q(self.paste_fg),
            paste_dim: q(self.paste_dim),

            md_heading_h1: q(self.md_heading_h1),
            md_heading_h1_mod: self.md_heading_h1_mod,
            md_heading_h2: q(self.md_heading_h2),
            md_heading_h2_mod: self.md_heading_h2_mod,
            md_heading_h3: q(self.md_heading_h3),
            md_heading_h3_mod: self.md_heading_h3_mod,
            md_heading_h4: q(self.md_heading_h4),
            md_heading_h4_mod: self.md_heading_h4_mod,
            md_heading_h5: q(self.md_heading_h5),
            md_heading_h5_mod: self.md_heading_h5_mod,
            md_heading_h6: q(self.md_heading_h6),
            md_heading_h6_mod: self.md_heading_h6_mod,
            md_code: q(self.md_code),
            md_task_checked: q(self.md_task_checked),
            md_task_unchecked: q(self.md_task_unchecked),
            md_muted: q(self.md_muted),
            md_code_bg: q(self.md_code_bg),
            md_text: q(self.md_text),
            link_fg: q(self.link_fg),
        }
    }

    /// Quantized cached kind. Windows boosts contrast so structural RGB survives display gamma.
    /// Basic or legacy ConHost pins chrome to ANSI names; otherwise every dark RGB collapses onto one ANSI16 slot.
    pub fn current() -> Self {
        let level = color_support::detect();
        if cache::terminal_native_locked() {
            return Self::terminal_default().quantized(level);
        }
        let kind = cache::current_kind();
        let overrides = cache::current_overrides();
        let effective_base_kind = overrides
            .as_ref()
            .and_then(|o| o.base.as_deref())
            .and_then(ThemeKind::from_name)
            .filter(|k| !k.is_auto())
            .unwrap_or(kind);
        let base = Self::for_kind(kind);
        let base = if let Some(overrides) = overrides.as_ref() {
            base.apply_overrides_raw(overrides)
        } else {
            base
        };
        // Before polarity adaptations: contrast boost and ANSI16 overrides would paint opaque bgs over `Reset`.
        if effective_base_kind.is_terminal_native() {
            return base.quantized(level);
        }
        // Sample polarity before quantizing
        // After quantization `bg_base` may land on a named or indexed entry whose luminance depends on the host palette
        let dark = base.is_dark();
        let adapted = if cfg!(target_os = "windows") {
            base.windows_contrast_boost(dark)
        } else {
            base
        };
        let adapted = adapted.quantized(level);
        // Basic or legacy ConHost below truecolor: naive quantization collapses dark RGB onto Black.
        // `has_color()` is required so `NO_COLOR` is not defeated by named-ANSI repaints of `Reset`.
        if level.has_color()
            && (level == color_support::ColorLevel::Basic
                || (crate::engine::render::glyphs::is_legacy_windows_console()
                    && !level.has_truecolor()))
        {
            adapted.ansi16_chrome_overrides(dark)
        } else {
            adapted
        }
    }

    /// Return the unquantized palette for a concrete [`ThemeKind`].
    pub fn for_kind(kind: ThemeKind) -> Self {
        match kind {
            ThemeKind::Dark => Self::dark(),
            ThemeKind::TokyoNight => Self::tokyonight(),
            ThemeKind::Light => Self::light(),
            ThemeKind::RosePineMoon => Self::rosepine_moon(),
            ThemeKind::OscuraMidnight => Self::oscura_midnight(),
            ThemeKind::Terminal => Self::terminal(),
            ThemeKind::Auto => Self::dark(),
        }
    }

    /// Apply [`ThemeOverrides`] on top of `self` (or `overrides.base` when specified) and quantize
    /// to the active terminal color capability.
    #[must_use]
    pub fn with_overrides(&self, overrides: &ThemeOverrides) -> Self {
        self.apply_overrides_raw(overrides)
            .quantized(color_support::detect())
    }

    fn apply_overrides_raw(&self, overrides: &ThemeOverrides) -> Self {
        let mut theme = match overrides.base.as_deref().and_then(ThemeKind::from_name) {
            Some(ThemeKind::Auto) | None => *self,
            Some(kind) => Self::for_kind(kind),
        };

        let parsed: Vec<(String, Color)> = overrides
            .colors
            .iter()
            .filter_map(|(k, v)| {
                let color = parse_color(v)?;
                let key = k.trim().to_ascii_lowercase().replace('-', "_");
                Some((key, color))
            })
            .collect();

        // Pass 1: semantic group aliases so explicit field keys in pass 2 always win.
        for (key, color) in &parsed {
            let color = *color;
            match key.as_str() {
                "bg" => theme.bg_base = color,
                "bg_surface" | "surface" => {
                    theme.bg_light = color;
                    theme.bg_dark = color;
                }
                "fg" => {
                    theme.text_primary = color;
                    theme.md_text = color;
                }
                "text_muted" | "muted" => {
                    theme.gray = color;
                    theme.md_muted = color;
                }
                "dim" => theme.gray_dim = color,
                "accent_primary" | "accent" => {
                    theme.accent_user = color;
                    theme.accent_model = color;
                    theme.prompt_border_active = color;
                }
                "border" => theme.prompt_border = color,
                "border_focus" => theme.prompt_border_active = color,
                "accent_warning" => theme.warning = color,
                "error" => theme.accent_error = color,
                "success" => theme.accent_success = color,
                _ => {}
            }
        }

        // Pass 2: exact field names.
        for (key, color) in parsed {
            match key.as_str() {
                "bg_base" => theme.bg_base = color,
                "bg_light" => theme.bg_light = color,
                "bg_dark" => theme.bg_dark = color,
                "bg_highlight" => theme.bg_highlight = color,
                "bg_hover" => theme.bg_hover = color,
                "bg_terminal" => theme.bg_terminal = color,
                "bg_visual" => theme.bg_visual = color,
                "accent_user" => theme.accent_user = color,
                "accent_assistant" => theme.accent_assistant = color,
                "accent_thinking" => theme.accent_thinking = color,
                "accent_tool" => theme.accent_tool = color,
                "accent_system" => theme.accent_system = color,
                "accent_error" => theme.accent_error = color,
                "accent_success" => theme.accent_success = color,
                "accent_running" => theme.accent_running = color,
                "accent_skill" => theme.accent_skill = color,
                "accent_plan" => theme.accent_plan = color,
                "accent_verify" => theme.accent_verify = color,
                "accent_remember" => theme.accent_remember = color,
                "accent_model" => theme.accent_model = color,
                "text_primary" => {
                    theme.text_primary = color;
                    theme.md_text = color;
                }
                "text_secondary" => theme.text_secondary = color,
                "gray_dim" => theme.gray_dim = color,
                "gray" => theme.gray = color,
                "gray_bright" => theme.gray_bright = color,
                "command" => theme.command = color,
                "path" => theme.path = color,
                "running" => theme.running = color,
                "warning" => theme.warning = color,
                "fuzzy_accent" => theme.fuzzy_accent = color,
                "selection_border" => theme.selection_border = color,
                "hover_border" => theme.hover_border = color,
                "prompt_border" => theme.prompt_border = color,
                "prompt_border_active" => theme.prompt_border_active = color,
                "scrollbar_bg" => theme.scrollbar_bg = color,
                "scrollbar_fg" => theme.scrollbar_fg = color,
                "diff_delete_bg" => theme.diff_delete_bg = color,
                "diff_delete_fg" => theme.diff_delete_fg = color,
                "diff_insert_bg" => theme.diff_insert_bg = color,
                "diff_insert_fg" => theme.diff_insert_fg = color,
                "diff_equal_fg" => theme.diff_equal_fg = color,
                "diff_gutter_fg" => theme.diff_gutter_fg = color,
                "paste_bg" => theme.paste_bg = color,
                "paste_fg" => theme.paste_fg = color,
                "paste_dim" => theme.paste_dim = color,
                "md_heading_h1" => theme.md_heading_h1 = color,
                "md_heading_h2" => theme.md_heading_h2 = color,
                "md_heading_h3" => theme.md_heading_h3 = color,
                "md_heading_h4" => theme.md_heading_h4 = color,
                "md_heading_h5" => theme.md_heading_h5 = color,
                "md_heading_h6" => theme.md_heading_h6 = color,
                "md_code" => theme.md_code = color,
                "md_code_bg" => theme.md_code_bg = color,
                "md_task_checked" => theme.md_task_checked = color,
                "md_task_unchecked" => theme.md_task_unchecked = color,
                "md_muted" => theme.md_muted = color,
                "md_text" => theme.md_text = color,
                "link_fg" => theme.link_fg = color,
                _ => {}
            }
        }
        theme
    }

    pub fn current_kind() -> ThemeKind {
        cache::current_kind()
    }

    /// Whether this theme paints no diff row bands (`diff_*_bg` is `Reset`).
    /// In that case changed diff lines carry a whole-line red/green *foreground* instead of syntax highlighting on a colored band.
    #[must_use]
    pub fn diff_uses_line_fg(&self) -> bool {
        use ratatui::style::Color;
        self.diff_delete_bg == Color::Reset && self.diff_insert_bg == Color::Reset
    }

    /// In-memory only; the event loop emits OSC 12. No-op while the terminal-native lock is engaged.
    pub fn apply_kind(kind: ThemeKind) -> ThemeKind {
        if cache::terminal_native_locked() {
            return cache::current_kind();
        }
        cache::set_auto_mode(kind.is_auto());
        let concrete = if kind.is_auto() {
            cache::resolve_auto()
        } else {
            kind
        };
        let effective = Self::clamp_to_terminal(concrete);
        cache::set(effective);
        cache::current_kind()
    }

    fn clamp_to_terminal(kind: ThemeKind) -> ThemeKind {
        if kind.requires_truecolor() && !color_support::detect().has_truecolor() {
            ThemeKind::Dark
        } else {
            kind
        }
    }

    /// Native ~12-unit RGB steps collapse under Windows display gamma; ConHost needs ~24-32 levels per channel.
    /// Prompt-block bg is asymmetric: a dark step on a light canvas weighs more, so that direction is pushed less.
    fn windows_contrast_boost(self, dark: bool) -> Self {
        use ratatui::style::Color;

        /// Move `color` `amount` levels per channel further from `base`.
        /// Returns `color` unchanged when either side isn't RGB.
        fn push_away(base: Color, color: Color, amount: i16) -> Color {
            let Color::Rgb(br, b_green, bb) = base else {
                return color;
            };
            let Color::Rgb(cr, cg, cb) = color else {
                return color;
            };
            let base_lum = br as i16 + b_green as i16 + bb as i16;
            let color_lum = cr as i16 + cg as i16 + cb as i16;
            let sign: i16 = if color_lum >= base_lum { 1 } else { -1 };
            let nudge = |c: u8| (c as i16 + sign * amount).clamp(0, 255) as u8;
            Color::Rgb(nudge(cr), nudge(cg), nudge(cb))
        }

        let bg = self.bg_base;
        let user_block_push = if dark { 28 } else { 8 };
        Self {
            bg_light: push_away(bg, self.bg_light, user_block_push),
            bg_dark: push_away(bg, self.bg_dark, 16),
            bg_highlight: push_away(bg, self.bg_highlight, 28),
            bg_hover: push_away(bg, self.bg_hover, 16),
            gray_dim: push_away(bg, self.gray_dim, 40),
            selection_border: push_away(bg, self.selection_border, 36),
            prompt_border: push_away(bg, self.prompt_border, 40),
            prompt_border_active: push_away(bg, self.prompt_border_active, 60),
            hover_border: push_away(bg, self.hover_border, 28),
            scrollbar_bg: push_away(bg, self.scrollbar_bg, 16),
            scrollbar_fg: push_away(bg, self.scrollbar_fg, 32),
            bg_visual: push_away(bg, self.bg_visual, 16),
            md_code_bg: push_away(bg, self.md_code_bg, 16),
            ..self
        }
    }

    /// Style for shell command suggestion ghost text (dimmed italic).
    /// Inherits [`Self::dim`]'s polarity-safe rule on terminal-native
    /// palettes instead of painting bright black.
    pub fn ghost_text_style(&self) -> ratatui::style::Style {
        self.dim().add_modifier(ratatui::style::Modifier::ITALIC)
    }

    /// True when `bg_base` reads as dark per BT.709 luminance.
    /// Must be called pre-quantization while `bg_base` is still RGB; named/Reset fall back to "dark" (the default theme polarity).
    pub fn is_dark(&self) -> bool {
        use ratatui::style::Color;
        let (r, g, b) = match self.bg_base {
            Color::Rgb(r, g, b) => (r, g, b),
            Color::Indexed(n) => crate::engine::render::color::indexed_to_rgb(n),
            _ => return true,
        };
        crate::engine::theme::osc11::classify_luminance(r, g, b)
            == crate::engine::theme::system_appearance::SystemAppearance::Dark
    }

    /// ANSI16 has two grays; naive distance collapses pastel accents onto that ramp and erases state.
    /// Pin chrome to the four neutrals and accents to hue-preserving slots (bright on dark, normal on light).
    /// Canvas-blending surfaces pin to theme polarity, not `Reset`, or the terminal profile punches holes through them.
    fn ansi16_chrome_overrides(self, dark: bool) -> Self {
        use ratatui::style::Color;
        // Theme polarity canvas: what the body bg should look like
        // Matches the natural quantize result for `bg_base` on both built-in themes
        // The explicit pin gives themes whose bg RGB doesn't quantize cleanly the right polarity anyway
        let canvas_bg = if dark { Color::Black } else { Color::White };
        // One palette step off the canvas: DarkGray (ANSI 8) on black, Gray (ANSI 7 / silver) on white
        // ANSI16 has no slot between these and the canvas, so the elevation reads louder than the truecolor design
        // But it's guaranteed visible on every 16-color terminal, including museum-grade `TERM=ansi` boxes
        let elevated_bg = if dark { Color::DarkGray } else { Color::Gray };
        // Max-contrast fg for focused chrome and assistant body.
        let high_contrast_fg = if dark { Color::White } else { Color::Black };
        // Mid-tone fg for muted labels: silver on black, dark-gray on white
        // This is the higher-contrast of the two muted slots, used for secondary text that still needs to read clearly
        let muted_fg = if dark { Color::Gray } else { Color::DarkGray };
        // Low-contrast fg for genuinely dim chrome (soft modal/picker frames, the unselected `>` prompt indicator)
        // The polarity is the INVERSE of `muted_fg`: DarkGray sits next to Black, Gray (silver) next to White
        // A separate slot keeps dim chrome from reading at the same weight as secondary text
        let dim_fg = if dark { Color::DarkGray } else { Color::Gray };

        // ── Polarity-aware semantic hues ────────────────────────────
        // Normal ANSI hues (idx 1-7) are designed at ~50% luminance and read well on light backgrounds
        // Light variants (idx 9-15) are full saturation and read well on dark backgrounds
        let red = if dark { Color::LightRed } else { Color::Red };
        let green = if dark {
            Color::LightGreen
        } else {
            Color::Green
        };
        let yellow = if dark {
            Color::LightYellow
        } else {
            Color::Yellow
        };
        let blue = if dark { Color::LightBlue } else { Color::Blue };
        let magenta = if dark {
            Color::LightMagenta
        } else {
            Color::Magenta
        };
        let cyan = if dark { Color::LightCyan } else { Color::Cyan };
        Self {
            // ── Elevated surfaces: one step off the canvas ──────────────
            // Hover/highlight/visual-selection rows need to read as a distinct "raised" band against the body
            // Without this every Dark bg field quantizes to Color::Black and these become invisible
            bg_light: elevated_bg,
            bg_highlight: elevated_bg,
            bg_hover: elevated_bg,
            bg_visual: elevated_bg,

            // Theme polarity, not Reset: 16-color cannot sunken-blend, and Reset would follow a disagreeing terminal canvas.
            bg_dark: canvas_bg,
            md_code_bg: canvas_bg,
            paste_bg: canvas_bg,
            scrollbar_bg: canvas_bg,

            // Idle frame uses muted_fg: DarkGray (ANSI 8) is near-bg on many palettes and the frame vanishes.
            // Hover stays DarkGray; selection is muted_fg; focus is high_contrast_fg.
            prompt_border: muted_fg,
            prompt_border_active: high_contrast_fg,
            selection_border: muted_fg,
            hover_border: Color::DarkGray,

            // Scrollbar thumb stays visible against the canvas-matched track.
            scrollbar_fg: muted_fg,

            // ── Foreground / text hierarchy ─────────────────────────────
            // Prompt textarea and chrome captions use these directly
            // blend_color cannot mix named ANSI, so they must already be readable slots
            text_primary: high_contrast_fg,
            text_secondary: muted_fg,
            md_text: high_contrast_fg,
            // Selected user-prompt `>` (drives the user selection accent and the OSC 12 cursor color) takes max-contrast fg
            // The selection pops against the canvas: White on dark, Black on light
            accent_user: high_contrast_fg,
            // Two-tier grey: `gray`/`gray_bright` carry secondary text and need readable contrast, so they take `muted_fg`
            // `gray_dim` is for genuinely faded chrome (modal frames, unselected `>` prompt indicator) and takes `dim_fg`
            // Two tiers is the most ANSI16 can express without colliding with the elevated-bg slot
            gray: muted_fg,
            gray_bright: muted_fg,
            gray_dim: dim_fg,

            // ANSI16 has 6 chromatic slots, so sub-hues fold onto the dominant family. Magenta covers the purple/violet accents.
            accent_assistant: magenta,
            accent_thinking: magenta,
            accent_running: magenta,
            accent_verify: magenta,
            // Red family: error states and diff deletes
            accent_error: red,
            diff_delete_fg: red,
            // Green family: success states, remember mode, diff inserts
            accent_success: green,
            accent_remember: green,
            diff_insert_fg: green,
            // Blue family: system messages, skill invocations, fuzzy search matches
            accent_system: blue,
            accent_skill: blue,
            fuzzy_accent: blue,
            // Cyan family: model name and the legacy `running` indicator (distinct from the magenta `accent_running` used for subagents).
            // ANSI16 has no separate teal slot, so the truecolor teal model accent folds onto cyan here.
            accent_model: cyan,
            running: cyan,
            // Yellow family: warning text, plan-mode gold, shell commands, file paths
            // ANSI16 has no orange or gold slot, so warm accents all fold onto yellow
            command: yellow,
            warning: yellow,
            path: yellow,
            accent_plan: yellow,

            // Markdown content: naive Basic quantize lands Dark md_code / md_muted / h4-h6 on DarkGray (ANSI 8)
            // Many palettes tune that slot near the background, so inline code and table borders vanish in tmux over ssh
            // `md_muted` uses muted_fg, not dim_fg: format_table also stacks DIM on table borders
            md_heading_h1: cyan,
            md_heading_h2: blue,
            md_heading_h3: magenta,
            md_heading_h4: high_contrast_fg,
            md_heading_h5: muted_fg,
            md_heading_h6: muted_fg,
            md_code: cyan,
            md_muted: muted_fg,
            md_task_checked: green,
            md_task_unchecked: muted_fg,
            link_fg: blue,
            diff_equal_fg: muted_fg,
            diff_gutter_fg: muted_fg,
            ..self
        }
    }
}

/// Gates OSC 112: an unprompted reset makes Ghostty latch the cursor color and stop tracking theme changes. Only undo what we painted.
static CURSOR_COLOR_APPLIED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// OSC 12 wants RGB even below truecolor, so every variant is resolved back. `Reset` yields `None` so the terminal keeps its profile cursor.
pub fn cursor_color_escape() -> Option<String> {
    let theme = Theme::current();
    let (r, g, b) = crate::engine::render::color::resolve_to_rgb(theme.accent_user)?;
    Some(format!("\x1b]12;rgb:{r:02x}/{g:02x}/{b:02x}\x07"))
}

/// Write [`cursor_color_escape`] inline. Startup only, while no writer thread is live; once the event loop runs, its OSC 12/112 tracker is the single emitter (including screen-mode switches).
pub fn apply_cursor_color() {
    use std::io::Write;
    use std::sync::atomic::Ordering;
    let Some(escape) = cursor_color_escape() else {
        // No RGB accent (terminal-native palette, NO_COLOR): undo our own
        // paint if any, otherwise stay silent (see `CURSOR_COLOR_APPLIED`).
        reset_cursor_color_if_applied();
        return;
    };
    CURSOR_COLOR_APPLIED.store(true, Ordering::Relaxed);
    {
        let _lock = std::io::stderr().lock();
        let Ok(mut stderr) = crate::engine::tty::dup_tui_stderr() else {
            return;
        };
        let _ = stderr.write_all(escape.as_bytes());
        let _ = stderr.flush();
    }
}

/// [`reset_cursor_color`] gated on this session having painted the cursor
/// (see [`CURSOR_COLOR_APPLIED`]), clearing the flag. Atomic swap, so
/// panic- and signal-handler-safe.
pub fn reset_cursor_color_if_applied() {
    if CURSOR_COLOR_APPLIED.swap(false, std::sync::atomic::Ordering::Relaxed) {
        reset_cursor_color();
    }
}

/// The OSC 112 escape that resets the terminal cursor color to its default.
pub const CURSOR_COLOR_RESET_ESCAPE: &str = "\x1b]112\x07";

/// Record that a cursor-color escape went out over the render wire (the event
/// loop's queued emitter, which bypasses [`apply_cursor_color`]), keeping
/// [`CURSOR_COLOR_APPLIED`] truthful for the teardown reset.
pub fn note_cursor_color_on_wire(applied: bool) {
    CURSOR_COLOR_APPLIED.store(applied, std::sync::atomic::Ordering::Relaxed);
}

/// Reset the terminal cursor color to the terminal's default via OSC 112.
///
/// Called on shutdown to restore the user's original cursor appearance.
pub fn reset_cursor_color() {
    use std::io::Write;
    {
        let _lock = std::io::stderr().lock();
        let Ok(mut stderr) = crate::engine::tty::dup_tui_stderr() else {
            return;
        };
        let _ = stderr.write_all(CURSOR_COLOR_RESET_ESCAPE.as_bytes());
        let _ = stderr.flush();
    }
}
