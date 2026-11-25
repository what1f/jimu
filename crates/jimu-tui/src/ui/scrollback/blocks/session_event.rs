//! Styled session notices supplied by the host.

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use crate::engine::render::wrapping::word_wrap_lines;
use crate::engine::theme::Theme;
use crate::ui::scrollback::appearance::AppearanceConfig;
use crate::ui::scrollback::block::BlockContent;
use crate::ui::scrollback::types::{
    AccentStyle, BlockContext, BlockLine, BlockOutput, DisplayMode, Selectable,
};

const RECAP_BODY_RANGE: u16 = 0;

/// Session notices with the flags needed by turn navigation and rendering.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    Notice(String),
    Warning(String),
    HookOutcome(String),
    TurnEnded(String),
    TurnFailed(String),
    Recap(String),
}

impl SessionEvent {
    pub fn message(&self) -> String {
        match self {
            Self::Notice(text)
            | Self::Warning(text)
            | Self::HookOutcome(text)
            | Self::TurnEnded(text)
            | Self::TurnFailed(text) => text.clone(),
            Self::Recap(summary) => format!("Recap: {summary}"),
        }
    }

    fn recap_summary(&self) -> Option<&str> {
        match self {
            Self::Recap(summary) => Some(summary),
            _ => None,
        }
    }

    fn is_warning_banner(&self) -> bool {
        matches!(self, Self::Warning(_) | Self::TurnFailed(_))
    }

    pub fn is_turn_terminal(&self) -> bool {
        matches!(self, Self::TurnEnded(_) | Self::TurnFailed(_))
    }
}

#[derive(Debug, Clone)]
pub struct SessionEventBlock {
    pub event: SessionEvent,
}

impl SessionEventBlock {
    pub fn new(event: SessionEvent) -> Self {
        Self { event }
    }

    /// A recap with real body content, i.e. not the empty loading spinner or a stray empty recap.
    /// Gates the interactive affordances (folding and j/k selection) so navigation never lands on a recap that can't fold.
    fn recap_has_body(&self) -> bool {
        self.event
            .recap_summary()
            .is_some_and(|s| !s.trim().is_empty())
    }

    /// Render a recap event in the tool-call visual style. While the recap is still being generated the entry is
    /// `is_running`, so only the header is shown. [`BlockLine::separator`] / [`Selectable::None`] keep drag-highlight
    /// and copy on the summary body, never the chrome label.
    fn recap_output(&self, ctx: &BlockContext, summary: &str) -> BlockOutput {
        let theme = Theme::current();
        let muted_collapsed =
            ctx.mute_when_collapsed(ctx.appearance.scrollback.blocks.tool.muted_collapsed);

        // "Recap" header: bold, neutral primary text (like a tool-call header)
        // Dimmed to muted gray while collapsed-and-unselected.
        let header_text_style = if muted_collapsed {
            theme.muted()
        } else {
            theme.primary()
        };
        let header_style = header_text_style.add_modifier(Modifier::BOLD);
        // Non-selectable chrome (same as Thinking / tool label prefixes).
        let header_line =
            || BlockLine::separator(Line::from(Span::styled("Recap".to_string(), header_style)));

        // Loading: header only; the animated gray sidebar is the feedback.
        if ctx.is_running {
            return BlockOutput {
                lines: vec![header_line()],
            };
        }

        match ctx.mode {
            DisplayMode::Collapsed => {
                let mut spans = vec![Span::styled("Recap".to_string(), header_style)];
                let preview = summary.lines().next().unwrap_or(summary).trim();
                if !preview.is_empty() {
                    spans.push(Span::styled(format!("  {preview}"), theme.muted()));
                }
                let line = crate::engine::render::line_utils::truncate_line(
                    Line::from(spans),
                    ctx.content_width(),
                );
                // Only the preview span is copyable, never the "Recap" label
                // No preview (empty after trim) means fully non-selectable
                let selectable = if preview.is_empty() {
                    Selectable::None
                } else {
                    Selectable::Spans(1..2)
                };
                BlockOutput {
                    lines: vec![BlockLine {
                        content: line,
                        selectable,
                        selection_range: (!preview.is_empty()).then_some(RECAP_BODY_RANGE),
                        selection_text: (!preview.is_empty()).then(|| preview.to_string()),
                        ..Default::default()
                    }],
                }
            }
            DisplayMode::Truncated | DisplayMode::Expanded => {
                let mut lines: Vec<BlockLine> = vec![header_line()];
                // Blank gap under the header is decoration, not copyable text.
                lines.push(BlockLine::separator(Line::from("")));

                let styled_lines = summary
                    .split('\n')
                    .map(|line| Line::from(Span::styled(line.to_string(), theme.muted())));
                let wrapped =
                    word_wrap_lines(styled_lines, (ctx.width as usize).saturating_sub(2).max(20));
                for wrapped_line in wrapped {
                    lines.push(
                        BlockLine::styled(wrapped_line)
                            .with_selection_range(Some(RECAP_BODY_RANGE)),
                    );
                }

                BlockOutput { lines }
            }
        }
    }
}

