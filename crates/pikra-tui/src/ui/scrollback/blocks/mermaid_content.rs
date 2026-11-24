//! Mermaid diagram detection and the on-screen affordance row.
//!
//! The markdown renderer draws ` ```mermaid ` blocks inline as Unicode box-drawing art.
//! This module detects those blocks in an agent message via the generic [`CodeBlockSpan`](crate::engine::markdown::CodeBlockSpan) API.
//! It exposes each diagram's clean source so a full-fidelity PNG can be rendered on demand.
//! It never renders and tracks no per-diagram render state (rendering is lazy, driven by the affordance row on click).
//! For `auto`/`on` a clickable affordance row (`◇ mermaid [Open Image] [Copy Image Path] [Copy Source]`) is placed beneath the inline art.
//! For `off` only the inline art is shown.
//! The rendered PNG is never drawn inline; it is reached only through the affordance row's actions.

use std::ops::Range;

use crate::engine::markdown::MarkdownRenderView;
use ratatui::text::Line;
use sha2::{Digest, Sha256};
use unicode_width::UnicodeWidthStr;

use crate::engine::theme::ThemeKind;
use crate::ui::scrollback::appearance::RenderMermaid;
use crate::ui::scrollback::types::{BlockLine, BlockOutput};

/// Fence info string identifying a Mermaid diagram.
pub const MERMAID_INFO: &str = "mermaid";

/// Subtle `◇ mermaid` marker: the leading (dim, non-clickable) label on the affordance row.
const MERMAID_LABEL: &str = "\u{25c7} mermaid";

/// Status hint shown in the affordance row while an on-click diagram render is in flight.
const MERMAID_RENDERING: &str = "rendering diagram\u{2026}";

/// Affordance-row button label: open the rendered PNG in the OS default app.
const AFFORDANCE_OPEN: &str = "[Open Image]";
/// Affordance-row button label: copy the rendered PNG's filesystem path.
const AFFORDANCE_COPY_PATH: &str = "[Copy Image Path]";
/// Affordance-row button label: copy the diagram's Mermaid source.
const AFFORDANCE_COPY_SOURCE: &str = "[Copy Source]";

/// Display-column gap between adjacent affordance-row buttons (and before the trailing status hint).
const AFFORDANCE_GAP: u16 = 3;

/// Width quantum (in display columns) for the cache key's width bucket.
/// Renders are reused across small resizes by bucketing the target width.
/// Only applies to [`MermaidRenderQuality::Terminal`]; the open tier ignores terminal width.
const MERMAID_WIDTH_BUCKET: u16 = 8;

/// Sentinel width-bucket for [`MermaidRenderQuality::Open`] (OS viewer / copy path).
/// Not derived from terminal columns, so open-tier PNGs never collide with terminal-budget renders of the same source+theme.
const OPEN_QUALITY_WIDTH_BUCKET: u16 = u16::MAX;

/// Quantize a target content-column count to the cache key's width bucket, so a sub-bucket resize maps to the same key (no re-render, no rescan).
fn width_bucket(target_width_cols: u16) -> u16 {
    target_width_cols / MERMAID_WIDTH_BUCKET
}

/// Output quality tier for a rendered Mermaid PNG.
/// `[Open Image]` / `[Copy Image Path]` use [`Open`] so the PNG is sharp in an OS image viewer.
/// A future terminal-budget path can use [`Terminal`] without sharing cache files with the open tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum MermaidRenderQuality {
    /// Sized from the terminal content width (HiDPI oversample + modest caps).
    #[default]
    Terminal,
    /// Auto-scaled for OS viewers: prefer at least 2x the intrinsic SVG size and a minimum pixel width, with higher height/area headroom.
    Open,
}

/// Content hash of a diagram source: the theme/width-independent component of a [`MermaidCacheKey`].
/// Matching a pending render against this rather than the full key keeps the `rendering…` hint tied to the diagram.
/// It stays tied even if the live theme/width changes mid-render.
pub(crate) fn hash_source(source: &str) -> [u8; 32] {
    Sha256::digest(source.as_bytes()).into()
}

/// A detected Mermaid block within a rendered agent message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MermaidBlock {
    /// The clean diagram source: the fence body with container markers (blockquote `>`, list indentation) stripped and CRLF normalized.
    /// Taken from [`CodeBlockSpan::body`](crate::engine::markdown::CodeBlockSpan::body).
    /// For a blockquoted or list-nested diagram this is the de-prefixed code, not the raw source slice.
    pub source: String,
    /// Range of pre-wrap rendered body lines this diagram occupies, as indices into [`MarkdownRenderView::lines`].
    /// Mirrors [`CodeBlockSpan::output_line_range`](crate::engine::markdown::CodeBlockSpan::output_line_range).
    pub prewrap_line_range: Range<usize>,
}

