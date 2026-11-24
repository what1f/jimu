//! Shared markdown content with cached word-wrapping.
//!
//! [`MarkdownContent`] wraps a [`StreamingMarkdownRenderer`] and caches the word-wrapped output.
//! Repeated calls to [`output()`](MarkdownContent::output) at the same width are free after the first wrap.
//! Used by both [`AgentMessageBlock`](super::AgentMessageBlock) and [`ThinkingBlock`](super::ThinkingBlock).

use std::cell::RefCell;

use ratatui::text::Line;

use crate::engine::render::wrapping::word_wrap_lines_with_joiners;
use crate::ui::scrollback::types::{BlockLine, BlockOutput};

use super::quote_bar::QuoteBarStrip;

pub(crate) const MARKDOWN_BODY_RANGE: u16 = 0;
#[cfg(feature = "syntax")]
fn highlight_syntax() -> Option<&'static crate::engine::markdown::Syntect> {
    Some(crate::engine::render::syntax::get_syntect())
}

#[cfg(not(feature = "syntax"))]
fn highlight_syntax() -> Option<&'static crate::engine::markdown::Syntect> {
    None
}
use crate::engine::markdown::StreamingMarkdownRenderer;
use crate::engine::theme::{ThemeKind, cache as theme_cache, md_style};

/// Mutable rendering state behind a single `RefCell`.
/// Groups the renderer and wrap-cache together.
/// `ensure_wrapped` (called from `&self` methods via the `BlockContent` trait) can then update the table-width setting and the cache in one borrow.
#[derive(Debug, Clone)]
struct RenderState {
    renderer: StreamingMarkdownRenderer,
    /// Cached word-wrap result keyed on `(width, generation, theme)`.
    cache_width: usize,
    cache_generation: u64,
    cache_theme: ThemeKind,
    cache_lines: Vec<Line<'static>>,
    cache_joiners: Vec<Option<String>>,
    /// Number of pre-wrap (renderer output) lines that were frozen at the time we last wrapped.
    /// Lines `0..frozen_pre_wrap_count` are stable and their wrapped output is cached in `cache_lines[0..frozen_wrapped_count]`.
    frozen_pre_wrap_count: usize,
    /// Number of post-wrap lines produced by the frozen prefix.
    frozen_wrapped_count: usize,
}

/// Shared markdown content with generation-tracked word-wrap cache. The wrap cache is keyed on `(width,
/// generation)`, so scrolling (which doesn't change content) returns the cached result instantly.
#[derive(Debug, Clone)]
pub struct MarkdownContent {
    state: RefCell<RenderState>,
    current_raw: bool,
    generation: u64,
}

/// Borrowed view of cached wrapped lines and joiners.
///
/// Returned by [`MarkdownContent::wrapped_lines`] for blocks that need to post-process the wrapped output (e.g., blending, truncation).
pub struct WrappedLines<'a> {
    pub lines: &'a [Line<'static>],
    pub joiners: &'a [Option<String>],
}

impl MarkdownContent {
    /// Create with initial text (rendered immediately).
    pub fn new(text: impl Into<String>) -> Self {
        Self::new_with_table_width(text, None)
    }

    /// Create with initial text and an optional table width constraint.
    /// When `max_table_width` is `Some(w)`, tables are constrained to fit within `w` display columns.
    /// Useful for pre-rendering markdown before the final display width is known (e.g., plan preview).
    pub fn new_with_table_width(text: impl Into<String>, max_table_width: Option<usize>) -> Self {
        Self::new_inner(text, max_table_width, true)
    }

    /// Create source-faithful content: CommonMark soft breaks are preserved as line breaks instead of collapsing to spaces.
    /// Each source line then maps 1:1 to a rendered line.
    /// Used by the line-numbered plan preview, where rendered lines must map back to file lines (e.g. for commenting on a line range).
    pub fn new_source_faithful(text: impl Into<String>, max_table_width: Option<usize>) -> Self {
        Self::new_inner(text, max_table_width, false)
    }

