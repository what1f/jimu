use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use ratatui::style::Color;
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::engine::render::color::blend_color;
use crate::engine::theme::Theme;
use crate::ui::scrollback::appearance::AppearanceConfig;

/// How to wrap content that exceeds the available width.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum WrapMode {
    #[default]
    Word,
    Character,
    Truncate,
}

/// Accent/bullet color style for a block.
/// Used by both `accent()` and `bullet()` trait methods.
/// When `animated` is true, the renderer uses a wave animation effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccentStyle {
    pub color: Color,
    pub animated: bool,
}

impl AccentStyle {
    /// Create a static (non-animated) accent style.
    pub const fn static_color(color: Color) -> Self {
        Self {
            color,
            animated: false,
        }
    }

    /// Create an animated accent style (wave effect for running blocks).
    pub const fn animated(color: Color) -> Self {
        Self {
            color,
            animated: true,
        }
    }

    /// The pulsing bullet of a row whose work is still running. `accent_running` is pre-dimmed by the collapsed-bullet
    /// ratio so the pulse peaks at the same brightness as the other collapsed blocks.
    pub fn animated_running(ctx: &BlockContext, theme: &Theme) -> Self {
        let dim = ctx.appearance.scrollback.display.dim_accent;
        let dimmed =
            blend_color(theme.bg_base, theme.accent_running, dim).unwrap_or(theme.accent_running);
        Self::animated(dimmed)
    }
}

pub use crate::ui::scrollback::appearance::BlockBackground;

/// How a block is currently displayed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum DisplayMode {
    Collapsed,
    Truncated,
    #[default]
    Expanded,
}

/// Which parts of a line can be selected for copying.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Selectable {
    /// All spans are selectable (default).
    #[default]
    All,
    /// Only these span indices are selectable (contiguous range).
    Spans(Range<usize>),
    /// Line is not selectable (decoration, acts as region boundary).
    None,
}

impl Selectable {
    /// Return the span range clamped to `len`, guaranteeing `start <= end <= len`.
    fn clamped_span_range(&self, len: usize) -> Option<Range<usize>> {
        match self {
            Selectable::Spans(range) => {
                let end = range.end.min(len);
                let start = range.start.min(end);
                Some(start..end)
            }
            _ => Option::None,
        }
    }
}

/// Context passed to block methods for rendering decisions.
#[derive(Debug, Clone)]
pub struct BlockContext {
    pub mode: DisplayMode,
    pub is_running: bool,
    pub width: u16,
    pub raw: bool,
    /// Optional row budget. When Some(n), the block must fit within n lines.
    pub max_lines: Option<u16>,
    /// Host-supplied appearance options.
    pub appearance: AppearanceConfig,
    /// Whether this entry is currently selected in the scrollback.
    pub is_selected: bool,
    /// Session/worktree cwd (`AgentSession.cwd`); with `None`, paths are never made relative.
    pub cwd: Option<PathBuf>,
}

impl BlockContext {
    /// Width of the bullet prefix (the char plus its trailing space), or 0 if disabled.
    pub fn bullet_indent(&self) -> usize {
        self.appearance
            .scrollback
            .blocks
            .tool
            .bullet
            .char()
            .map(|c| unicode_width::UnicodeWidthStr::width(c) + 1)
            .unwrap_or(0)
    }

    /// Effective content width after subtracting the bullet prefix (if enabled).
    /// Blocks that render single-line collapsed content should use this instead of `self.width`.
    /// Otherwise they overflow past the bullet character that `RenderBlock::output()` prepends.
    pub fn content_width(&self) -> usize {
        (self.width as usize).saturating_sub(self.bullet_indent())
    }

    /// Whether a collapsed block should render with the muted style.
    /// Selected blocks stay bright everywhere except legacy ConHost.
    /// There the selected/unselected color gap reads as palette noise after 16-color quantization, and the selection box already indicates focus.
    pub fn mute_when_collapsed(&self, muted_collapsed_enabled: bool) -> bool {
        if !muted_collapsed_enabled {
            return false;
        }
        crate::engine::render::glyphs::is_legacy_windows_console() || !self.is_selected
    }
}

