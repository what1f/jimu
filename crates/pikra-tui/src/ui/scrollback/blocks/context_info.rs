//! ContextInfoBlock: the typed `/context` display rendered in scrollback.
//!
//! Holds the raw [`ContextInfo`] snapshot and the active model name, and rebuilds the styled output on every `output()` call.
//! This is the same pattern as [`super::SessionEventBlock`]: keep typed data, format at render time.
//! `Theme::current()` is re-resolved on every redraw, so switching themes after running `/context` updates the colors immediately.
//! Formatting at capture time would have baked in stale colors.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::engine::render::wrapping::word_wrap_lines;
use crate::engine::theme::{Theme, quantize};
use crate::ui::scrollback::appearance::AppearanceConfig;
use crate::ui::scrollback::block::BlockContent;
use crate::ui::scrollback::types::{AccentStyle, BlockContext, BlockLine, BlockOutput};
/// Usage data computed by the harness.
#[derive(Debug, Clone, Default)]
pub struct ContextInfo {
    pub used: u64,
    pub total: u64,
    pub system_prompt_tokens: u64,
    pub tool_definitions_count: u64,
    pub tool_definitions_tokens: u64,
    pub compaction_count: u64,
    pub turn_count: u64,
    pub tool_call_count: u64,
    /// Total conversation items (system + user + assistant + tool responses).
    pub message_count: u64,
    /// Bytes/4 estimate of all non-system conversation items.
    pub message_tokens: u64,
    pub free_tokens: u64,
    pub usage_pct: u8,
    /// The resolved auto-compact threshold percent (0-100) for the active model at the time this snapshot was captured.
    pub auto_compact_threshold_percent: u8,
    /// Itemized usage rows (skills, workflows, MCP servers, AGENTS.md).
    /// Empty on partial snapshots.
    pub usage_categories: Vec<TokenUsageCategory>,
}

/// An itemized entry in the context usage breakdown.
#[derive(Debug, Clone, Default)]
pub struct TokenUsageCategory {
    pub label: String,
    pub tokens: u64,
    pub detail: Option<String>,
}

fn count_detail(count: u64, noun: &str) -> String {
    let suffix = if count == 1 { "" } else { "s" };
    format!("{count} {noun}{suffix}")
}

/// Categorical bar: each cell uses its category's glyph and color, filled left-to-right in legend order; free capacity is muted outlines.
/// The informational rows never enter the bar.
#[derive(Debug, Clone)]
pub struct ContextInfoBlock {
    /// The captured context-window snapshot.
    pub snapshot: ContextInfo,
    /// Active model name at the time of capture (display-only).
    pub model: String,
}

/// Shape of the categorical bar: how the 100 cells are laid out. Both shapes hold the same 100 cells, so the visual
/// breakdown (which categories occupy which share of the bar) is identical. Only the aspect ratio changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BarLayout {
    /// Cells per row.
    row_len: usize,
    /// Number of rows.
    rows: usize,
}

impl BarLayout {
    /// 5 rows × 20 cells.
    /// Renders in ~39 columns (20 glyphs + 19 separator spaces).
    const WIDE: Self = Self {
        row_len: 20,
        rows: 5,
    };

    /// 10 rows × 10 cells.
    /// Renders in ~19 columns (10 glyphs + 9 separator spaces).
    const NARROW: Self = Self {
        row_len: 10,
        rows: 10,
    };

    /// Terminal width (in columns) at which the bar switches from [`Self::WIDE`] to [`Self::NARROW`]. The wide layout
    /// needs 39 columns just for the bar. 50 leaves ~11 columns of margin. 50 is also roughly where the legend rows ·
    /// 12 tools`) start to word-wrap. The breakpoint therefore matches the rest of the block's responsive behavior.
    const NARROW_BREAKPOINT: u16 = 50;

    /// Choose a layout that fits the available terminal width.
    fn for_width(width: u16) -> Self {
        if width < Self::NARROW_BREAKPOINT {
            Self::NARROW
        } else {
            Self::WIDE
        }
    }

    /// Total cells in the bar; always 100 for both shipped layouts.
    /// Kept as a method so a future layout with a different cell count doesn't silently misalign with the legend percentages.
    const fn total(self) -> usize {
        self.row_len * self.rows
    }
}

/// One legend or informational row, before column formatting.
struct LegendRow {
    glyph: &'static str,
    color: Color,
    label: String,
    tokens: u64,
    detail: Option<String>,
}

