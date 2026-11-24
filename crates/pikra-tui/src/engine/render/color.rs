//! Blends colors toward a base color for fade transitions, e.g. sticky headers being pushed off screen.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::text::{Line, Span};

/// The 6 channel values in the 256-color 6×6×6 cube.
const CUBE_VALUES: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// 0–15 xterm ANSI, 16–231 color cube, 232–255 grayscale. A customized terminal palette will differ.
pub fn indexed_to_rgb(index: u8) -> (u8, u8, u8) {
    match index {
        // Standard colors (0-7): common xterm defaults
        0 => (0, 0, 0),
        1 => (128, 0, 0),
        2 => (0, 128, 0),
        3 => (128, 128, 0),
        4 => (0, 0, 128),
        5 => (128, 0, 128),
        6 => (0, 128, 128),
        7 => (192, 192, 192),
        // Bright colors (8–15)
        8 => (128, 128, 128),
        9 => (255, 0, 0),
        10 => (0, 255, 0),
        11 => (255, 255, 0),
        12 => (0, 0, 255),
        13 => (255, 0, 255),
        14 => (0, 255, 255),
        15 => (255, 255, 255),
        // 6×6×6 color cube (16–231)
        16..=231 => {
            let n = index - 16;
            // n is 0..=215, so each cube axis is 0..=5.
            let Some(&r) = CUBE_VALUES.get((n / 36) as usize) else {
                return (0, 0, 0);
            };
            let Some(&g) = CUBE_VALUES.get(((n % 36) / 6) as usize) else {
                return (0, 0, 0);
            };
            let Some(&b) = CUBE_VALUES.get((n % 6) as usize) else {
                return (0, 0, 0);
            };
            (r, g, b)
        }
        // Grayscale ramp (232–255): value = 8 + (index − 232) × 10
        232..=255 => {
            let v = 8 + (index - 232) * 10;
            (v, v, v)
        }
    }
}

/// Nearest of the color cube and the grayscale ramp by squared Euclidean distance.
pub fn nearest_indexed(r: u8, g: u8, b: u8) -> u8 {
    // --- nearest in the 6×6×6 color cube (16–231) ---
    let ri = nearest_cube_channel(r);
    let gi = nearest_cube_channel(g);
    let bi = nearest_cube_channel(b);
    let cube_idx = 16 + 36 * ri as u16 + 6 * gi as u16 + bi as u16;
    let Some(&cube_r) = CUBE_VALUES.get(ri as usize) else {
        return 16;
    };
    let Some(&cube_g) = CUBE_VALUES.get(gi as usize) else {
        return 16;
    };
    let Some(&cube_b) = CUBE_VALUES.get(bi as usize) else {
        return 16;
    };
    let cube_dist = sq_dist(r, g, b, cube_r, cube_g, cube_b);

    // --- nearest in the grayscale ramp (232–255) ---
    // Ramp values: 8, 18, 28, …, 238  (24 entries)
    let lum = (r as u16 + g as u16 + b as u16) / 3;
    let gray_step = if lum <= 3 {
        0u8
    } else if lum >= 243 {
        23
    } else {
        ((lum as i16 - 8 + 5) / 10).clamp(0, 23) as u8
    };
    let gv = (8 + gray_step as u16 * 10) as u8;
    let gray_dist = sq_dist(r, g, b, gv, gv, gv);

    if gray_dist < cube_dist {
        232 + gray_step
    } else {
        cube_idx as u8
    }
}

/// Find the nearest index (0–5) into [`CUBE_VALUES`] for a single channel.
fn nearest_cube_channel(v: u8) -> u8 {
    let mut best = 0u8;
    let mut best_d = v.abs_diff(CUBE_VALUES[0]) as u16;
    for (i, &cube) in CUBE_VALUES.iter().enumerate().skip(1) {
        let d = v.abs_diff(cube) as u16;
        if d < best_d {
            best = i as u8;
            best_d = d;
        }
    }
    best
}

/// Squared Euclidean distance between two RGB colors.
fn sq_dist(r1: u8, g1: u8, b1: u8, r2: u8, g2: u8, b2: u8) -> u32 {
    let dr = r1 as i32 - r2 as i32;
    let dg = g1 as i32 - g2 as i32;
    let db = b1 as i32 - b2 as i32;
    (dr * dr + dg * dg + db * db) as u32
}

/// Returns `None` for named ANSI colors (Color::Red, etc.) and Color::Reset.
fn color_to_rgb(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        Color::Indexed(n) => Some(indexed_to_rgb(n)),
        _ => None,
    }
}

/// `None` only for `Reset`. Named colors use xterm 0-15 defaults; a customized terminal palette will differ.
pub fn resolve_to_rgb(color: Color) -> Option<(u8, u8, u8)> {
    let idx: u8 = match color {
        Color::Rgb(r, g, b) => return Some((r, g, b)),
        Color::Indexed(n) => return Some(indexed_to_rgb(n)),
        Color::Black => 0,
        Color::Red => 1,
        Color::Green => 2,
        Color::Yellow => 3,
        Color::Blue => 4,
        Color::Magenta => 5,
        Color::Cyan => 6,
        Color::Gray => 7,
        Color::DarkGray => 8,
        Color::LightRed => 9,
        Color::LightGreen => 10,
        Color::LightYellow => 11,
        Color::LightBlue => 12,
        Color::LightMagenta => 13,
        Color::LightCyan => 14,
        Color::White => 15,
        Color::Reset => return None,
    };
    Some(indexed_to_rgb(idx))
}

