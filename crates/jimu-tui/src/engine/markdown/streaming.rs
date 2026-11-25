//! Streaming/incremental markdown renderer.
//!
//! Provides `StreamingMarkdownRenderer`, which renders markdown that arrives in chunks (e.g., from an LLM streaming response).
//!
//! # How It Works
//!
//! Instead of re-rendering the entire document on each chunk, it:
//! 1. Accumulates incoming chunks into an internal buffer
//! 2. Detects "checkpoints", stable block boundaries where output won't change
//! 3. Freezes rendered output up to the last checkpoint
//! 4. Only re-renders the "tail" after the checkpoint
//!
//! This reduces complexity from O(N²) to approximately O(N) for streaming.
//!
//! # Example
//!
//! ```ignore
//! let mut renderer = StreamingMarkdownRenderer::new(style, true);
//!
//! // As tokens arrive from LLM:
//! for token in stream {
//!     renderer.push_and_render(&token, Some(&syntect));
//!     let view = renderer.view();
//!     display(view.lines);
//! }
//! ```

use crate::engine::markdown::open_code_highlighter::OpenCodeHighlighter;

use crate::engine::markdown::{
    LatexDelimiterNormalizer, MarkdownBuffers, MarkdownRenderOutput, MarkdownRenderView,
    MarkdownStyle, Syntect, render_markdown_ratatui_with_link_id,
};

/// Tracks the frozen state for truncation.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FrozenState {
    /// Number of frozen lines (= number of frozen line_source_map entries).
    pub(crate) lines_len: usize,
    /// Number of frozen source bytes.
    pub(crate) source_bytes: usize,
    /// Next link ID, advanced ONLY when a checkpoint advances the frozen boundary (i.e. when frozen lines and their hyperlinks become permanent).
    /// IDs assigned to url_scan hits in the unfrozen tail are regenerated on every `rerender_tail` call.
    /// They only become stable once the line they live on becomes frozen.
    pub(crate) next_link_id: u32,
}

/// Incremental markdown renderer that efficiently handles streaming input.
///
/// Maintains frozen (stable) content and only re-renders the unfrozen tail, reducing render time for long streaming content.
pub struct StreamingMarkdownRenderer {
    /// Accumulated source text (all chunks concatenated).
    source: String,

    /// Single output buffer: frozen content at start, tail appended after.
    output: MarkdownRenderOutput,

    /// Frozen state: where to truncate before re-rendering tail.
    pub(crate) frozen: FrozenState,

    /// Reusable buffers for highlighting and rendering (avoids allocation per render).
    buffers: MarkdownBuffers,

    /// Rendering style.
    style: MarkdownStyle,

    /// Whether to use pretty mode (hide markdown syntax).
    pretty: bool,

    /// Maximum width for rendered tables (in display columns).
    max_table_width: Option<usize>,

    /// Whether CommonMark soft breaks collapse to a space (default `true`).
    /// Set `false` for source-faithful rendering (plan preview).
    collapse_soft_breaks: bool,

    /// Incremental highlighter for the trailing still-open fenced code block.
    /// Cleared (so it rebuilds) on any state reset that would change output: theme/style, pretty mode, table width, soft-break mode, or `clear()`.
    open_code: Option<OpenCodeHighlighter>,

    /// Streaming LaTeX delimiter normalizer.
    /// Rewrites `\(…\)` / `\[…\]` / `\begin{equation}` into the canonical `$` / `$$` forms before text is appended to `source`.
    /// Held-back ambiguous bytes (a partial delimiter at a chunk boundary) are flushed by `finish()`.
    normalizer: LatexDelimiterNormalizer,
}

impl std::fmt::Debug for StreamingMarkdownRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamingMarkdownRenderer")
            .field("source_len", &self.source.len())
            .field("frozen_lines", &self.frozen.lines_len)
            .field("frozen_bytes", &self.frozen.source_bytes)
            .field("output_lines", &self.output.lines.len())
            .field("pretty", &self.pretty)
            .finish()
    }
}

