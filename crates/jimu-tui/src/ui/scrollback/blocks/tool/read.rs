#[cfg(feature = "syntax")]
use std::path::Path;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};

use super::{LineRange, TOOL_HEADER_RANGE};
#[cfg(feature = "syntax")]
use crate::engine::render::syntax::get_syntect;
use crate::engine::render::wrapping::word_wrap_lines_with_joiners;
use crate::engine::theme::Theme;
use crate::ui::scrollback::block::BlockContent;
use crate::ui::scrollback::media::ScrollbackImageRef;
use crate::ui::scrollback::types::{
    AccentStyle, BlockBackground, BlockContext, BlockLine, BlockOutput, DisplayMode, Selectable,
};

const FIRST_LINES: usize = 5;
const LAST_LINES: usize = 3;

use crate::ui::scrollback::appearance::AppearanceConfig;
fn skill_name_from_path(path: &str) -> Option<&str> {
    let p = std::path::Path::new(path);
    if p.file_name()?.to_str()? == "SKILL.md" {
        p.parent()?.file_name()?.to_str()
    } else {
        None
    }
}

/// What kind of non-text media this read produced.
#[derive(Debug, Clone)]
pub enum ReadMediaKind {
    /// Image file (PNG, JPEG, etc.)
    Image,
    /// PDF rendered as page images.
    Pdf { pages: usize },
}

#[derive(Debug, Clone)]
pub struct ReadToolCallBlock {
    /// Path to the file being read.
    pub path: String,
    /// Line range if specified: [start, end] (1-based, inclusive).
    pub line_range: Option<LineRange>,
    /// Error message if the tool call failed (None means success).
    pub error: Option<String>,
    /// When the tool started running.
    pub started_at: Option<std::time::Instant>,
    /// Elapsed time in ms after completion.
    pub elapsed_ms: Option<i64>,
    /// Raw file content (unformatted). `None` for errors, images, PDFs.
    pub content: Option<String>,
    /// Total number of lines in the file (from `FileContent.total_lines`).
    pub total_lines: Option<usize>,
    /// Inline image reference (for image file reads).
    pub image_ref: Option<ScrollbackImageRef>,
    /// Non-text media kind (image, PDF).
    pub media_kind: Option<ReadMediaKind>,
    /// Whether this ordinary read targets a memory v2 scope.
    pub is_memory_activity: bool,
    /// Actual tool name preserved for `ToolRowView`.
    pub tool_name: Option<String>,
}