/// Lerp from `base` (opacity 0) toward `original` (opacity 1).
#[inline]
pub fn blend_channel(base: u8, original: u8, opacity: f32) -> u8 {
    // result = base + (original - base) * opacity
    //        = base * (1 - opacity) + original * opacity
    let result = base as f32 * (1.0 - opacity) + original as f32 * opacity;
    result.round() as u8
}

/// Indexed inputs quantize back to a 256-color index. `None` for named ANSI: their RGB is terminal-dependent.
pub fn blend_color(base: Color, original: Color, opacity: f32) -> Option<Color> {
    let (base_r, base_g, base_b) = color_to_rgb(base)?;
    let (orig_r, orig_g, orig_b) = color_to_rgb(original)?;

    let r = blend_channel(base_r, orig_r, opacity);
    let g = blend_channel(base_g, orig_g, opacity);
    let b = blend_channel(base_b, orig_b, opacity);

    // On 256-color terminals the theme quantizes all colors to Indexed at startup, so any Indexed input means the terminal cannot handle raw RGB
    Some(match (base, original) {
        (Color::Indexed(_), _) | (_, Color::Indexed(_)) => Color::Indexed(nearest_indexed(r, g, b)),
        _ => Color::Rgb(r, g, b),
    })
}

/// Named ANSI spans are left unchanged; their RGB is terminal-dependent.
pub fn blend_line(line: Line<'static>, base: Color, opacity: f32) -> Line<'static> {
    let blended_spans: Vec<Span<'static>> = line
        .spans
        .into_iter()
        .map(|span| {
            let mut style = span.style;
            if let Some(fg) = style.fg
                && let Some(blended) = blend_color(base, fg, opacity)
            {
                style.fg = Some(blended);
            }
            Span::styled(span.content, style)
        })
        .collect();
    Line::from(blended_spans).style(line.style)
}

/// Spans without an explicit fg get `default_fg` before blending. Named ANSI is left unchanged.
pub fn blend_line_with_default(
    line: Line<'static>,
    base: Color,
    default_fg: Color,
    opacity: f32,
) -> Line<'static> {
    let blended_spans: Vec<Span<'static>> = line
        .spans
        .into_iter()
        .map(|span| {
            let mut style = span.style;
            let fg = style.fg.unwrap_or(default_fg);
            if let Some(blended) = blend_color(base, fg, opacity) {
                style.fg = Some(blended);
            }
            Span::styled(span.content, style)
        })
        .collect();
    Line::from(blended_spans).style(line.style)
}

/// Blends fg and bg. Named ANSI is left unchanged; its RGB is terminal-dependent.
pub fn fade_region(buf: &mut Buffer, area: Rect, base_color: Color, opacity: f32) {
    blend_area(
        buf,
        area,
        Some((base_color, opacity)),
        Some((base_color, opacity)),
    );
}

/// `None` leaves that channel. Named ANSI cells are skipped; their RGB is terminal-dependent.
pub fn blend_area(
    buf: &mut Buffer,
    area: Rect,
    fg: Option<(Color, f32)>,
    bg: Option<(Color, f32)>,
) {
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            if let Some(cell) = buf.cell_mut((x, y)) {
                if let Some((target, opacity)) = fg
                    && let Some(blended) = blend_color(target, cell.fg, opacity)
                {
                    cell.set_fg(blended);
                }
                if let Some((target, opacity)) = bg
                    && let Some(blended) = blend_color(target, cell.bg, opacity)
                {
                    cell.set_bg(blended);
                }
            }
        }
    }
}

/// Profile palettes cannot express the blend (`bg` is Reset/named ANSI), so fall back to DIM or unfocused panels never recede.
pub fn recede_area(buf: &mut Buffer, area: Rect, bg: Color, opacity: f32) {
    if color_to_rgb(bg).is_some() {
        blend_area(buf, area, Some((bg, opacity)), None);
        return;
    }
    use ratatui::style::Modifier;
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.modifier.insert(Modifier::DIM);
                // Many terminals ignore faint when bold is set (the theme's
                // bold prompts would stay at full weight): drop bold too.
                cell.modifier.remove(Modifier::BOLD);
            }
        }
    }
}

/// Post-pass: row builders bake their own fgs, so a uniform row must be repainted after it is drawn.
pub fn force_area_fg(buf: &mut Buffer, area: Rect, fg: Color) {
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_fg(fg);
            }
        }
    }
}

/// Reset modifiers so bold/italic do not bleed through. Inexpressible `blend_bg` becomes DIM so previously-DIM text does not pop brighter.
pub fn dim_area(buf: &mut Buffer, area: Rect, blend_bg: ratatui::style::Color, blend_factor: f32) {
    use ratatui::style::Modifier;

    let blendable = color_to_rgb(blend_bg).is_some();
    let replacement = if blendable {
        Modifier::empty()
    } else {
        Modifier::DIM
    };
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.modifier = replacement;
            }
        }
    }
    if blendable {
        crate::engine::render::color::blend_area(buf, area, Some((blend_bg, blend_factor)), None);
    }
}
