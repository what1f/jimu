use std::borrow::Cow;
use std::cell::OnceCell;
use std::path::Path;

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;

use crate::engine::render::color::blend_color;
use crate::engine::render::{Renderable, SafeBuf};
use crate::engine::theme::{self, Theme};
use crate::ui::scrollback::BlockOutput;
use crate::ui::scrollback::appearance::AppearanceConfig;
use crate::ui::scrollback::block::{BlockContent, RenderBlock};
use crate::ui::scrollback::entry::ScrollbackEntry;
use crate::ui::scrollback::layout::HorizontalLayout;
use crate::ui::scrollback::types::{AccentStyle, BlockBackground, DisplayMode, Selectable};

/// Animation speed for running blocks (radians per tick).
/// ~0.15 gives a smooth wave that travels the block in ~40 ticks.
const WAVE_SPEED: f32 = 0.15;

pub struct EntryRenderer<'a> {
    entry: &'a ScrollbackEntry,
    theme: &'a Theme,
    /// Deliberately NOT an eager `AppearanceConfig::default()`: that conversion reads `Theme::current()` and quantizes every color.
    /// Profiling a resize showed it running once per entry only to be overwritten.
    appearance: OnceCell<Cow<'a, AppearanceConfig>>,
    tick: u64,
    /// Number of rows to skip from the top of the entry.
    /// When non-zero, the renderer acts as if the entry starts `skip_rows` rows lower, omitting that many top rows (vpad, then content lines).
    /// The remainder renders into `area`; this eliminates scratch-buffer rendering of partially-visible entries.
    skip_rows: u16,
    /// Whether this entry's block is groupable (participates in dense groups).
    /// When true AND display_mode == Collapsed, the bullet is dimmed.
    groupable: bool,
    /// Whether this entry is currently selected in the scrollback.
    is_selected: bool,
    /// Mouse position for timestamp hover detection.
    mouse_pos: Option<(u16, u16)>,
    /// When non-zero, this entry renders as a group truncation header ("╶╶ N more") instead of its normal block content.
    group_header_count: u16,
    /// When true, this is a collapse header for an expanded group ("▾ N tool calls" instead of "╶╶ N more").
    group_collapse_header: bool,
    /// Aggregated group-header label; when set, group-header rows render it instead of the plain "N more" / "N tool calls" text.
    /// The variant picks the chrome: verb-run headers wear running/error accents from the run state, truncation headers keep the dimmed fold chrome.
    group_header_label: Option<&'a crate::ui::scrollback::state::verb_group::GroupHeaderLabel>,
    /// When true, suppress the block's background band (force `BlockBackground::None`) so the entry blends with the
    /// terminal's own background.
    flat_background: bool,
    /// When true, reclaim the accent column for content (chrome width drops by [`HorizontalLayout::ACCENT`]).
    /// Minimal mode pairs this with zeroed `block_pad_{left,right}`, so content starts at column 0, aligned with the welcome card.
    hide_accent: bool,
    /// Paint the accent bar with [`Modifier::DIM`] on top of its color.
    /// A rail that resolved to `Color::Reset` then reads as chrome rather than full-brightness content.
    dim_accent: bool,
    /// Session/worktree cwd (`AgentSession.cwd`) for Expanded tool paths.
    cwd: Option<&'a Path>,
}

impl<'a> EntryRenderer<'a> {
    pub fn new(entry: &'a ScrollbackEntry, theme: &'a Theme) -> Self {
        Self {
            entry,
            theme,
            appearance: OnceCell::new(),
            tick: 0,
            skip_rows: 0,
            groupable: false,
            is_selected: false,
            mouse_pos: None,
            group_header_count: 0,
            group_collapse_header: false,
            group_header_label: None,
            flat_background: false,
            hide_accent: false,
            dim_accent: false,
            cwd: None,
        }
    }

    pub fn with_cwd(mut self, cwd: Option<&'a Path>) -> Self {
        self.cwd = cwd;
        self
    }

    /// Suppress the block background band so the entry blends with the terminal's own background (minimal mode).
    /// See [`Self::flat_background`].
    pub fn with_flat_background(mut self, flat: bool) -> Self {
        self.flat_background = flat;
        self
    }

    /// Reclaim the accent column for content (minimal mode's flush-left look). See [`Self::hide_accent`].
    pub fn with_hide_accent(mut self, hide: bool) -> Self {
        self.hide_accent = hide;
        self
    }

    /// See [`Self::dim_accent`].
    /// Height-neutral: `chrome_width` is unchanged.
    pub fn with_dim_accent(mut self, dim: bool) -> Self {
        self.dim_accent = dim;
        self
    }

