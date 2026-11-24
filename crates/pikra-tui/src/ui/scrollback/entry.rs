use std::cell::{Ref, RefCell};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};

use super::block::{BlockContent, RenderBlock};
use super::types::{BlockContext, BlockOutput, DisplayMode, RenderedBlockOutput};
use crate::engine::theme::{ThemeKind, cache as theme_cache};
use crate::ui::scrollback::appearance::AppearanceConfig;

#[derive(Debug, Clone)]
struct CachedOutput {
    width: u16,
    raw: bool,
    theme: ThemeKind,
    is_selected: bool,
    cwd: Option<PathBuf>,
    rendered: RenderedBlockOutput,
}

/// Caches truncated-mode line count so layout need not call `block.output()` (syntect or full wrap) on every redraw.
/// Streaming invalidates the layout cache on each push; without this, every entry height is recomputed.
/// `cwd` is keyed because Edit/Read header wrap can change between absolute and relative paths.
type CachedTruncatedHeight = (u16, bool, ThemeKind, Option<PathBuf>, u16);

/// Unique identifier for a scrollback entry.
/// EntryIds are stable across mutations: they won't become invalid if other entries are added or removed.
/// Use this for external handles to entries (e.g., streaming tasks that need to push chunks to a specific block).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EntryId(u64);

impl EntryId {
    /// Create a new EntryId with a specific value.
    /// For production use, prefer getting EntryId from `ScrollbackState::push()` which assigns IDs automatically.
    /// This is mainly for placeholders/testing.
    pub fn new(id: u64) -> Self {
        Self(id)
    }

    pub fn value(self) -> u64 {
        self.0
    }
}

/// Saturates at `u16::MAX` so a multi-MB block cannot overflow `virtual_y`.
fn wrapped_lines_from_widths(widths: &[u32], content_width: u16) -> u16 {
    let cw = content_width.max(1) as usize;
    let mut total: usize = 0;
    for &w in widths {
        let w = w as usize;
        total += if w == 0 { 1 } else { w.div_ceil(cw) };
        if total >= u16::MAX as usize {
            return u16::MAX;
        }
    }
    total.max(1) as u16
}

/// A scrollback entry: block content and display state.
#[derive(Debug, Clone)]
pub struct ScrollbackEntry {
    pub id: EntryId,

    pub block: RenderBlock,

    /// Whether block is still running (for animation, auto-collapse).
    pub is_running: bool,

    /// Whether this entry is currently waiting on user input (permission prompt, ask-user-question, etc.).
    /// When true, the renderer replaces the wave "loading" animation with a pulsing-circle bullet to draw attention without implying active work.
    /// Maintained by `AgentView` from `permission_queue` and `question_view` state via `ScrollbackState::set_pending_user_input`.
    pub is_pending_user_input: bool,

    pub display_mode: DisplayMode,

    pub display_mode_pinned: bool,

    /// Raw mode: if true and block has_raw_mode(), render markdown as raw.
    pub raw: bool,

    pub created_at: Option<DateTime<Local>>,

    /// When this entry finished running (monotonic). Used by the renderer to flash the accent briefly after completion.
    pub finished_at: Option<std::time::Instant>,

    /// Interior-mutable so EntryRenderer (which holds `&self`) can populate and read the cache without &mut self. The
    /// `is_selected` key is only meaningful for blocks whose output varies by selection state (currently only
    /// `UserPrompt`). `cwd` is keyed so Expanded tool path paint (relative vs absolute) invalidates.
    cached_output: RefCell<Option<CachedOutput>>,

    /// Cached truncated-mode height. See [`CachedTruncatedHeight`] for why this needs its own cache separate from `cached_output`.
    ///
    /// Populated lazily by `ensure_truncated_height_cached`. Cleared by `invalidate_cache` together with `cached_output`.
    cached_truncated_height: RefCell<Option<CachedTruncatedHeight>>,

    /// Cached cheap height-estimate line count: `(content_width, lines)`.
    /// Lets a same-width rebuild reuse the estimate instead of re-cloning the block's source text. Cleared by `invalidate_cache`.
    cached_estimate_lines: RefCell<Option<(u16, u16)>>,

    /// Display width of each source line. Width-independent, so unlike every other cache here it survives a resize.
    /// Re-deriving it per width is what made a resize cost O(total conversation bytes).
    cached_line_widths: RefCell<Option<Vec<u32>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectiveOutputKind {
    Cached,
    Selected,
}

pub enum EffectiveOutputData<'a> {
    Borrowed(Ref<'a, BlockOutput>),
    Owned(BlockOutput),
}

impl EffectiveOutputData<'_> {
    #[allow(clippy::should_implement_trait)]
    pub fn as_ref(&self) -> &BlockOutput {
        match self {
            Self::Borrowed(output) => output,
            Self::Owned(output) => output,
        }
    }
}

