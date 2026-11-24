use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use unicode_width::UnicodeWidthStr;

use crate::engine::render::color::blend_line_with_default;
use crate::engine::theme::Theme;
use crate::ui::scrollback::block::BlockContent;
use crate::ui::scrollback::types::{
    AccentStyle, BlockBackground, BlockContext, BlockLine, BlockOutput, DisplayMode,
};

use super::markdown_content::MarkdownContent;
use super::quote_bar::QuoteBarStrip;
use crate::ui::scrollback::appearance::AppearanceConfig;

/// TODO: hard-coded because `AppView::minimal_key_intercept` matches this chord literally instead of going through the keybinding registry.
/// Resolve the label from the registry once it does, so a remap is advertised correctly.
const EXPAND_HINT: &str = "ctrl+e to expand";

const EXPAND_HINT_GAP: &str = "  ";

/// Append the dim `(ctrl+e to expand)` hint to a collapsed header line. The `Collapsed` guard matters because
/// `render_empty_placeholder` reuses the collapsed renderer for an empty body in other modes. There the hint would
/// be a lie.
fn append_expand_hint(line: Line<'static>, ctx: &BlockContext) -> Line<'static> {
    if !ctx
        .appearance
        .scrollback
        .blocks
        .thinking
        .collapsed_expand_hint
        || ctx.mode != DisplayMode::Collapsed
    {
        return line;
    }
    let hint = format!("{EXPAND_HINT_GAP}({EXPAND_HINT})");
    let used: usize = line.spans.iter().map(|s| s.content.width()).sum();
    if used + hint.width() > ctx.content_width() {
        return line;
    }
    let mut line = line;
    line.spans.push(Span::styled(hint, Theme::current().dim()));
    line
}

/// Attributes only, no foreground: minimal's terminal-native palette makes color-based de-emphasis a no-op.
/// Terminals that merely *ignore* SGR 3 (tmux without `sitm`) are not gated: there is no reliable probe, and they
/// keep the other two cues.
fn body_emphasis_patch(ctx: &BlockContext) -> Option<Style> {
    if !ctx.appearance.scrollback.blocks.thinking.body_dim_italic {
        return None;
    }
    let mut modifiers = Modifier::DIM;
    if !crate::engine::render::glyphs::is_legacy_windows_console() {
        modifiers |= Modifier::ITALIC;
    }
    Some(Style::new().add_modifier(modifiers))
}

/// Columns spent by the `┃ ` body prefix when the rail renders under the
/// header's bullet ([`crate::ui::scrollback::appearance::ThinkingConfig::rail_under_bullet`]).
const BODY_RAIL_WIDTH: usize = 2;

/// Whether the reasoning rail renders inside the body rows (directly below the header's bullet) instead of as the reserved accent column.
/// Minimal-only: there every other block starts flush at column 0 with its own `◆`.
/// An accent column would indent the header's diamond out of line and read as a second, different gutter treatment.
///
/// Gated on `accent_enabled` like the accent-column rail it replaces (`rail_style`):
/// a pager.toml `accent_enabled = false` turns off the reasoning rail wherever it is drawn.
fn rail_under_bullet(ctx: &BlockContext) -> bool {
    let cfg = &ctx.appearance.scrollback.blocks.thinking;
    cfg.rail_under_bullet && cfg.accent_enabled
}

/// One body row's rail prefix: dim like the accent-column rail it replaces, so it stays a quiet structural cue under `NO_COLOR`.
fn body_rail_span(ctx: &BlockContext) -> Span<'static> {
    let cfg = &ctx.appearance.scrollback.blocks.thinking;
    Span::styled(
        format!("{} ", crate::engine::render::glyphs::accent_bar()),
        Style::default().fg(cfg.accent).add_modifier(Modifier::DIM),
    )
}