/// Whether a fence info string identifies a Mermaid diagram: its first whitespace-delimited token equals `mermaid` (case-insensitive).
/// ` ```mermaid `, ` ```Mermaid `, and ` ```mermaid theme=base ` all match while a code block in another language does not.
fn is_mermaid_info(info: &str) -> bool {
    info.split_whitespace()
        .next()
        .is_some_and(|token| token.eq_ignore_ascii_case(MERMAID_INFO))
}

/// The view's code-block spans that are Mermaid fences, in document order.
fn mermaid_spans<'a>(
    view: &'a MarkdownRenderView,
) -> impl Iterator<Item = &'a crate::engine::markdown::CodeBlockSpan> {
    view.code_blocks
        .iter()
        .filter(|span| is_mermaid_info(&span.info))
}

/// Filter a rendered view's code-block spans down to Mermaid fences.
/// Returns one [`MermaidBlock`] per closed ` ```mermaid ` fence, in document order, carrying the clean de-prefixed diagram source.
/// Allocates a `source` String per block; for the per-frame render path that only needs line positions use [`mermaid_block_ranges`] instead.
pub fn mermaid_blocks(view: &MarkdownRenderView) -> Vec<MermaidBlock> {
    mermaid_spans(view)
        .map(|span| MermaidBlock {
            source: span.body.clone(),
            prewrap_line_range: span.output_line_range.clone(),
        })
        .collect()
}

/// Pre-wrap line ranges of the view's Mermaid fences, in document order.
///
/// The allocation-free counterpart of [`mermaid_blocks`] for the render hot path (caption placement needs only line positions, never the source).
pub fn mermaid_block_ranges(view: &MarkdownRenderView) -> Vec<Range<usize>> {
    mermaid_spans(view)
        .map(|span| span.output_line_range.clone())
        .collect()
}

/// `Light` is the only light theme. `Terminal` has no polarity of its own — a rendered diagram is a raster with a
/// baked background, so it takes the dark treatment that minimal mode already gets. It lives here (not in the
/// engine crate) so the always-compiled detection module stays independent of the optional `mermaid` feature.
pub fn theme_is_dark(theme: ThemeKind) -> bool {
    !matches!(theme, ThemeKind::Light)
}

/// Cache key for a rendered diagram: content hash, theme, quality tier, and (for the terminal tier) bucketed width.
/// Keys the rendered-PNG cache. Theme, quality, and width are part of the key.
/// A theme switch, resize, or open-vs-terminal tier is then a lookup (usually a hit) or a fresh render, never a stale-color/size diagram.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MermaidCacheKey {
    /// SHA-256 hash of the diagram source.
    pub source_hash: [u8; 32],
    /// Active theme (its surface color is baked into the rendered diagram).
    pub theme: ThemeKind,
    /// Target render width quantized to [`MERMAID_WIDTH_BUCKET`] columns for [`MermaidRenderQuality::Terminal`].
    /// [`OPEN_QUALITY_WIDTH_BUCKET`] for [`MermaidRenderQuality::Open`].
    pub width_bucket: u16,
    /// Terminal-budget vs OS-viewer quality tier.
    pub quality: MermaidRenderQuality,
}

impl MermaidCacheKey {
    /// Derive a cache key from a diagram's source, the active theme, target render width, and quality tier.
    /// Width is in display columns and ignored for [`MermaidRenderQuality::Open`].
    pub fn derive(
        source: &str,
        theme: ThemeKind,
        target_width_cols: u16,
        quality: MermaidRenderQuality,
    ) -> Self {
        let width_bucket = match quality {
            MermaidRenderQuality::Terminal => width_bucket(target_width_cols),
            MermaidRenderQuality::Open => OPEN_QUALITY_WIDTH_BUCKET,
        };
        Self {
            source_hash: hash_source(source),
            theme,
            width_bucket,
            quality,
        }
    }