impl ReadToolCallBlock {
    /// Pre-completed blocks have no meaningful local timing, so `started_at` is `None`.
    /// Timing is only set for blocks that enter a running UI state (via `set_last_running(true)` in `ScrollbackState`).
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            line_range: None,
            error: None,
            started_at: None,
            elapsed_ms: None,
            content: None,
            total_lines: None,
            image_ref: None,
            media_kind: None,
            is_memory_activity: false,
            tool_name: None,
        }
    }

    pub fn with_memory_activity(mut self) -> Self {
        self.is_memory_activity = true;
        self
    }

    pub fn with_line_range(mut self, range: LineRange) -> Self {
        self.line_range = Some(range);
        self
    }

    pub fn with_content(mut self, content: String, total_lines: usize) -> Self {
        self.content = Some(content);
        self.total_lines = Some(total_lines);
        self
    }

    /// Set error (marks as failed).
    pub fn with_error(mut self, error: impl Into<String>) -> Self {
        self.error = Some(error.into());
        self
    }

    pub fn is_success(&self) -> bool {
        self.error.is_none()
    }

    /// Whether the block has non-empty text content to display.
    pub fn has_content(&self) -> bool {
        self.content.as_ref().is_some_and(|c| !c.is_empty())
    }

    /// Skill name when this read targets a skill definition (`SKILL.md`).
    /// All skill-read detection goes through here.
    pub fn skill_name(&self) -> Option<&str> {
        skill_name_from_path(&self.path)
    }

    /// Whether this read targets a skill definition rather than a plain file.
    pub fn is_skill_read(&self) -> bool {
        self.skill_name().is_some()
    }

    /// Set the error, computing elapsed time if not already set.
    pub fn set_error(&mut self, error: Option<String>) {
        if self.elapsed_ms.is_none()
            && let Some(start) = self.started_at
        {
            self.elapsed_ms = Some(start.elapsed().as_millis() as i64);
        }
        self.error = error;
    }

    /// Finalize elapsed time from `started_at`.
    ///
    /// Idempotent: no-op if `started_at` is `None` (pre-completed block) or if `elapsed_ms` is already set (already finalized).
    pub fn finish(&mut self) {
        if self.elapsed_ms.is_some() {
            return;
        }
        if let Some(start) = self.started_at {
            self.elapsed_ms = Some(start.elapsed().as_millis() as i64);
        }
    }

    pub fn elapsed_ms(&self) -> Option<i64> {
        match self.elapsed_ms {
            Some(ms) => Some(ms),
            None => self
                .started_at
                .map(|start| start.elapsed().as_millis() as i64),
        }
    }

    /// Render header line: `Read path (start-end)`.
    fn collapsed_line(
        &self,
        theme: &Theme,
        muted: bool,
        dim_details: bool,
        surface: crate::engine::render::tool_paths::ToolPathSurface,
        cwd: Option<&std::path::Path>,
        width: Option<usize>,
    ) -> Line<'static> {
        let text_style = if muted {
            theme.muted()
        } else {
            theme.primary()
        };
        let bold_style = text_style.add_modifier(Modifier::BOLD);
        let path_style = if muted {
            theme.muted()
        } else {
            theme.fg(theme.path)
        };
        let detail_style = if dim_details {
            theme.dim()
        } else {
            theme.muted()
        };

        // SKILL.md reads render as "Skill {skill_name}".
        if let Some(skill) = self.skill_name() {
            return Line::from(vec![
                Span::styled("Skill ", bold_style),
                Span::styled(skill.to_owned(), path_style),
            ]);
        }

        let prefix = "Read ";
        let range_suffix = self
            .line_range
            .map(|r| {
                if let Some(total) = self.total_lines
                    && total > r.end.saturating_sub(r.start) + 1
                {
                    format!(" ({} of {total})", r)
                } else {
                    format!(" ({})", r)
                }
            })
            .unwrap_or_default();
        // Extra suffix for errors or empty content
        let extra_suffix = if self.content.as_ref().is_some_and(|c| c.is_empty()) {
            " (empty)".to_string()
        } else if let Some(media) = &self.media_kind {
            match media {
                ReadMediaKind::Image => " (image)".to_string(),
                ReadMediaKind::Pdf { pages } => format!(" ({pages} pages)"),
            }
        } else {
            String::new()
        };
        let total_suffix_len = range_suffix.len() + extra_suffix.len();
        let path = crate::engine::render::tool_paths::path_for_tool_surface(
            &self.path,
            surface,
            cwd,
            width,
            prefix.len() + total_suffix_len,
        );

        let mut spans = vec![
            Span::styled(prefix, bold_style),
            Span::styled(path, path_style),
        ];

        if !range_suffix.is_empty() {
            spans.push(Span::styled(range_suffix, detail_style));
        }

        if !extra_suffix.is_empty() {
            spans.push(Span::styled(extra_suffix, detail_style));
        }

        Line::from(spans)
    }

    /// Header line with only the path (or skill name) span selectable.
    fn header_block_line(&self, line: Line<'static>, cwd: Option<&std::path::Path>) -> BlockLine {
        let path_end = 2.min(line.spans.len()).max(1);
        let link_target = if self.skill_name().is_some() {
            None
        } else {
            crate::engine::render::osc8::tool_path_file_target(&self.path, cwd)
        };
        BlockLine {
            selectable: Selectable::Spans(1..path_end),
            selection_range: Some(TOOL_HEADER_RANGE),
            content: line,
            link_target,
            ..Default::default()
        }
    }

    /// Render content lines with absolute line numbers in the gutter.
    ///
    /// Wraps all lines first, then applies truncation, matching ExecuteToolCallBlock.
    fn render_content_lines(
        &self,
        theme: &Theme,
        width: usize,
        truncate: Option<(usize, usize)>,
    ) -> Vec<BlockLine> {
        let Some(content) = &self.content else {
            return Vec::new();
        };

        let base_line = self.line_range.map_or(1, |r| r.start);
        let raw_lines: Vec<&str> = content.lines().collect();

        let gutter_width = digit_count(base_line + raw_lines.len().saturating_sub(1));
        let content_width = width.saturating_sub(gutter_width + 2).max(20);

        // Use Theme::dim/primary so terminal-native (minimal) maps grays to SGR dim / default fg instead of raw gray_dim slots
        let gutter_style = theme.dim();
        let text_style = theme.primary();

        #[cfg(feature = "syntax")]
        let syntect = get_syntect();
        #[cfg(feature = "syntax")]
        let mut highlighter = syntect.highlight_lines_by_file_path(Path::new(&self.path));

        let styled_lines: Vec<Line<'static>> = raw_lines
            .iter()
            .enumerate()
            .map(|(i, text)| {
                let gutter = format!("{:>w$}  ", base_line + i, w = gutter_width);
                let mut spans = vec![Span::styled(gutter, gutter_style)];
                #[cfg(feature = "syntax")]
                spans.extend(crate::engine::render::syntax::highlight_line(
                    text,
                    &mut highlighter,
                    syntect,
                    text_style,
                ));
                #[cfg(not(feature = "syntax"))]
                spans.push(Span::styled((*text).to_owned(), text_style));
                Line::from(spans)
            })
            .collect();

        let (wrapped, joiners) = word_wrap_lines_with_joiners(styled_lines, content_width);
        let total = wrapped.len();

        let mut lines = Vec::new();

        if let Some((first, last)) = truncate {
            let threshold = first + last;
            if total > threshold {
                for (wrapped_line, joiner) in wrapped.iter().zip(joiners.iter()).take(first) {
                    lines.push(
                        BlockLine::styled(wrapped_line.clone())
                            .with_panel_background(theme.bg_dark)
                            .with_joiner(joiner.clone()),
                    );
                }
                lines.push(
                    BlockLine::separator(Line::from(Span::styled("\u{2026}", theme.muted())))
                        .with_panel_background(theme.bg_dark),
                );
                for (wrapped_line, joiner) in wrapped.iter().zip(joiners.iter()).skip(total - last)
                {
                    lines.push(
                        BlockLine::styled(wrapped_line.clone())
                            .with_panel_background(theme.bg_dark)
                            .with_joiner(joiner.clone()),
                    );
                }
            } else {
                for (wrapped_line, joiner) in wrapped.into_iter().zip(joiners) {
                    lines.push(
                        BlockLine::styled(wrapped_line)
                            .with_panel_background(theme.bg_dark)
                            .with_joiner(joiner),
                    );
                }
            }
        } else {
            for (wrapped_line, joiner) in wrapped.into_iter().zip(joiners) {
                lines.push(
                    BlockLine::styled(wrapped_line)
                        .with_panel_background(theme.bg_dark)
                        .with_joiner(joiner),
                );
            }
        }

        lines
    }
}

