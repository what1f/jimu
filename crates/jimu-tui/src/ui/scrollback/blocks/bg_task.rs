//! Three kinds: Started, Completed, Failed.
//! All render as always-collapsed, groupable blocks with dimmed colored bullets (same dimming as execute blocks).
//! Enter / Ctrl-F opens the block viewer with stdout from the central store.

use std::time::Duration;

use ratatui::style::Modifier;
use ratatui::text::{Line, Span, Text};

use crate::engine::theme::Theme;
use crate::engine::util::format_duration;
use crate::ui::scrollback::appearance::AppearanceConfig;
use crate::ui::scrollback::block::BlockContent;
use crate::ui::scrollback::types::{AccentStyle, BlockContext, BlockOutput, DisplayMode};

/// The signal recorded on a task the user stopped.
pub const KILLED_SIGNAL: &str = "killed";

#[derive(Debug, Clone)]
pub enum BgTaskKind {
    /// Task was started (process is running).
    Started,
    /// Task completed successfully.
    Completed { elapsed: Duration },
    /// Task failed (non-zero exit, signal, timeout, etc.).
    Failed {
        elapsed: Duration,
        exit_code: Option<i32>,
        signal: Option<String>,
    },
}

/// The block is always collapsed, not foldable, groupable, and selectable.
/// Enter / Ctrl-F opens the block viewer with stdout from the central store.
#[derive(Debug, Clone)]
pub struct BgTaskBlock {
    /// The command that was run.
    pub command: String,
    /// Background task ID (for looking up stdout in the central store).
    pub task_id: String,
    pub kind: BgTaskKind,
    /// Optional description (from the tool call's description field).
    pub description: Option<String>,
}

impl BgTaskBlock {
    pub fn started(command: impl Into<String>, task_id: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            task_id: task_id.into(),
            kind: BgTaskKind::Started,
            description: None,
        }
    }

    pub fn completed(
        command: impl Into<String>,
        task_id: impl Into<String>,
        elapsed: Duration,
    ) -> Self {
        Self {
            command: command.into(),
            task_id: task_id.into(),
            kind: BgTaskKind::Completed { elapsed },
            description: None,
        }
    }

    pub fn failed(
        command: impl Into<String>,
        task_id: impl Into<String>,
        elapsed: Duration,
        exit_code: Option<i32>,
        signal: Option<String>,
    ) -> Self {
        Self {
            command: command.into(),
            task_id: task_id.into(),
            kind: BgTaskKind::Failed {
                elapsed,
                exit_code,
                signal,
            },
            description: None,
        }
    }

    pub fn with_description(mut self, description: Option<String>) -> Self {
        self.description = description;
        self
    }

    pub fn is_running(&self) -> bool {
        matches!(self.kind, BgTaskKind::Started)
    }

    /// Mark a Started block as completed.
    pub fn mark_completed(&mut self, elapsed: Duration) {
        self.kind = BgTaskKind::Completed { elapsed };
    }

    /// Mark a Started block as failed.
    pub fn mark_failed(
        &mut self,
        elapsed: Duration,
        exit_code: Option<i32>,
        signal: Option<String>,
    ) {
        self.kind = BgTaskKind::Failed {
            elapsed,
            exit_code,
            signal,
        };
    }
}

impl BlockContent for BgTaskBlock {
    fn output(&self, ctx: &BlockContext) -> BlockOutput {
        let theme = Theme::current();
        // When selected, lift only the bold "Task" label to `text_primary` so it reads as undimmed
        // This mirrors `read.rs` / `search.rs`, which bump only the label and leave the rest at `muted`
        // The detail text (verb and description) stays muted in every state
        let bold = if ctx.is_selected {
            theme.primary().add_modifier(Modifier::BOLD)
        } else {
            theme.muted().add_modifier(Modifier::BOLD)
        };
        let muted = theme.muted();

        // Collapse newlines for single-line display (ratatui drops '\n' as zero-width, merging adjacent lines without spacing)
        let command = self.command.replace('\n', " ");

        // Prefer description over raw command for the collapsed one-line display.
        // The full command is always available in the block viewer (preamble).
        let display = match &self.description {
            Some(d) if !d.trim().is_empty() => d.replace('\n', " "),
            _ => command,
        };
        let line = match &self.kind {
            BgTaskKind::Started => Line::from(vec![
                Span::styled("Task ", bold),
                Span::styled("started: ", muted),
                Span::styled(display, muted),
            ]),
            BgTaskKind::Completed { elapsed } => Line::from(vec![
                Span::styled("Task ", bold),
                Span::styled(
                    format!("completed in {}: ", format_duration(*elapsed)),
                    muted,
                ),
                Span::styled(display, muted),
            ]),
            BgTaskKind::Failed {
                elapsed,
                exit_code,
                signal,
            } => {
                // Detect kill signals to show "killed" instead of "failed"
                let is_killed = signal
                    .as_deref()
                    .is_some_and(|s| matches!(s, KILLED_SIGNAL | "SIGTERM" | "SIGKILL" | "oom"));
                let verb = if is_killed { "killed" } else { "failed" };
                let detail = if is_killed {
                    String::new()
                } else {
                    match (exit_code, signal) {
                        (_, Some(sig)) => format!(" ({})", sig),
                        (Some(code), None) => format!(" (exit {})", code),
                        (None, None) => String::new(),
                    }
                };
                Line::from(vec![
                    Span::styled("Task ", bold),
                    Span::styled(format!("{verb} in {}: ", format_duration(*elapsed)), muted),
                    Span::styled(format!("{}{}", display, detail), muted),
                ])
            }
        };

        BlockOutput {
            lines: vec![line.into()],
        }
    }