/// A single line of block output.
#[derive(Debug, Clone)]
pub struct BlockLine {
    pub content: Line<'static>,
    pub background: Option<Color>,
    /// Whether `background` is a decorative "panel" band rather than semantic shading. Semantic shading always paints.
    pub background_is_panel: bool,
    /// Column where the background starts: 0 paints full width, a positive value paints from that column.
    pub bg_start_col: u16,
    pub wrap: WrapMode,
    pub selectable: Selectable,
    /// Logical selection range id within this block output.
    /// Ids count up from 0; `u16::MAX` is reserved for the synthetic group-header row the renderer adds (`render::GROUP_HEADER_RANGE_ID`).
    pub selection_range: Option<u16>,
    /// Optional source-of-truth text for the selectable portion of this line.
    pub selection_text: Option<String>,
    /// Soft-wrap joiner: how this line connects to the previous when copying. The first line of a block should always
    /// have `None`.
    pub joiner: Option<String>,
    /// Link target for rows whose painted text cannot recover it (tool headers).
    pub link_target: Option<crate::engine::render::osc8::LinkTarget>,
    /// Display width of the `subsequent_indent` prefix on wrapped continuation lines. This width is NOT part of the
    /// logical pre-wrap content, so hyperlink column mapping must exclude it when rebuilding pre-wrap coordinates from
    /// post-wrap segments.
    pub indent_width: usize,
}

impl Default for BlockLine {
    fn default() -> Self {
        Self {
            content: Line::default(),
            background: None,
            background_is_panel: false,
            bg_start_col: 0,
            wrap: WrapMode::Word,
            selectable: Selectable::All,
            selection_range: None,
            selection_text: None,
            joiner: None,
            link_target: None,
            indent_width: 0,
        }
    }
}

impl From<Line<'static>> for BlockLine {
    fn from(content: Line<'static>) -> Self {
        Self {
            content,
            ..Default::default()
        }
    }
}

impl BlockLine {
    /// Fully selectable plain text line.
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            content: Line::raw(s.into()),
            selectable: Selectable::All,
            ..Default::default()
        }
    }

    /// Styled line, fully selectable.
    pub fn styled(content: Line<'static>) -> Self {
        Self {
            content,
            selectable: Selectable::All,
            ..Default::default()
        }
    }

    /// Decoration line (not selectable, acts as region boundary).
    pub fn separator(content: Line<'static>) -> Self {
        Self {
            content,
            selectable: Selectable::None,
            ..Default::default()
        }
    }

    /// Set the background color.
    pub fn with_background(mut self, color: Color) -> Self {
        self.background = Some(color);
        self
    }

    /// Set a decorative "panel" background (tool result preview boxes). Unlike `with_background` (semantic shading:
    /// diff insert/delete rows, markdown code-block fill), a panel background is suppressed when the entry renders flat
    /// (minimal mode) so the preview blends with the terminal's own background.
    pub fn with_panel_background(mut self, color: Color) -> Self {
        self.background = Some(color);
        self.background_is_panel = true;
        self
    }

    /// Set a partial background color starting from a specific column.
    pub fn with_background_from(mut self, color: Color, start_col: u16) -> Self {
        self.background = Some(color);
        self.bg_start_col = start_col;
        self
    }

    /// Set the wrap mode.
    pub fn with_wrap(mut self, mode: WrapMode) -> Self {
        self.wrap = mode;
        self
    }

    /// Set the logical selection range id.
    pub fn with_selection_range(mut self, range: Option<u16>) -> Self {
        self.selection_range = range;
        self
    }

    /// Set explicit selectable text for this line.
    pub fn with_selection_text(mut self, text: Option<String>) -> Self {
        self.selection_text = text;
        self
    }

    /// Set the soft-wrap joiner.
    pub fn with_joiner(mut self, joiner: Option<String>) -> Self {
        self.joiner = joiner;
        self
    }
}

