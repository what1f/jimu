use crate::ui::scrollback::block::BlockContent;
use crate::ui::scrollback::types::{AccentStyle, BlockContext, BlockOutput};

use super::markdown_content::MarkdownContent;
use super::mermaid_content::{self, MermaidContent};
use crate::ui::scrollback::appearance::AppearanceConfig;

/// Block displaying an agent message with streaming markdown support. When text arrives in chunks, call
/// `push_chunk()` to append without re-rendering everything.
#[derive(Debug, Clone)]
pub struct AgentMessageBlock {
    content: MarkdownContent,
    /// Cached image references extracted from the markdown source.
    image_refs: Vec<crate::ui::scrollback::media::ScrollbackImageRef>,
    /// Cached video references extracted from the markdown source.
    video_refs: Vec<crate::ui::scrollback::media::ScrollbackVideoRef>,
    /// Detected ` ```mermaid ` diagrams and render skeleton, populated at construction/finish (never per streaming chunk) like the media refs.
    mermaid: MermaidContent,
}

impl AgentMessageBlock {
    /// Create a new agent message block with complete text.
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let image_refs = crate::ui::scrollback::media::extract_image_refs(&text);
        let video_refs = crate::ui::scrollback::media::extract_video_refs(&text);
        let content = MarkdownContent::new(text);
        let mermaid = content.mermaid_content();
        Self {
            content,
            image_refs,
            video_refs,
            mermaid,
        }
    }

    /// Create an empty block for streaming.
    pub fn streaming() -> Self {
        Self {
            content: MarkdownContent::streaming(),
            image_refs: Vec::new(),
            video_refs: Vec::new(),
            mermaid: MermaidContent::default(),
        }
    }

    /// Push a streaming chunk of markdown text.
    pub fn push_chunk(&mut self, chunk: &str) {
        self.content.push_chunk(chunk);
    }

    /// Push a chunk without rendering immediately.
    pub fn push_chunk_deferred(&mut self, chunk: &str) {
        self.content.push_chunk_deferred(chunk);
    }

    /// Get the source markdown text.
    pub fn text(&self) -> String {
        self.content.text()
    }

    /// Whether the source markdown is empty (zero-alloc, unlike `text()`).
    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }

    /// Finish streaming and do a full re-render for safety.
    pub fn finish(&mut self) {
        self.content.finish();
        let text = self.content.text();
        self.image_refs = crate::ui::scrollback::media::extract_image_refs(&text);
        self.video_refs = crate::ui::scrollback::media::extract_video_refs(&text);
        // Detection runs once the render is final, after the renderer freezes, never per streaming chunk
        self.mermaid = self.content.mermaid_content();
    }

    /// The detected Mermaid diagrams for this message (empty until finished or constructed from complete text).
    pub fn mermaid(&self) -> &MermaidContent {
        &self.mermaid
    }

    /// Set the raw mode, re-rendering if it changed.
    pub fn set_raw_mode(&mut self, raw: bool) {
        self.content.set_raw_mode(raw);
    }

    /// Access the underlying markdown content (for viewer item building).
    pub fn content(&self) -> &MarkdownContent {
        &self.content
    }

    /// Mutable access to the underlying markdown content.
    pub fn content_mut(&mut self) -> &mut MarkdownContent {
        &mut self.content
    }

    /// Get copyable text for this block.
    /// When `raw` is true, returns the raw markdown source.
    /// When `raw` is false, returns the rendered text (styles stripped).
    pub fn copy_text(&self, raw: bool) -> String {
        if raw {
            self.content.text()
        } else {
            self.content.rendered_plain_text()
        }
    }
}

impl AgentMessageBlock {
    /// Resolve the diagram display mode from the user setting without building `output()`; cheap enough to gate the per-frame affordance path.
    fn mermaid_display_mode(
        &self,
        appearance: &AppearanceConfig,
    ) -> mermaid_content::MermaidDisplay {
        // Minimal mode commits static text with no draw loop to paint the clickable affordance row, so suppress it there
        // The diagram art still renders; its source stays natively selectable
        // The inline-overlay force-off flag is set iff minimal
        mermaid_content::mermaid_display_static(
            appearance.render_mermaid,
            !cfg!(feature = "mermaid")
                || crate::engine::terminal::image::scrollback_inline_overlay_forced_off(),
        )
    }

    /// It is deterministic for a given `ctx`, so the two calls produce matching rows and offsets without a shared cache
    /// that could drift. Only callers that have already confirmed there are diagrams and we are not in raw mode should
    /// reach here. The common diagram-free path never pays this build.
    fn rendered_output(
        &self,
        ctx: &BlockContext,
    ) -> (BlockOutput, Vec<mermaid_content::DiagramAffordance>) {
        let mut out = self.content.output(ctx.width as usize);
        // Diagram pre-wrap ranges in document order
        // The fence count and order are width-invariant, so range index `idx` pairs with the diagram's source (`self.mermaid.source(idx)`)
        let ranges = self.content.mermaid_block_ranges();

        match self.mermaid_display_mode(&ctx.appearance) {
            mermaid_content::MermaidDisplay::SourceOnly => (out, Vec::new()),
            mermaid_content::MermaidDisplay::Affordances => {
                let affordances =
                    mermaid_content::apply_affordance_rows(&mut out, &ranges, |idx| {
                        self.mermaid.source(idx).unwrap_or_default().to_string()
                    });
                (out, affordances)
            }
        }
    }
}

impl BlockContent for AgentMessageBlock {
    fn output(&self, ctx: &BlockContext) -> BlockOutput {
        // Common path: no diagrams (or raw mode) renders plain markdown, with no affordance machinery and no extra output rebuild
        if ctx.raw || self.mermaid.is_empty() {
            return self.content.output(ctx.width as usize);
        }
        self.rendered_output(ctx).0
    }

    fn diagram_affordances(&self, ctx: &BlockContext) -> Vec<mermaid_content::DiagramAffordance> {
        // Affordance rows exist only under the affordance display with diagrams
        // For every other (much more common) case, return without building output()
        if ctx.raw
            || self.mermaid.is_empty()
            || self.mermaid_display_mode(&ctx.appearance)
                != mermaid_content::MermaidDisplay::Affordances
        {
            return Vec::new();
        }
        self.rendered_output(ctx).1
    }

    fn estimate_extra_rows(&self, appearance: &AppearanceConfig) -> u16 {
        // Each diagram inserts one treatment row (affordance row or fallback caption) into output() that the source-text estimate can't see
        // Count one per diagram (a safe over-estimate if a range is empty) so the off-screen estimate never under-reserves
        // Raw mode and the `off` setting add no such row
        if self.mermaid.is_empty()
            || self.content.is_raw()
            || self.mermaid_display_mode(appearance) == mermaid_content::MermaidDisplay::SourceOnly
        {
            return 0;
        }
        self.mermaid.len() as u16
    }

    fn accent(&self, _ctx: &BlockContext) -> Option<AccentStyle> {
        None
    }

    fn has_vpad_for(&self, _appearance: &AppearanceConfig) -> bool {
        false
    }

    fn has_raw_mode(&self) -> bool {
        true
    }

    fn is_foldable(&self) -> bool {
        false
    }

    fn image_references(&self) -> &[crate::ui::scrollback::media::ScrollbackImageRef] {
        &self.image_refs
    }

    fn video_references(&self) -> &[crate::ui::scrollback::media::ScrollbackVideoRef] {
        &self.video_refs
    }
}