    /// Background to paint where the block itself has none (accent column, gutter, bullets).
    /// In flat mode this is `Color::Reset`, the terminal's default background, so the entry stays transparent instead of an opaque `bg_base` strip.
    /// Otherwise it is `bg_base`.
    fn fallback_bg(&self) -> ratatui::style::Color {
        if self.flat_background {
            ratatui::style::Color::Reset
        } else {
            self.theme.bg_base
        }
    }

    /// The block's rail style, resolved through a full context so selection and width reach it.
    fn accent(&self, content_width: u16) -> Option<AccentStyle> {
        let mut ctx = self
            .entry
            .context(content_width, self.appearance(), self.cwd);
        ctx.is_selected = self.is_selected;
        self.entry.block.accent(&ctx)
    }

    /// Shared by every rail branch so it cannot be dim in one running state and bright in another.
    fn accent_paint_style(&self, color: ratatui::style::Color) -> Style {
        let style = Style::default().fg(color);
        if self.dim_accent {
            style.add_modifier(ratatui::style::Modifier::DIM)
        } else {
            style
        }
    }

    pub fn with_appearance(mut self, appearance: AppearanceConfig) -> Self {
        self.appearance = OnceCell::from(Cow::Owned(appearance));
        self
    }

    /// Preferred inside the O(history) layout loops, which would otherwise clone the config once per entry.
    pub fn with_appearance_ref(mut self, appearance: &'a AppearanceConfig) -> Self {
        self.appearance = OnceCell::from(Cow::Borrowed(appearance));
        self
    }

    fn appearance(&self) -> &AppearanceConfig {
        self.appearance
            .get_or_init(|| Cow::Owned(AppearanceConfig::default()))
    }

    pub fn with_tick(mut self, tick: u64) -> Self {
        self.tick = tick;
        self
    }

    /// Skip the first `n` rows of the entry when rendering.
    /// The skipped rows consume vpad first, then content lines.
    /// This allows partially-visible entries to be rendered directly into the output buffer without a scratch buffer intermediate.
    pub fn with_skip_rows(mut self, n: u16) -> Self {
        self.skip_rows = n;
        self
    }

    /// Mark this entry as groupable, which dims its bullet when collapsed.
    pub fn with_groupable(mut self, groupable: bool) -> Self {
        self.groupable = groupable;
        self
    }

    pub fn with_selected(mut self, selected: bool) -> Self {
        self.is_selected = selected;
        self
    }

    /// Set the mouse position for timestamp hover detection.
    pub fn with_mouse_pos(mut self, pos: Option<(u16, u16)>) -> Self {
        self.mouse_pos = pos;
        self
    }

    /// When non-zero, the renderer draws a compact "╶╶ N more" line instead of the block's normal content.
    pub fn with_group_header_count(mut self, count: u16) -> Self {
        self.group_header_count = count;
        self
    }

    pub fn with_group_collapse_header(mut self, collapse: bool) -> Self {
        self.group_collapse_header = collapse;
        self
    }

    /// Set the aggregated label for a group-header row (either fold family).
    pub fn with_group_header_label(
        mut self,
        label: Option<&'a crate::ui::scrollback::state::verb_group::GroupHeaderLabel>,
    ) -> Self {
        self.group_header_label = label;
        self
    }