/// Flatten a rendered line's spans into the plain text drawn on that row.
/// Shared by selection-text derivation and the search-highlight post-pass.
pub fn line_plain_text(line: &Line) -> String {
    let mut out = String::new();
    line_plain_text_into(line, &mut out);
    out
}

/// Append a rendered line's plain text to `out`, reusing its capacity.
/// Lets the per-frame highlight pass avoid a fresh allocation for every visible row.
pub fn line_plain_text_into(line: &Line, out: &mut String) {
    for span in &line.spans {
        out.push_str(span.content.as_ref());
    }
}

pub fn derive_selection_text(line: &BlockLine) -> String {
    if let Some(text) = &line.selection_text {
        return text.clone();
    }

    match &line.selectable {
        Selectable::None => String::new(),
        Selectable::All => {
            // Table rows carry render-only trailing padding (added so the app owns every column); strip it so it never reaches the clipboard
            // Every `Selectable::All` line is trimmed, matching conventional terminal/tmux copy behavior
            // The canonical single-block `y` copy is unaffected; it uses the pre-wrap path
            let text = line_plain_text(&line.content);
            let trimmed = text.trim_end();
            if trimmed.len() == text.len() {
                text
            } else {
                trimmed.to_string()
            }
        }
        sel @ Selectable::Spans(_) => {
            let Some(r) = sel.clamped_span_range(line.content.spans.len()) else {
                return String::new();
            };
            let Some(spans) = line.content.spans.get(r) else {
                return String::new();
            };
            spans.iter().map(|span| span.content.as_ref()).collect()
        }
    }
}

/// The exact text painted in `line`'s selectable columns, in logical order. Unlike [`derive_selection_text`] it
/// never trims trailing padding or substitutes a copy override.
pub fn painted_selectable_region(line: &BlockLine) -> String {
    match selectable_cols(&line.content, &line.selectable) {
        Some(cols) => slice_display_cols(&line_plain_text(&line.content), cols.start, cols.end),
        None => String::new(),
    }
}

/// Slice `text` to the graphemes overlapping the display-column range `[start, end)`.
/// A wide grapheme is kept whole when the range covers any of its cells.
/// Graphemes that only touch a boundary (start at `end` or end at `start`) stay excluded.
pub fn slice_display_cols(text: &str, start: u16, end: u16) -> String {
    if start >= end || text.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    let mut col = 0u16;

    for grapheme in text.graphemes(true) {
        let width = grapheme_width(grapheme) as u16;
        let next_col = col.saturating_add(width);

        if width == 0 {
            if col >= start && col < end {
                out.push_str(grapheme);
            }
            continue;
        }
        if next_col <= start {
            col = next_col;
            continue;
        }
        if col >= end {
            break;
        }
        out.push_str(grapheme);
        col = next_col;
    }

    out
}

/// Display-cell range of the grapheme covering `col`, or `None` when `col` is past the last grapheme.
/// Zero-width graphemes occupy no cell and never advance the column, matching `slice_display_cols` and the paint path.
pub fn grapheme_cells_at(text: &str, col: u16) -> Option<std::ops::Range<u16>> {
    let mut c = 0u16;

    for grapheme in text.graphemes(true) {
        let width = grapheme_width(grapheme) as u16;
        if width == 0 {
            continue;
        }

        let next = c.saturating_add(width);
        if next > col {
            return Some(c..next);
        }
        c = next;
    }

    None
}

/// Exclusive display column just past the grapheme occupying `col`, or the text's total painted width when `col` is past the last one.
pub fn col_past_grapheme(text: &str, col: u16) -> u16 {
    match grapheme_cells_at(text, col) {
        Some(cells) => cells.end,
        // Per-grapheme total (matches `grapheme_cells_at`), so a ligature row's last cell isn't dropped when `col` snaps to the row end
        None => u16::try_from(str_display_cells(text)).unwrap_or(u16::MAX),
    }
}

pub fn block_line_selectable_width(line: &BlockLine) -> u16 {
    derive_selection_text(line).width() as u16
}