    /// Stable, filesystem-safe filename for this key's on-disk PNG.
    /// The name carries the content hash, theme, width bucket, quality tag, and render revision.
    /// The same diagram at the same theme/width/tier reuses one file and never leaks the source in the name.
    pub fn cache_filename(&self) -> String {
        use std::fmt::Write as _;
        let mut name = String::with_capacity(64 + 24);
        for byte in self.source_hash {
            let _ = write!(name, "{byte:02x}");
        }
        let quality_tag = match self.quality {
            MermaidRenderQuality::Terminal => "t",
            MermaidRenderQuality::Open => "o",
        };
        let _ = write!(
            name,
            "-{}-{}-{}-r{RENDER_REVISION}.png",
            self.theme as u8, self.width_bucket, quality_tag
        );
        name
    }
}

/// Render-pipeline revision baked into [`MermaidCacheKey::cache_filename`].
/// Bump whenever the renderer's output changes for the same source/theme/width/tier.
const RENDER_REVISION: u8 = 3;

/// Detected Mermaid diagrams for one agent message. A detection skeleton: it records detection results and exposes
/// each diagram's source, but never renders and tracks no per-diagram render state. Constructed once at message
/// construction/finish (never per streaming chunk), like the image/video references.
#[derive(Debug, Clone, Default)]
pub struct MermaidContent {
    blocks: Vec<MermaidBlock>,
}

impl MermaidContent {
    /// Detect Mermaid blocks in a finished render view.
    pub fn from_view(view: &MarkdownRenderView) -> Self {
        Self {
            blocks: mermaid_blocks(view),
        }
    }

    /// Whether the message contains any Mermaid diagrams.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Number of detected diagrams.
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// Diagram source at `idx`, if it exists.
    pub fn source(&self, idx: usize) -> Option<&str> {
        self.blocks.get(idx).map(|b| b.source.as_str())
    }
}

/// How a detected Mermaid block's affordance row is presented.
/// The diagram itself is always drawn inline as Unicode art by the markdown renderer.
/// The rendered PNG is never inline (it is reached only through the affordance row).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MermaidDisplay {
    /// The inline diagram art alone, with no affordance row (`render_mermaid = off`).
    SourceOnly,
    /// The inline diagram art plus the clickable affordance row (`◇ mermaid [Open Image] [Copy Image Path] [Copy Source]`): `auto`/`on`.
    Affordances,
}

/// The render engine is always compiled in, so engine availability is not a factor. Terminal graphics capability is
/// intentionally not consulted either: the affordance row is text plus mouse hit-rects, so it works everywhere.
/// (The rendered PNG opens in the OS viewer, never inline.).
pub fn mermaid_display(setting: RenderMermaid) -> MermaidDisplay {
    match setting {
        RenderMermaid::Off => MermaidDisplay::SourceOnly,
        RenderMermaid::Auto | RenderMermaid::On => MermaidDisplay::Affordances,
    }
}

/// The clickable affordance row is painted by the interactive draw loop, which minimal never runs. It would commit
/// as a blank reserved line and its buttons would be inert. Suppressing it keeps the inline diagram art (the source
/// stays natively selectable) without the dead row.
pub fn mermaid_display_static(setting: RenderMermaid, static_commit: bool) -> MermaidDisplay {
    if static_commit {
        MermaidDisplay::SourceOnly
    } else {
        mermaid_display(setting)
    }
}

/// Which click action an affordance-row button triggers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AffordanceKind {
    /// Render the diagram (if not already cached) at the live theme/width, then open the resulting PNG in the OS default app.
    Open,
    /// Render the diagram (if not already cached), then copy the PNG's path.
    CopyPath,
    /// Copy the diagram's Mermaid source (no render needed).
    CopySource,
}

/// One button in a diagram's affordance row, with its start column so the painted label and the click hit-rect can't drift.
/// Every button is always clickable; `[Open]`/`[Copy path]` render lazily on click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AffordanceButton {
    /// Display label, e.g. `[Open]`.
    pub label: &'static str,
    /// The click action this button triggers.
    pub kind: AffordanceKind,
    /// Start column, in display cells from the affordance row's left edge.
    pub col: u16,
}