    fn new_inner(
        text: impl Into<String>,
        max_table_width: Option<usize>,
        collapse_soft_breaks: bool,
    ) -> Self {
        let mut renderer = StreamingMarkdownRenderer::new(md_style::style(), true);
        renderer.set_max_table_width(max_table_width);
        renderer.set_collapse_soft_breaks(collapse_soft_breaks);
        let text = text.into();
        let expanded = crate::ui::scrollback::appearance::expand_tabs(&text);
        renderer.push(&expanded);
        // finish() (not render()) so the streaming LaTeX-delimiter normalizer flushes any trailing held-back delimiter bytes
        // This is a complete, one-shot document
        renderer.finish(highlight_syntax());
        Self {
            state: RefCell::new(RenderState {
                renderer,
                cache_width: 0,
                cache_generation: 0,
                cache_theme: theme_cache::current_kind(),
                cache_lines: Vec::new(),
                cache_joiners: Vec::new(),
                frozen_pre_wrap_count: 0,
                frozen_wrapped_count: 0,
            }),
            current_raw: false,
            generation: 1,
        }
    }

    /// Create empty for streaming.
    pub fn streaming() -> Self {
        Self {
            state: RefCell::new(RenderState {
                renderer: StreamingMarkdownRenderer::new(md_style::style(), true),
                cache_width: 0,
                cache_generation: 0,
                cache_theme: theme_cache::current_kind(),
                cache_lines: Vec::new(),
                cache_joiners: Vec::new(),
                frozen_pre_wrap_count: 0,
                frozen_wrapped_count: 0,
            }),
            current_raw: false,
            generation: 0,
        }
    }

    /// Append a streaming chunk and re-render.
    pub fn push_chunk(&mut self, chunk: &str) {
        let expanded = crate::ui::scrollback::appearance::expand_tabs(chunk);
        self.state
            .get_mut()
            .renderer
            .push_and_render(&expanded, highlight_syntax());
        self.generation += 1;
    }

    /// Append a chunk without rendering immediately.
    ///
    /// Used for historical replay during `session/load` so the pager can batch markdown work and render once after replay completes.
    pub fn push_chunk_deferred(&mut self, chunk: &str) {
        let expanded = crate::ui::scrollback::appearance::expand_tabs(chunk);
        self.state.get_mut().renderer.push(&expanded);
        self.generation += 1;
    }

    /// Finish streaming: full re-render for correctness.
    pub fn finish(&mut self) {
        let state = self.state.get_mut();
        state.renderer.finish(highlight_syntax());
        // finish() does a full re-render; reset frozen tracking so the next ensure_wrapped re-wraps everything from the new output
        state.frozen_pre_wrap_count = 0;
        state.frozen_wrapped_count = 0;
        self.generation += 1;
    }

    /// Get the source markdown text.
    pub fn text(&self) -> String {
        self.state.borrow().renderer.source().to_string()
    }

    /// Whether the source markdown is empty (zero-alloc, unlike `text()`).
    pub fn is_empty(&self) -> bool {
        self.state.borrow().renderer.source().is_empty()
    }

    /// Get the rendered text as plain text (all ratatui styles stripped).
    ///
    /// Useful for copy-to-clipboard in pretty mode.
    pub fn rendered_plain_text(&self) -> String {
        let state = self.state.borrow();
        let view = state.renderer.view();
        let mut result = String::new();
        for (i, line) in view.lines.iter().enumerate() {
            if i > 0 {
                result.push('\n');
            }
            for span in &line.spans {
                result.push_str(&span.content);
            }
        }
        result
    }

    /// Get the line source map (rendered line index to source line number).
    /// Each entry maps a pre-wrap rendered line to the source line it came from.
    /// Used for cursor stability when toggling raw/pretty mode.
    pub fn line_source_map(&self) -> Vec<usize> {
        self.state.borrow().renderer.view().line_source_map.to_vec()
    }

