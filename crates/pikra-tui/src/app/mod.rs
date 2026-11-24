//! Harness-independent terminal conversation host.
mod commands;
mod dialogs;
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
        theme::{Theme, ThemeKind, cache, env_appearance, osc11, system_appearance},
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
    Submitted(Result<(), String>),
    Result(Result<(), String>),
    Session(Result<(), String>),
    Rewind(Result<Option<String>, String>),
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
    #[cfg(feature = "history-search")]
    prompts: Vec<crate::ui::views::history_search::HistoryEntry>,
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
            #[cfg(feature = "history-search")]
            prompts: vec![],
            generation: 0,
        };
        app.composer
            .set_placeholder(app.config.text.input_placeholder.clone());
        if app.harness.capabilities().queue.is_none() {
            app.queue_pane
                .set_mutation(crate::ui::views::queue_mutation::QueueMutation::ReadOnly);
        }
        app.refresh_commands();
        app
    }
    pub fn controller(&self) -> TuiController {
        self.controller.clone()
    }
    fn start(&mut self, native: bool) {
        self.native = native;
        let osc = if native && self.config.theme_kind.is_auto() {
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
        self.apply_theme(self.config.theme_kind);
        self.restore();
    }
    fn restore(&mut self) {
        let history = self.harness.initial_history();
        #[cfg(feature = "history-search")]
        {
            self.prompts = history
                .iter()
                .filter_map(|h| {
                    if let crate::harness::HistoryMessage::User { text, .. } = h {
                        Some(crate::ui::views::history_search::HistoryEntry { text: text.clone() })
                    } else {
                        None
                    }
                })
                .collect();
        }
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
    fn apply_theme(&mut self, kind: ThemeKind) {
        self.config.theme_kind = kind;
        if self.native && kind.is_auto() && self.appearance.is_none() {
            self.appearance = system_appearance::SystemAppearanceWatcher::start_if_auto(true);
            self.observed = self.appearance.as_ref().and_then(|w| w.current());
        }
        cache::set_auto_mode(kind.is_auto());
        cache::set(if kind.is_auto() {
            self.auto_palette
        } else {
            kind
        });
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
        if self.config.theme_kind.is_auto() {
            self.apply_theme(ThemeKind::Auto);
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
            ControlCommand::ToggleScreenMode => self.toggle(),
            ControlCommand::SetTheme(name) => self.theme_command(&name),
            ControlCommand::ClearTranscript => self.clear(),
            ControlCommand::Quit => self.quit = true,
            ControlCommand::Redraw => {
                self.refresh_commands();
                self.refresh_meters_and_queue();
            }
        }
    }
    fn abort(&mut self) {
        if self.aborting.is_some() {
            return;
        }
        let h = self.harness.clone();
        self.aborting = Some(Box::pin(async move { h.abort().await }));
    }
    fn submit(&mut self, input: UserInput, follow: bool) {
        if input.text.trim().is_empty() && input.images.is_empty() {
            return;
        }
        if input.images.is_empty()
            && let Some(cmd) = input.text.trim().strip_prefix('/')
        {
            self.command(cmd);
            return;
        }
        #[cfg(feature = "history-search")]
        if !input.text.is_empty()
            && self
                .prompts
                .last()
                .is_none_or(|entry| entry.text != input.text)
        {
            self.prompts
                .push(crate::ui::views::history_search::HistoryEntry {
                    text: input.text.clone(),
                });
        }
        let h = self.harness.clone();
        let active = self.running;
        if !active {
            self.history.pending_user(input.text.clone());
            self.status = ProgramStatus::working();
            self.turn_error = None;
            self.starting_turn = true;
            self.running = true;
        }
        self.jobs.push(Box::pin(async move {
            if !active {
                Outcome::Submitted(h.submit(input).await)
            } else if follow {
                Outcome::Result(h.follow_up(input).await)
            } else {
                Outcome::Result(h.steer(input).await)
            }
        }));
    }
}