/// The full affordance-row layout: the leading `◇ mermaid` label, the three buttons (with columns), and the trailing status hint (with column).
/// The painter and the click hit-rects draw from one source of truth and can't drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AffordanceRow {
    /// `(start_col, text)` of the leading dim, non-clickable `◇ mermaid` label.
    pub label: (u16, &'static str),
    /// `[Open Image] [Copy Image Path] [Copy Source]`, left-to-right with their columns (shifted right past the leading label).
    pub buttons: [AffordanceButton; 3],
    /// `(start_col, text)` of the trailing `rendering…` hint, present only while an on-click render for this diagram is in flight.
    pub status: Option<(u16, &'static str)>,
}

/// The affordance row's three buttons laid out left-to-right starting at `start_col` (which leaves room for the leading `◇ mermaid` label).
fn affordance_buttons(start_col: u16) -> [AffordanceButton; 3] {
    let specs = [
        (AFFORDANCE_OPEN, AffordanceKind::Open),
        (AFFORDANCE_COPY_PATH, AffordanceKind::CopyPath),
        (AFFORDANCE_COPY_SOURCE, AffordanceKind::CopySource),
    ];
    let mut col = start_col;
    specs.map(|(label, kind)| {
        let button = AffordanceButton { label, kind, col };
        col += UnicodeWidthStr::width(label) as u16 + AFFORDANCE_GAP;
        button
    })
}

/// The whole affordance-row layout for a diagram.
/// The leading `◇ mermaid` label, the three (always-clickable) buttons shifted past it, and the trailing `rendering…` hint when `rendering` is true.
/// One source of truth shared by the painter and hit-testing, so the painted columns and click hit-rects align.
pub fn affordance_row(rendering: bool) -> AffordanceRow {
    let buttons_start = UnicodeWidthStr::width(MERMAID_LABEL) as u16 + AFFORDANCE_GAP;
    let buttons = affordance_buttons(buttons_start);
    let status = rendering.then(|| buttons.last()).flatten().map(|last| {
        let after = last.col + UnicodeWidthStr::width(last.label) as u16 + AFFORDANCE_GAP;
        (after, MERMAID_RENDERING)
    });
    AffordanceRow {
        label: (0, MERMAID_LABEL),
        buttons,
        status,
    }
}

/// A diagram's clickable affordance row, anchored within a block's output.
/// Carries no raster, only the row position plus the diagram source the affordance buttons act on.
/// Rendering is lazy, driven from the source on click, so no rendered path is tracked here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagramAffordance {
    /// Post-wrap, block-relative row offset of the affordance row (its index in the block's `output()` lines).
    pub row_offset: u16,
    /// Diagram source (the fence body); the data every button acts on.
    pub source: String,
}

/// Post-wrap end row of every pre-wrap line, indexed by pre-wrap line number. It can't drift from the media-row /
/// hyperlink mappings.
fn prewrap_end_rows(lines: &[BlockLine]) -> Vec<usize> {
    let mut ends = Vec::new();
    for (row, prewrap) in crate::ui::scrollback::types::prewrap_index_per_row(lines)
        .into_iter()
        .enumerate()
    {
        if let Some(slot) = ends.get_mut(prewrap) {
            *slot = row + 1;
        } else {
            ends.push(row + 1);
        }
    }
    ends
}

/// Row in `lines` at which a continuation row sits right after each non-empty pre-wrap range's last body row.
/// Callers insert back-to-front without invalidating earlier positions.
fn diagram_insert_rows(lines: &[BlockLine], ranges: &[Range<usize>]) -> Vec<(usize, usize)> {
    let ends = prewrap_end_rows(lines);
    ranges
        .iter()
        .enumerate()
        .filter(|(_, range)| !range.is_empty())
        .filter_map(|(idx, range)| ends.get(range.end - 1).map(|&insert_at| (insert_at, idx)))
        .collect()
}

/// A non-selectable continuation row inserted beneath a diagram.
/// `separator` means not selectable (excluded from copy).
/// The empty joiner marks it a continuation of the diagram's last logical line, so the pre-wrap to post-wrap walk for hyperlinks is unaffected.
fn continuation_row(line: Line<'static>) -> BlockLine {
    BlockLine::separator(line).with_joiner(Some(String::new()))
}

/// Insert a blank, non-selectable affordance row beneath each detected diagram and return one [`DiagramAffordance`]
/// per inserted row (document order). So it neither shifts pre-wrap line indices nor reaches the clipboard.
pub(crate) fn apply_affordance_rows(
    output: &mut BlockOutput,
    prewrap_ranges: &[Range<usize>],
    mut source_for: impl FnMut(usize) -> String,
) -> Vec<DiagramAffordance> {
    let inserts = diagram_insert_rows(&output.lines, prewrap_ranges);
    let affordances: Vec<DiagramAffordance> = inserts
        .iter()
        .enumerate()
        .map(|(k, &(insert_at, idx))| DiagramAffordance {
            row_offset: (insert_at + k) as u16,
            source: source_for(idx),
        })
        .collect();
    for &(insert_at, _) in inserts.iter().rev() {
        output
            .lines
            .insert(insert_at, continuation_row(Line::from(String::new())));
    }
    affordances
}