/// Prefix every body row with [`body_rail_span`] so the rail runs from under the header's bullet down the whole body.
/// The prefix span is excluded from selection (same mechanism as `prepend_bullet`).
///
/// Markdown leaves code-block fill on the line *style*, which `Buffer::set_line`
/// patches under every span — the rail prefix included. Hoist it onto the
/// per-line [`BlockLine::background`] starting after the rail (the same
/// line-style → background split as `MarkdownContent::output`), so the fill
/// spans the row's content but the rail cell stays unshaded.
fn apply_body_rail(output: &mut BlockOutput, ctx: &BlockContext) {
    if !rail_under_bullet(ctx) {
        return;
    }
    for line in &mut output.lines {
        line.content.spans.insert(0, body_rail_span(ctx));
        crate::ui::scrollback::types::shift_selection_metadata_for_prefix(line, 1);
        if let Some(bg) = line.content.style.bg.take() {
            line.background = Some(bg);
            line.bg_start_col = BODY_RAIL_WIDTH as u16;
        }
    }
}

/// Body wrap width: the in-body rail prefix spends [`BODY_RAIL_WIDTH`] of the content columns.
/// The markdown must wrap that much narrower to keep `desired_height` and the painted rows in agreement.
fn body_wrap_width(ctx: &BlockContext) -> usize {
    let width = ctx.width as usize;
    if rail_under_bullet(ctx) {
        width.saturating_sub(BODY_RAIL_WIDTH).max(1)
    } else {
        width
    }
}

/// Block displaying agent thinking content with markdown rendering.
///
/// Uses [`MarkdownContent`] for incremental markdown rendering with cached word-wrapping, plus special display modes:
/// - **Collapsed**: Shows "Thought" or "Thought for Xs" if time is set
/// - **Truncated** (default): Shows "…" then the last N lines
/// - **Expanded**: Full content
#[derive(Debug, Clone)]
pub struct ThinkingBlock {
    content: MarkdownContent,

