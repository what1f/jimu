//! Harness-independent terminal conversation host.
mod commands;
mod dialogs;
mod external_editor;
mod history_store;
mod image_paste;
mod input;
mod mouse;
mod runner;
pub mod screen;
mod transcript;
mod viewer;

use crate::{
    engine::{
        input::KeyboardNormalizer,
        terminal::osc7501::ProgramStatus,
        theme::{
            Theme, ThemeKind, ThemeOverrides, cache, env_appearance, osc11, system_appearance,
        },
        x10_filter::X10ReassemblyFilter,
    },
    harness::{
        AutocompleteProvider, BoxFuture, ChromeConfig, CombinedAutocompleteProvider,
        ContextMeterSnapshot, QueuedItem, SlashCommand, ToolRendererRegistry, TuiController,
        TuiEvent, TuiHarness, UiText, UsageStats, UserInput, chrome::ControlCommand,
    },
    ui::views::prompt_widget::PromptWidget,
};

pub use crate::ui::scrollback::DisplayMode;
use crossterm::event::Event;
use dialogs::Modal;
use futures_channel::mpsc::UnboundedReceiver;
use futures_util::stream::FuturesUnordered;
use screen::FrameHits;
pub use screen::ScreenMode;
use std::time::Instant;
use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::Arc,
};
use transcript::Transcript;

/// Additional localized labels and command descriptions. Keys are the English defaults.
#[derive(Default, Clone)]
pub struct Labels(pub BTreeMap<String, String>);
impl Labels {
    pub fn get<'a>(&'a self, key: &'a str) -> &'a str {
        self.0.get(key).map_or(key, String::as_str)
    }
}
/// Optional host clipboard implementation. The default uses the platform clipboard utility.
pub type ClipboardWriter = Arc<dyn Fn(&str) -> std::io::Result<()> + Send + Sync>;
/// Presentation and host extensions for one conversation.
/// Host callback invoked with the active theme name whenever the user switches to a built-in or
/// custom theme.
pub type ThemeNameCallback = Arc<dyn Fn(&str) + Send + Sync>;
/// Optional external editor hook invoked on `Ctrl+G` / `/edit` with the current draft and working
/// directory. When unset, launches `$VISUAL` / `$EDITOR` / `vi` on a temporary `.md` file.
pub type ExternalEditorRunner =
    Arc<dyn Fn(&str, Option<&std::path::Path>) -> Result<String, String> + Send + Sync>;

pub struct TuiAppConfig {
    pub screen_mode: ScreenMode,
    pub theme_kind: ThemeKind,
    pub text: UiText,
    pub labels: Labels,
    pub chrome: ChromeConfig,
    pub tool_renderers: ToolRendererRegistry,
    pub slash_commands: Vec<Arc<dyn SlashCommand>>,
    pub autocomplete: Option<Arc<dyn AutocompleteProvider>>,
    /// Root for file completion. Defaults to the working directory. Set to None to disable it.
    pub directory: Option<PathBuf>,
    pub clipboard: Option<ClipboardWriter>,
    /// Optional path to a JSONL file storing cross-session prompt history. Defaults to `None`
    /// (in-memory session history only; library code never hardcodes a brand directory).
    pub prompt_history_path: Option<PathBuf>,
    /// Maximum number of deduplicated prompt history entries kept in memory. `None` for unlimited.
    pub prompt_history_limit: Option<usize>,
    /// Optional custom external editor runner for `Ctrl+G` / `/edit`.
    pub external_editor: Option<ExternalEditorRunner>,
    /// Optional initial user message submitted automatically once the TUI starts.
    pub initial_prompt: Option<String>,
    /// Open the session picker modal immediately on startup (`-r` / `--resume`).
    pub open_resume_picker: bool,
    /// Optional startup notices displayed once in the transcript (e.g. config warnings).
    pub initial_notices: Vec<(crate::harness::NoticeLevel, String)>,
    /// Optional callback invoked when the user switches the color theme.
    pub on_theme_change: Option<Arc<dyn Fn(ThemeKind) + Send + Sync>>,
    /// Optional callback invoked with the active theme name whenever the user switches to a
    /// built-in or custom theme.
    pub on_theme_name_change: Option<ThemeNameCallback>,
    /// Optional color overrides layered on top of `theme_kind`.
    pub theme_overrides: Option<ThemeOverrides>,
    /// Named custom themes injected by the host and selectable via `/theme`.
    pub custom_themes: BTreeMap<String, ThemeOverrides>,
    /// Optional name of a theme in `custom_themes` to activate on startup.
    pub initial_custom_theme: Option<String>,
}
impl Default for TuiAppConfig {
    fn default() -> Self {
        Self {
            screen_mode: ScreenMode::Fullscreen,
            theme_kind: ThemeKind::Auto,
            text: Default::default(),
            labels: Default::default(),
            chrome: Default::default(),
            tool_renderers: Default::default(),
            slash_commands: vec![],
            autocomplete: None,
            directory: std::env::current_dir().ok(),
            clipboard: None,
            prompt_history_path: None,
            prompt_history_limit: Some(1000),
            external_editor: None,
            initial_prompt: None,
            open_resume_picker: false,
            initial_notices: vec![],
            on_theme_change: None,
            on_theme_name_change: None,
            theme_overrides: None,
            custom_themes: BTreeMap::new(),
            initial_custom_theme: None,
        }
    }
}

