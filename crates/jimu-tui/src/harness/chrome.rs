//! Host-customizable chrome slots, slash commands, autocomplete providers, and localized copy.

use super::{
    BoxFuture, ContextMeterSnapshot, InteractionRequest, NoticeLevel, PlanDecision, QuestionAnswer,
    QuestionPrompt, TuiHarness, UsageStats,
};
use crate::engine::{
    render::line_utils::truncate_line,
    theme::{Theme, ThemeOverrides},
};
use futures_channel::{mpsc, oneshot};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Localized UI copy. All defaults are concise and brand-neutral.
#[derive(Debug, Clone)]
pub struct UiText {
    pub deny: String,
    pub allow: String,
    pub always_allow: String,
    pub denied: String,
    pub approve_plan: String,
    pub reject_plan: String,
    pub compacting: String,
    pub compacted: String,
    pub compact_unneeded: String,
    pub no_skills: String,
    pub no_tasks: String,
    pub no_mcp: String,
    pub unknown_command: String,
    pub input_placeholder: String,
}

impl Default for UiText {
    fn default() -> Self {
        Self {
            deny: "Deny".into(),
            allow: "Allow".into(),
            always_allow: "Always allow for this session".into(),
            denied: "Permission denied".into(),
            approve_plan: "Approve plan".into(),
            reject_plan: "Reject plan".into(),
            compacting: "Summarizing conversation…".into(),
            compacted: "Conversation summarized".into(),
            compact_unneeded: "Conversation is already short".into(),
            no_skills: "No skills available".into(),
            no_tasks: "No running background tasks".into(),
            no_mcp: "No MCP servers connected".into(),
            unknown_command: "Unknown command".into(),
            input_placeholder: String::new(),
        }
    }
}

/// Control request sent from a [`TuiController`] to the live TUI loop.
pub enum ControlCommand {
    Interaction(InteractionRequest),
    Notice {
        level: NoticeLevel,
        message: String,
    },
    Toast(String),
    ToggleScreenMode,
    SetTheme(String),
    SetThemeOverrides(Option<ThemeOverrides>),
    RegisterCustomTheme {
        name: String,
        overrides: ThemeOverrides,
    },
    ReplaceCustomThemes(BTreeMap<String, ThemeOverrides>),
    ClearTranscript,
    Quit,
    RequestRedraw,
    InvalidateHistoryLayout,
}

/// Thread-safe handle allowing hosts, middleware, and slash commands to interact with the live TUI.
#[derive(Clone)]
pub struct TuiController {
    tx: mpsc::UnboundedSender<ControlCommand>,
    text: Arc<UiText>,
}

impl TuiController {
    pub fn channel(text: Arc<UiText>) -> (Self, mpsc::UnboundedReceiver<ControlCommand>) {
        let (tx, rx) = mpsc::unbounded();
        (Self { tx, text }, rx)
    }

    pub fn text(&self) -> &UiText {
        &self.text
    }

    pub fn send_interaction(&self, req: InteractionRequest) -> bool {
        self.tx
            .unbounded_send(ControlCommand::Interaction(req))
            .is_ok()
    }

    pub async fn show_select(
        &self,
        title: impl Into<String>,
        options: Vec<String>,
    ) -> Option<usize> {
        let (tx, rx) = oneshot::channel();
        self.send_interaction(InteractionRequest::Select {
            title: title.into(),
            options,
            reply: tx,
        });
        rx.await.ok().flatten()
    }

    pub async fn show_confirm(&self, title: impl Into<String>) -> bool {
        self.show_select(title, vec![self.text.deny.clone(), self.text.allow.clone()])
            .await
            == Some(1)
    }

    pub async fn ask_question(&self, prompt: QuestionPrompt) -> Option<QuestionAnswer> {
        let (tx, rx) = oneshot::channel();
        self.send_interaction(InteractionRequest::AskQuestion { prompt, reply: tx });
        rx.await.ok().flatten()
    }

    pub async fn approve_plan(
        &self,
        title: impl Into<String>,
        markdown: impl Into<String>,
    ) -> Option<PlanDecision> {
        let (tx, rx) = oneshot::channel();
        self.send_interaction(InteractionRequest::ApprovePlan {
            title: title.into(),
            markdown: markdown.into(),
            reply: tx,
        });
        rx.await.ok()
    }

    pub fn push_notice(&self, level: NoticeLevel, message: impl Into<String>) {
        let _ = self.tx.unbounded_send(ControlCommand::Notice {
            level,
            message: message.into(),
        });
    }

