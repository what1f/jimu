// Adapted from the Grok Build pager (Apache-2.0). Copyright 2023-2026 SpaceXAI.

use crate::engine::textarea::ElementId;
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, OnceLock},
};

/// A single image pasted into the prompt.
///
/// Tracks everything needed for display, preview, persistence, and eventual submission as a `ContentBlock::Image`.
#[derive(Debug, Clone)]
pub struct PastedImage {
    /// The [`ElementId`] of the corresponding `KIND_IMAGE` element in the `TextArea` buffer. Used to reconcile live elements against stored images.
    pub element_id: ElementId,

    /// 1-based display number for the current prompt (e.g. the `1` in `[Image #1]`). Reset when the prompt is cleared.
    pub display_number: usize,

    /// MIME type of the encoded image (e.g. `"image/png"`).
    pub mime_type: String,

    /// Header-validated dimensions used for pre-insertion/send policy.
    pub dimensions: Option<(u32, u32)>,

    /// Size of the encoded image bytes.
    pub byte_len: usize,

    /// Encoded image bytes kept in memory.
    /// Set to `None` once the image is durably written to [`session_image_path`](Self::session_image_path), to avoid holding large buffers.
    pub encoded_bytes: Option<Arc<[u8]>>,

    /// Original user-visible path for file-path pastes.
    ///
    /// This remains stable after persistence so previews show the path the user pasted. Model/send loading uses `session_image_path` first.
    pub source_path: Option<PathBuf>,

    /// Temporary staging path before the image is finalized into the session directory. Cleaned up when the chip is removed or on send.
    pub staged_temp_path: Option<PathBuf>,

    /// Final durable path under `session_dir(info)/images/`.
    /// Once set, this is the canonical on-disk location and [`encoded_bytes`](Self::encoded_bytes) may be released.
    pub session_image_path: Option<PathBuf>,

    /// Shared preview preparation state; draw only reads its resolved result.
    pub preview: PromptImagePreview,
}

impl PastedImage {
    pub fn preview_preparation(&self) -> Option<PromptImagePreviewPreparation> {
        if !self.preview.is_pending() {
            return None;
        }
        Some(PromptImagePreviewPreparation {
            preview: self.preview.clone(),
            source: self.encoded_bytes.as_ref()?.clone(),
            protocol: crate::engine::terminal::image::prompt_preview_graphics_protocol(),
            dimensions: self.dimensions,
        })
    }

    pub fn prepare_preview_blocking(&self) {
        if let Some(preparation) = self.preview_preparation() {
            preparation.run();
        }
    }

    pub fn preview_dimensions(&self) -> Option<(u32, u32)> {
        self.dimensions.or_else(|| self.preview.dimensions())
    }
}

/// Shared readiness of the terminal pixel payload for a pasted image.
#[derive(Debug, Clone)]
pub struct PromptImagePreview {
    identity: u64,
    result: Arc<OnceLock<PromptImagePreviewResult>>,
}

#[derive(Debug)]
enum PromptImagePreviewResult {
    Ready {
        bytes: Arc<[u8]>,
        dimensions: (u32, u32),
    },
    Failed,
    Unsupported {
        dimensions: (u32, u32),
    },
}

impl Default for PromptImagePreview {
    fn default() -> Self {
        Self {
            identity: crate::engine::terminal::overlay::next_owner_id(),
            result: Arc::new(OnceLock::new()),
        }
    }
}

impl PromptImagePreview {
    pub fn identity(&self) -> u64 {
        self.identity
    }

    pub fn is_pending(&self) -> bool {
        self.result.get().is_none()
    }

    pub fn is_failed(&self) -> bool {
        matches!(self.result.get(), Some(PromptImagePreviewResult::Failed))
    }

    pub fn prepared(&self) -> Option<(&[u8], (u32, u32))> {
        match self.result.get()? {
            PromptImagePreviewResult::Ready { bytes, dimensions } => {
                Some((bytes.as_ref(), *dimensions))
            }
            PromptImagePreviewResult::Failed | PromptImagePreviewResult::Unsupported { .. } => None,
        }
    }

    pub fn dimensions(&self) -> Option<(u32, u32)> {
        match self.result.get()? {
            PromptImagePreviewResult::Ready { dimensions, .. }
            | PromptImagePreviewResult::Unsupported { dimensions } => Some(*dimensions),
            PromptImagePreviewResult::Failed => None,
        }
    }

    fn finish(&self, result: PromptImagePreviewResult) {
        let _ = self.result.set(result);
    }

    pub fn mark_failed(&self) {
        self.finish(PromptImagePreviewResult::Failed);
    }
}

/// Off-thread work required to prepare one prompt image preview.
#[derive(Debug, Clone)]
pub struct PromptImagePreviewPreparation {
    preview: PromptImagePreview,
    source: Arc<[u8]>,
    protocol: crate::engine::terminal::image::GraphicsProtocol,
    dimensions: Option<(u32, u32)>,
}

