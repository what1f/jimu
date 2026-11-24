// Adapted from the Grok Build pager (Apache-2.0). Copyright 2023-2026 SpaceXAI.

//! The logo is hidden entirely on legacy Windows consoles: the ConHost raster fonts do not cover the U+2800 braille block, so it renders as tofu.

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::engine::render::color::blend_color;
use crate::engine::theme::Theme;

#[derive(Clone, Copy)]
pub struct WelcomeLogo<'a> {
    pub full: &'a str,
    pub compact: &'a str,
}

impl Default for WelcomeLogo<'_> {
    fn default() -> Self {
        Self {
            full: "⠀⠀⠀⣀⣤⣤⣤⣀⠀⠀⠀
⠀⠀⣴⠟⠉⠀⠉⠻⣦⠀⠀
⠀⣼⠃⠀⠀⣀⠀⠀⠘⣧⠀
⠀⢿⡀⠀⠈⠿⠁⠀⢀⡿⠀
⠀⠀⠻⣦⣄⣀⣠⣴⠟⠀⠀
⠀⠀⠀⠀⠉⠉⠉⠀⠀⠀⠀",
            compact: "⠀⣠⣤⣄⠀
⢰⡏⠀⢹⡆
⠀⠻⠿⠟⠀",
        }
    }
}

/// Height at or above which the small logo is shown (below it, no logo).
const SMALL_LOGO_MIN_HEIGHT: u16 = 22;
/// Height at or above which the full logo is shown.
const FULL_LOGO_MIN_HEIGHT: u16 = 26;

/// Which logo art the stacked column shows.
/// The terminal height picks the tier; the stacked layout steps it down only while the column would not fit beside the draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogoTier {
    Full,
    Compact,
    Hidden,
}

impl LogoTier {
    pub fn for_height(window_height: u16) -> Self {
        Self::for_height_and_hidden(window_height, logo_hidden())
    }

    /// Takes the legacy-console flag as a parameter so tests can drive it directly.
    fn for_height_and_hidden(window_height: u16, hidden: bool) -> Self {
        if hidden || window_height < SMALL_LOGO_MIN_HEIGHT {
            Self::Hidden
        } else if window_height < FULL_LOGO_MIN_HEIGHT {
            Self::Compact
        } else {
            Self::Full
        }
    }

    fn art(self, logo: WelcomeLogo<'_>) -> Option<&str> {
        match self {
            Self::Full => Some(logo.full),
            Self::Compact => Some(logo.compact),
            Self::Hidden => None,
        }
    }

    pub fn rows(self, logo: WelcomeLogo<'_>) -> u16 {
        self.art(logo).map_or(0, count_lines)
    }

    /// The next smaller tier; `None` once hidden.
    pub fn step_down(self) -> Option<Self> {
        match self {
            Self::Full => Some(Self::Compact),
            Self::Compact => Some(Self::Hidden),
            Self::Hidden => None,
        }
    }
}

/// The braille art has no ASCII stand-in; see the module doc.
fn logo_hidden() -> bool {
    crate::engine::render::glyphs::is_legacy_windows_console()
}

fn non_empty_lines(logo: &str) -> impl Iterator<Item = &str> {
    logo.lines().filter(|l| !l.is_empty())
}

fn count_lines(logo: &str) -> u16 {
    non_empty_lines(logo).count() as u16
}

fn visual_width(logo: &str) -> u16 {
    non_empty_lines(logo)
        .map(unicode_width::UnicodeWidthStr::width)
        .max()
        .unwrap_or(24) as u16
}

/// Animation phase in seconds since the first render.
/// The phase is wall-clock based so the shimmer speed is independent of the frame rate.
fn anim_phase_secs() -> f32 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f32()
}

/// Shimmer redraw cadence in frames per second.
/// The sweep is slow, so a few fps looks smooth while sparing the long-lived welcome screen from full-rate repaints.
const SHIMMER_FPS: f32 = 12.0;

/// Quantized shimmer frame for the current wall-clock phase.
/// The welcome screen redraws only when this advances, throttling the animation to ~`SHIMMER_FPS` rather than the full event-loop tick rate.
/// The frame is pinned to 0 when the logo is hidden.
pub fn shimmer_frame() -> u64 {
    if logo_hidden() {
        return 0;
    }
    (anim_phase_secs() * SHIMMER_FPS) as u64
}