pub enum TuiInput {
    Terminal(Event),
    Parts(UserInput),
}
impl From<Event> for TuiInput {
    fn from(event: Event) -> Self {
        Self::Terminal(event)
    }
}

enum Outcome {
    Done,
    Submitted(Result<(), (String, UserInput)>),
    Queued(Result<(), (String, UserInput)>),
    Result(Result<(), String>),
    Session(Result<(), String>),
    Rewind(Result<crate::harness::RewindOutcome, String>),
    ShellDone {
        call_id: String,
        result: Result<crate::harness::ShellCommandResult, String>,
    },
}
/// Create the host, clone its controller for interactive extensions, then call run or run_on.
pub struct TuiApp {
    config: TuiAppConfig,
    harness: Arc<dyn TuiHarness>,
    controller: TuiController,
    controls: UnboundedReceiver<ControlCommand>,
    jobs: FuturesUnordered<BoxFuture<'static, Outcome>>,
    history: Transcript,
    composer: PromptWidget,
    focus: Focus,
    hits: FrameHits,
    mouse: mouse::MouseState,
    x10: X10ReassemblyFilter,
    search: Option<crate::engine::input::line_editor::LineEditor>,
    queue_pane: crate::ui::views::queue_pane::QueuePane,
    queue_open: bool,
    toast: Option<(String, Instant)>,
    last_ctrl_c: Option<Instant>,
    last_esc: Option<Instant>,
    status: ProgramStatus,
    turn_error: Option<String>,
    completion: CombinedAutocompleteProvider,
    modals: VecDeque<Modal>,
    normalizer: KeyboardNormalizer,
    theme: Theme,
    auto_palette: ThemeKind,
    active_custom_theme: Option<String>,
    overlay_overrides: Option<ThemeOverrides>,
    native: bool,
    appearance: Option<system_appearance::SystemAppearanceWatcher>,
    observed: Option<system_appearance::SystemAppearance>,
    starting_turn: bool,
    palette: u64,
    mode: ScreenMode,
    running: bool,
    aborting: Option<BoxFuture<'static, ()>>,
    quit: bool,
    usage: UsageStats,
    context: Option<ContextMeterSnapshot>,
    queue: Vec<QueuedItem>,
    /// Deduplicated prompt history ordered newest-first (index 0 is most recent).
    prompts: Vec<String>,
    /// Active Up/Down in-place history navigation index into `prompts`.
    history_nav_index: Option<usize>,
    /// Draft stashed when entering Up/Down history recall so stepping back down restores it.
    history_nav_draft: Option<crate::ui::views::prompt_widget::StashedPrompt>,
    /// Bash mode flag captured before entering history recall so stepping back down restores it.
    history_nav_bash_mode: bool,
    /// User-stashed draft (`Ctrl+S` / `Alt+S`), restored after the next send or on pop.
    prompt_stash: Option<crate::ui::views::prompt_widget::StashedPrompt>,
    /// Armed for one keypress right after `Ctrl+S` so immediate `Ctrl+Z` pops the stash back.
    stash_undo_armed: bool,
    /// Persistent multiline composer mode (`Enter` inserts newline, `Shift/Alt+Enter` sends).
    multiline_mode: bool,
    /// Quick local shell input mode (`!` on empty prompt).
    bash_mode: bool,
    /// Active cancellation flag for a running `!` shell command.
    bash_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// Pending external editor draft requested via `Ctrl+G` or `/edit`.
    pending_external_editor: Option<String>,
    /// Counter for generating unique local bash tool call IDs.
    bash_serial: u64,
    generation: u64,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Prompt,
    Welcome(usize),
    Transcript,
    Queue,
}

