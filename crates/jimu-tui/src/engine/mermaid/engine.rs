//! The [`MermaidEngine`] trait, error type, resource limits, and the panic-isolating [`render_checked`] entry point.

use std::panic::AssertUnwindSafe;

use super::{RenderParams, RenderedDiagram};

/// Why a diagram failed to render.
///
/// Every variant degrades to the same user-facing fallback (show the source as a code block); the split only matters for what gets logged.
#[derive(thiserror::Error, Debug)]
pub enum MermaidError {
    /// The source could not be parsed into a diagram.
    #[error("mermaid parse error: {0}")]
    Parse(String),
    /// The diagram parsed but layout failed.
    #[error("mermaid layout error: {0}")]
    Layout(String),
    /// The SVG could not be rasterized to PNG.
    #[error("mermaid rasterize error: {0}")]
    Rasterize(String),
    /// An external engine exceeded its wall-clock budget.
    #[error("mermaid render timed out")]
    Timeout,
    /// The engine cannot render this input (unknown/exotic diagram, disabled engine, or a breached resource limit such as oversized source).
    #[error("mermaid render unsupported: {0}")]
    Unsupported(String),
    /// The engine panicked; [`render_checked`] caught it and carries the panic message here.
    /// Only intercepted under `panic = "unwind"`; under `panic = "abort"` the process aborts instead (see [`render_checked`]).
    #[error("mermaid engine panicked: {0}")]
    Panic(String),
}

/// Caps [`render_checked`] applies before the engine runs, so untrusted source can't trivially exhaust memory via an oversized payload.
/// A synchronous render cannot time itself out, so the pager enforces the wall-clock budget out of process via [`super::run_with_timeout`].
/// Output area and height are capped by the rendering engine using [`super::MAX_OUTPUT_MEGAPIXELS`] and [`RenderParams::max_height_px`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderLimits {
    /// Maximum accepted source length in bytes.
    /// Larger input is rejected with [`MermaidError::Unsupported`] *before* the engine runs.
    pub max_source_bytes: usize,
}

impl Default for RenderLimits {
    fn default() -> Self {
        // 64 KiB, comfortably larger than any hand-authored diagram
        Self {
            max_source_bytes: 64 * 1024,
        }
    }
}

/// A pluggable Mermaid rendering backend.
/// Prefer calling [`render_checked`] over [`MermaidEngine::render`] directly: it applies [`RenderLimits`] and isolates panics.
/// Implementations must be cheap to share (`Send + Sync`) so a worker pool can hold one behind an `Arc`.
pub trait MermaidEngine: Send + Sync {
    /// Render `source` to a PNG using `params`.
    ///
    /// Implementations may panic on pathological input; callers are expected to wrap this via [`render_checked`].
    fn render(&self, source: &str, params: &RenderParams) -> Result<RenderedDiagram, MermaidError>;
}

/// Render `source` with `engine`, enforcing `limits` and isolating panics.
/// Source larger than [`RenderLimits::max_source_bytes`] is rejected with [`MermaidError::Unsupported`] **without invoking the engine**; An engine panic is caught and returned as [`MermaidError::Panic`].
/// Even under unwind, `catch_unwind` cannot catch aborts from stack overflow or allocation failure.
pub fn render_checked(
    engine: &dyn MermaidEngine,
    source: &str,
    params: &RenderParams,
    limits: &RenderLimits,
) -> Result<RenderedDiagram, MermaidError> {
    if source.len() > limits.max_source_bytes {
        return Err(MermaidError::Unsupported(format!(
            "source is {} bytes, over the {}-byte limit",
            source.len(),
            limits.max_source_bytes
        )));
    }

    // AssertUnwindSafe: a panic aborts this render and is converted to an error; no observable state from the engine is reused afterward
    match std::panic::catch_unwind(AssertUnwindSafe(|| engine.render(source, params))) {
        Ok(result) => result,
        Err(payload) => {
            let msg = panic_message(payload);
            // The panic message can embed untrusted source fragments, so keep its content out of the default `warn` stream and behind `debug`

            Err(MermaidError::Panic(msg))
        }
    }
}

/// Best-effort extraction of a human-readable message from a panic payload.
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "engine panicked".to_string()
    }
}