    /// Render a compact "╶╶ N more" group header line.
    /// Uses the collapsed accent char for the accent column, and renders the header text with dimmed styling to separate it from real entries.
    /// Avoids the full `self.accent()` path (which allocates a `BlockContext`) by reading the accent color directly from the theme.
    fn render_group_header(&self, area: Rect, buf: &mut Buffer) {
        use crate::ui::scrollback::state::verb_group::GroupHeaderLabel;

        let layout_cfg = &self.appearance().scrollback.layout;
        let accent_w = if self.hide_accent {
            0
        } else {
            HorizontalLayout::ACCENT
        };
        let [accent_area, _left_pad, content_area, _right_pad] = Layout::horizontal([
            Constraint::Length(accent_w),
            Constraint::Length(layout_cfg.block_pad_left),
            Constraint::Min(1),
            Constraint::Length(layout_cfg.block_pad_right),
        ])
        .areas(area);

        let bg = self.theme.bg_base;

        // The column is reserved but never painted here, and an unowned cell survives the frame diff
        // Whatever glyph the previous frame left at this position would stay
        fill_bg_spaces(buf, accent_area, bg);

        // Verb-group header: an aggregated "Verb N noun" label whose diamond takes the run-state color
        // An active group's glyph animates with the same wave as a running tool row's bullet
        if let Some(GroupHeaderLabel::VerbRun(vg)) = self.group_header_label {
            let glyph_color = if vg.failed {
                self.theme.accent_error
            } else if vg.running {
                let brightness = theme::wave_brightness(
                    self.tick,
                    self.skip_rows,
                    self.appearance().animation.wave_rows,
                    WAVE_SPEED,
                );
                blend_color(bg, self.theme.accent_tool, brightness)
                    .unwrap_or(self.theme.accent_tool)
            } else {
                self.theme.gray
            };

            // Diamond chrome in BOTH states, same family as the "N more" headers.
            // The selection caret (the expandable indicator in scrollback_pane.rs) overdraws the diamond on the selected row
            // It flips `›`/`⌄` with the group's fold state
            let prefix = group_header_chrome_prefix();
            let mut spans = vec![ratatui::text::Span::styled(
                prefix,
                Style::default().fg(glyph_color),
            )];
            spans.extend(vg.line.spans.iter().cloned());
            let line = ratatui::text::Line::from(spans);
            // Group-header content is registered selectable (GROUP_HEADER_RANGE_ID)
            // Its selection maps visual columns, so it must paint visual too
            buf.set_line_safe_bidi(content_area.x, content_area.y, &line, content_area.width);
            return;
        }

        // Render header: a dimmed ◈, then brighter text that stands out
        // The aggregated label describes the hidden rows through the shared bucket vocabulary ("Ran 6 commands")
        // When the caller supplied none (the render loop owns the reasons), the plain count remains
        let n = self.group_header_count;
        let diamond_style = Style::default().fg(self.theme.gray);
        let text_style = Style::default()
            .fg(self.theme.gray_bright)
            .add_modifier(ratatui::style::Modifier::BOLD);
        let mut spans = vec![ratatui::text::Span::styled(
            group_header_chrome_prefix(),
            diamond_style,
        )];
        // The VerbRun variant returned above, so only a truncation label can reach this row's span assembly
        if let Some(GroupHeaderLabel::Truncation(label)) = self.group_header_label {
            spans.extend(label.line.spans.iter().cloned());
        } else {
            let label = if self.group_collapse_header {
                format!("{n} tool calls & thoughts")
            } else {
                format!("{n} more")
            };
            spans.push(ratatui::text::Span::styled(label, text_style));
        }
        let line = ratatui::text::Line::from(spans);
        // Group-header content is registered selectable (GROUP_HEADER_RANGE_ID)
        // Its selection maps visual columns, so it must paint visual too
        buf.set_line_safe_bidi(content_area.x, content_area.y, &line, content_area.width);
    }

    /// Get the chrome width (accent + padding) for this renderer's appearance.
    ///
    /// When [`Self::hide_accent`] is set the accent column is reclaimed, so chrome is just the block pads (typically zeroed in minimal mode).
    pub fn chrome_width(&self) -> u16 {
        let pads = self.appearance().scrollback.layout.block_pad_left
            + self.appearance().scrollback.layout.block_pad_right;
        if self.hide_accent {
            pads
        } else {
            HorizontalLayout::ACCENT + pads
        }
    }

    /// Legacy fixed chrome-width estimate for callers with no appearance to borrow (off-screen mermaid sizing); the live value is `chrome_width()`.
    pub const CHROME_WIDTH: u16 = 1 + 2 + 1; // accent + left_pad + right_pad (legacy)

    /// Whether this entry should display a timestamp on the first content line.
    /// Timestamps are shown for user and agent messages (including /btw responses and mid-turn interjections).
    /// Thinking traces, tool calls, and system messages get none.
    fn should_show_timestamp(&self) -> bool {
        matches!(
            self.entry.block,
            RenderBlock::UserPrompt(_) | RenderBlock::AgentMessage(_) | RenderBlock::Btw(_)
        )
    }

    /// Width reserved for the timestamp on the right side of content lines.
    ///
    /// When non-zero, content is wrapped at `content_width - reserved` so text never collides with the timestamp overlay.
    fn timestamp_reserved(&self) -> u16 {
        if self.appearance().show_timestamps && self.should_show_timestamp() {
            10 // max short format: "  12:30 PM"
        } else {
            0
        }
    }

    /// Thinking entries take no rows when the Appearance toggle is off.
    fn thinking_hidden(&self) -> bool {
        self.entry
            .is_hidden_thinking(self.appearance().show_thinking_blocks)
    }

