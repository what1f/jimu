//! Provides lazily-initialized `Syntect` instances for code highlighting.
//! Dark themes (Dark, TokyoNight) share `grok-night.tmTheme`; Light uses `grok-day.tmTheme` with deepened colors for light backgrounds.
//!
//! ## Terminal-native palette (minimal lock + `terminal` theme)
//!
//! While [`crate::engine::theme::cache::terminal_native_active`] holds (minimal mode's lock, or the `Terminal` kind), syntect still loads a night `.tmTheme`.
//! Its pastel RGB tokens collapse to **White** after naive ANSI-16 quantization, which is invisible on light terminal profiles.
//!
//! On that palette we do **not** detect light/dark. Instead:
//! 1. Near-gray tokens become `Color::Reset` (terminal default fg; always readable).
//! 2. Chromatic tokens map to base ANSI-16 accents (Red/Green/Yellow/Blue/Magenta/Cyan), never White/Black/bright variants.

use std::sync::OnceLock;

pub use crate::engine::markdown::Syntect;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

use crate::engine::theme::ThemeKind;

static SYNTECT_GROKNIGHT: OnceLock<Syntect> = OnceLock::new();
static SYNTECT_TOKYONIGHT: OnceLock<Syntect> = OnceLock::new();
static SYNTECT_GROKDAY: OnceLock<Syntect> = OnceLock::new();

/// Convert syntect style to ratatui foreground-only style, quantized for terminal color support (or polarity-safe under the terminal-native lock).
pub fn syntect_to_ratatui_fg(style: syntect::highlighting::Style) -> Style {
    let fg = syntect_rgb_to_fg(style.foreground.r, style.foreground.g, style.foreground.b);
    let mut out = Style::default().fg(fg);
    use syntect::highlighting::FontStyle;
    if style.font_style.contains(FontStyle::BOLD) {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    out
}

/// Map a syntect RGB triplet to a ratatui foreground color.
///
/// On the terminal-native palette, uses [`polarity_safe_syntax_fg`]; otherwise quantizes via the normal theme color pipeline.
pub fn syntect_rgb_to_fg(r: u8, g: u8, b: u8) -> Color {
    if crate::engine::theme::cache::terminal_native_active() {
        polarity_safe_syntax_fg(r, g, b)
    } else {
        crate::engine::theme::quantize(Color::Rgb(r, g, b))
    }
}

/// Low-chroma tokens use [`Color::Reset`] so host fg keeps contrast on both polarities.
/// Saturated hues map to base ANSI only — never White, Black, or Light*, which vanish on the opposite polarity.
pub fn polarity_safe_syntax_fg(r: u8, g: u8, b: u8) -> Color {
    let max = r.max(g).max(b) as i32;
    let min = r.min(g).min(b) as i32;
    let chroma = max - min;
    // Night default body (~#c8c8c8) and dim comments are near-gray.
    if chroma < 40 {
        return Color::Reset;
    }
    // Integer HSV hue in degrees [0, 360).
    let (ri, gi, bi) = (r as i32, g as i32, b as i32);
    let h = if max == ri {
        let mut h = (gi - bi) * 60 / chroma;
        if h < 0 {
            h += 360;
        }
        h
    } else if max == gi {
        (bi - ri) * 60 / chroma + 120
    } else {
        (ri - gi) * 60 / chroma + 240
    };
    // Magenta starts at 255° so Tokyo Night purple (#bb9af7, ~261°) lands Magenta rather than Blue; pure blues (~221°) stay Blue
    match h {
        0..30 | 330..=360 => Color::Red,
        30..90 => Color::Yellow,
        90..150 => Color::Green,
        150..210 => Color::Cyan,
        210..255 => Color::Blue,
        _ => Color::Magenta,
    }
}

/// Under the terminal-native lock, syntect tokens are remapped via [`polarity_safe_syntax_fg`].
/// Highlight failure uses `fallback` (typically Reset) so contrast is not lost.
pub fn highlight_line(
    text: &str,
    highlighter: &mut Option<syntect::easy::HighlightLines<'_>>,
    syntect: &Syntect,
    fallback: Style,
) -> Vec<Span<'static>> {
    if let Some(hl) = highlighter.as_mut()
        && let Ok(ranges) = hl.highlight_line(&format!("{text}\n"), &syntect.syntax_set)
    {
        let mut spans = Vec::new();
        for (style, segment) in ranges {
            let mut s = segment.to_owned();
            while s.ends_with('\n') || s.ends_with('\r') {
                s.pop();
            }
            if s.is_empty() {
                continue;
            }
            spans.push(Span::styled(s, syntect_to_ratatui_fg(style)));
        }
        if !spans.is_empty() {
            return spans;
        }
    }
    vec![Span::styled(text.to_string(), fallback)]
}

/// Terminal-native lock reports nominal `Dark`, so this returns the dark theme.
/// Colors are remapped later; do not load a light theme from OS/terminal polarity.
pub fn get_syntect() -> &'static Syntect {
    match crate::engine::theme::Theme::current_kind() {
        ThemeKind::Dark
        | ThemeKind::RosePineMoon
        | ThemeKind::OscuraMidnight
        // Terminal remaps every token in `syntect_rgb_to_fg`, so the
        // source palette only has to be a full one — polarity is irrelevant.
        | ThemeKind::Terminal
        | ThemeKind::Auto => SYNTECT_GROKNIGHT
            .get_or_init(|| Syntect::new(include_bytes!("assets/grok-night.tmTheme"))),
        ThemeKind::TokyoNight => SYNTECT_TOKYONIGHT
            .get_or_init(|| Syntect::new(include_bytes!("assets/tokyo-night.tmTheme"))),
        ThemeKind::Light => SYNTECT_GROKDAY
            .get_or_init(|| Syntect::new(include_bytes!("assets/grok-day.tmTheme"))),
    }
}