    /// Get the pre-wrap rendered lines (before word wrapping).
    /// Returns cloned lines from the markdown renderer's current output.
    /// These are styled `Line<'static>` objects at their natural width, suitable for feeding into a ListPane which handles its own wrapping.
    pub fn pre_wrap_lines(&self) -> Vec<Line<'static>> {
        self.state.borrow().renderer.view().lines.to_vec()
    }

    /// Access the pre-wrap hyperlink targets via a closure, avoiding allocation.
    pub fn with_hyperlinks<R>(
        &self,
        f: impl FnOnce(&[crate::engine::markdown::HyperlinkTarget]) -> R,
    ) -> R {
        let state = self.state.borrow();
        f(state.renderer.view().hyperlinks)
    }

    pub fn with_table_copy_meta<R>(
        &self,
        f: impl FnOnce(&[crate::engine::markdown::TableCopyMeta]) -> R,
    ) -> R {
        let state = self.state.borrow();
        f(state.renderer.view().tables)
    }

    /// Pre-wrap line ranges of the ` ```mermaid ` blocks in the current rendered output, reflecting the current render width.
    /// Avoids allocation (no source rebuild) so the caption path can call it every frame.
    /// The detection skeleton with the diagram source lives in [`mermaid_content`](Self::mermaid_content).
    pub fn mermaid_block_ranges(&self) -> Vec<std::ops::Range<usize>> {
        let state = self.state.borrow();
        super::mermaid_content::mermaid_block_ranges(&state.renderer.view())
    }

    /// Build the Mermaid detection skeleton from the current rendered output.
    /// Call at construction/finish (never per streaming chunk) to capture the detected diagrams.
    /// Detection only; rendering is lazy, driven by the affordance row on click.
    pub fn mermaid_content(&self) -> super::mermaid_content::MermaidContent {
        let state = self.state.borrow();
        super::mermaid_content::MermaidContent::from_view(&state.renderer.view())
    }

    /// Get the current generation counter.
    /// Bumped on every content mutation (push_chunk, finish, set_raw_mode).
    /// Used by viewers to detect when items need rebuilding.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether the content is currently in raw mode.
    pub fn is_raw(&self) -> bool {
        self.current_raw
    }

    /// Drop the word-wrap cache (`cache_lines` / `cache_joiners`). Only entries near the viewport need it hot.
    pub fn evict_wrap_cache(&self) {
        let mut state = self.state.borrow_mut();
        if state.cache_lines.is_empty() && state.cache_joiners.is_empty() {
            return;
        }
        state.cache_lines = Vec::new();
        state.cache_joiners = Vec::new();
        state.cache_generation = u64::MAX; // force rebuild on next use
        state.frozen_pre_wrap_count = 0;
        state.frozen_wrapped_count = 0;
    }

    /// Toggle raw mode, re-rendering if changed.
    pub fn set_raw_mode(&mut self, raw: bool) {
        if self.current_raw != raw {
            self.current_raw = raw;
            let state = self.state.get_mut();
            state.renderer.set_pretty(!raw);
            state.renderer.render(highlight_syntax());
            // set_pretty resets renderer frozen state
            state.frozen_pre_wrap_count = 0;
            state.frozen_wrapped_count = 0;
            self.generation += 1;
        }
    }

    /// Ensure the wrap cache is populated for the given width. Uses incremental wrapping: only re-wraps lines after the
    /// renderer's frozen boundary.
    fn ensure_wrapped(&self, width: usize) {
        let mut state = self.state.borrow_mut();
        let current_theme = theme_cache::current_kind();

        // If the theme changed, update the renderer's style so the re-render below picks up the new colors
        // Resetting cache_generation forces the cache to rebuild even if width and content haven't changed
        if state.cache_theme != current_theme {
            state.renderer.set_style(md_style::style());
            state.cache_theme = current_theme;
            state.cache_generation = u64::MAX; // force cache miss
            // set_style resets renderer frozen state, so our tracking is stale
            state.frozen_pre_wrap_count = 0;
            state.frozen_wrapped_count = 0;
        }

        if state.cache_width == width && state.cache_generation == self.generation {
            return;
        }

        // Width or theme changed: full re-wrap (frozen cache invalid)
        let width_changed = state.cache_width != width;
        if width_changed {
            state.frozen_pre_wrap_count = 0;
            state.frozen_wrapped_count = 0;
        }

        // Update table width and re-render (only re-renders tail internally).
        state.renderer.set_max_table_width(Some(width));
        state.renderer.render(highlight_syntax());

        let frozen_count = state.renderer.frozen_lines_count();

        // Only two ranges need wrapping. We clone the line slices we need *before* mutating
        // cache_lines, because view() borrows the renderer immutably.

        // Step 1: Wrap any newly frozen lines
        let new_frozen_wrapped = if frozen_count > state.frozen_pre_wrap_count {
            state
                .renderer
                .view()
                .lines
                .get(state.frozen_pre_wrap_count..frozen_count)
                .map(|new_frozen| word_wrap_lines_with_joiners(new_frozen.to_vec(), width))
        } else {
            None
        };

        // Step 2: Wrap the tail (unfrozen) lines
        let total_lines = state.renderer.view().lines.len();
        let tail_wrapped = if frozen_count < total_lines {
            state
                .renderer
                .view()
                .lines
                .get(frozen_count..)
                .map(|tail| word_wrap_lines_with_joiners(tail.to_vec(), width))
        } else {
            None
        };

        // Now mutate the cache (no more borrows of view/renderer)
        // Truncate the stale tail, keeping only the previously frozen wrapped output
        let frozen_wc = state.frozen_wrapped_count;
        state.cache_lines.truncate(frozen_wc);
        state.cache_joiners.truncate(frozen_wc);

        // Append newly frozen wrapped lines
        if let Some((new_lines, new_joiners)) = new_frozen_wrapped {
            state.cache_lines.extend(new_lines);
            state.cache_joiners.extend(new_joiners);
            state.frozen_pre_wrap_count = frozen_count;
            state.frozen_wrapped_count = state.cache_lines.len();
        }

        // Append tail wrapped lines
        if let Some((tail_lines, tail_joiners)) = tail_wrapped {
            state.cache_lines.extend(tail_lines);
            state.cache_joiners.extend(tail_joiners);
        }

        state.cache_width = width;
        state.cache_generation = self.generation;
    }

    /// Access cached wrapped lines + joiners for post-processing.
    /// The closure receives a [`WrappedLines`] reference valid for the duration of the call.
    /// That avoids cloning when the caller only needs to inspect or slice the lines (e.g., ThinkingBlock truncation).
    pub fn with_wrapped_lines<R>(&self, width: usize, f: impl FnOnce(WrappedLines<'_>) -> R) -> R {
        self.ensure_wrapped(width);
        let state = self.state.borrow();
        f(WrappedLines {
            lines: &state.cache_lines,
            joiners: &state.cache_joiners,
        })
    }

    /// Build a [`BlockOutput`] from the cached wrapped lines.
    /// Each line is converted to a [`BlockLine`] with joiner and optional background color (from the line's style, e.g., for code blocks).
    /// This is the common path used by [`AgentMessageBlock`](super::AgentMessageBlock).
    pub fn output(&self, width: usize) -> BlockOutput {
        // Raw mode shows the source `>` markers verbatim; nothing to exclude
        let strip = QuoteBarStrip::new(!self.current_raw);
        self.with_wrapped_lines(width, |wrapped| {
            if wrapped.lines.is_empty() {
                BlockOutput {
                    lines: vec![Line::from("").into()],
                }
            } else {
                BlockOutput {
                    lines: wrapped
                        .lines
                        .iter()
                        .zip(wrapped.joiners.iter())
                        .map(|(line, joiner)| {
                            let mut content = line.clone();
                            let selectable = strip.selectable(&mut content);
                            // For list/blockquote lines, even the first wrapped line includes the
                            // "│ " prefix. Measure indent_width from the actual line content to
                            // correctly exclude it from logical width calculations.
                            let indent_width = compute_subsequent_indent_width(line);
                            let mut block_line = BlockLine::styled(content)
                                .with_selection_range(Some(MARKDOWN_BODY_RANGE))
                                .with_joiner(joiner.clone());
                            block_line.selectable = selectable;
                            block_line.indent_width = indent_width;
                            if let Some(bg) = line.style.bg {
                                block_line.with_background(bg)
                            } else {
                                block_line
                            }
                        })
                        .collect(),
                }
            }
        })
    }
}

/// Compute the display width of the `subsequent_indent` prefix on a wrapped continuation line. This width is NOT
/// part of the logical pre-wrap content, so hyperlink column mapping must exclude it. Returns 0 for lines without
/// recognizable indent prefixes.
pub(super) fn compute_subsequent_indent_width(line: &Line<'_>) -> usize {
    use unicode_width::UnicodeWidthStr;

    // Flatten the line's spans into plain text
    let flat: String = line.spans.iter().map(|s| s.content.as_ref()).collect();

    // Blockquote/list lines start with one or more "│ " (U+2502 + space) prefixes
    let mut width = 0;
    let mut chars = flat.chars();
    while let Some('\u{2502}') = chars.next() {
        if chars.next() == Some(' ') {
            width += UnicodeWidthStr::width("\u{2502} ");
        } else {
            break;
        }
    }
    width
}