impl TuiApp {
    pub fn new(harness: Arc<dyn TuiHarness>, config: TuiAppConfig) -> Self {
        let (controller, controls) = TuiController::channel(Arc::new(config.text.clone()));
        let mode = config.screen_mode;
        let overlay_overrides = config.theme_overrides.clone();
        let mut app = Self {
            config,
            harness,
            controller,
            controls,
            mode,
            jobs: Default::default(),
            history: Default::default(),
            composer: PromptWidget::new(),
            focus: Focus::Prompt,
            hits: Default::default(),
            mouse: Default::default(),
            x10: X10ReassemblyFilter::new(),
            search: None,
            queue_pane: crate::ui::views::queue_pane::QueuePane::new(),
            queue_open: false,
            toast: None,
            last_ctrl_c: None,
            last_esc: None,
            status: ProgramStatus::idle(),
            turn_error: None,
            completion: Default::default(),
            modals: Default::default(),
            normalizer: KeyboardNormalizer::from_terminal_context(),
            theme: Theme::current(),
            auto_palette: ThemeKind::Dark,
            active_custom_theme: None,
            overlay_overrides,
            native: false,
            appearance: None,
            observed: None,
            starting_turn: false,
            palette: 0,
            running: false,
            aborting: None,
            quit: false,
            usage: Default::default(),
            context: None,
            queue: vec![],
            prompts: vec![],
            history_nav_index: None,
            history_nav_draft: None,
            history_nav_bash_mode: false,
            prompt_stash: None,
            stash_undo_armed: false,
            multiline_mode: false,
            bash_mode: false,
            bash_cancel: None,
            pending_external_editor: None,
            bash_serial: 0,
            generation: 0,
        };
        app.composer
            .set_placeholder(app.config.text.input_placeholder.clone());
        app.sync_scrollback_appearance();
        if app.harness.capabilities().queue.is_none() {
            app.queue_pane
                .set_mutation(crate::ui::views::queue_mutation::QueueMutation::ReadOnly);
        }
        app.refresh_commands();
        app
    }
    fn sync_scrollback_appearance(&mut self) {
        let mut appearance = self.history.state.appearance().clone();
        appearance.chrome_slots = crate::ui::scrollback::appearance::ChromeSlotsHandle(Some(
            self.config.chrome.slots.clone(),
        ));
        self.history.state.set_appearance(appearance);
    }
    pub fn controller(&self) -> TuiController {
        self.controller.clone()
    }
    fn start(&mut self, native: bool) {
        self.native = native;
        if let Some(custom_name) = self.config.initial_custom_theme.clone()
            && let Some((key, _)) = self.find_custom_theme(&custom_name)
        {
            self.active_custom_theme = Some(key);
        }
        let osc = if native && self.effective_base_kind().is_auto() {
            osc11::detect_via_osc11()
        } else {
            None
        };
        let appearance = osc
            .or_else(|| env_appearance::parse_colorfgbg(std::env::var("COLORFGBG").ok().as_deref()))
            .or_else(|| native.then(system_appearance::detect_desktop).flatten());
        self.auto_palette = appearance.map_or(ThemeKind::Dark, |a| {
            system_appearance::to_theme_kind(a, None, None)
        });
        self.recompute_active_theme();
        self.restore();
        for (level, message) in self.config.initial_notices.clone() {
            self.history.notice(level, message);
        }
    }
    fn restore(&mut self) {
        let history = self.harness.initial_history();
        let persisted = self
            .config
            .prompt_history_path
            .as_deref()
            .map(|path| history_store::load_newest_first(path, self.config.prompt_history_limit))
            .unwrap_or_default();
        let session_prompts = history.iter().filter_map(|h| {
            if let crate::harness::HistoryMessage::User { text, .. } = h {
                Some(text.clone())
            } else {
                None
            }
        });
        self.prompts = history_store::merge_newest_first(
            session_prompts,
            persisted,
            self.config.prompt_history_limit,
        );
        self.reset_history_nav();
        self.running = false;
        self.starting_turn = false;
        self.turn_error = None;
        self.status = ProgramStatus::idle();
        self.reset_transcript();
        self.history.restore(history, &self.config.tool_renderers);
        self.usage = self.harness.initial_usage();
        let caps = self.harness.capabilities();
        self.context = caps.context_meter.and_then(|c| c.context_usage());
        let queue = caps.queue.map(|c| c.pending_inputs()).unwrap_or_default();
        self.set_queue(queue);
        self.refresh_commands();
    }
    fn refresh_meters_and_queue(&mut self) {
        let caps = self.harness.capabilities();
        if let Some(c) = caps.context_meter {
            self.context = c.context_usage();
        }
        if let Some(q) = caps.queue {
            self.set_queue(q.pending_inputs());
        }
    }
    fn effective_base_kind(&self) -> ThemeKind {
        if let Some(kind) = self
            .overlay_overrides
            .as_ref()
            .and_then(|o| o.base.as_deref())
            .and_then(ThemeKind::from_name)
        {
            return kind;
        }
        if let Some(name) = self.active_custom_theme.as_deref()
            && let Some(kind) = self
                .config
                .custom_themes
                .get(name)
                .and_then(|c| c.base.as_deref())
                .and_then(ThemeKind::from_name)
        {
            return kind;
        }
        self.config.theme_kind
    }