/// Column widths for the legend and informational rows, measured from the rows that actually render.
/// Token counts, percentages, and detail counts then stay aligned no matter the labels or magnitudes.
struct RowLayout {
    label_width: usize,
    tokens_width: usize,
    percent_width: usize,
    count_width: usize,
}

impl RowLayout {
    /// Measure column widths over every row that will render.
    /// Widths are in codepoints, not bytes.
    fn measure<'a>(rows: impl Iterator<Item = &'a LegendRow> + Clone, total: u64) -> Self {
        Self {
            label_width: rows
                .clone()
                .map(|r| r.label.chars().count())
                .max()
                .unwrap_or(0)
                + 1,
            tokens_width: rows
                .clone()
                .map(|r| fmt_tok(r.tokens).chars().count())
                .max()
                .unwrap_or(0),
            percent_width: rows
                .clone()
                .map(|r| Self::percent(r.tokens, total).chars().count())
                .max()
                .unwrap_or(0),
            count_width: rows
                .filter_map(|r| r.detail.as_deref())
                .filter_map(|d| d.split(' ').next().map(|n| n.chars().count()))
                .max()
                .unwrap_or(0),
        }
    }

    /// The parenthesized share of the window, e.g. `"(0.6%)"`.
    fn percent(tokens: u64, total: u64) -> String {
        format!("({})", percent_of_window(tokens, total))
    }

    /// The row's numeric cells: tokens and percent, each right-aligned.
    fn cells(&self, tokens: u64, total: u64) -> String {
        format!(
            "{:>tokens_width$} tokens   {:>percent_width$}",
            fmt_tok(tokens),
            Self::percent(tokens, total),
            tokens_width = self.tokens_width,
            percent_width = self.percent_width,
        )
    }

    /// The detail suffix with the leading count right-aligned so the nouns line up: `" · 25 tools"` over `" ·  1 server"`.
    /// Details follow the `TokenUsageCategory::detail` count-then-noun convention; text with no space renders unaligned.
    fn detail_suffix(&self, detail: &str) -> String {
        match detail.split_once(' ') {
            Some((count, rest)) => {
                format!(
                    " \u{00b7} {count:>count_width$} {rest}",
                    count_width = self.count_width
                )
            }
            None => format!(" \u{00b7} {detail}"),
        }
    }

    /// Render one row.
    /// WIDE: a single column-aligned line.
    /// NARROW: glyph and label on the first line, numeric data indented by one space on the second so it clusters under the label.
    fn render(
        &self,
        row: &LegendRow,
        bar: BarLayout,
        total: u64,
        label_style: Style,
        muted: Style,
    ) -> Vec<Line<'static>> {
        let glyph = Span::styled(format!("{} ", row.glyph), Style::default().fg(row.color));
        let suffix = row.detail.as_deref().map(|d| self.detail_suffix(d));
        if bar == BarLayout::NARROW {
            let first = Line::from(vec![glyph, Span::styled(row.label.clone(), label_style)]);
            let mut second = vec![
                Span::raw(" "),
                Span::styled(
                    format!(
                        "{} tokens   {}",
                        fmt_tok(row.tokens),
                        Self::percent(row.tokens, total)
                    ),
                    muted,
                ),
            ];
            if let Some(extra) = suffix {
                second.push(Span::styled(extra, muted));
            }
            vec![first, Line::from(second)]
        } else {
            let mut spans = vec![
                glyph,
                Span::styled(
                    format!(
                        "{:<label_width$}",
                        row.label,
                        label_width = self.label_width
                    ),
                    label_style,
                ),
                Span::raw(" "),
                Span::styled(self.cells(row.tokens, total), muted),
            ];
            if let Some(extra) = suffix {
                spans.push(Span::styled(extra, muted));
            }
            vec![Line::from(spans)]
        }
    }
}

impl ContextInfoBlock {
    pub fn new(snapshot: ContextInfo, model: impl Into<String>) -> Self {
        Self {
            snapshot,
            model: model.into(),
        }
    }