    /// Optional elapsed time in milliseconds (from server).
    /// When set, collapsed view shows "Thought for Xs".
    elapsed_time_ms: Option<i64>,
    /// When the thinking block started (local timestamp for live elapsed).
    started_at: Option<std::time::Instant>,
}
impl ThinkingBlock {
    /// Create a new thinking block with complete text.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            content: MarkdownContent::new(text),
            elapsed_time_ms: None,
            started_at: None,
        }
    }

    /// Create an empty block for streaming.
    pub fn streaming() -> Self {
        Self {
            content: MarkdownContent::streaming(),
            elapsed_time_ms: None,
            started_at: Some(std::time::Instant::now()),
        }
    }

    /// Create an empty streaming block for historical replay. A local wall-clock timer would then freeze to ~0ms in
    /// [`finish`] and render a bogus "Thought for 0.0s".
    pub fn streaming_replay() -> Self {
        Self {
            content: MarkdownContent::streaming(),
            elapsed_time_ms: None,
            started_at: None,
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

    /// Finish streaming and do a full re-render for safety.
    /// Freezes the local elapsed time from `started_at`.
    /// The collapsed view then shows the actual wall-clock duration the user experienced, not the server-reported delta.
    pub fn finish(&mut self) {
        self.content.finish();
        // Freeze local elapsed if no server time has been set.
        // The local timer (started_at to now) captures the full duration from block creation to finish, which is what the user perceives
        if self.elapsed_time_ms.is_none()
            && let Some(start) = self.started_at
        {
            self.elapsed_time_ms = Some(start.elapsed().as_millis() as i64);
        }
    }

    /// Get the source text.
    pub fn text(&self) -> String {
        self.content.text()
    }

    /// Get the elapsed thinking time in milliseconds.
    ///
    /// Returns server-reported time if available, otherwise live elapsed from `started_at` (for running thinking blocks).
    pub fn elapsed_time_ms(&self) -> Option<i64> {
        match self.elapsed_time_ms {
            Some(ms) => Some(ms),
            None => self
                .started_at
                .map(|start| start.elapsed().as_millis() as i64),
        }
    }

    /// Set the elapsed time (in milliseconds).
    ///
    /// When set, the collapsed view will show "Thought for Xs".
    pub fn set_elapsed_time_ms(&mut self, time_ms: Option<i64>) {
        self.elapsed_time_ms = time_ms;
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

    /// Format elapsed time for display.
    fn format_time(&self) -> Option<String> {
        self.elapsed_time_ms.map(|ms| {
            let secs = ms as f64 / 1000.0;
            if secs < 60.0 {
                format!("{:.1}s", secs)
            } else {
                let mins = (secs / 60.0).floor() as u32;
                let remaining = secs - (mins as f64 * 60.0);
                format!("{}m{:.0}s", mins, remaining)
            }
        })
    }

    /// Build the header line: "Thinking." (running) or "Thought for Xs" (done). Respects muted_collapsed: when
    /// collapsed and muting is on, uses muted style. The selected header then reads as undimmed, the same rule as the
    /// tool-call variants.
    fn header_line(&self, ctx: &BlockContext) -> Line<'static> {
        let theme = Theme::current();
        let tool_cfg = &ctx.appearance.scrollback.blocks.tool;
        let thinking_cfg = &ctx.appearance.scrollback.blocks.thinking;
        let is_collapsed = ctx.mode == DisplayMode::Collapsed;
        let is_muted = is_collapsed && ctx.mute_when_collapsed(tool_cfg.muted_collapsed);

        // Bright on selection or config opt-in, but never while muted; keeps legacy-ConHost collapse uniformly muted
        let use_bright = !is_muted && (ctx.is_selected || thinking_cfg.header_bright);

        let label_style = if use_bright {
            theme.primary().bold()
        } else {
            theme.muted().bold()
        };

        let detail_style = theme.muted();

        if ctx.is_running {
            Line::from(Span::styled("Thinking…", label_style))
        } else if let Some(time_str) = self.format_time() {
            Line::from(vec![
                Span::styled("Thought", label_style),
                Span::styled(format!(" for {time_str}"), detail_style),
            ])
        } else {
            Line::from(Span::styled("Thought", label_style))
        }
    }

    /// Render the collapsed view: header line only, truncated to fit.
    fn render_collapsed(&self, ctx: &BlockContext) -> BlockOutput {
        let line = self.header_line(ctx);
        let line = append_expand_hint(line, ctx);
        let line = crate::engine::render::line_utils::truncate_line(line, ctx.content_width());
        BlockOutput {
            lines: vec![BlockLine::separator(line)],
        }
    }

    /// Prepend header to output, if header config is enabled.
    /// Full mode keeps a blank row under the title. Minimal (`rail_under_bullet`) does not — the body starts on the next row.
    fn maybe_prepend_header(&self, mut output: BlockOutput, ctx: &BlockContext) -> BlockOutput {
        if ctx.appearance.scrollback.blocks.thinking.header {
            if !rail_under_bullet(ctx) {
                output.lines.insert(0, BlockLine::separator(Line::from("")));
            }
            output
                .lines
                .insert(0, BlockLine::separator(self.header_line(ctx)));
        }
        output
    }

    /// One wrapped markdown line rendered as a selectable, blended [`BlockLine`]. Quote-bar exclusion must run before
    /// blending: blending rewrites span fg colors, which would defeat the bar-style detection. Blending preserves span
    /// structure, so the computed span indices stay valid after it.
    fn thinking_body_line(
        line: &Line<'static>,
        joiner: &Option<String>,
        strip: &QuoteBarStrip,
        bg_base: Color,
        fg_default: Color,
        blend_factor: f32,
        emphasis: Option<Style>,
    ) -> BlockLine {
        let mut content = line.clone();
        let selectable = strip.selectable(&mut content);
        let indent_width = if joiner.is_some() {
            super::markdown_content::compute_subsequent_indent_width(line)
        } else {
            0
        };
        let mut blended = blend_line_with_default(content, bg_base, fg_default, blend_factor);
        if let Some(emphasis) = emphasis {
            for span in &mut blended.spans {
                span.style = span.style.patch(emphasis);
            }
        }
        let mut block_line = BlockLine::styled(blended)
            .with_selection_range(Some(0))
            .with_joiner(joiner.clone());
        block_line.selectable = selectable;
        block_line.indent_width = indent_width;
        block_line
    }

    /// Render truncated view: optional header, then "…", then the last N lines.
    fn render_truncated(&self, ctx: &BlockContext) -> BlockOutput {
        let config = &ctx.appearance.scrollback.blocks.thinking;
        let n = config.truncated_lines as usize;
        let width = body_wrap_width(ctx);
        let blend_factor = config.bg_blend;
        let emphasis = body_emphasis_patch(ctx);
        let strip = QuoteBarStrip::new(!self.content.is_raw());

        self.content.with_wrapped_lines(width, |wrapped| {
            if wrapped.lines.is_empty() {
                return self.render_empty_placeholder(ctx);
            }

            let theme = Theme::current();
            let bg_base = theme.bg_base;
            let fg_default = theme.text_primary;

            let total = wrapped.lines.len();
            if total <= n {
                // Content fits within N lines, show all (with blending)
                let mut output = BlockOutput {
                    lines: wrapped
                        .lines
                        .iter()
                        .zip(wrapped.joiners.iter())
                        .map(|(line, joiner)| {
                            Self::thinking_body_line(
                                line,
                                joiner,
                                &strip,
                                bg_base,
                                fg_default,
                                blend_factor,
                                emphasis,
                            )
                        })
                        .collect(),
                };
                apply_body_rail(&mut output, ctx);
                return self.maybe_prepend_header(output, ctx);
            }

            // Build truncated output: "…" then the last N lines
            let theme = Theme::current();
            let mut output_lines = Vec::with_capacity(n + 1);

            // Ellipsis line
            let ellipsis = Line::from(Span::styled("…", theme.muted()));
            output_lines.push(ellipsis.into());

            // Last N lines (with blending)
            for (line, joiner) in wrapped
                .lines
                .iter()
                .zip(wrapped.joiners.iter())
                .skip(total - n)
            {
                output_lines.push(Self::thinking_body_line(
                    line,
                    joiner,
                    &strip,
                    bg_base,
                    fg_default,
                    blend_factor,
                    emphasis,
                ));
            }

            let mut output = BlockOutput {
                lines: output_lines,
            };
            apply_body_rail(&mut output, ctx);
            self.maybe_prepend_header(output, ctx)
        })
    }

    /// Render expanded view: full content.
    fn render_expanded(&self, ctx: &BlockContext) -> BlockOutput {
        let config = &ctx.appearance.scrollback.blocks.thinking;
        let width = body_wrap_width(ctx);
        let blend_factor = config.bg_blend;
        let emphasis = body_emphasis_patch(ctx);
        let strip = QuoteBarStrip::new(!self.content.is_raw());

        self.content.with_wrapped_lines(width, |wrapped| {
            if wrapped.lines.is_empty() {
                return self.render_empty_placeholder(ctx);
            }

            let theme = Theme::current();
            let bg_base = theme.bg_base;
            let fg_default = theme.text_primary;

            let mut output = BlockOutput {
                lines: wrapped
                    .lines
                    .iter()
                    .zip(wrapped.joiners.iter())
                    .map(|(line, joiner)| {
                        Self::thinking_body_line(
                            line,
                            joiner,
                            &strip,
                            bg_base,
                            fg_default,
                            blend_factor,
                            emphasis,
                        )
                    })
                    .collect(),
            };
            apply_body_rail(&mut output, ctx);
            self.maybe_prepend_header(output, ctx)
        })
    }

    /// Placeholder for empty thinking block; shows the same header as collapsed mode ("Thinking…" or "Thought for Xs").
    fn render_empty_placeholder(&self, ctx: &BlockContext) -> BlockOutput {
        self.render_collapsed(ctx)
    }

    /// The rail's accent style (also the running bullet's), independent of
    /// where the rail is drawn (accent column vs in-body prefix).
    fn rail_style(&self, ctx: &BlockContext) -> Option<AccentStyle> {
        let cfg = &ctx.appearance.scrollback.blocks.thinking;
        if !cfg.accent_enabled {
            return None;
        }
        // No accent when collapsed; accent is only for expanded/truncated content
        // TODO: revisit if we want accent in collapsed state with header enabled.
        if ctx.mode == DisplayMode::Collapsed {
            return None;
        }
        if cfg.animate && ctx.is_running {
            Some(AccentStyle::animated(cfg.accent))
        } else {
            Some(AccentStyle::static_color(cfg.accent))
        }
    }
}

impl BlockContent for ThinkingBlock {
    fn output(&self, ctx: &BlockContext) -> BlockOutput {
        match ctx.mode {
            DisplayMode::Collapsed => self.render_collapsed(ctx),
            DisplayMode::Truncated => self.render_truncated(ctx),
            DisplayMode::Expanded => self.render_expanded(ctx),
        }
    }

    fn accent(&self, ctx: &BlockContext) -> Option<AccentStyle> {
        // With the in-body rail the accent column must stay unreserved and unpainted; the rail lives in the body rows instead
        if rail_under_bullet(ctx) {
            return None;
        }
        self.rail_style(ctx)
    }

    /// Thinking bullet: default (None) when not running, animated when running.
    /// This means collapsed thinking shows gray bullet, running thinking syncs with accent.
    fn bullet(&self, ctx: &BlockContext) -> Option<AccentStyle> {
        if ctx.is_running {
            // Sync bullet with the rail animation when running — via
            // `rail_style`, not `accent`, so the in-body rail mode keeps the
            // animated bullet.
            self.rail_style(ctx)
        } else {
            None // default gray/primary
        }
    }

    fn background(&self, _ctx: &BlockContext) -> BlockBackground {
        BlockBackground::None
    }

    fn accent_background(&self, _ctx: &BlockContext) -> bool {
        false
    }

    fn has_vpad_for(&self, _appearance: &AppearanceConfig) -> bool {
        false
    }

    fn has_raw_mode(&self) -> bool {
        true
    }

    fn is_foldable(&self) -> bool {
        true
    }

    fn next_fold_mode(&self, current: DisplayMode, is_running: bool) -> DisplayMode {
        if is_running {
            match current {
                DisplayMode::Collapsed | DisplayMode::Truncated => DisplayMode::Expanded,
                DisplayMode::Expanded => DisplayMode::Truncated,
            }
        } else {
            match current {
                DisplayMode::Collapsed => DisplayMode::Expanded,
                DisplayMode::Truncated | DisplayMode::Expanded => DisplayMode::Collapsed,
            }
        }
    }

    fn collapse_mode(&self, is_running: bool) -> DisplayMode {
        if is_running {
            DisplayMode::Truncated
        } else {
            DisplayMode::Collapsed
        }
    }

    fn default_display_mode(&self) -> DisplayMode {
        DisplayMode::Truncated
    }

    fn finished_display_mode(&self) -> Option<DisplayMode> {
        Some(DisplayMode::Collapsed)
    }

    fn has_bullet(&self, ctx: &BlockContext) -> bool {
        let cfg = &ctx.appearance.scrollback.blocks.thinking;
        let has_header_visible = ctx.mode == DisplayMode::Collapsed || cfg.header;
        has_header_visible
            && ctx
                .appearance
                .scrollback
                .blocks
                .tool
                .bullet
                .char()
                .is_some()
    }

    fn is_groupable(&self) -> bool {
        true
    }

    fn preamble(&self, ctx: &BlockContext) -> Option<Text<'static>> {
        // Use expanded (bright) styling, not muted collapsed
        let bright_ctx = BlockContext {
            mode: DisplayMode::Expanded,
            ..ctx.clone()
        };
        Some(Text::from(self.header_line(&bright_ctx)))
    }
}
