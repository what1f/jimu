//! Generic rendering primitives used by the scrollback and viewport.
pub mod bidi;
pub mod color;
pub mod draw;
pub mod glyphs;
pub mod highlight;
#[cfg(feature = "inline-image")]
pub mod image_overlay;
pub mod line_utils;
pub mod link_opener;
#[cfg(any(
    feature = "mermaid",
    all(feature = "inline-image", target_os = "macos")
))]
pub(crate) mod media_directory;
pub mod osc8;
pub mod renderable;
pub mod safe_buf;
pub mod scrollbar;
#[cfg(feature = "syntax")]
pub mod syntax;
pub mod terminal_output;
pub mod tool_paths;
pub mod wrapping;
#[cfg(feature = "inline-image")]
pub use image_overlay::render_image_overlay;
pub use renderable::Renderable;
pub use safe_buf::SafeBuf;