impl Clone for StreamingMarkdownRenderer {
    fn clone(&self) -> Self {
        // Create a fresh renderer and push all source text
        // This recreates the frozen state correctly.
        // We must propagate `max_table_width` BEFORE pushing/rendering so the clone produces identical output to the original
        let mut new = Self::new(self.style, self.pretty);
        new.set_max_table_width(self.max_table_width);
        new.set_collapse_soft_breaks(self.collapse_soft_breaks);
        // `self.source` is already normalized, so append it verbatim rather than re-running the normalizer
        // A re-run could hold back a trailing ambiguous suffix and make the clone's source diverge
        // Copy the normalizer state separately so any held-back bytes survive the clone
        new.push_normalized(&self.source);
        new.render(None);
        new.normalizer = self.normalizer.clone();
        new
    }
}

impl StreamingMarkdownRenderer {
    /// Create a new streaming renderer.
    pub fn new(style: MarkdownStyle, pretty: bool) -> Self {
        Self {
            source: String::new(),
            output: MarkdownRenderOutput::new(),
            frozen: FrozenState::default(),
            buffers: MarkdownBuffers::new(),
            style,
            pretty,
            max_table_width: None,
            collapse_soft_breaks: true,
            open_code: None,
            normalizer: LatexDelimiterNormalizer::new(),
        }
    }

    /// Replace the markdown style and trigger a full re-render.
    ///
    /// Used when the theme changes at runtime so existing blocks pick up the new colors on the next render pass.
    pub fn set_style(&mut self, style: MarkdownStyle) {
        self.style = style;
        self.frozen = FrozenState::default();
        self.output.clear();
        // Theme/style change alters colors, so any cached highlight is stale.
        self.open_code = None;
    }

    /// Set the maximum width for rendered tables.
    /// When set, column widths are shrunk proportionally so the table fits within the given number of display columns.
    /// If the width changes, frozen state is reset to ensure consistent rendering.
    pub fn set_max_table_width(&mut self, width: Option<usize>) {
        if self.max_table_width != width {
            self.max_table_width = width;
            // Reset frozen state since table formatting may change
            self.frozen = FrozenState::default();
            self.output.clear();
            self.open_code = None;
        }
    }

    /// Set whether CommonMark soft breaks collapse to a space.
    /// Resets frozen state when the mode changes.
    pub fn set_collapse_soft_breaks(&mut self, collapse: bool) {
        if self.collapse_soft_breaks != collapse {
            self.collapse_soft_breaks = collapse;
            self.frozen = FrozenState::default();
            self.output.clear();
            self.open_code = None;
        }
    }

    /// Push a new chunk of markdown text (no rendering).
    /// Call `render()` to process accumulated content, or use `push_and_render()` for convenience.
    pub fn push(&mut self, chunk: &str) {
        let normalized = self.normalizer.push(chunk);
        self.source.push_str(&normalized);
    }

    /// Append already-normalized source text, bypassing the delimiter normalizer.
    /// Used by `clone()` to reproduce an existing (already normalized) `source` exactly.
    /// The cloned normalizer state is copied separately so any held-back bytes are preserved.
    fn push_normalized(&mut self, text: &str) {
        self.source.push_str(text);
    }

    /// Render accumulated content.
    /// Pass `None` for syntect to disable syntax highlighting for code blocks.
    /// The `syntect` theme must stay stable between renders; switch themes via [`set_style`](Self::set_style), which clears that cache.
    pub fn render(&mut self, syntect: Option<&Syntect>) {
        self.rerender_tail(syntect);
    }

    /// Push a chunk and render immediately (convenience method).
    /// Equivalent to `push(chunk)` followed by `render(syntect)`.
    /// Use this for real-time streaming where you want to display after each chunk.
    pub fn push_and_render(&mut self, chunk: &str, syntect: Option<&Syntect>) {
        let normalized = self.normalizer.push(chunk);
        self.source.push_str(&normalized);
        self.rerender_tail(syntect);
    }