pub struct EffectiveOutput<'a> {
    pub ctx: BlockContext,
    pub output: EffectiveOutputData<'a>,
    pub has_vpad: bool,
    pub kind: EffectiveOutputKind,
}

impl EffectiveOutput<'_> {
    pub fn output(&self) -> &BlockOutput {
        self.output.as_ref()
    }
}

impl ScrollbackEntry {
    /// Create a new entry with expanded display mode.
    /// For production use, prefer `ScrollbackState::push()` which assigns the EntryId automatically.
    /// This constructor is mainly for testing.
    pub fn new(block: RenderBlock) -> Self {
        Self::with_id(EntryId(0), block)
    }

    /// Create a new entry with a specific ID.
    ///
    /// The display mode is set to the block's default (Expanded for most, Truncated for thinking blocks).
    pub fn with_id(id: EntryId, block: RenderBlock) -> Self {
        let display_mode = block.default_display_mode();
        Self {
            id,
            block,
            is_running: false,
            is_pending_user_input: false,
            display_mode,
            display_mode_pinned: false,
            raw: false,
            created_at: Some(Local::now()),
            finished_at: None,
            cached_output: RefCell::new(None),
            cached_truncated_height: RefCell::new(None),
            cached_estimate_lines: RefCell::new(None),
            cached_line_widths: RefCell::new(None),
        }
    }

    /// Create a new entry that is currently running.
    /// For production use, prefer `ScrollbackState::push()` which assigns the EntryId automatically.
    /// This constructor is mainly for testing.
    pub fn running(block: RenderBlock) -> Self {
        Self::running_with_id(EntryId(0), block)
    }

    /// Create a new running entry with a specific ID.
    ///
    /// The display mode is set to the block's default (Expanded for most, Truncated for thinking blocks).
    pub fn running_with_id(id: EntryId, block: RenderBlock) -> Self {
        let display_mode = block.default_display_mode();
        Self {
            id,
            block,
            is_running: true,
            is_pending_user_input: false,
            display_mode,
            display_mode_pinned: false,
            raw: false,
            created_at: Some(Local::now()),
            finished_at: None,
            cached_output: RefCell::new(None),
            cached_truncated_height: RefCell::new(None),
            cached_estimate_lines: RefCell::new(None),
            cached_line_widths: RefCell::new(None),
        }
    }

    /// Set the display mode (builder pattern).
    pub fn with_display_mode(mut self, mode: DisplayMode) -> Self {
        self.display_mode = mode;
        self
    }

    /// Toggle raw mode if the block supports it.
    pub fn toggle_raw(&mut self) {
        if self.block.has_raw_mode() {
            self.raw = !self.raw;
            self.block.set_raw_mode(self.raw);
            self.invalidate_cache();
        }
    }

    /// Toggle between display modes.
    /// Most blocks toggle between Collapsed and Expanded.
    /// Some blocks (like thinking) cycle through 3 modes.
    pub fn toggle_fold(&mut self) {
        if self.is_foldable() {
            self.display_mode = self
                .block
                .next_fold_mode(self.display_mode, self.is_running);
            self.invalidate_cache();
        }
    }

    pub fn display_mode(&self) -> DisplayMode {
        self.display_mode
    }

    pub fn set_display_mode(&mut self, mode: DisplayMode) {
        if self.display_mode != mode {
            self.display_mode = mode;
            self.invalidate_cache();
        }
    }

    /// Mark the block as completed (no longer running).
    ///
    /// Also clears `is_pending_user_input` since a completed tool cannot be waiting on a user response anymore.
    pub fn mark_completed(&mut self) {
        self.is_running = false;
        self.is_pending_user_input = false;
        self.invalidate_cache();
    }

    /// Invalidate cached output after a content change.
    pub fn invalidate_cache(&mut self) {
        self.invalidate_width_caches();
        *self.cached_line_widths.borrow_mut() = None;
    }

    /// Invalidate only the caches keyed by terminal width: the resize path.
    pub fn invalidate_width_caches(&mut self) {
        *self.cached_output.borrow_mut() = None;
        *self.cached_truncated_height.borrow_mut() = None;
        *self.cached_estimate_lines.borrow_mut() = None;
    }

    /// Drop the heavyweight cached render output (and the block's internal rebuildable caches) while KEEPING the cheap height caches.
    /// Layout (entry heights, scroll position) is untouched; re-rendering happens transparently if the entry scrolls back into view.
    /// Returns `true` when something was actually dropped (for sweep stats).
    pub(crate) fn evict_render_cache(&self) -> bool {
        let had_output = self.cached_output.borrow().is_some();
        if had_output {
            *self.cached_output.borrow_mut() = None;
        }
        self.block.evict_render_caches();
        had_output
    }