    pub fn show_toast(&self, message: impl Into<String>) {
        let _ = self
            .tx
            .unbounded_send(ControlCommand::Toast(message.into()));
    }

    pub fn toggle_screen_mode(&self) {
        let _ = self.tx.unbounded_send(ControlCommand::ToggleScreenMode);
    }

    pub fn set_theme(&self, name: impl Into<String>) {
        let _ = self
            .tx
            .unbounded_send(ControlCommand::SetTheme(name.into()));
    }

    pub fn set_theme_overrides(&self, overrides: Option<ThemeOverrides>) {
        let _ = self
            .tx
            .unbounded_send(ControlCommand::SetThemeOverrides(overrides));
    }

    pub fn register_custom_theme(&self, name: impl Into<String>, overrides: ThemeOverrides) {
        let _ = self.tx.unbounded_send(ControlCommand::RegisterCustomTheme {
            name: name.into(),
            overrides,
        });
    }

    pub fn replace_custom_themes(&self, themes: BTreeMap<String, ThemeOverrides>) {
        let _ = self
            .tx
            .unbounded_send(ControlCommand::ReplaceCustomThemes(themes));
    }

    pub fn clear_transcript(&self) {
        let _ = self.tx.unbounded_send(ControlCommand::ClearTranscript);
    }

    pub fn quit(&self) {
        let _ = self.tx.unbounded_send(ControlCommand::Quit);
    }

    /// Trigger a lightweight frame redraw (chrome, status line, spinner, meters) without
    /// invalidating cached conversation history layouts.
    pub fn request_redraw(&self) {
        let _ = self.tx.unbounded_send(ControlCommand::RequestRedraw);
    }

    /// Invalidate cached scrollback history layouts and redraw (use when theme colors or
    /// `UserMessage`/`ToolRow` decorations change).
    pub fn invalidate_history_layout(&self) {
        let _ = self
            .tx
            .unbounded_send(ControlCommand::InvalidateHistoryLayout);
    }
}

/// Read-only snapshot supplied to [`ChromeSlots`] on each draw pass.
pub struct ChromeState<'a> {
    pub running: bool,
    pub usage: &'a UsageStats,
    pub context: Option<&'a ContextMeterSnapshot>,
    pub model_name: Option<&'a str>,
    pub queued_count: usize,
    pub theme: &'a Theme,
    pub width: u16,
    pub cwd: &'a Path,
    pub git_branch: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub permission_mode: Option<&'a str>,
    pub fast_mode: bool,
    pub session_id: Option<&'a str>,
}

/// Lightweight synchronous decoration returned by [`ChromeSlots`] component hooks.
///
/// Leaving `replace` as `None` preserves native `jimu-tui` highlighting, chips, and diffs while
/// allowing hosts to wrap blocks with prefix/suffix lines or a custom left border color.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SurfaceDecoration {
    pub prefix_lines: Vec<Line<'static>>,
    pub suffix_lines: Vec<Line<'static>>,
    pub left_border: Option<Color>,
    pub replace: Option<Vec<Line<'static>>>,
}

/// Read-only tool row metadata passed to [`ChromeSlots::decorate_tool_row`].
#[derive(Debug, Clone, Copy)]
pub struct ToolRowView<'a> {
    pub name: &'a str,
    pub running: bool,
    pub is_error: bool,
    pub width: u16,
}