    /// Compute height as if displayed in Truncated mode. Goes through the entry's truncated-height cache so repeated
    /// layout rebuilds don't re-run `block.output()`.
    pub fn compute_truncated_height(&self, width: u16) -> u16 {
        if self.thinking_hidden() {
            return 0;
        }
        let content_width = width
            .saturating_sub(self.chrome_width())
            .saturating_sub(self.timestamp_reserved());
        self.entry
            .ensure_truncated_height_cached(content_width, self.appearance(), self.cwd)
    }

    /// Extra rows to reserve for inline media preview (images/video poster).
    /// Returns 0 on non-graphics terminals or when the block has no media.
    fn inline_media_rows(&self, content_width: u16) -> u16 {
        use crate::ui::scrollback::media::scrollback_inline_overlay_active;

        if !scrollback_inline_overlay_active() {
            return 0;
        }
        let Some(media) = self.entry.block.inline_media() else {
            return 0;
        };
        // Shared with `inline_media_placements` so the reserved rows match the painted placement exactly
        let (_image_rows, total_rows) =
            crate::ui::scrollback::media::inline_media_reserved_rows(&media, content_width);
        total_rows
    }

    /// Mirrors `desired_height` but derives the content line count from the block's raw source text (`searchable_text`)
    /// instead of laying it out. On-screen entries get their EXACT `desired_height`, so visible content is never
    /// estimated.
    pub fn estimate_height(&self, width: u16) -> u16 {
        if self.thinking_hidden() {
            return 0;
        }
        let content_width = width
            .saturating_sub(self.chrome_width())
            .saturating_sub(self.timestamp_reserved());
        let content_lines = self.estimate_content_lines(content_width);
        // `inline_media_rows` (in `assemble_height`) covers trailing tool media.
        // `estimate_extra_rows` adds the Mermaid treatment rows (one affordance row or fallback caption per diagram)
        // Those live inside `output()` and are invisible to this source-based estimate, so it never under-reserves
        self.assemble_height(content_width, content_lines)
            .saturating_add(self.entry.block.estimate_extra_rows(self.appearance()))
    }

    /// Estimate the rendered content-line count without laying the block out.
    /// Memoized per content width on the entry, cleared by `invalidate_cache`.
    /// A full layout rebuild at the same width (e.g. after a fold or group-expand) then doesn't re-clone every entry's source text.
    fn estimate_content_lines(&self, content_width: u16) -> u16 {
        if let Some(lines) = self.entry.cached_estimate_lines(content_width) {
            return lines;
        }
        // Collapsed / Truncated foldable entries render a compact ~1-line header, NOT their (often huge) hidden body
        // Use the ENTRY-level foldability, matching the fold path
        let lines = if self.entry.display_mode != DisplayMode::Expanded && self.entry.is_foldable()
        {
            1
        } else {
            self.entry.estimate_source_lines(content_width)
        };
        self.entry.store_estimate_lines(content_width, lines);
        lines
    }

    /// Combine a content-line count with this entry's vpad + inline-media rows.
    /// Saturating throughout so a multi-MB block whose line count hits the u16 ceiling can't overflow and corrupt `virtual_y` / `total_height`.
    /// Shared by `desired_height` and `estimate_height` so the assembly stays canonical.
    fn assemble_height(&self, content_width: u16, content_lines: u16) -> u16 {
        let vpad: u16 = if self.entry.block.has_vpad_for(self.appearance()) {
            2
        } else {
            0
        };
        content_lines
            .saturating_add(vpad)
            .saturating_add(self.inline_media_rows(content_width))
    }