    fn accent(&self, ctx: &BlockContext) -> Option<AccentStyle> {
        let theme = Theme::current();
        match &self.kind {
            BgTaskKind::Started if ctx.is_running => {
                Some(AccentStyle::static_color(theme.accent_running))
            }
            _ => None,
        }
    }

    fn bullet(&self, ctx: &BlockContext) -> Option<AccentStyle> {
        let theme = Theme::current();
        match &self.kind {
            BgTaskKind::Started => {
                if ctx.is_running {
                    Some(AccentStyle::animated_running(ctx, &theme))
                } else {
                    // Normal gray after finish_running() is called
                    None
                }
            }
            BgTaskKind::Completed { .. } => Some(AccentStyle::static_color(theme.accent_success)),
            BgTaskKind::Failed { .. } => Some(AccentStyle::static_color(theme.accent_error)),
        }
    }

    fn has_vpad_for(&self, _appearance: &AppearanceConfig) -> bool {
        false
    }

    fn has_raw_mode(&self) -> bool {
        false
    }

    fn is_foldable(&self) -> bool {
        false
    }

    fn default_display_mode(&self) -> DisplayMode {
        DisplayMode::Collapsed
    }

    fn is_selectable(&self) -> bool {
        true
    }

    fn has_bullet(&self, _ctx: &BlockContext) -> bool {
        true
    }

    fn is_groupable(&self) -> bool {
        true
    }

    fn preamble(&self, ctx: &BlockContext) -> Option<Text<'static>> {
        let theme = Theme::current();
        let mut lines = Vec::new();

        // Description first (primary text), then a blank separator, then the `$ command` with bash syntax highlighting
        // When there is no description, the command stands alone with no leading blank row
        let description = self
            .description
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        if let Some(desc) = description {
            // Trim trailing whitespace per line and collapse runs of blank lines to a single blank
            // Multi-line descriptions can carry noisy internal blank rows that would otherwise stretch the preamble
            let mut prev_blank = false;
            for line in desc.lines() {
                let trimmed = line.trim_end();
                if trimmed.is_empty() {
                    if prev_blank {
                        continue;
                    }
                    prev_blank = true;
                } else {
                    prev_blank = false;
                }
                lines.push(Line::from(Span::styled(
                    trimmed.to_string(),
                    theme.primary(),
                )));
            }
            lines.push(Line::from(""));
        }

        // Multi-line `$ command` must be separate ratatui Lines: a single Line drops '\n' as zero-width and smashes `cmd1\ncmd2` into `cmd1cmd2`
        // The smashed form shows when expanding a started bg task in the block viewer
        // Match execute / permission-panel soft-wrap so physical newlines and long lines render the same way as foreground shell tool calls
        push_shell_command_preamble_lines(&mut lines, &self.command, ctx.width as usize, &theme);

        Some(Text::from(lines))
    }
}

/// Append a soft-wrapped `$ command` block to `lines` (first row prefixed with `$ `, continuations hang-indented under the command body).
fn push_shell_command_preamble_lines(
    lines: &mut Vec<Line<'static>>,
    command: &str,
    width: usize,
    theme: &Theme,
) {
    use unicode_width::UnicodeWidthStr;

    let prefix = "$ ";
    let hang = UnicodeWidthStr::width(prefix);
    let cmd_width = width.saturating_sub(hang).max(1);

    let command = if command.trim().is_empty() {
        "\u{2026}"
    } else {
        command
    };

    let cmd_rows =
        crate::ui::scrollback::command::render_bash_command_display_lines(command, cmd_width);

    let hang_indent: String = " ".repeat(hang);
    if cmd_rows.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            prefix.to_string(),
            theme.dim(),
        )]));
        return;
    }
    for (i, row) in cmd_rows.into_iter().enumerate() {
        let mut spans = if i == 0 {
            vec![Span::styled(prefix.to_string(), theme.dim())]
        } else {
            vec![Span::raw(hang_indent.clone())]
        };
        spans.extend(row.spans);
        lines.push(Line::from(spans));
    }
}