/// Count decimal digits in a number (for gutter width).
fn digit_count(n: usize) -> usize {
    n.checked_ilog10().map_or(1, |d| d as usize + 1)
}

impl BlockContent for ReadToolCallBlock {
    fn output(&self, ctx: &BlockContext) -> BlockOutput {
        let theme = Theme::current();
        let tool_cfg = &ctx.appearance.scrollback.blocks.tool;
        let muted_collapsed = ctx.mute_when_collapsed(tool_cfg.muted_collapsed);

        let cwd = ctx.cwd.as_deref();
        match ctx.mode {
            DisplayMode::Collapsed => BlockOutput {
                lines: vec![self.header_block_line(
                    self.collapsed_line(
                        &theme,
                        muted_collapsed,
                        tool_cfg.dim_details,
                        crate::engine::render::tool_paths::ToolPathSurface::Collapsed,
                        cwd,
                        Some(ctx.content_width()),
                    ),
                    cwd,
                )],
            },
            DisplayMode::Truncated | DisplayMode::Expanded => {
                let truncate = if ctx.mode == DisplayMode::Truncated {
                    Some((FIRST_LINES, LAST_LINES))
                } else {
                    None
                };
                let header = self.collapsed_line(
                    &theme,
                    false,
                    tool_cfg.dim_details,
                    crate::engine::render::tool_paths::ToolPathSurface::Expanded,
                    cwd,
                    None,
                );
                let mut lines: Vec<BlockLine> = vec![self.header_block_line(header, cwd)];
                if self.has_content() {
                    lines.push(BlockLine::separator(Line::from("")));
                    lines.extend(self.render_content_lines(&theme, ctx.width as usize, truncate));
                } else if let Some(err) = &self.error {
                    lines.push(BlockLine::separator(Line::from("")));
                    let error_style = Style::default().fg(theme.accent_error);
                    for line in err.lines() {
                        lines.push(BlockLine::separator(Line::from(Span::styled(
                            line.to_string(),
                            error_style,
                        ))));
                    }
                }
                BlockOutput { lines }
            }
        }
    }

    fn accent(&self, _ctx: &BlockContext) -> Option<AccentStyle> {
        None
    }

    fn bullet(&self, _ctx: &BlockContext) -> Option<AccentStyle> {
        if self.error.is_some() {
            let theme = Theme::current();
            Some(AccentStyle::static_color(theme.accent_error))
        } else {
            None
        }
    }

    fn has_vpad_for(&self, _appearance: &AppearanceConfig) -> bool {
        false
    }

    fn background(&self, _ctx: &BlockContext) -> BlockBackground {
        BlockBackground::None
    }

    fn has_raw_mode(&self) -> bool {
        false
    }

    fn is_foldable(&self) -> bool {
        self.has_content()
    }

    fn default_display_mode(&self) -> DisplayMode {
        DisplayMode::Collapsed
    }

    fn finished_display_mode(&self) -> Option<DisplayMode> {
        Some(DisplayMode::Collapsed)
    }

    fn next_fold_mode(&self, current: DisplayMode, _is_running: bool) -> DisplayMode {
        match current {
            DisplayMode::Collapsed => DisplayMode::Truncated,
            DisplayMode::Truncated | DisplayMode::Expanded => DisplayMode::Collapsed,
        }
    }

    fn image_references(&self) -> &[ScrollbackImageRef] {
        match &self.image_ref {
            Some(r) => std::slice::from_ref(r),
            None => &[],
        }
    }

    fn preamble(&self, ctx: &BlockContext) -> Option<Text<'static>> {
        let theme = Theme::current();
        let dim_details = ctx.appearance.scrollback.blocks.tool.dim_details;
        Some(Text::from(self.collapsed_line(
            &theme,
            false,
            dim_details,
            crate::engine::render::tool_paths::ToolPathSurface::Fullscreen,
            ctx.cwd.as_deref(),
            None,
        )))
    }
}