    /// Internal: render the unfrozen tail and update frozen state.
    fn rerender_tail(&mut self, syntect: Option<&Syntect>) {
        // Truncate output to frozen state (discard stale tail)
        self.output.lines.truncate(self.frozen.lines_len);
        self.output.line_source_map.truncate(self.frozen.lines_len);
        // Discard stale tail hyperlinks (keep frozen ones)
        self.output
            .hyperlinks
            .retain(|h| h.line_index < self.frozen.lines_len);
        self.output
            .tables
            .retain(|t| t.line_index < self.frozen.lines_len);
        // Discard stale tail code-block spans, keeping frozen ones (those whose body lies entirely within the frozen prefix)
        // A still-open fence in the tail has no span, so spans become stable only once frozen
        self.output
            .code_blocks
            .retain(|cb| cb.output_line_range.end <= self.frozen.lines_len);

        // Render the tail (unfrozen portion) using reusable buffers.
        // When the frozen source ends without a trailing newline (e.g., a thematic break `---` at the end of a chunk), the tail can start with `\n`
        // That newline is the block-terminating newline the frozen block consumed; skip it to avoid a spurious blank line
        let mut tail_start = self.frozen.source_bytes;
        if tail_start > 0
            && self.source.as_bytes().get(tail_start - 1) != Some(&b'\n')
            && self.source.as_bytes().get(tail_start) == Some(&b'\n')
        {
            tail_start += 1;
        }
        let Some(tail) = self.source.get(tail_start..) else {
            return;
        };
        // Lazily create the incremental open-code cache once syntect is present.
        // It rebuilds itself on fence/offset change, so a stale cache from a previous tail (e.g. after a checkpoint advanced) is self-correcting.
        let open_code = match syntect {
            Some(syn) => Some(
                self.open_code
                    .get_or_insert_with(|| OpenCodeHighlighter::new(syn)),
            ),
            None => None,
        };
        let (tail_output, checkpoint, tail_next_link_id) = render_markdown_ratatui_with_link_id(
            tail,
            self.style,
            self.pretty,
            &mut self.buffers,
            syntect,
            self.max_table_width,
            self.frozen.next_link_id,
            self.collapse_soft_breaks,
            open_code,
        );

        // Append tail to output
        self.output.lines.extend(tail_output.lines);
        self.output
            .line_source_map
            .extend(tail_output.line_source_map);

        // Offset tail hyperlink line indices by frozen line count and append
        let frozen_lines = self.frozen.lines_len;
        self.output
            .hyperlinks
            .extend(tail_output.hyperlinks.into_iter().map(|mut h| {
                h.line_index += frozen_lines;
                h
            }));
        self.output
            .tables
            .extend(tail_output.tables.into_iter().map(|mut t| {
                t.line_index += frozen_lines;
                t
            }));

        // Append tail code-block spans, rebasing their tail-relative ranges to document coordinates, mirroring the hyperlink offsetting
        // Output lines shift by the frozen line count, source bytes by the tail's start offset
        self.output
            .code_blocks
            .extend(tail_output.code_blocks.into_iter().map(|mut cb| {
                cb.output_line_range.start += frozen_lines;
                cb.output_line_range.end += frozen_lines;
                cb.source_byte_range.start += tail_start;
                cb.source_byte_range.end += tail_start;
                cb
            }));

        // Detect plain URLs` suffix in pretty-mode markdown links, bare URLs in prose).
        // (a) Non-streaming callers` during session replay) never call `finish()`.
        // Without url_scan here their URLs would never become HyperlinkTargets
        let tail_lines = self.output.lines.get(frozen_lines..).unwrap_or(&[]);
        let (extra_links, post_scan_next_id) =
            crate::engine::markdown::url_scan::detect_plain_urls_with_offset(
                tail_lines,
                frozen_lines,
                &self.output.hyperlinks,
                tail_next_link_id,
            );
        self.output.hyperlinks.extend(extra_links);

        // Sort hyperlinks by (line_index, column_range.start), matching the invariant `finish()` enforces
        // Downstream consumers (`map_hyperlinks_to_overlay`, link map builders) rely on that order
        self.output
            .hyperlinks
            .sort_by_key(|h| (h.line_index, h.column_range.start));

        // If checkpoint found, update frozen state.
        // The checkpoint's source_bytes is relative to the tail we rendered, so add tail_start
        // tail_start may be greater than frozen.source_bytes if we skipped a leading newline
        if let Some(cp) = checkpoint {
            self.frozen = FrozenState {
                lines_len: self.frozen.lines_len + cp.output_lines,
                source_bytes: tail_start + cp.source_bytes,
                next_link_id: post_scan_next_id,
            };
        }
    }