    /// Memoized cheap height-estimate line count for `content_width`, if cached.
    pub fn cached_estimate_lines(&self, content_width: u16) -> Option<u16> {
        self.cached_estimate_lines
            .borrow()
            .filter(|&(w, _)| w == content_width)
            .map(|(_, lines)| lines)
    }

    /// Store the cheap height-estimate line count for `content_width`.
    pub fn store_estimate_lines(&self, content_width: u16, lines: u16) {
        *self.cached_estimate_lines.borrow_mut() = Some((content_width, lines));
    }

    /// Cheap wrapped-line estimate for the block's source text at `content_width`.
    /// An APPROXIMATION: it ignores word boundaries, and for a markdown block it reflects the last rendered view.
    /// On-screen entries are always measured exactly, so nothing depends on it being right.
    pub fn estimate_source_lines(&self, content_width: u16) -> u16 {
        let mut slot = self.cached_line_widths.borrow_mut();
        let widths = slot.get_or_insert_with(|| {
            let Some(text) = self.block.searchable_text() else {
                return Vec::new();
            };
            // Renderers drop a single trailing newline.
            let text = text.strip_suffix('\n').unwrap_or(&text);
            text.split('\n')
                .map(|line| unicode_width::UnicodeWidthStr::width(line) as u32)
                .collect()
        });
        wrapped_lines_from_widths(widths, content_width)
    }

    /// Ensure the cache is populated for the given width/appearance/selection.
    /// This works with `&self` (via RefCell) so `EntryRenderer` can call it without needing `&mut self`.
    /// After calling this, use `cached_output_ref()` to borrow the output.
    pub fn ensure_cached(
        &self,
        width: u16,
        appearance: &AppearanceConfig,
        is_selected: bool,
        cwd: Option<&Path>,
    ) {
        // UserPrompt, ToolCall, Thinking, BgTask and Subagent vary their output() based on is_selected; for all other blocks the output is identical
        // Normalize to false for those blocks so selection changes don't thrash the cache
        let effective_selected = is_selected
            && (self.block.is_user_prompt()
                || self.block.is_tool_call()
                || self.block.is_thinking()
                || self.block.is_bg_task()
                || self.block.is_subagent());

        let current_theme = theme_cache::current_kind();
        let cwd_key = cwd.map(|p| p.to_path_buf());
        {
            let cache = self.cached_output.borrow();
            if let Some(cached) = cache.as_ref()
                && cached.width == width
                && cached.raw == self.raw
                && cached.theme == current_theme
                && cached.is_selected == effective_selected
                && cached.cwd == cwd_key
            {
                return; // cache hit
            }
        }

        // Cache miss: regenerate
        let ctx = BlockContext {
            mode: self.display_mode,
            is_running: self.is_running,
            width,
            raw: self.raw,
            max_lines: None,
            appearance: appearance.clone(),
            is_selected: effective_selected,
            cwd: cwd_key.clone(),
        };
        let rendered = self.block.rendered_output(&ctx);
        *self.cached_output.borrow_mut() = Some(CachedOutput {
            width,
            raw: self.raw,
            theme: current_theme,
            is_selected: effective_selected,
            cwd: cwd_key,
            rendered,
        });
    }