    /// Build the styled lines for an arbitrary content width.
    /// The usage modal's "Context usage" tab reuses it, so the modal and the minimal-mode scrollback block render the same breakdown.
    pub fn lines_for_width(&self, theme: &Theme, width: u16) -> Vec<Line<'static>> {
        self.build_lines(theme, BarLayout::for_width(width))
    }

    /// Build the styled lines using the supplied theme and bar layout. Called from `output()` on every redraw so theme
    /// switches take effect without re-running `/context`. The theme is passed in rather than re-resolved here.
    fn build_lines(&self, theme: &Theme, bar: BarLayout) -> Vec<Line<'static>> {
        let snapshot = &self.snapshot;
        let model = &self.model;

        let used = snapshot.used;
        let total = snapshot.total;
        let usage_pct = snapshot.usage_pct;
        let tool_tokens = snapshot.tool_definitions_tokens;
        let tool_count = snapshot.tool_definitions_count;
        let turn_count = snapshot.turn_count;
        let tool_call_count = snapshot.tool_call_count;
        let compaction_count = snapshot.compaction_count;

        let tokens = window_tokens(
            used,
            total,
            snapshot.system_prompt_tokens,
            snapshot.message_tokens,
        );
        let system_tokens = tokens.system;
        let message_tokens = tokens.messages;
        let overhead_tokens = tokens.overhead;
        let free_tokens = tokens.free;

        let muted = theme.muted();
        let primary = Style::default()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD);

        // Per-category colors used in both the bar and the legend so the two visualizations are scannable side-by-side
        // Messages get the brightest treatment (primary): they dominate the breakdown and are the conversation the user is actually steering
        // System prompt uses the same diamond glyph as messages but in muted gray so it reads as a quiet base layer
        let system_color = quantize(theme.gray_bright); // gray
        let tools_color = quantize(theme.accent_skill); // teal / skill accent
        let messages_color = quantize(theme.text_primary); // primary (white)
        let empty_color = quantize(theme.gray_dim); // free / outline
        let overhead_color = quantize(theme.accent_verify);

        // Each category gets its own glyph and color so the bar reads as a stacked breakdown at a glance. Routed through
        // `glyphs` so the diamonds degrade to CP437-safe stand-ins (`◆`→`♦`, `◇`→`○`). Legacy Windows consoles can't
        // render the U+25Cx diamonds.
        let system_glyph = crate::engine::render::glyphs::diamond_filled(); // ◆ (gray)
        let tools_glyph = crate::engine::render::glyphs::diamond_dotted(); // ◈
        let messages_glyph = crate::engine::render::glyphs::diamond_filled(); // ◆ (primary)
        let free_glyph = crate::engine::render::glyphs::diamond_hollow(); // ◇
        let overhead_glyph = crate::engine::render::glyphs::diamond_filled(); // ◆ (violet)

        let total_cells = bar.total();
        let cells_for = |tokens: u64| -> usize {
            if total == 0 {
                0
            } else {
                ((tokens as f64 / total as f64) * total_cells as f64).round() as usize
            }
        };
        let used_cells = cells_for(used).min(total_cells);
        let system_cells = cells_for(system_tokens).min(used_cells);
        let messages_cells = cells_for(message_tokens).min(used_cells - system_cells);
        let overhead_cells = used_cells - system_cells - messages_cells;
        let free_cells = total_cells - used_cells;

        let mut cells: Vec<(&'static str, Color)> = Vec::with_capacity(total_cells);
        for _ in 0..system_cells {
            cells.push((system_glyph, system_color));
        }
        for _ in 0..messages_cells {
            cells.push((messages_glyph, messages_color));
        }
        for _ in 0..overhead_cells {
            cells.push((overhead_glyph, overhead_color));
        }
        for _ in 0..free_cells {
            cells.push((free_glyph, empty_color));
        }
        debug_assert_eq!(cells.len(), total_cells);

        let mut bar_lines: Vec<Line<'static>> = Vec::with_capacity(bar.rows);
        for row_idx in 0..bar.rows {
            let start = row_idx * bar.row_len;
            let end = (start + bar.row_len).min(cells.len());
            let mut spans = Vec::with_capacity(bar.row_len * 2);
            for (i, (glyph, color)) in cells.get(start..end).into_iter().flatten().enumerate() {
                if i > 0 {
                    spans.push(Span::raw(" "));
                }
                spans.push(Span::styled(
                    (*glyph).to_string(),
                    Style::default().fg(*color),
                ));
            }
            bar_lines.push(Line::from(spans));
        }

        // Tool definitions are already in Reasoning/overhead
        // Usage categories are already in Messages
        let mut legend_rows = vec![
            LegendRow {
                glyph: system_glyph,
                color: system_color,
                label: "System prompt".to_string(),
                tokens: system_tokens,
                detail: None,
            },
            LegendRow {
                glyph: messages_glyph,
                color: messages_color,
                label: "Messages".to_string(),
                tokens: message_tokens,
                detail: None,
            },
        ];
        if overhead_tokens > 0 {
            legend_rows.push(LegendRow {
                glyph: overhead_glyph,
                color: overhead_color,
                label: "Reasoning/overhead".to_string(),
                tokens: overhead_tokens,
                detail: None,
            });
        }
        legend_rows.push(LegendRow {
            glyph: free_glyph,
            color: empty_color,
            label: "Free".to_string(),
            tokens: free_tokens,
            detail: None,
        });
        let info_rows: Vec<LegendRow> = std::iter::once(LegendRow {
            glyph: tools_glyph,
            color: tools_color,
            label: "Tool definitions".to_string(),
            tokens: tool_tokens,
            detail: Some(count_detail(tool_count, "tool")),
        })
        .chain(snapshot.usage_categories.iter().map(|c| LegendRow {
            glyph: tools_glyph,
            color: tools_color,
            label: c.label.clone(),
            tokens: c.tokens,
            detail: c.detail.clone(),
        }))
        .collect();
        let layout = RowLayout::measure(legend_rows.iter().chain(info_rows.iter()), total);
        let label_style = Style::default().fg(theme.text_secondary);

        let mut lines: Vec<Line<'static>> = vec![
            // Header: bold white "Context"
            Line::from(Span::styled("Context", primary)),
            // Blank row between header and the at-a-glance summary
            Line::from(""),
            // Sub-header: token totals and percent. Uses `text_secondary` for a touch more contrast than `muted` so the
            // at-a-glance numbers stand apart from the breakdown/footer rows. The percentage is recomputed from `used / total`
            // so we get two decimal places of precision.
            Line::from(Span::styled(
                format!(
                    "{} / {} tokens ({:.2}%)",
                    fmt_tok_big(used),
                    fmt_tok_big(total),
                    precise_usage_percent(used, total),
                ),
                Style::default().fg(theme.text_secondary),
            )),
            // Model name (one step dimmer than the tokens line so it reads as a supporting caption rather than the primary number)
            Line::from(Span::styled(
                model.to_string(),
                Style::default().fg(theme.gray_bright),
            )),
            // Blank space before the bar
            Line::from(""),
        ];
        lines.extend(bar_lines);
        lines.push(Line::from(""));
        for row in &legend_rows {
            lines.extend(layout.render(row, bar, total, label_style, muted));
        }
        lines.push(Line::from(""));

        lines.push(Line::from(Span::styled("Already counted above", muted)));
        for row in &info_rows {
            lines.extend(layout.render(row, bar, total, label_style, muted));
        }
        lines.push(Line::from(""));

        // `threshold_tokens` uses `div_ceil` rather than truncating integer division. Without `div_ceil`, tiny totals
        // could produce `remaining == 0` while `usage_pct < threshold_percent`. That would show `~0 tokens remaining` for
        // a context window that isn't actually at the threshold.
        if total > 0 {
            let threshold_percent = snapshot.auto_compact_threshold_percent;
            let threshold_tokens = total.saturating_mul(threshold_percent as u64).div_ceil(100);
            let remaining = threshold_tokens.saturating_sub(used);
            let (text, style) = if usage_pct >= threshold_percent {
                (
                    format!("Auto-compact triggers next turn (at {threshold_percent}%)"),
                    Style::default().fg(quantize(theme.warning)),
                )
            } else {
                // Use `fmt_tok_big` (same as the header) so the remaining count rolls over to `m` for wide context windows
                // A 4m window at 60% reads `~1.0m tokens remaining`, not `~1000k tokens remaining`
                (
                    format!(
                        "Auto-compact at {threshold_percent}% \u{00b7} ~{} tokens remaining",
                        fmt_tok_big(remaining)
                    ),
                    muted,
                )
            };
            lines.push(Line::from(Span::styled(text, style)));
            lines.push(Line::from(""));
        }

        // Footer stats
        lines.push(Line::from(Span::styled(
            format!(
                "Turns: {turn_count} \u{00b7} Tool calls: {tool_call_count} \u{00b7} Compactions: {compaction_count}"
            ),
            muted,
        )));

        // The tip about approaching auto-compact only shows in the gap between the "getting close" mark (80%) and the
        // actual auto-compact threshold. A second warning-styled tip suggesting a manual `/compact` would just stack
        // visually and contradict itself.
        if (80..snapshot.auto_compact_threshold_percent).contains(&usage_pct) {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Tip: run /compact to free up context space.".to_string(),
                Style::default().fg(quantize(theme.warning)),
            )));
        }

        lines
    }
}