    /// Get a view of the current rendered output.
    /// This is cheap: it returns a reference to cached output.
    /// The output was computed during `render()` or `push_and_render()`.
    pub fn view(&self) -> MarkdownRenderView<'_> {
        self.output.as_view()
    }

    /// Get the accumulated source text.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Get the number of frozen source bytes.
    pub fn frozen_bytes(&self) -> usize {
        self.frozen.source_bytes
    }

    /// Get the number of frozen output lines.
    pub fn frozen_lines_count(&self) -> usize {
        self.frozen.lines_len
    }

    /// Reset the renderer, clearing all accumulated content.
    /// Also resets `max_table_width` to `None` for symmetry with the freshly-constructed state.
    /// Otherwise a later `set_max_table_width(Some(prev_width))` is silently a no-op (no state reset) because the equality check sees no change.
    pub fn clear(&mut self) {
        self.source.clear();
        self.output.clear();
        self.frozen = FrozenState::default();
        self.max_table_width = None;
        self.open_code = None;
        self.normalizer.reset();
    }

    /// Set pretty mode (true = hide syntax, false = show raw markdown).
    ///
    /// If the mode changes, frozen state is reset to ensure consistent rendering.
    pub fn set_pretty(&mut self, pretty: bool) {
        if self.pretty != pretty {
            self.pretty = pretty;
            // Reset frozen state so everything re-renders with the new mode
            self.frozen = FrozenState::default();
            self.output.clear();
            self.open_code = None;
        }
    }

    /// Get current pretty mode.
    pub fn pretty(&self) -> bool {
        self.pretty
    }

    /// Consume the renderer and return the owned output.
    ///
    /// Use this when streaming is complete and you want owned data.
    pub fn into_output(self) -> MarkdownRenderOutput {
        self.output
    }

    /// Finalize streaming with a full re-render.
    /// `render()` also runs the URL detector and sort, so `finish()` adds nothing those calls didn't already produce.
    pub fn finish(&mut self, syntect: Option<&Syntect>) -> MarkdownRenderView<'_> {
        // Flush any bytes the normalizer held back at the last chunk boundary (e.g. a trailing partial delimiter).
        // The full re-render must see the complete, normalized source
        let flushed = self.normalizer.finish();
        self.source.push_str(&flushed);

        // Do a full re-render of the entire source, preserving max_table_width.
        let mut buffers = MarkdownBuffers::new();
        let (full_output, _, full_next_link_id) = render_markdown_ratatui_with_link_id(
            &self.source,
            self.style,
            self.pretty,
            &mut buffers,
            syntect,
            self.max_table_width,
            // The full render restarts link IDs at 0, so our counter must also reset to the post-render value
            0,
            self.collapse_soft_breaks,
            // finish() is a full batch re-render: never use the incremental cache.
            None,
        );

        self.output = full_output;

        // Scan rendered lines for plain, non-md URLs that pulldown-cmark didn't emit as Tag::Link
        // Dedup against existing hyperlinks by (line_index, column_range) overlap to avoid double-linking
        let (extra_links, post_scan_next_id) = crate::engine::markdown::url_scan::detect_plain_urls(
            &self.output.lines,
            &self.output.hyperlinks,
            full_next_link_id,
        );
        self.output.hyperlinks.extend(extra_links);

        // Sort hyperlinks by (line_index, column_range.start) so downstream consumers see a well-ordered list
        self.output
            .hyperlinks
            .sort_by_key(|h| (h.line_index, h.column_range.start));

        // Mark everything as frozen (streaming is complete)
        self.frozen = FrozenState {
            lines_len: self.output.lines.len(),
            source_bytes: self.source.len(),
            next_link_id: post_scan_next_id,
        };

        // Streaming is over: release the highlighter caches (open-block state and closed-fence memo) rather than keep them for the block's lifetime
        // They are lazily rebuilt if rendering ever resumes
        self.open_code = None;

        self.output.as_view()
    }

    /// Finalize streaming and return owned output.
    ///
    /// Combines `finish()` and `into_output()`: does a full re-render and returns the owned result.
    pub fn finish_into_output(mut self, syntect: Option<&Syntect>) -> MarkdownRenderOutput {
        self.finish(syntect);
        self.output
    }
}