    /// The forward ([`rendered_row_of_logical_line`]) and inverse ([`logical_line_of_rendered_row`]) derive from one
    /// predicate and can't drift. They still occupy rows, so the full-enumeration index is used.
    pub(crate) fn logical_line_start_rows(&self, width: u16) -> (Vec<u16>, u16) {
        let content_width = width
            .saturating_sub(self.chrome_width())
            .saturating_sub(self.timestamp_reserved());
        // Compute vpad and populate the cache before borrowing the cached output
        // `ensure_cached` takes the RefCell mutably on a miss, so the `Ref` from `cached_output_ref` must come after it
        let ctx = self
            .entry
            .context(content_width, self.appearance(), self.cwd);
        let vpad_top: u16 = if self.entry.block.has_vpad(&ctx) {
            1
        } else {
            0
        };
        self.entry
            .ensure_cached(content_width, self.appearance(), false, self.cwd);
        let output = self.entry.cached_output_ref();
        let starts = output
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| {
                !matches!(line.selectable, Selectable::None) && line.joiner.is_none()
            })
            .map(|(idx, _)| vpad_top.saturating_add(u16::try_from(idx).unwrap_or(u16::MAX)))
            .collect();
        let last_content_row = vpad_top.saturating_add(
            u16::try_from(output.lines.len().saturating_sub(1)).unwrap_or(u16::MAX),
        );
        (starts, last_content_row)
    }

    /// Per painted buffer row: WRAPLINE only when the next `BlockLine` is a mid-word
    /// soft wrap (`joiner == Some("")`). Word (`" "`) and hard (`"\n"`) joiners stay
    /// `false` so native copy keeps those separators.
    ///
    /// Exact-width hard breaks stay `false`. Last-column occupancy is not a wrap signal.
    pub fn row_soft_wraps(&self, height: u16) -> Vec<bool> {
        let mut flags = vec![false; usize::from(height)];
        if height == 0 || (self.group_header_count > 0 && !self.group_collapse_header) {
            return flags;
        }
        let output = self.entry.cached_output_ref();
        let vpad_top = u16::from(self.entry.block.has_vpad_for(self.appearance()));
        let (header_rows, skip_remaining) = if self.group_collapse_header {
            if self.skip_rows == 0 {
                (1u16, 0u16)
            } else {
                (0u16, self.skip_rows.saturating_sub(1))
            }
        } else {
            (0u16, self.skip_rows)
        };
        let vpad_top_visible = skip_remaining < vpad_top;
        let content_skip = skip_remaining.saturating_sub(vpad_top);
        let start_y = header_rows.saturating_add(u16::from(vpad_top_visible));

        let mut painted = 0u16;
        for (i, line) in output.lines.iter().enumerate() {
            if i < usize::from(content_skip) {
                continue;
            }
            if painted > 0 && line.joiner.as_deref().is_some_and(str::is_empty) {
                let wrap_y = start_y.saturating_add(painted.saturating_sub(1));
                let cont_y = start_y.saturating_add(painted);
                if cont_y < height
                    && let Some(flag) = flags.get_mut(usize::from(wrap_y))
                {
                    *flag = true;
                }
            }
            painted = painted.saturating_add(1);
            if start_y.saturating_add(painted) >= height {
                break;
            }
        }
        flags
    }

    /// The rendered-row offset (from the entry's top, including any top vpad row) where the search index's
    /// `logical_line`-th line begins at `width`. Past the last logical line, clamps to the final content row.
    pub fn rendered_row_of_logical_line(&self, width: u16, logical_line: usize) -> u16 {
        let (starts, last_content_row) = self.logical_line_start_rows(width);
        starts
            .get(logical_line)
            .copied()
            .unwrap_or(last_content_row)
    }

    /// Inverse of [`rendered_row_of_logical_line`]. Shares `logical_line_start_rows` with that method so the two
    /// provably round-trip.
    pub fn logical_line_of_rendered_row(&self, width: u16, row: u16) -> usize {
        let (starts, _) = self.logical_line_start_rows(width);
        // `starts` is ascending: the count of starts at or before `row`, minus 1, is the 0-based index of the logical line containing `row`
        starts
            .partition_point(|&start| start <= row)
            .saturating_sub(1)
    }
}

/// Diamond chrome prefix every group header draws before its text: verb-run labels, truncation labels, and plain counts alike, in both fold states.
/// Selection geometry for labeled headers derives from this same string (see [`group_header_chrome_prefix_width`]) so render and hitbox can't drift.
pub(crate) fn group_header_chrome_prefix() -> String {
    format!("{} ", crate::engine::render::glyphs::diamond_dotted())
}

/// Display width of [`group_header_chrome_prefix`].
pub(crate) fn group_header_chrome_prefix_width() -> u16 {
    unicode_width::UnicodeWidthStr::width(group_header_chrome_prefix().as_str()) as u16
}

/// Fill a region with background-styled spaces so the frame owns these cells;
/// the cell diff won't repaint an unowned cell, so a stale glyph would persist.
fn fill_bg_spaces(buf: &mut Buffer, rect: Rect, bg: ratatui::style::Color) {
    let style = Style::default().bg(bg);
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_char(' ');
                cell.set_style(style);
            }
        }
    }
}