/// Per-glyph shine opacity in `[0, 1]` at normalized diagonal position `diag` (0 is bottom-left, 1 is top-right) and animation time `secs`.
/// A raised-cosine band sweeps from bottom-left to top-right and parks off-screen between sweeps; a gentle global pulse breathes underneath it.
/// 0 keeps the resting gray, 1 is full bright.
fn shine_opacity(diag: f32, secs: f32) -> f32 {
    const BAND: f32 = 0.38; // half-width of the shine band; wider means a more gradual falloff
    const CYCLE: f32 = 4.0; // seconds for one sweep plus its rest
    const SWEEP_FRAC: f32 = 0.32; // portion of the cycle spent sweeping (~1.3s glint, rest idles)
    const SHINE: f32 = 0.33; // peak shine strength
    const PULSE: f32 = 0.06; // global breathing amount
    const PULSE_SECS: f32 = 5.0; // breathing period

    let p = (secs % CYCLE) / CYCLE;
    let q = (p / SWEEP_FRAC).min(1.0); // parks the band off-screen during the rest
    let band_pos = -BAND + q * (1.0 + 2.0 * BAND);
    let pulse = PULSE * (0.5 - 0.5 * (std::f32::consts::TAU * secs / PULSE_SECS).cos());

    let d = (diag - band_pos).abs();
    let shine = if d < BAND {
        0.5 * (1.0 + (std::f32::consts::PI * d / BAND).cos())
    } else {
        0.0
    };
    (pulse + SHINE * shine).clamp(0.0, 1.0)
}

fn render_into(area: Rect, buf: &mut Buffer, theme: &Theme, logo: &str) {
    let lines: Vec<&str> = non_empty_lines(logo).collect();
    let rows = lines.len().max(1) as f32;
    let cols = lines
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(1)
        .max(1) as f32;
    let secs = anim_phase_secs();

    // Blend each glyph from the resting gray toward the bright text color by its shine opacity, so a sheen sweeps across the braille art
    // Adjacent glyphs that land on the same blended color share one Span to hold down the per-frame allocation
    let base = theme.gray;
    let hilite = theme.text_primary;
    let logo_lines: Vec<Line> = lines
        .iter()
        .enumerate()
        .map(|(row, line)| {
            let mut spans: Vec<Span> = Vec::new();
            let mut run = String::new();
            let mut run_color: Option<Color> = None;
            for (col, ch) in line.chars().enumerate() {
                // Sweep along the diagonal from bottom-left to top-right: the coordinate grows as col increases and row decreases
                let diag = (col as f32 + (rows - 1.0 - row as f32)) / (cols + rows);
                let color = blend_color(base, hilite, shine_opacity(diag, secs)).unwrap_or(base);
                if run_color != Some(color) {
                    if let Some(prev) = run_color {
                        spans.push(Span::styled(
                            std::mem::take(&mut run),
                            Style::default().fg(prev),
                        ));
                    }
                    run_color = Some(color);
                }
                run.push(ch);
            }
            if let Some(prev) = run_color {
                spans.push(Span::styled(run, Style::default().fg(prev)));
            }
            Line::from(spans).alignment(Alignment::Center)
        })
        .collect();
    Paragraph::new(logo_lines).render(area, buf);
}

pub fn logo_visual_width(window_height: u16, logo: WelcomeLogo<'_>) -> u16 {
    LogoTier::for_height(window_height)
        .art(logo)
        .map_or(24, visual_width)
}

pub fn render_logo_tier(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    tier: LogoTier,
    logo: WelcomeLogo<'_>,
) {
    if let Some(art) = tier.art(logo) {
        render_into(area, buf, theme, art);
    }
}

pub fn full_logo_line_count(logo: WelcomeLogo<'_>) -> u16 {
    if logo_hidden() {
        0
    } else {
        count_lines(logo.full)
    }
}

pub fn full_logo_visual_width(logo: WelcomeLogo<'_>) -> u16 {
    if logo_hidden() || logo.full.is_empty() {
        0
    } else {
        visual_width(logo.full)
    }
}

pub fn render_full_logo(area: Rect, buf: &mut Buffer, theme: &Theme, logo: WelcomeLogo<'_>) {
    if !logo_hidden() {
        render_into(area, buf, theme, logo.full);
    }
}