/// Format a token count compactly (`123`, `1.2k`, `999k`). The cutover from `{.1}k` to plain `{}k` happens at
/// 99_500 (not 100_000) to avoid a precision discontinuity.
fn fmt_tok(n: u64) -> String {
    if n >= 99_500 {
        // Round half-up to the nearest 1k; equivalent to `(n + 500) / 1000` for u64, which avoids the f64 rounding artifact described above
        format!("{}k", (n + 500) / 1000)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        format!("{n}")
    }
}

/// Compute `used / total * 100` as f64 with safe handling of `total == 0`.
fn precise_usage_percent(used: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (used as f64 / total as f64) * 100.0
    }
}

/// Like [`fmt_tok`] but rolls over to `1.0m` at one million.
/// Used for the at-a-glance totals line so a 1M / 2M / 4M context window reads naturally as `1.0m` rather than `1000k`.
/// Per-category legend rows stay on [`fmt_tok`] so a fractional-million breakdown still shows the finer-grained `k` resolution.
fn fmt_tok_big(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}m", n as f64 / 1_000_000.0)
    } else {
        fmt_tok(n)
    }
}

/// `system`, `messages`, `overhead`, and `free` sum to `total`.
struct WindowTokens {
    system: u64,
    messages: u64,
    overhead: u64,
    free: u64,
}