impl BlockContent for SessionEventBlock {
    fn output(&self, ctx: &BlockContext) -> BlockOutput {
        // Recap renders in the tool-call style (bullet, bold header, body)
        if let Some(summary) = self.event.recap_summary() {
            return self.recap_output(ctx, summary);
        }

        let theme = Theme::current();
        // Failures and re-auth / context-overflow prompts are actionable, not informational; render them in the warning color rather than muted
        let style = if self.event.is_warning_banner() {
            ratatui::style::Style::default().fg(theme.warning)
        } else {
            theme.muted()
        };

        let text = self.event.message();
        let wrapped = if text.contains('\n') {
            let input_lines = text
                .split('\n')
                .map(|s| Line::from(Span::styled(s.to_owned(), style)));
            word_wrap_lines(input_lines, ctx.width as usize)
        } else {
            word_wrap_lines(
                std::iter::once(Line::from(Span::styled(text, style))),
                ctx.width as usize,
            )
        };
        let mut lines: Vec<BlockLine> = wrapped
            .into_iter()
            .map(|line| BlockLine::styled(line).with_selection_range(Some(0)))
            .collect();

        if lines.is_empty() {
            lines.push(BlockLine::styled(Line::from("")).with_selection_range(Some(0)));
        }
        BlockOutput { lines }
    }

    fn accent(&self, ctx: &BlockContext) -> Option<AccentStyle> {
        let theme = Theme::current();
        if self.event.recap_summary().is_some() {
            // Loading: animated sidebar so there's feedback that the recap is being generated
            // Gray rather than the magenta `accent_running`: the recap is a passive marker, not an active tool turn
            if ctx.is_running {
                return Some(AccentStyle::animated(theme.gray));
            }
            // Finished: neutral tool accent bar when expanded (no special color).
            return (ctx.mode != DisplayMode::Collapsed)
                .then(|| AccentStyle::static_color(theme.accent_tool));
        }
        if self.event.is_warning_banner() {
            Some(AccentStyle::static_color(theme.warning))
        } else {
            None
        }
    }

    fn bullet(&self, ctx: &BlockContext) -> Option<AccentStyle> {
        // Recap: animated dot while loading; default gray dot when collapsed-idle; accent color when expanded
        // A hook outcome keeps the default gray, matching its muted text; other events never show a bullet
        if matches!(self.event, SessionEvent::HookOutcome(_))
            || (self.event.recap_summary().is_some()
                && !ctx.is_running
                && ctx.mode == DisplayMode::Collapsed)
        {
            return None;
        }
        self.accent(ctx)
    }

    fn has_vpad_for(&self, _appearance: &AppearanceConfig) -> bool {
        false // Compact like SystemMessageBlock
    }

    fn has_raw_mode(&self) -> bool {
        false
    }

    fn is_foldable(&self) -> bool {
        // A recap with body content folds; other events are single informational lines with nothing to collapse
        self.recap_has_body()
    }

    fn is_selectable(&self) -> bool {
        // Recap is tool-like: navigable so it can be folded, but only once it has body content (mirrors `is_foldable`)
        // That way j/k never lands on the loading spinner or an empty recap. Other events stay non-interactive.
        self.recap_has_body()
    }

    fn default_display_mode(&self) -> DisplayMode {
        DisplayMode::Expanded
    }

    fn has_bullet(&self, ctx: &BlockContext) -> bool {
        // Recap and hook outcomes only, gated on the shared tool bullet so they track the tool rows' appearance setting
        (self.event.recap_summary().is_some() || matches!(self.event, SessionEvent::HookOutcome(_)))
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
}