impl Renderable for EntryRenderer<'_> {
    fn desired_height(&self, width: u16) -> u16 {
        if self.thinking_hidden() {
            return 0;
        }
        let content_width = width
            .saturating_sub(self.chrome_width())
            .saturating_sub(self.timestamp_reserved());
        // Use cached output for height calculation
        // The is_selected flag only affects styling (e.g., UserPrompt prefix color), not line count
        // The non-selected cached output therefore gives the correct height
        self.entry
            .ensure_cached(content_width, self.appearance(), false, self.cwd);
        // Clamp the line count: a pathologically large block could exceed u16.
        let content_lines = self.entry.cached_output_ref().len().min(u16::MAX as usize) as u16;
        self.assemble_height(content_width, content_lines)
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        let chrome = self.chrome_width();
        if area.width < chrome + 1 || area.height == 0 || self.thinking_hidden() {
            return;
        }

        // Expand header ("╶╶ N more"): replaces block content entirely.
        // Only truncated (non-expanded) groups take this path
        if self.group_header_count > 0 && !self.group_collapse_header {
            self.render_group_header(area, buf);
            return;
        }

        // Collapse header ("▾ N tool calls & thoughts"): a standalone header entry (height=1) that replaces the first group entry's content
        // The remaining group entries keep their normal heights, giving the header its own selectable index with independent interaction
        let (area, skip_rows) = if self.group_collapse_header {
            if self.skip_rows == 0 {
                // Header is visible: render it in the first row
                let header_area = Rect::new(area.x, area.y, area.width, 1);
                self.render_group_header(header_area, buf);
                let remaining_height = area.height.saturating_sub(1);
                if remaining_height == 0 {
                    return;
                }
                let remaining = Rect::new(area.x, area.y + 1, area.width, remaining_height);
                (remaining, 0u16)
            } else {
                // Header is scrolled off: reduce skip_rows by 1 for content
                (area, self.skip_rows - 1)
            }
        } else {
            (area, self.skip_rows)
        };

        let layout_cfg = &self.appearance().scrollback.layout;
        // Minimal (`hide_accent`): reclaim the accent column so content is flush-left
        // Fullscreen keeps the 1-col gutter even when a block has no painted accent (so columns stay aligned across entry types)
        let accent_w = if self.hide_accent {
            0
        } else {
            HorizontalLayout::ACCENT
        };
        let [accent_area, left_pad, content_area, right_pad] = Layout::horizontal([
            Constraint::Length(accent_w),
            Constraint::Length(layout_cfg.block_pad_left),
            Constraint::Min(1),
            Constraint::Length(layout_cfg.block_pad_right),
        ])
        .areas(area);

        // Build context directly: we only need it for background/accent decisions, not the output
        // This avoids a full block.output() call (including syntax highlighting for edit blocks) that was previously thrown away
        let mut ctx = self
            .entry
            .context(content_area.width, self.appearance(), self.cwd);
        ctx.is_selected = self.is_selected;
        // Minimal mode blends committed/tail blocks with the real terminal background
        // Suppress the block's own band; accents and per-line code shading stay
        let bg = if self.flat_background {
            BlockBackground::None
        } else {
            self.entry.block.background(&ctx)
        };

        // Fill padding with background if block has a background
        let bg_color = match bg {
            BlockBackground::None => None,
            BlockBackground::Light => Some(self.theme.bg_light),
            BlockBackground::Dark => Some(self.theme.bg_dark),
        };

        if let Some(bg_color) = bg_color {
            let bg_style = Style::default().bg(bg_color);

            // Fill the entire visible area (already accounts for skip_rows via area sizing)
            let full_start_y = area.y;
            let full_end_y = area.y + area.height;

            // Fill accent area if block wants accent background
            if self.entry.block.accent_background(&ctx) {
                for y in full_start_y..full_end_y {
                    for x in accent_area.x..accent_area.x + accent_area.width {
                        if let Some(cell) = buf.cell_mut((x, y)) {
                            cell.set_style(bg_style);
                        }
                    }
                }
            }

            // Fill left padding
            for y in full_start_y..full_end_y {
                for x in left_pad.x..left_pad.x + left_pad.width {
                    if let Some(cell) = buf.cell_mut((x, y)) {
                        cell.set_style(bg_style);
                    }
                }
            }

            // Fill content area
            for y in full_start_y..full_end_y {
                for x in content_area.x..content_area.x + content_area.width {
                    if let Some(cell) = buf.cell_mut((x, y)) {
                        cell.set_style(bg_style);
                    }
                }
            }

            // Fill right padding
            for y in full_start_y..full_end_y {
                for x in right_pad.x..right_pad.x + right_pad.width {
                    if let Some(cell) = buf.cell_mut((x, y)) {
                        cell.set_style(bg_style);
                    }
                }
            }
        }

        // Briefly flashed when a tool call or thinking block finishes
        // Read/Search/Edit carry no accent of their own, so the flash falls back to green
        let recently_finished = !self.entry.is_running
            && self.entry.finished_at.is_some_and(|t| {
                t.elapsed().as_millis()
                    < crate::ui::scrollback::state::FINISH_FLASH_DURATION_MS as u128
            })
            && matches!(
                self.entry.block,
                RenderBlock::ToolCall(_) | RenderBlock::Thinking(_)
            );

        // Collapsed rows are one-line summaries, so a rail beside them is noise, flash included
        // Truncated counts as open: it shows real content, and thinking blocks rest in that state
        let accent = (!self.hide_accent && self.entry.display_mode != DisplayMode::Collapsed)
            .then(|| {
                if recently_finished {
                    let color = self
                        .entry
                        .block
                        .accent_color()
                        .unwrap_or(self.theme.accent_success);
                    Some(AccentStyle::static_color(color))
                } else {
                    self.accent(content_area.width)
                }
            })
            .flatten();

        // The column stays reserved either way, so nothing reflows
        // Clear it or a previous frame bleeds through
        if accent.is_none() {
            fill_bg_spaces(buf, accent_area, bg_color.unwrap_or(self.fallback_bg()));
        }

        if let Some(accent_style) = accent {
            let color = accent_style.color;

            if self.entry.is_pending_user_input && accent_style.animated {
                // Pending user input freezes the wave: a solid rail reads as "paused on you" without the spinner motion.
                let style = self.accent_paint_style(color);
                for y in accent_area.y..accent_area.y + accent_area.height {
                    buf.set_string_safe(
                        accent_area.x,
                        y,
                        crate::engine::render::glyphs::accent_bar(),
                        style,
                    );
                }
            } else if accent_style.animated {
                let bg = bg_color.unwrap_or(self.fallback_bg());
                let wave_rows = self.appearance().animation.wave_rows;

                for row in 0..accent_area.height {
                    let y = accent_area.y + row;
                    let brightness =
                        theme::wave_brightness(self.tick, skip_rows + row, wave_rows, WAVE_SPEED);
                    let animated_color = blend_color(bg, color, brightness).unwrap_or(color);
                    let style = self.accent_paint_style(animated_color);
                    buf.set_string_safe(
                        accent_area.x,
                        y,
                        crate::engine::render::glyphs::accent_bar(),
                        style,
                    );
                }
            } else {
                let style = self.accent_paint_style(color);
                for y in accent_area.y..accent_area.y + accent_area.height {
                    buf.set_string_safe(
                        accent_area.x,
                        y,
                        crate::engine::render::glyphs::accent_bar(),
                        style,
                    );
                }
            }
        }

        // Render content using cache (keyed on is_selected for blocks like UserPrompt that adjust styling based on selection state)
        // When timestamps are enabled, wrap content at a narrower width so text never collides with the right-aligned timestamp
        let ts_reserved = self.timestamp_reserved();
        let text_width = content_area.width.saturating_sub(ts_reserved);
        self.entry
            .ensure_cached(text_width, self.appearance(), self.is_selected, self.cwd);
        let cached_ref = self.entry.cached_output_ref();
        let output: &BlockOutput = &cached_ref;
        let has_vpad = self.entry.block.has_vpad(&ctx);
        // Determine how many rows of vpad/content to skip.
        // Layout is: [vpad_top?] [content lines...] [vpad_bottom?]
        let vpad_top = if has_vpad { 1u16 } else { 0 };
        let skip_remaining = skip_rows;

        // Skip vpad_top (0 or 1 row)
        let vpad_top_visible = if skip_remaining < vpad_top {
            // Partial skip into vpad: vpad is still visible
            // (vpad is 0 or 1 row, so this means skip_rows == 0)
            true
        } else {
            false
        };
        let content_skip = skip_remaining.saturating_sub(vpad_top);

        let mut row = content_area.y;
        let max_row = content_area.y + content_area.height;

        // Top vpad (only if not skipped)
        if vpad_top_visible && row < max_row {
            row += 1;
        }

        // Own the timestamp gutter per row (no-bg blocks skip the full-area fill) so a wide glyph can't strand a ghost
        // Per-row bg keeps code blocks whole
        let own_gutter = ts_reserved > 0 && bg_color.is_none() && content_area.width > ts_reserved;

        // Content lines: skip the first `content_skip` lines
        for line in output.lines.iter().skip(content_skip as usize) {
            if row >= max_row {
                break;
            }

            // Apply line-specific background if set
            // Decorative panel bands (tool result previews) are dropped in flat mode so they blend with the terminal's own background
            // Semantic shading (diff rows, code-block fill) always paints
            let line_bg = if self.flat_background && line.background_is_panel {
                None
            } else {
                line.background
            };
            if let Some(line_bg) = line_bg {
                let bg_x = content_area.x + line.bg_start_col;
                let bg_width = content_area.width.saturating_sub(line.bg_start_col);
                if bg_width > 0 {
                    let line_rect = Rect::new(bg_x, row, bg_width, 1);
                    buf.set_style(line_rect, Style::default().bg(line_bg));
                }
            }

            buf.set_line_safe_bidi(content_area.x, row, &line.content, content_area.width);

            if own_gutter {
                let gutter = Rect::new(content_area.x + text_width, row, ts_reserved, 1);
                fill_bg_spaces(buf, gutter, line_bg.unwrap_or(self.fallback_bg()));
            }

            row += 1;
        }

        // Overlay timestamp on the first content line for message blocks.
        // Short format (h:mm AM/PM) by default; expands to full format (HH:mm:ss | MMM DD) when the mouse hovers over the timestamp area
        // Gated on appearance.show_timestamps (toggled via /timestamps).
        if self.appearance().show_timestamps
            && content_skip == 0
            && !output.is_empty()
            && self.should_show_timestamp()
            && let Some(ts) = self.entry.created_at
        {
            let first_content_y = content_area.y + if vpad_top_visible { 1 } else { 0 };
            // Check if mouse is hovering the timestamp zone (rightmost 10 cols of the first content row)
            let ts_hovered = self.mouse_pos.is_some_and(|(mx, my)| {
                my == first_content_y
                    && mx >= content_area.x + content_area.width.saturating_sub(10)
                    && mx < content_area.x + content_area.width
            });
            let ts_str = if ts_hovered {
                ts.format("  %H:%M:%S | %b %d").to_string()
            } else {
                ts.format("  %-I:%M %p").to_string()
            };
            let ts_width = ts_str.len() as u16;
            if content_area.width > ts_width + 1 && first_content_y < max_row {
                let ts_x = content_area.x + content_area.width - ts_width;
                let ts_style = Style::default().fg(self.theme.gray);
                buf.set_string_safe(ts_x, first_content_y, &ts_str, ts_style);
            }
        }

        // Post-pass: adjust bullet color based on block state. The running wave is skipped so the entry reads as "paused
        // on you", not "loading".
        if skip_rows == 0 && self.entry.block.has_bullet(&ctx) {
            let bullet_style = self.entry.block.bullet(&ctx);
            let bullet_y = content_area.y + if has_vpad { 1 } else { 0 };

            if bullet_y >= max_row {
                // bullet not visible; skip post-pass
            } else if self.entry.is_pending_user_input {
                // Pending user input: leave the bullet glyph alone, just freeze its color at the block's bullet color. They would
                // otherwise render in default gray and lose the cue entirely. The fallback intentionally matches the turn-status
                // diamond and the drain-blocked diamond.
                let color = bullet_style
                    .map(|s| s.color)
                    .unwrap_or(self.theme.accent_user);
                if let Some(cell) = buf.cell_mut((content_area.x, bullet_y)) {
                    cell.fg = color;
                }
            } else if let Some(style) = bullet_style {
                if style.animated {
                    // Animated bullet: wave effect synced with accent
                    let bg = bg_color.unwrap_or(self.fallback_bg());
                    let wave_rows = self.appearance().animation.wave_rows;
                    let brightness = theme::wave_brightness(self.tick, 0, wave_rows, WAVE_SPEED);
                    let animated_color =
                        blend_color(bg, style.color, brightness).unwrap_or(style.color);
                    if let Some(cell) = buf.cell_mut((content_area.x, bullet_y)) {
                        cell.fg = animated_color;
                    }
                } else if self.groupable
                    && self.entry.display_mode == DisplayMode::Collapsed
                    && !self.is_selected
                {
                    // Collapsed groupable with colored bullet: dim it.
                    // When selected, keep the bullet at its original full color so the selection reads as undimmed
                    let bg = bg_color.unwrap_or(self.fallback_bg());
                    let dim = self.appearance().scrollback.display.dim_accent;
                    let dimmed = blend_color(bg, style.color, dim).unwrap_or(style.color);
                    if let Some(cell) = buf.cell_mut((content_area.x, bullet_y)) {
                        cell.fg = dimmed;
                    }
                }
                // Static and not collapsed-groupable: the bullet already has its correct color from prepend_bullet(); no post-pass needed
            }
        }
    }
}