pub fn shift_selection_metadata_for_prefix(line: &mut BlockLine, prefix_span_count: usize) {
    if prefix_span_count == 0 {
        return;
    }

    line.selectable = match &line.selectable {
        Selectable::All => Selectable::Spans(prefix_span_count..line.content.spans.len()),
        Selectable::Spans(range) => {
            let shifted =
                Selectable::Spans(range.start + prefix_span_count..range.end + prefix_span_count);
            let r = shifted
                .clamped_span_range(line.content.spans.len())
                .unwrap();
            Selectable::Spans(r)
        }
        Selectable::None => Selectable::None,
    };
}

fn grapheme_width(grapheme: &str) -> usize {
    UnicodeWidthStr::width(grapheme)
}

/// Terminal cells `s` occupies when painted: the sum of its grapheme widths. Otherwise the last cell of such a line
/// can't be selected or copied.
pub(crate) fn str_display_cells(s: &str) -> usize {
    s.graphemes(true).map(grapheme_width).sum()
}

fn span_display_cells(span: &Span) -> usize {
    str_display_cells(&span.content)
}

/// Complete output produced by a block for rendering.
#[derive(Debug, Clone, Default)]
pub struct BlockOutput {
    pub lines: Vec<BlockLine>,
}

/// Rare selection metadata: source bytes omitted from a visible row, and a way to reach a source row that paints blank but is not empty.
/// TODO: Copy the absolute Read/Edit target for a full painted-path drag; partial drags copy painted columns only to keep highlight and clipboard aligned.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SelectionBoundary {
    prefix: String,
    suffix: String,
    has_empty_row_anchor: bool,
}

impl SelectionBoundary {
    pub(crate) fn new(prefix: String, suffix: String) -> Self {
        Self {
            prefix,
            suffix,
            has_empty_row_anchor: false,
        }
    }

    /// Make an otherwise empty source row reachable through its first blank terminal cell without painting or copying a placeholder.
    pub(crate) fn empty_row_anchor(prefix: String, suffix: String) -> Self {
        Self {
            prefix,
            suffix,
            has_empty_row_anchor: true,
        }
    }

    pub(crate) fn anchored_cols(&self, cols: Range<u16>) -> Range<u16> {
        if self.has_empty_row_anchor && cols.is_empty() {
            cols.start..cols.start.saturating_add(1)
        } else {
            cols
        }
    }

