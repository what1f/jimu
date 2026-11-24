//! Size-limited diagram rendering with a host-selected engine.
mod engine;
mod mmdc;
mod subprocess;
pub use engine::{MermaidEngine, MermaidError, RenderLimits, render_checked};
pub use mmdc::{MmdcEngine, detect_mmdc};
pub use subprocess::{SubprocessError, run_with_timeout};
pub const MAX_OUTPUT_MEGAPIXELS: u32 = 32;

/// Which color scheme a diagram should be rendered for.
///
/// Mapped from the pager's theme by the caller; only the light/dark split is relevant to diagram rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MermaidTheme {
    /// Light surfaces with dark text (e.g. `Light`).
    #[default]
    Light,
    /// Dark surfaces with light text (e.g. `Dark`, `TokyoNight`).
    Dark,
}

/// Default opaque surface colors.
/// Available to hosts through [`MermaidTheme::surface_background`].
pub(crate) const LIGHT_SURFACE: Rgba = Rgba::new(0xFA, 0xFA, 0xFA, 0xFF);
pub(crate) const DARK_SURFACE: Rgba = Rgba::new(0x18, 0x18, 0x1B, 0xFF);

impl MermaidTheme {
    /// The default opaque surface color a diagram blends into for this theme.
    /// Used as the raster background when the caller supplies no explicit [`RenderParams::background`].
    /// The color approximates a typical terminal scrollback surface so the PNG sits flush with the grid.
    pub fn surface_background(self) -> Rgba {
        match self {
            MermaidTheme::Light => LIGHT_SURFACE,
            MermaidTheme::Dark => DARK_SURFACE,
        }
    }
}

/// A straight 8-bit-per-channel, non-premultiplied RGBA color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgba {
    /// Red channel, 0 to 255.
    pub r: u8,
    /// Green channel, 0 to 255.
    pub g: u8,
    /// Blue channel, 0 to 255.
    pub b: u8,
    /// Alpha channel, 0 (transparent) to 255 (opaque).
    pub a: u8,
}

impl Rgba {
    /// Construct an [`Rgba`] from its four channels.
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// Format as an opaque `#RRGGBB` hex string (alpha is ignored).
    pub fn to_hex(self) -> String {
        format!("#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
    }
}

/// Parameters controlling a single diagram render.
///
/// [`target_width_px`](Self::target_width_px) is the primary size driver and arrives already HiDPI-oversampled by the caller.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RenderParams {
    /// Color scheme to render for.
    pub theme: MermaidTheme,
    /// Target output width in pixels.
    /// The CLI uses this as its requested diagram size in pixels.
    /// `0` falls back to [`scale`](Self::scale).
    pub target_width_px: u32,
    /// Reject output taller than this number of pixels.
    /// `0` disables the height clamp (output area is still bounded by [`MAX_OUTPUT_MEGAPIXELS`]).
    pub max_height_px: u32,
    /// Oversample factor applied **only** when `target_width_px == 0`.
    /// The default `target_width_px` is non-zero, so this field is inert by default.
    pub scale: f32,
    /// Minimum output width in pixels; `0` disables.
    /// Raises the requested diagram size when it exceeds the target width.
    /// Useful when a small diagram is opened in an OS image viewer.
    pub min_width_px: u32,
    /// Opaque background fill.
    /// `None` renders on a transparent background so the terminal cell color shows through.
    pub background: Option<Rgba>,
}

impl Default for RenderParams {
    fn default() -> Self {
        Self {
            theme: MermaidTheme::Light,
            target_width_px: 1024,
            max_height_px: 4096,
            // Inert by default (target_width_px drives sizing); 1.0 so a caller that zeroes target_width_px without touching scale gets 1:1
            scale: 1.0,
            min_width_px: 0,
            background: None,
        }
    }
}

impl RenderParams {
    /// Sizing tuned for opening a PNG in an OS image viewer.
    /// Prefers 2x the SVG's intrinsic size, raises small diagrams to at least `min_width_px`, and allows a taller canvas than the in-terminal sizing.
    /// Height and the crate-wide megapixel/axis caps still apply.
    pub fn for_os_viewer(theme: MermaidTheme, min_width_px: u32, max_height_px: u32) -> Self {
        Self {
            theme,
            // Drive from `scale` and `min_width_px` so large SVGs keep aspect at 2x and small SVGs are upscaled to a readable minimum width
            target_width_px: 0,
            max_height_px,
            scale: 2.0,
            min_width_px,
            background: Some(theme.surface_background()),
        }
    }
}

/// A rendered diagram: PNG bytes plus the exact raster dimensions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedDiagram {
    /// The encoded PNG image.
    pub png: Vec<u8>,
    /// Output width in pixels.
    pub width_px: u32,
    /// Output height in pixels.
    pub height_px: u32,
}