/// `system_tokens` and `message_tokens` can exceed `used`.
/// `used` can exceed `total`.
fn window_tokens(used: u64, total: u64, system_tokens: u64, message_tokens: u64) -> WindowTokens {
    let used = used.min(total);
    let system = system_tokens.min(used);
    let messages = message_tokens.min(used - system);
    WindowTokens {
        system,
        messages,
        overhead: used - system - messages,
        free: total.saturating_sub(used),
    }
}

/// Format a category's share of the total context window as a percentage.
fn percent_of_window(part: u64, total: u64) -> String {
    if total == 0 {
        return "-".to_string();
    }
    // Tiny nonzero shares floor at 0.1% so the column stays clean.
    let p = ((part as f64 / total as f64) * 100.0).max(if part > 0 { 0.1 } else { 0.0 });
    if p < 10.0 {
        format!("{p:.1}%")
    } else {
        format!("{p:.0}%")
    }
}

impl BlockContent for ContextInfoBlock {
    fn output(&self, ctx: &BlockContext) -> BlockOutput {
        let theme = Theme::current();
        // Responsive bar shape: narrow terminals get a 10×10 bar so it doesn't get clipped or pushed under the legend
        let bar = BarLayout::for_width(ctx.width);
        let styled_lines = self.build_lines(&theme, bar);
        let wrapped = word_wrap_lines(styled_lines, ctx.width as usize);
        let all_lines: Vec<BlockLine> = wrapped
            .into_iter()
            .map(|line| BlockLine::styled(line).with_selection_range(Some(0)))
            .collect();

        // Apply max_lines budget if set
        let lines = if let Some(max) = ctx.max_lines {
            let max = max as usize;
            if all_lines.len() > max && max > 0 {
                let take_count = if max > 1 { max - 1 } else { 1 };
                let mut truncated: Vec<BlockLine> =
                    all_lines.into_iter().take(take_count).collect();
                if let Some(last) = truncated.last_mut() {
                    let content_end = last.content.spans.len();
                    last.content
                        .spans
                        .push(Span::styled(" \u{2026}".to_string(), theme.muted()));
                    last.selectable =
                        crate::ui::scrollback::types::Selectable::Spans(0..content_end);
                }
                truncated
            } else {
                all_lines
            }
        } else {
            all_lines
        };

        if lines.is_empty() {
            BlockOutput {
                lines: vec![BlockLine::styled(Line::from("")).with_selection_range(Some(0))],
            }
        } else {
            BlockOutput { lines }
        }
    }

    fn accent(&self, _ctx: &BlockContext) -> Option<AccentStyle> {
        None
    }

    fn has_vpad_for(&self, _appearance: &AppearanceConfig) -> bool {
        false // Compact like SystemMessageBlock
    }

    fn has_raw_mode(&self) -> bool {
        false
    }

    fn is_foldable(&self) -> bool {
        false
    }

    fn is_selectable(&self) -> bool {
        false
    }

    fn is_groupable(&self) -> bool {
        true
    }
}