    /// Borrow the cached output.
    ///
    /// Panics if `ensure_cached` was not called first for the current width.
    pub fn cached_output_ref(&self) -> Ref<'_, BlockOutput> {
        Ref::map(self.cached_output.borrow(), |opt| {
            &opt.as_ref()
                .expect("ensure_cached must be called first")
                .rendered
                .output
        })
    }

    pub(crate) fn cached_rendered_output_ref(&self) -> Ref<'_, RenderedBlockOutput> {
        Ref::map(self.cached_output.borrow(), |opt| {
            &opt.as_ref()
                .expect("ensure_cached must be called first")
                .rendered
        })
    }

    /// Without this cache, `block.output` runs uncached on every layout rebuild and re-highlights every entry on each streaming push.
    /// Key is `(content_width, raw, theme, cwd)`; `is_selected` is excluded because line count never depends on selection.
    /// Cleared together with `cached_output` by `invalidate_cache`.
    pub fn ensure_truncated_height_cached(
        &self,
        content_width: u16,
        appearance: &AppearanceConfig,
        cwd: Option<&Path>,
    ) -> u16 {
        let current_theme = theme_cache::current_kind();
        let cwd_key = cwd.map(|p| p.to_path_buf());
        {
            let cache = self.cached_truncated_height.borrow();
            if let Some(&(cached_width, cached_raw, cached_theme, ref cached_cwd, height)) =
                cache.as_ref()
                && cached_width == content_width
                && cached_raw == self.raw
                && cached_theme == current_theme
                && *cached_cwd == cwd_key
            {
                return height;
            }
        }

        // Force Truncated for sticky-header height; include cwd (header wrap).
        let ctx = self.context_with_mode(content_width, DisplayMode::Truncated, appearance, cwd);
        let output = self.block.output(&ctx);
        let has_vpad = self.block.has_vpad(&ctx);
        let content_height = output.len() as u16;
        let vpad = if has_vpad { 2 } else { 0 };
        let height = content_height + vpad;

        *self.cached_truncated_height.borrow_mut() =
            Some((content_width, self.raw, current_theme, cwd_key, height));
        height
    }

    pub fn effective_output(
        &self,
        width: u16,
        appearance: &AppearanceConfig,
        is_selected: bool,
        cwd: Option<&Path>,
    ) -> EffectiveOutput<'_> {
        let mut ctx = self.context(width, appearance, cwd);
        ctx.is_selected = is_selected;

        let has_vpad = self.block.has_vpad(&ctx);
        self.ensure_cached(width, appearance, is_selected, cwd);
        EffectiveOutput {
            ctx,
            output: EffectiveOutputData::Borrowed(self.cached_output_ref()),
            has_vpad,
            kind: if is_selected {
                EffectiveOutputKind::Selected
            } else {
                EffectiveOutputKind::Cached
            },
        }
    }

    /// Get the block output, using cache if valid.
    /// The cache doesn't track appearance; the caller should invalidate on appearance change.
    pub fn output(
        &mut self,
        width: u16,
        appearance: &AppearanceConfig,
        cwd: Option<&Path>,
    ) -> &BlockOutput {
        self.ensure_cached(width, appearance, false, cwd);
        // ensure_cached just populated the cache, so unwrap through the RefCell
        let cache = self.cached_output.get_mut();
        &cache.as_ref().unwrap().rendered.output
    }

    pub fn context(
        &self,
        width: u16,
        appearance: &AppearanceConfig,
        cwd: Option<&Path>,
    ) -> BlockContext {
        BlockContext {
            mode: self.display_mode,
            is_running: self.is_running,
            width,
            raw: self.raw,
            max_lines: None,
            appearance: appearance.clone(),
            is_selected: false,
            cwd: cwd.map(|p| p.to_path_buf()),
        }
    }

    pub fn is_foldable(&self) -> bool {
        self.block.is_foldable()
    }

    /// True for a thinking block hidden by the Appearance toggle. Takes the flag as a param so hot layout loops can hoist the cache read.
    pub fn is_hidden_thinking(&self, show_thinking: bool) -> bool {
        self.block.is_thinking() && !show_thinking
    }

    /// Render without touching the entry's cache (dashboard peek renders at a foreign width).
    pub fn output_uncached(&self, ctx: &BlockContext) -> BlockOutput {
        self.block.rendered_output(ctx).output
    }

    pub fn context_with_budget(
        &self,
        width: u16,
        max_lines: u16,
        appearance: &AppearanceConfig,
        cwd: Option<&Path>,
    ) -> BlockContext {
        BlockContext {
            mode: self.display_mode,
            is_running: self.is_running,
            width,
            raw: self.raw,
            max_lines: Some(max_lines),
            appearance: appearance.clone(),
            is_selected: false,
            cwd: cwd.map(|p| p.to_path_buf()),
        }
    }

    /// This is used to compute heights for different display modes without modifying the entry's actual display_mode (avoiding cloning).
    pub fn context_with_mode(
        &self,
        width: u16,
        mode: DisplayMode,
        appearance: &AppearanceConfig,
        cwd: Option<&Path>,
    ) -> BlockContext {
        BlockContext {
            mode,
            is_running: self.is_running,
            width,
            raw: self.raw,
            max_lines: None,
            appearance: appearance.clone(),
            is_selected: false,
            cwd: cwd.map(|p| p.to_path_buf()),
        }
    }

    /// This is used for rendering sticky headers where we want. Expanded content (not collapsed summary). But truncated
    /// to a specific number of lines. This avoids mutating the entry's display_mode during render.
    pub fn context_with_mode_and_budget(
        &self,
        width: u16,
        mode: DisplayMode,
        max_lines: u16,
        appearance: &AppearanceConfig,
        is_selected: bool,
        cwd: Option<&Path>,
    ) -> BlockContext {
        BlockContext {
            mode,
            is_running: self.is_running,
            width,
            raw: self.raw,
            max_lines: Some(max_lines),
            appearance: appearance.clone(),
            is_selected,
            cwd: cwd.map(|p| p.to_path_buf()),
        }
    }
}
