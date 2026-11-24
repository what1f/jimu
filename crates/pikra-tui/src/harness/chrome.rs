//! Host-customizable chrome slots, slash commands, autocomplete providers, and localized copy.

use super::{
    BoxFuture, ContextMeterSnapshot, InteractionRequest, NoticeLevel, PlanDecision, QuestionAnswer,
    QuestionPrompt, TuiHarness, UsageStats,
};
use crate::engine::theme::Theme;
use futures_channel::{mpsc, oneshot};
use ratatui::text::Line;
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
    Notice { level: NoticeLevel, message: String },
    ToggleScreenMode,
    SetTheme(String),
    ClearTranscript,
    Quit,
    Redraw,
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

    pub fn toggle_screen_mode(&self) {
        let _ = self.tx.unbounded_send(ControlCommand::ToggleScreenMode);
    }

    pub fn set_theme(&self, name: impl Into<String>) {
        let _ = self
            .tx
            .unbounded_send(ControlCommand::SetTheme(name.into()));
    }

    pub fn clear_transcript(&self) {
        let _ = self.tx.unbounded_send(ControlCommand::ClearTranscript);
    }

    pub fn quit(&self) {
        let _ = self.tx.unbounded_send(ControlCommand::Quit);
    }

    pub fn request_redraw(&self) {
        let _ = self.tx.unbounded_send(ControlCommand::Redraw);
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
}

/// Customizable outer chrome regions. Returning an empty `Vec` consumes zero rows.
pub trait ChromeSlots: Send + Sync {
    fn welcome(&self, _state: &ChromeState<'_>) -> Vec<Line<'static>> {
        Vec::new()
    }

    fn header(&self, _state: &ChromeState<'_>) -> Vec<Line<'static>> {
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
}

struct DefaultChrome;
impl ChromeSlots for DefaultChrome {}

/// Host-configurable chrome wrapper, welcome card metadata, and OSC 7501 status reporting.
#[derive(Clone)]
pub struct ChromeConfig {
    pub slots: Arc<dyn ChromeSlots>,
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

/// Built-in provider combining slash commands, `@file` paths rooted in an optional directory,
/// and arbitrary extra host providers.
#[derive(Default)]
pub struct CombinedAutocompleteProvider {
    pub commands: Vec<(String, String)>,
    pub directory: Option<PathBuf>,
    pub providers: Vec<Arc<dyn AutocompleteProvider>>,
}

impl AutocompleteProvider for CombinedAutocompleteProvider {
    fn suggestions(&self, token: &str) -> Vec<CompletionItem> {
        let mut out = Vec::new();
        if let Some(query) = token.strip_prefix('/') {
            for (name, desc) in &self.commands {
                if fuzzy_subseq(name, query) {
                    out.push(CompletionItem {
                        token: format!("/{name}"),
                        description: (!desc.is_empty()).then(|| desc.clone()),
                    });
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

fn fuzzy_subseq(text: &str, query: &str) -> bool {
    let lower = text.to_lowercase();
    let mut chars = lower.chars();
    query.to_lowercase().chars().all(|c| chars.any(|t| t == c))
}