/// Customizable outer chrome regions and lightweight component decoration hooks.
/// Returning an empty `Vec` or `None` consumes zero rows and zero overhead.
pub trait ChromeSlots: Send + Sync {
    fn welcome(&self, _state: &ChromeState<'_>) -> Vec<Line<'static>> {
        Vec::new()
    }

    fn header(&self, _state: &ChromeState<'_>) -> Vec<Line<'static>> {
        Vec::new()
    }

    /// Optional rows rendered directly above the prompt composer (between transcript and prompt).
    fn above_prompt(&self, _state: &ChromeState<'_>) -> Vec<Line<'static>> {
        Vec::new()
    }

    fn footer(&self, _state: &ChromeState<'_>) -> Vec<Line<'static>> {
        Vec::new()
    }

    fn status_line(&self, _state: &ChromeState<'_>) -> Option<Vec<Line<'static>>> {
        None
    }

    /// Optional dynamic status flags appended to the prompt box's bottom border.
    fn status_flags(&self, _state: &ChromeState<'_>) -> Vec<String> {
        Vec::new()
    }

    /// Optional synchronous decoration for rendered user messages.
    fn decorate_user_message(
        &self,
        _text: &str,
        _image_count: usize,
        _width: u16,
        _theme: &Theme,
    ) -> Option<SurfaceDecoration> {
        None
    }

    /// Optional synchronous decoration for rendered tool call rows.
    fn decorate_tool_row(
        &self,
        _row: &ToolRowView<'_>,
        _theme: &Theme,
    ) -> Option<SurfaceDecoration> {
        None
    }

    /// Optional replacement span for the active running spinner/indicator in the status bar.
    fn spinner_override(&self, _state: &ChromeState<'_>) -> Option<Span<'static>> {
        None
    }
}

struct DefaultChrome;
impl ChromeSlots for DefaultChrome {}

/// Host-configurable chrome wrapper, welcome card metadata, and OSC 7501 status reporting.
#[derive(Clone)]
pub struct ChromeConfig {
    pub slots: Arc<dyn ChromeSlots>,
    /// Render the built-in 1-row top header bar in fullscreen transcript view. Defaults to `false`.
    pub show_header: bool,
    /// Optional application title shown in header/welcome card and emitted as OSC 7501 `app=`.
    pub app_name: Option<String>,
    /// Optional version label shown alongside `app_name` in the welcome hero card.
    pub app_version: Option<String>,
    /// Optional subtitle shown beneath the welcome title.
    pub welcome_subtitle: Option<String>,
    /// Optional heading for `welcome_notes` (defaults to `"What's new"` when notes are non-empty).
    pub welcome_notes_title: Option<String>,
    /// Optional bullet points displayed inside the welcome hero card.
    pub welcome_notes: Vec<String>,
    /// Optional subtle footer hint rendered under the bottom-right of the welcome prompt card.
    pub footer_hint: Option<String>,
    /// Optional short status labels rendered on the prompt box's bottom border (e.g. `"approve"`).
    pub status_flags: Vec<String>,
    /// Emit OSC 7501 Program Status Protocol updates (`idle`/`working`/`blocked`/`done`/`error`/`clear`).
    /// Enabled by default.
    pub osc7501_enabled: bool,
}

impl Default for ChromeConfig {
    fn default() -> Self {
        Self {
            slots: Arc::new(DefaultChrome),
            show_header: false,
            app_name: None,
            app_version: None,
            welcome_subtitle: None,
            welcome_notes_title: None,
            welcome_notes: Vec::new(),
            footer_hint: None,
            status_flags: Vec::new(),
            osc7501_enabled: true,
        }
    }
}

/// Context passed to a custom [`SlashCommand`].
pub struct SlashContext {
    pub args: String,
    pub harness: Arc<dyn TuiHarness>,
    pub controller: TuiController,
}

/// Host-registered slash command (`/name ...`).
pub trait SlashCommand: Send + Sync {
    /// Command name without the leading `/`.
    fn name(&self) -> &str;

    /// Concise one-line description shown in the slash dropdown and `/help`.
    fn description(&self) -> &str {
        ""
    }

    /// Optional inline argument completions for `/<name> <query>`.
    fn completions(&self, _query: &str) -> Vec<CompletionItem> {
        Vec::new()
    }

    /// Execute asynchronously while the TUI continues rendering dialogs and streaming output.
    fn run(&self, ctx: SlashContext) -> BoxFuture<'_, ()>;
}

/// Completion item returned by an [`AutocompleteProvider`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    pub token: String,
    pub description: Option<String>,
}

/// Suggests completion candidates for the token before the cursor.
pub trait AutocompleteProvider: Send + Sync {
    fn suggestions(&self, token: &str) -> Vec<CompletionItem>;
}

/// Built-in provider combining slash commands, skills, `@file` paths rooted in an optional
/// directory, and arbitrary extra host providers.
#[derive(Default)]
pub struct CombinedAutocompleteProvider {
    pub commands: Vec<(String, String)>,
    pub skills: Vec<(String, String)>,
    pub directory: Option<PathBuf>,
    pub providers: Vec<Arc<dyn AutocompleteProvider>>,
}

impl CombinedAutocompleteProvider {
    /// Suggest skill-only completions for mid-text `/query` or `$query` tokens.
    pub fn skill_suggestions(&self, token: &str) -> Vec<CompletionItem> {
        let (prefix, query) = if let Some(q) = token.strip_prefix('/') {
            ('/', q)
        } else if let Some(q) = token.strip_prefix('$') {
            ('$', q)
        } else {
            return Vec::new();
        };
        if !is_valid_slash_query(query) {
            return Vec::new();
        }
        let mut out = Vec::new();
        for (name, desc) in &self.skills {
            if fuzzy_subseq(name, query) || fuzzy_subseq(&format!("skill:{name}"), query) {
                out.push(CompletionItem {
                    token: format!("{prefix}{name}"),
                    description: (!desc.is_empty()).then(|| desc.clone()),
                });
            }
        }
        out.sort_by(|a, b| a.token.cmp(&b.token));
        out
    }
}

/// Matches Claude Code's slash-menu character guard: once a second `/` or other non-command char
/// appears (e.g. `/Users/foo`), slash completion immediately closes.
pub fn is_valid_slash_query(query: &str) -> bool {
    query
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '-' | '_'))
}

impl AutocompleteProvider for CombinedAutocompleteProvider {
    fn suggestions(&self, token: &str) -> Vec<CompletionItem> {
        let mut out = Vec::new();
        if let Some(query) = token.strip_prefix('/') {
            if is_valid_slash_query(query) {
                for (name, desc) in &self.commands {
                    if fuzzy_subseq(name, query) {
                        out.push(CompletionItem {
                            token: format!("/{name}"),
                            description: (!desc.is_empty()).then(|| desc.clone()),
                        });
                    }
                }
                for (name, desc) in &self.skills {
                    let collides = self.commands.iter().any(|(cmd, _)| cmd == name);
                    let display_name = if collides {
                        format!("skill:{name}")
                    } else {
                        name.clone()
                    };
                    if fuzzy_subseq(name, query) || fuzzy_subseq(&display_name, query) {
                        out.push(CompletionItem {
                            token: format!("/{display_name}"),
                            description: (!desc.is_empty()).then(|| desc.clone()),
                        });
                    }
                }
            }
        } else if let Some(query) = token.strip_prefix('$') {
            if is_valid_slash_query(query) {
                for (name, desc) in &self.skills {
                    if fuzzy_subseq(name, query) {
                        out.push(CompletionItem {
                            token: format!("${name}"),
                            description: (!desc.is_empty()).then(|| desc.clone()),
                        });
                    }
                }
            }
        } else if let Some(query) = token.strip_prefix('@')
            && let Some(root) = &self.directory
        {
            let (parent, name) = query
                .rsplit_once('/')
                .map_or(("", query), |(p, n)| (&query[..p.len() + 1], n));
            if let Ok(entries) = std::fs::read_dir(root.join(Path::new(parent))) {
                for entry in entries.flatten() {
                    let file_name = entry.file_name().to_string_lossy().into_owned();
                    if fuzzy_subseq(&file_name, name) {
                        let suffix = if entry.file_type().is_ok_and(|f| f.is_dir()) {
                            "/"
                        } else {
                            ""
                        };
                        out.push(CompletionItem {
                            token: format!("@{parent}{file_name}{suffix}"),
                            description: None,
                        });
                    }
                }
            }
        }
        out.sort_by(|a, b| a.token.cmp(&b.token));
        for provider in &self.providers {
            for item in provider.suggestions(token) {
                if !out.iter().any(|existing| existing.token == item.token) {
                    out.push(item);
                }
            }
        }
        out
    }
}

pub(crate) fn fuzzy_subseq(text: &str, query: &str) -> bool {
    let lower = text.to_lowercase();
    let mut chars = lower.chars();
    query.to_lowercase().chars().all(|c| chars.any(|t| t == c))
}

fn short_dir_name(cwd: &Path) -> String {
    cwd.file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| crate::engine::util::display_location_path(cwd))
}

fn fmt_status_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 10_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else if n < 1_000_000 {
        format!("{}K", n / 1_000)
    } else if n < 10_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else {
        format!("{}M", n / 1_000_000)
    }
}

/// Build the built-in single-row bottom status bar for the given [`ChromeState`].
///
/// Hosts that intercept [`ChromeSlots::status_line`] (e.g. JS `ui.render("StatusLine")` wrappers)
/// can pass this line as native content so `next(e)` preserves the default model/directory/context
/// readout.
pub fn render_default_status_line(state: &ChromeState<'_>) -> Line<'static> {
    render_default_status_line_with_spinner(state, None)
}

/// Variant of [`render_default_status_line`] accepting an optional running indicator override
/// (such as [`ChromeSlots::spinner_override`]).
pub fn render_default_status_line_with_spinner(
    state: &ChromeState<'_>,
    spinner_override: Option<Span<'static>>,
) -> Line<'static> {
    let width = usize::from(state.width);
    if width == 0 {
        return Line::default();
    }
    let theme = state.theme;
    let bg = theme.bg_base;
    let sep = || Span::styled("  ", Style::default().bg(bg));

    let mut left: Vec<Span<'static>> = Vec::new();
    if let Some(model) = state.model_name.filter(|m| !m.is_empty()) {
        left.push(Span::styled(
            model.to_owned(),
            Style::default().fg(theme.accent_model).bg(bg),
        ));
        if let Some(effort) = state.effort.filter(|e| !e.is_empty()) {
            left.push(Span::styled(format!(" {effort}"), theme.muted().bg(bg)));
        }
        if state.fast_mode {
            left.push(Span::styled(
                " fast",
                Style::default().fg(theme.warning).bg(bg),
            ));
        }
    } else if state.fast_mode {
        left.push(Span::styled(
            "fast",
            Style::default().fg(theme.warning).bg(bg),
        ));
    }

    let dir = short_dir_name(state.cwd);
    if !dir.is_empty() {
        if !left.is_empty() {
            left.push(sep());
        }
        left.push(Span::styled(dir, theme.muted().bg(bg)));
    }
    if let Some(branch) = state.git_branch.filter(|b| !b.is_empty()) {
        if !left.is_empty() {
            left.push(Span::styled(" ", Style::default().bg(bg)));
        }
        left.push(Span::styled(
            branch.to_owned(),
            Style::default().fg(theme.gray_bright).bg(bg),
        ));
    }

    let mut right: Vec<Span<'static>> = Vec::new();
    if let Some(ctx) = state.context
        && ctx.context_window > 0
    {
        let used = ctx.tokens.unwrap_or(ctx.estimated_tokens);
        let cap = u64::from(ctx.context_window);
        let pct_f = ctx.percent.unwrap_or_else(|| {
            if cap > 0 {
                (used as f64 / cap as f64) * 100.0
            } else {
                0.0
            }
        });
        let pct = pct_f.round().clamp(0.0, 999.0) as u64;
        let fg = if pct_f >= 90.0 {
            theme.accent_error
        } else if pct_f >= 75.0 {
            theme.warning
        } else {
            theme.gray
        };
        right.push(Span::styled(
            format!(
                "{}/{} ({pct}%)",
                fmt_status_tokens(used),
                fmt_status_tokens(cap)
            ),
            Style::default().fg(fg).bg(bg),
        ));
    }
    if let Some(perm) = state.permission_mode.filter(|p| !p.is_empty()) {
        if !right.is_empty() {
            right.push(sep());
        }
        let style = if perm.eq_ignore_ascii_case("plan") {
            Style::default().fg(theme.accent_plan).bg(bg)
        } else {
            theme.muted().bg(bg)
        };
        right.push(Span::styled(perm.to_owned(), style));
    }
    if state.running {
        if !right.is_empty() {
            right.push(sep());
        }
        let indicator = spinner_override
            .unwrap_or_else(|| Span::styled("●", Style::default().fg(theme.accent_success).bg(bg)));
        right.push(indicator);
    }

    let pad = usize::from(width >= 4);
    let inner_width = width.saturating_sub(pad * 2);
    let mut right_line = Line::from(right);
    if right_line.width() > inner_width {
        right_line = truncate_line(right_line, inner_width);
    }
    let right_w = right_line.width();
    let max_left_w = if right_w == 0 {
        inner_width
    } else {
        inner_width.saturating_sub(right_w + 2)
    };
    let left_line = truncate_line(Line::from(left), max_left_w);
    let left_w = left_line.width();
    let gap = inner_width.saturating_sub(left_w + right_w);

    let mut spans = Vec::with_capacity(left_line.spans.len() + right_line.spans.len() + 3);
    if pad > 0 {
        spans.push(Span::styled(" ", Style::default().bg(bg)));
    }
    spans.extend(left_line.spans);
    if gap > 0 {
        spans.push(Span::styled(" ".repeat(gap), Style::default().bg(bg)));
    }
    spans.extend(right_line.spans);
    if pad > 0 {
        spans.push(Span::styled(" ", Style::default().bg(bg)));
    }

    Line::from(spans).style(Style::default().bg(bg))
}