    fn recompute_active_theme(&mut self) {
        let base_kind = self.effective_base_kind();
        if self.native && base_kind.is_auto() && self.appearance.is_none() {
            self.appearance = system_appearance::SystemAppearanceWatcher::start_if_auto(true);
            self.observed = self.appearance.as_ref().and_then(|w| w.current());
        }
        let concrete_kind = if base_kind.is_auto() {
            self.auto_palette
        } else {
            base_kind
        };
        let mut merged_colors = BTreeMap::new();
        if let Some(name) = self.active_custom_theme.as_deref()
            && let Some(custom) = self.config.custom_themes.get(name)
        {
            merged_colors.extend(custom.colors.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        if let Some(overlay) = &self.overlay_overrides {
            merged_colors.extend(overlay.colors.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        let final_overrides = (!merged_colors.is_empty()).then_some(ThemeOverrides {
            base: None,
            colors: merged_colors,
        });
        cache::set_auto_mode(base_kind.is_auto());
        cache::set_overrides(final_overrides);
        cache::set(concrete_kind);
        self.refresh_active_theme();
    }

    fn apply_theme(&mut self, kind: ThemeKind) {
        self.config.theme_kind = kind;
        self.active_custom_theme = None;
        if let Some(overlay) = &mut self.overlay_overrides {
            overlay.base = None;
        }
        self.recompute_active_theme();
    }

    fn apply_custom_theme(&mut self, name: String) {
        self.active_custom_theme = Some(name);
        if let Some(overlay) = &mut self.overlay_overrides {
            overlay.base = None;
        }
        self.recompute_active_theme();
    }

    fn apply_theme_overrides(&mut self, overrides: Option<ThemeOverrides>) {
        self.config.theme_overrides = overrides.clone();
        self.overlay_overrides = overrides;
        self.recompute_active_theme();
    }

    fn replace_custom_themes(&mut self, themes: BTreeMap<String, ThemeOverrides>) {
        self.config.custom_themes = themes;
        if let Some(active) = self.active_custom_theme.clone() {
            if let Some((canonical, _)) = self.find_custom_theme(&active) {
                self.active_custom_theme = Some(canonical);
            } else {
                self.active_custom_theme = None;
            }
            self.recompute_active_theme();
        }
    }

    fn find_custom_theme(&self, name: &str) -> Option<(String, ThemeOverrides)> {
        if let Some(ov) = self.config.custom_themes.get(name) {
            return Some((name.to_owned(), ov.clone()));
        }
        let lower = name.to_ascii_lowercase();
        self.config
            .custom_themes
            .iter()
            .find(|(k, _)| k.to_ascii_lowercase() == lower)
            .map(|(k, v)| (k.clone(), v.clone()))
    }

    fn refresh_active_theme(&mut self) {
        self.theme = Theme::current();
        self.palette += 1;
        self.history.state.invalidate_heights();
        if let Some(modal) = self.modals.front_mut() {
            modal.retheme();
        }
    }
    fn appearance_changed(&mut self) -> bool {
        let current = self.appearance.as_ref().and_then(|w| w.current());
        if current == self.observed {
            return false;
        }
        self.observed = current;
        let Some(a) = current else { return false };
        self.auto_palette = system_appearance::to_theme_kind(a, None, None);
        if self.effective_base_kind().is_auto() {
            self.recompute_active_theme();
            return true;
        }
        false
    }
    fn toggle(&mut self) {
        self.focus = Focus::Prompt;
        self.mouse.reset();
        self.history.state.clear_text_selection();
        self.mode = match self.mode {
            ScreenMode::Inline => ScreenMode::Fullscreen,
            ScreenMode::Fullscreen => ScreenMode::Inline,
        };
    }
    fn clear(&mut self) {
        self.new_session_command();
    }

    fn reset_transcript(&mut self) {
        self.history = Transcript::default();
        self.history.state.set_cwd(self.config.directory.clone());
        self.sync_scrollback_appearance();
        self.focus = Focus::Prompt;
        self.search = None;
        self.mouse.reset();
        self.generation += 1;
    }

    fn event(&mut self, event: TuiEvent) {
        match &event {
            TuiEvent::TurnStarted => {
                if !self.starting_turn {
                    self.turn_error = None;
                }
                self.running = true;
                self.starting_turn = false;
                self.status = ProgramStatus::working();
            }
            TuiEvent::Error(message) if self.starting_turn => {
                self.starting_turn = false;
                self.running = false;
                self.history.finish_pending();
                self.status = ProgramStatus::error(message);
            }
            TuiEvent::Error(message)
            | TuiEvent::AssistantMessageEnd {
                error: Some(message),
            } => {
                if self.running {
                    self.turn_error = Some(message.clone());
                } else {
                    self.status = ProgramStatus::error(message);
                }
            }
            TuiEvent::TurnEnded => {
                self.starting_turn = false;
                self.running = false;
                self.status = self
                    .turn_error
                    .take()
                    .map_or_else(ProgramStatus::done, ProgramStatus::error);
            }
            _ => {}
        }
        match event {
            TuiEvent::UsageUpdate(usage) => self.usage = usage,
            TuiEvent::ContextUsageUpdate(context) => self.context = Some(context),
            TuiEvent::QueueChanged(queue) => self.set_queue(queue),
            TuiEvent::Interaction(request) => {
                self.history.state.clear_text_selection();
                self.mouse.cancel_gesture();
                let index = self
                    .modals
                    .iter()
                    .position(|modal| !modal.is_interaction())
                    .unwrap_or(self.modals.len());
                self.modals
                    .insert(index, Modal::interaction(request, &self.config.text));
            }
            event => self.history.event(event, &self.config.tool_renderers),
        }
    }

    fn program_status(&self) -> ProgramStatus {
        if let Some(blocked) = self.modals.front().and_then(Modal::blocked_status) {
            blocked
        } else if self.running {
            let status = ProgramStatus::working();
            self.history
                .active_tool()
                .map_or_else(ProgramStatus::working, |name| status.with_msg(name))
        } else {
            self.status.clone()
        }
    }
    fn control(&mut self, cmd: ControlCommand) {
        match cmd {
            ControlCommand::Interaction(req) => self.event(TuiEvent::Interaction(req)),
            ControlCommand::Notice { level, message } => self.history.notice(level, message),
            ControlCommand::Toast(message) => self.show_toast(&message),
            ControlCommand::ToggleScreenMode => self.toggle(),
            ControlCommand::SetTheme(name) => self.theme_command(&name),
            ControlCommand::SetThemeOverrides(overrides) => self.apply_theme_overrides(overrides),
            ControlCommand::RegisterCustomTheme { name, overrides } => {
                let is_active = self
                    .active_custom_theme
                    .as_deref()
                    .is_some_and(|active| active.eq_ignore_ascii_case(&name));
                self.config.custom_themes.insert(name.clone(), overrides);
                if is_active {
                    self.active_custom_theme = Some(name);
                    self.recompute_active_theme();
                }
            }
            ControlCommand::ReplaceCustomThemes(themes) => self.replace_custom_themes(themes),
            ControlCommand::ClearTranscript => self.clear(),
            ControlCommand::Quit => self.quit = true,
            ControlCommand::RequestRedraw => {
                self.refresh_commands();
                self.refresh_meters_and_queue();
            }
            ControlCommand::InvalidateHistoryLayout => {
                self.history.state.invalidate_heights();
                self.refresh_commands();
                self.refresh_meters_and_queue();
            }
        }
    }
    fn abort(&mut self) {
        if let Some(cancel) = &self.bash_cancel {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        if self.aborting.is_some() {
            return;
        }
        let h = self.harness.clone();
        self.aborting = Some(Box::pin(async move { h.abort().await }));
    }

    pub(super) fn record_prompt_history(&mut self, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        let is_dup_latest = self.prompts.first().is_some_and(|first| first == text);
        history_store::push_newest(
            &mut self.prompts,
            text.to_owned(),
            self.config.prompt_history_limit,
        );
        if !is_dup_latest && let Some(path) = self.config.prompt_history_path.as_deref() {
            history_store::append_entry(path, text);
        }
    }

    #[cfg(feature = "history-search")]
    pub(super) fn history_search_entries(
        &self,
    ) -> Vec<crate::ui::views::history_search::HistoryEntry> {
        self.prompts
            .iter()
            .cloned()
            .map(|text| crate::ui::views::history_search::HistoryEntry { text })
            .collect()
    }

    pub(super) fn reset_history_nav(&mut self) {
        self.history_nav_index = None;
        self.history_nav_draft = None;
        self.history_nav_bash_mode = false;
    }

    pub(super) fn is_busy(&self) -> bool {
        self.running || self.bash_cancel.is_some()
    }

    pub(super) fn request_external_editor(&mut self) {
        self.composer.completion_dropdown_close();
        #[cfg(feature = "history-search")]
        if self.composer.history_search.is_active() {
            self.composer.history_search.deactivate();
        }
        self.pending_external_editor = Some(self.composer.text().to_owned());
    }

    pub(super) fn apply_external_editor_result(&mut self, result: Result<String, String>) {
        match result {
            Ok(edited) => {
                self.reset_history_nav();
                self.composer.set_text(&edited);
                self.composer.set_cursor(edited.len());
                self.refresh_completion();
            }
            Err(err) => self.show_toast(&err),
        }
    }

    pub(super) fn run_bash_command(&mut self, command: String) {
        let trimmed = command.trim().to_owned();
        if trimmed.is_empty() {
            return;
        }
        if self.bash_cancel.is_some() {
            self.show_toast("A shell command is already running");
            return;
        }
        if self.harness.capabilities().shell.is_none() {
            self.show_toast("Local shell mode is unavailable");
            return;
        }
        self.record_prompt_history(&format!("!{trimmed}"));
        self.bash_serial = self.bash_serial.wrapping_add(1);
        let call_id = format!("local-bash-{}", self.bash_serial);
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.bash_cancel = Some(Arc::clone(&cancel));
        self.status = ProgramStatus::working();
        self.event(TuiEvent::ToolStart {
            call_id: call_id.clone(),
            name: "bash".to_owned(),
            effective_name: Some("!".to_owned()),
            args: serde_json::json!({ "command": trimmed }),
        });
        let h = self.harness.clone();
        let cmd_for_job = trimmed.clone();
        let id_for_job = call_id.clone();
        self.jobs.push(Box::pin(async move {
            let result = match h.capabilities().shell {
                Some(shell) => shell.run_shell(cmd_for_job.clone(), cancel).await,
                None => Err("Local shell mode is unavailable".to_owned()),
            };
            Outcome::ShellDone {
                call_id: id_for_job,
                result,
            }
        }));
    }

    fn submit(&mut self, input: UserInput, follow: bool) {
        self.submit_with_display(input, None, follow);
    }

    fn submit_with_display(
        &mut self,
        mut input: UserInput,
        display_override: Option<String>,
        follow: bool,
    ) {
        if input.text.trim().is_empty() && input.images.is_empty() {
            return;
        }
        // Preserve leading whitespace (Claude Code convention): only a literal leading `/`
        // enters slash command / skill dispatch; leading spaces pass straight through as text.
        let mut display_text: Option<String> = display_override;
        let mut restore_text = input.text.clone();
        if input.images.is_empty()
            && let Some(cmd) = input.text.trim_end().strip_prefix('/')
        {
            match self.dispatch_slash(cmd) {
                commands::SlashDispatch::Handled => return,
                commands::SlashDispatch::PassThrough => {}
                commands::SlashDispatch::SendSkill { display, expanded } => {
                    display_text = Some(display.clone());
                    restore_text = display;
                    input.text = expanded;
                }
            }
        }
        if display_text.is_none() && !input.images.is_empty() {
            display_text = Some(image_paste::format_user_display(
                &input.text,
                input.images.len(),
            ));
        }
        let history_text = display_text.as_deref().unwrap_or(&input.text);
        self.record_prompt_history(history_text);
        let h = self.harness.clone();
        let active = self.running;
        let restore_draft = UserInput {
            text: restore_text,
            images: input.images.clone(),
        };
        if !active {
            if let Some(display) = display_text {
                self.history
                    .pending_user_with_submitted(display, input.text.clone());
            } else {
                self.history.pending_user(input.text.clone());
            }
            self.status = ProgramStatus::working();
            self.turn_error = None;
            self.starting_turn = true;
            self.running = true;
        }
        self.jobs.push(Box::pin(async move {
            if !active {
                Outcome::Submitted(h.submit(input).await.map_err(|err| (err, restore_draft)))
            } else if follow {
                Outcome::Queued(h.follow_up(input).await.map_err(|err| (err, restore_draft)))
            } else {
                Outcome::Queued(h.steer(input).await.map_err(|err| (err, restore_draft)))
            }
        }));
    }

    fn restore_composer_draft(&mut self, draft: UserInput) {
        if !self.composer.is_effectively_empty() {
            return;
        }
        if draft.text.trim().is_empty() && draft.images.is_empty() {
            return;
        }
        for image in draft.images {
            let pasted = crate::ui::views::prompt_widget::PastedImage::new(
                image.mime_type,
                image.data,
                None,
                image.label.map(std::path::PathBuf::from),
            );
            let _ = self.composer.insert_image(pasted);
        }
        if !draft.text.is_empty() {
            if self.composer.text().is_empty() {
                self.composer.set_text(&draft.text);
            } else {
                self.composer.append_text(&draft.text);
            }
        }
        self.refresh_completion();
    }
}