impl PromptImagePreviewPreparation {
    pub fn preview(&self) -> PromptImagePreview {
        self.preview.clone()
    }

    pub fn run(self) {
        let Some(dimensions) = self
            .dimensions
            .or_else(|| decode_image_dimensions(&self.source))
        else {
            self.preview.finish(PromptImagePreviewResult::Failed);
            return;
        };
        let result = match self.protocol {
            crate::engine::terminal::image::GraphicsProtocol::Kitty => {
                let bytes = if crate::engine::terminal::image::kitty_format_from_bytes(&self.source)
                    .is_some()
                {
                    self.source
                } else {
                    let Some(converted) =
                        crate::engine::terminal::image::prepare_kitty_overlay_image_bytes(
                            &self.source,
                        )
                    else {
                        self.preview.finish(PromptImagePreviewResult::Failed);
                        return;
                    };
                    Arc::from(converted)
                };
                PromptImagePreviewResult::Ready { bytes, dimensions }
            }
            crate::engine::terminal::image::GraphicsProtocol::ITerm2 => {
                PromptImagePreviewResult::Ready {
                    bytes: self.source,
                    dimensions,
                }
            }
            crate::engine::terminal::image::GraphicsProtocol::None => {
                PromptImagePreviewResult::Unsupported { dimensions }
            }
        };
        self.preview.finish(result);
    }
}

// -------------------------------------------------------------------------
// Display helpers
// -------------------------------------------------------------------------

/// Build the buffer text for an image chip.
///
/// Always path-free: `[Image #1]`. Filepaths live on the [`PastedImage`] record and appear only in the hover/cursor preview overlay.
pub fn display_text(display_number: usize) -> String {
    format!("[Image #{display_number}]")
}

// -------------------------------------------------------------------------
// Reconciliation
// -------------------------------------------------------------------------

/// Whether cleanup may delete a durable copy in the session image directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPathPolicy {
    Preserve,
    Delete,
}

/// Drops chips whose ids are gone so deleted images are not submitted. Call at cleanup boundaries, not every keystroke.
pub fn reconcile(
    session_path_policy: SessionPathPolicy,
    images: &mut Vec<PastedImage>,
    live_ids: &HashSet<ElementId>,
) {
    images.retain(|image| {
        if live_ids.contains(&image.element_id) {
            return true;
        }
        cleanup_image(session_path_policy, image);
        false
    });
}

/// Drain `images` and clean up the paths each record owns.
/// Does not reset `image_counter`; use [`reset_counter`] when the counter should also be zeroed.
pub fn drain_and_cleanup(session_path_policy: SessionPathPolicy, images: &mut Vec<PastedImage>) {
    for image in images.drain(..) {
        cleanup_image(session_path_policy, &image);
    }
}

/// Reset the monotonic image counter to 0.
/// Pair with [`drain_and_cleanup`] when a prompt is fully reset (Ctrl+C, `set_text("")`, successful send).
pub fn reset_counter(image_counter: &mut usize) {
    *image_counter = 0;
}

/// Drain images and reset the counter in one call.
/// Prefer [`drain_and_cleanup`] and [`reset_counter`] when only one side is needed.
pub(super) fn clear(images: &mut Vec<PastedImage>, image_counter: &mut usize) {
    drain_and_cleanup(SessionPathPolicy::Preserve, images);
    reset_counter(image_counter);
}

/// Delete paths owned by an image according to the session-path policy.
pub fn cleanup_image(session_path_policy: SessionPathPolicy, image: &PastedImage) {
    if session_path_policy == SessionPathPolicy::Delete
        && let Some(path) = image.session_image_path.as_ref()
    {
        let _ = std::fs::remove_file(path);
    }
    if let Some(path) = image.staged_temp_path.as_ref() {
        let _ = std::fs::remove_file(path);
    }
}

fn decode_image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    #[cfg(any(feature = "inline-image", feature = "clipboard-image"))]
    {
        image::ImageReader::new(std::io::Cursor::new(bytes))
            .with_guessed_format()
            .ok()?
            .into_dimensions()
            .ok()
    }
    #[cfg(not(any(feature = "inline-image", feature = "clipboard-image")))]
    {
        let _ = bytes;
        None
    }
}

impl PastedImage {
    /// Create an attachment from validated clipboard or file data.
    pub fn new(
        mime_type: impl Into<String>,
        bytes: Vec<u8>,
        dimensions: Option<(u32, u32)>,
        source_path: Option<PathBuf>,
    ) -> Self {
        Self {
            element_id: ElementId::from_raw(0),
            display_number: 0,
            mime_type: mime_type.into(),
            dimensions,
            byte_len: bytes.len(),
            encoded_bytes: Some(bytes.into()),
            source_path,
            staged_temp_path: None,
            session_image_path: None,
            preview: PromptImagePreview::default(),
        }
    }
}