    pub(crate) fn apply(
        &self,
        selected: String,
        include_prefix: bool,
        include_suffix: bool,
    ) -> String {
        let mut output = String::with_capacity(
            selected.len()
                + usize::from(include_prefix) * self.prefix.len()
                + usize::from(include_suffix) * self.suffix.len(),
        );
        if include_prefix {
            output.push_str(&self.prefix);
        }
        output.push_str(&selected);
        if include_suffix {
            output.push_str(&self.suffix);
        }
        output
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SelectionBoundaryEntry {
    pub(crate) line_index: usize,
    pub(crate) boundary: Arc<SelectionBoundary>,
}

/// Sparse immutable metadata stored beside the output, keyed to exact line indices.
/// Ordinary outputs keep `None`; clones share the boundary payloads through `Arc`.
#[derive(Debug, Clone, Default)]
pub(crate) struct SelectionBoundaries(Option<Arc<[SelectionBoundaryEntry]>>);

impl SelectionBoundaries {
    pub(crate) fn from_entries(entries: Vec<SelectionBoundaryEntry>) -> Self {
        if entries.is_empty() {
            Self(None)
        } else {
            Self(Some(Arc::from(entries)))
        }
    }

    pub(crate) fn get(&self, line_index: usize) -> Option<&Arc<SelectionBoundary>> {
        self.0
            .as_deref()?
            .iter()
            .find(|entry| entry.line_index == line_index)
            .map(|entry| &entry.boundary)
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RenderedBlockOutput {
    pub(crate) output: BlockOutput,
    pub(crate) boundaries: SelectionBoundaries,
}

impl From<BlockOutput> for RenderedBlockOutput {
    fn from(output: BlockOutput) -> Self {
        Self {
            output,
            boundaries: SelectionBoundaries::default(),
        }
    }
}

impl BlockOutput {
    pub fn new() -> Self {
        Self::default()
    }

    /// Plain text, all lines fully selectable.
    pub fn plain(text: &str) -> Self {
        Self {
            lines: text.lines().map(BlockLine::text).collect(),
        }
    }

    pub fn push(&mut self, line: BlockLine) {
        self.lines.push(line);
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn height(&self) -> u16 {
        self.lines.len() as u16
    }

    /// Wrap the first line with the prefix and the last line with the suffix.
    /// Decorations are NOT selectable.
    pub fn with_decorations(
        mut self,
        prefix: Option<Span<'static>>,
        suffix: Option<Span<'static>>,
    ) -> Self {
        if let Some(prefix_span) = prefix
            && let Some(first) = self.lines.first_mut()
        {
            let mut new_spans = vec![prefix_span];
            new_spans.extend(first.content.spans.iter().cloned());
            first.content = Line::from(new_spans);
            shift_selection_metadata_for_prefix(first, 1);
        }

        if let Some(suffix_span) = suffix
            && let Some(last) = self.lines.last_mut()
        {
            let content_end = last.content.spans.len();
            last.content.spans.push(suffix_span);

            last.selectable = match &last.selectable {
                Selectable::All => Selectable::Spans(0..content_end),
                Selectable::Spans(r) => Selectable::Spans(r.clone()),
                Selectable::None => Selectable::None,
            };
        }

        self
    }
}

/// Pre-wrap (logical source) line index for each post-wrap output row. The first row is always index 0.
pub(crate) fn prewrap_index_per_row(lines: &[BlockLine]) -> Vec<usize> {
    let mut indices = Vec::with_capacity(lines.len());
    let mut prewrap = 0usize;
    for (row, line) in lines.iter().enumerate() {
        if row > 0 && line.joiner.is_none() {
            prewrap += 1;
        }
        indices.push(prewrap);
    }
    indices
}

/// Convert span indices to display columns without narrowing to terminal-sized `u16` coordinates.
pub(crate) fn selectable_cols_usize(line: &Line, selectable: &Selectable) -> Option<Range<usize>> {
    match selectable {
        Selectable::None => None,
        Selectable::All => Some(0..line.spans.iter().map(span_display_cells).sum()),
        sel @ Selectable::Spans(_) => {
            let r = sel.clamped_span_range(line.spans.len())?;
            let start_col = line
                .spans
                .get(..r.start)?
                .iter()
                .map(span_display_cells)
                .sum();
            let end_col = line
                .spans
                .get(..r.end)?
                .iter()
                .map(span_display_cells)
                .sum();
            Some(start_col..end_col)
        }
    }
}

/// Convert span indices to terminal-sized columns for hit-testing.
pub fn selectable_cols(line: &Line, selectable: &Selectable) -> Option<Range<u16>> {
    let cols = selectable_cols_usize(line, selectable)?;
    Some(u16::try_from(cols.start).ok()?..u16::try_from(cols.end).ok()?)
}

/// Returns the logical columns unchanged when reordering is off or the row isn't reordered. Outside content is only
/// the left-anchored chrome prefix and the trailing suffix, never interleaved between the region's own runs.
/// Reordering preserves width, so only the start offset can change.
pub fn visual_selectable_cols(line: &BlockLine) -> Option<Range<u16>> {
    let logical = selectable_cols(&line.content, &line.selectable)?;
    if !crate::engine::render::bidi::is_enabled() {
        return Some(logical);
    }
    let plain = line_plain_text(&line.content);
    let ranges = crate::engine::render::bidi::logical_cols_to_visual(
        &plain,
        logical.start as usize,
        logical.end as usize,
    );
    let mut it = ranges.into_iter();
    let Some((first_start, first_end)) = it.next() else {
        return Some(logical);
    };
    let (mut vstart, mut vend) = (first_start, first_end);
    for (s, e) in it {
        vstart = vstart.min(s);
        vend = vend.max(e);
    }
    Some(u16::try_from(vstart).unwrap_or(logical.start)..u16::try_from(vend).unwrap_or(logical.end))
}
