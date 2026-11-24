use super::{Focus, ScreenMode, TuiApp, TuiInput, dialogs::DialogAction};
use crate::{
    engine::{
        input::{key::KeyShortcut, line_editor::LineEditOutcome},
        theme::ThemeKind,
        x10_filter::TimedInputEvent,
    },
    harness::{AutocompleteProvider, UserInput},
    ui::views::{
        prompt_widget::EnterOutcome,
        queue_pane::QueueEvent,
        suggestion_controller::{CompletionItemParsed, SuggestionSource},
    },
};
use crossterm::event::{
    Event, KeyCode as K, KeyEvent, KeyEventKind, KeyModifiers as M, MouseEventKind,
};
use std::time::{Duration, Instant};

const DOUBLE_CTRL_C_WINDOW: Duration = Duration::from_millis(1500);
const DOUBLE_ESC_WINDOW: Duration = Duration::from_millis(500);

impl TuiApp {
    pub(super) fn input(&mut self, input: TuiInput) {
        match input {
            TuiInput::Parts(parts) => self.submit(parts, false),
            TuiInput::Terminal(event) => {
                for input in self.x10.filter(vec![TimedInputEvent::now(event)]) {
                    self.terminal_input(input.event);
                }
            }
        }
    }

    fn terminal_input(&mut self, mut event: Event) {
        match &mut event {
            Event::Key(key) if key.kind == KeyEventKind::Release => return,
            Event::Key(key) => *key = self.normalizer.rescue_key(*key).unwrap_or(*key),
            Event::Mouse(mouse) => {
                self.mouse.pos = Some((mouse.column, mouse.row));
                if mouse.kind == MouseEventKind::Moved && self.modals.is_empty() {
                    self.composer.handle_mouse(mouse);
                }
            }
            Event::FocusLost | Event::Resize(_, _) => {
                self.mouse.reset();
                self.history.state.clear_text_selection();
            }
            _ => {}
        }
        if let Some(modal) = self.modals.front_mut()
            && matches!(event, Event::Key(_) | Event::Mouse(_) | Event::Paste(_))
        {
            self.last_ctrl_c = None;
            self.last_esc = None;
            match modal.input(&event) {
                DialogAction::Stay => {}
                DialogAction::Close => {
                    self.modals.pop_front();
                }
                DialogAction::Pick(target, id) => {
                    self.modals.pop_front();
                    self.pick(target, id);
                }
                DialogAction::Copy(text) => self.copy(&text),
            }
            return;
        }
        match event {
            Event::Key(key) => self.key(key),
            Event::Paste(text) if let Some(search) = &mut self.search => {
                if search.insert_paste(&text) == LineEditOutcome::TextChanged {
                    self.history.state.set_search(search.text());
                }
            }
            Event::Paste(text) => {
                self.last_ctrl_c = None;
                self.last_esc = None;
                self.focus_prompt();
                self.composer.handle_paste(&text);
                self.refresh_completion();
            }
            Event::Mouse(mouse) => self.mouse_event(mouse),
            _ => {}
        }
    }

    pub(super) fn refresh_completion(&mut self) {
        #[cfg(feature = "history-search")]
        if self.composer.history_search.is_active() {
            return;
        }
        let text = self.composer.text();
        let cursor = self.composer.cursor();
        let range = self.composer.completion_token().filter(|range| {
            text[range.clone()].starts_with('@') || text[..range.start].trim().is_empty()
        });
        let Some(range) = range else {
            self.composer.completion_dropdown_close();
            return;
        };
        let token = &text[range.start..cursor];
        if cursor == range.end
            && let Some(name) = token.strip_prefix('/')
            && self
                .completion
                .commands
                .iter()
                .any(|(command, _)| command == name)
        {
            self.composer.completion_dropdown_close();
            return;
        }
        let items = self
            .completion
            .suggestions(token)
            .into_iter()
            .filter_map(|item| {
                if item.token == text[range.clone()] {
                    return None;
                }
                let mut insert_text = text.to_owned();
                let mut replacement = item.token.clone();
                if range.end == text.len() && !replacement.ends_with('/') {
                    replacement.push(' ');
                }
                insert_text.replace_range(range.clone(), &replacement);
                Some(CompletionItemParsed {
                    display: item.token,
                    description: item.description.unwrap_or_default(),
                    insert_text,
                    source: SuggestionSource::None,
                    priority: 0,
                    replace_range: Some(range.clone()),
                    token_text: Some(replacement),
                    truncated: false,
                })
            })
            .take(100)
            .collect();
        self.composer.set_completions(items);
    }

    pub(super) fn focus_prompt(&mut self) {
        self.focus = Focus::Prompt;
        self.search = None;
        self.history.state.set_selected(None);
    }

    pub(super) fn cycle_theme(&mut self) {
        let current = if self.config.theme_kind.is_auto() {
            self.auto_palette
        } else {
            self.config.theme_kind
        };
        self.apply_theme(if current == ThemeKind::Light {
            ThemeKind::Dark
        } else {
            ThemeKind::Light
        });
    }

    fn handle_ctrl_c(&mut self) {
        self.last_esc = None;
        if self.running {
            self.last_ctrl_c = None;
            self.abort();
            return;
        }
        if !self.composer.is_effectively_empty() {
            self.last_ctrl_c = None;
            self.composer.set_text_discarding_images("");
            self.refresh_completion();
            return;
        }
        let now = Instant::now();
        if self
            .last_ctrl_c
            .is_some_and(|prev| now.duration_since(prev) <= DOUBLE_CTRL_C_WINDOW)
        {
            self.last_ctrl_c = None;
            self.quit = true;
        } else {
            self.last_ctrl_c = Some(now);
            self.show_toast("Press Ctrl+C again to exit");
        }
    }

    fn handle_prompt_esc(&mut self) {
        self.last_ctrl_c = None;
        if self.running {
            self.last_esc = None;
            self.abort();
            return;
        }
        if !self.composer.is_effectively_empty() {
            self.last_esc = None;
            return;
        }
        if self.harness.capabilities().rewind.is_none() {
            self.last_esc = None;
            return;
        }
        let now = Instant::now();
        if self
            .last_esc
            .is_some_and(|prev| now.duration_since(prev) <= DOUBLE_ESC_WINDOW)
        {
            self.last_esc = None;
            self.open_rewind();
        } else {
            self.last_esc = Some(now);
        }
    }

    fn key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(M::CONTROL);
        let alt = key.modifiers.contains(M::ALT);
        let is_ctrl_c = ctrl && key.code == K::Char('c');
        let is_bare_esc = key.code == K::Esc && key.modifiers.is_empty();
        if !is_ctrl_c {
            self.last_ctrl_c = None;
        }
        if !is_bare_esc {
            self.last_esc = None;
        }

        if ctrl {
            match key.code {
                K::Char('q') => {
                    self.quit = true;
                    return;
                }
                K::Char('c') => {
                    self.handle_ctrl_c();
                    return;
                }
                K::Char('o') => {
                    self.history.state.toggle_expand_all();
                    return;
                }
                _ => {}
            }
        }
        if alt && !ctrl {
            match key.code {
                K::Char('t') | K::Char('T') => {
                    self.cycle_thinking_level();
                    return;
                }
                K::Char('p') | K::Char('P')
                    if self.harness.capabilities().model_picker.is_some() =>
                {
                    self.command("model");
                    return;
                }
                _ => {}
            }
        }
        if key.code == K::BackTab && !self.composer.completion_dropdown_open() {
            self.cycle_permission_mode();
            return;
        }

        if let Some(search) = &mut self.search {
            match key.code {
                K::Esc => {
                    self.search = None;
                    self.history.state.clear_search();
                }
                K::Enter => {
                    self.search = None;
                    self.history.state.poll_search();
                }
                _ => {
                    if search.handle_key(&key) == LineEditOutcome::TextChanged {
                        self.history.state.set_search(search.text());
                    }
                }
            }
            return;
        }
        #[cfg(feature = "history-search")]
        if self.composer.history_search.is_active() {
            self.composer.handle_key(&key);
            if !self.composer.history_search.is_active() {
                self.refresh_completion();
            }
            return;
        }
        if self.hits.welcome.is_some() && !self.composer.completion_dropdown_open() {
            let actions = self.welcome_actions();
            match (self.focus, key.code) {
                (Focus::Welcome(i), K::Enter) if key.modifiers.is_empty() => {
                    if let Some((id, _, _)) = actions.get(i) {
                        self.welcome_action(id);
                    }
                    return;
                }
                (Focus::Welcome(i), K::Up | K::Down) if key.modifiers.is_empty() => {
                    let n = actions.len();
                    self.focus = Focus::Welcome(if key.code == K::Down {
                        (i + 1) % n
                    } else {
                        (i + n - 1) % n
                    });
                    return;
                }
                (Focus::Prompt, K::Up | K::Down)
                    if key.modifiers.is_empty() && self.composer.is_effectively_empty() =>
                {
                    self.focus = Focus::Welcome(if key.code == K::Down {
                        0
                    } else {
                        actions.len() - 1
                    });
                    return;
                }
                (Focus::Welcome(_), K::Esc | K::Tab) => {
                    self.focus_prompt();
                    return;
                }
                _ => {}
            }
        }
        if self.focus == Focus::Queue {
            match key.code {
                K::Esc | K::Tab => self.focus_prompt(),
                K::Delete | K::Backspace | K::Char('x' | 'e') | K::Enter => {
                    match self
                        .queue_pane
                        .handle_key(&key, KeyShortcut::ctrl(K::Enter))
                    {
                        Some(QueueEvent::DeleteSelected { id }) => {
                            self.remove_queued_row(id, false)
                        }
                        Some(QueueEvent::EditSelected { id }) => self.remove_queued_row(id, true),
                        _ => {}
                    }
                }
                _ => {
                    self.queue_pane.handle_navigation_key(&key);
                }
            }
            return;
        }
        if self.mode == ScreenMode::Fullscreen {
            if !self.composer.completion_dropdown_open()
                && matches!(key.code, K::PageUp | K::PageDown)
            {
                if key.code == K::PageUp {
                    self.history.state.page_up();
                } else {
                    self.history.state.page_down();
                }
                return;
            }
            if ctrl && matches!(key.code, K::Up | K::Down) {
                self.focus = Focus::Transcript;
                self.move_block(key.code == K::Down);
                return;
            }
        }
        if self.focus == Focus::Transcript {
            match key.code {
                K::Char('j') | K::Down => self.move_block(true),
                K::Char('k') | K::Up => self.move_block(false),
                K::Char(' ') => {
                    if !self.history.state.toggle_group_expansion() {
                        self.history.state.toggle_fold_selected();
                    }
                }
                K::Enter => self.open_selected(),
                K::Char('y') => {
                    let state = &self.history.state;
                    let text = state.selected_text().or_else(|| {
                        state
                            .selected()
                            .and_then(|i| state.get(i)?.block.copy_text(false))
                    });
                    if let Some(text) = text {
                        self.copy(&text);
                    }
                }
                K::Esc | K::Tab => {
                    self.history.state.clear_text_selection();
                    self.focus_prompt();
                }
                K::Char('?') => self.shortcuts(),
                _ => {}
            }
            return;
        }
        self.focus = Focus::Prompt;
        if self.composer.completion_dropdown_open() && key.code == K::Esc {
            self.composer.completion_dropdown_close();
            return;
        }
        match key.code {
            K::Esc if key.modifiers.is_empty() => self.handle_prompt_esc(),
            K::Char('d') if ctrl && self.composer.is_effectively_empty() => self.quit = true,
            K::Char('?') if !ctrl && !alt && self.composer.is_effectively_empty() => {
                self.shortcuts()
            }
            K::Enter if ctrl || alt => self.send_prompt(true),
            _ => match self.composer.route_enter(&key) {
                EnterOutcome::Submit => self.send_prompt(false),
                EnterOutcome::NewlineInserted => self.refresh_completion(),
                EnterOutcome::PassThrough => {
                    let before = (self.composer.text().to_owned(), self.composer.cursor());
                    match self.composer.handle_key(&key) {
                        #[cfg(feature = "history-search")]
                        crate::ui::views::prompt_widget::PromptEvent::SearchHistory => {
                            self.composer.open_history(&self.prompts, false);
                        }
                        _ if before.0 != self.composer.text()
                            || before.1 != self.composer.cursor() =>
                        {
                            self.refresh_completion()
                        }
                        _ => {}
                    }
                }
            },
        }
    }

    fn send_prompt(&mut self, follow: bool) {
        if let Some(text) = self.composer.try_send() {
            let text = self.composer.submitted_text_without_image_chips(&text);
            self.composer.set_text_discarding_images("");
            self.composer.clear_history();
            self.refresh_completion();
            self.submit(UserInput::text(text), follow);
        }
    }

    fn move_block(&mut self, down: bool) {
        if self.history.state.selected().is_none() {
            self.history.state.on_activate();
        } else if down {
            self.history.state.select_next();
        } else {
            self.history.state.select_prev();
        }
    }

    fn open_selected(&mut self) {
        let state = &mut self.history.state;
        if state.selected().is_none() {
            state.on_activate();
        }
        if let Some(id) = state.selected().and_then(|i| state.get(i).map(|e| e.id)) {
            self.open_block(id);
        }
    }
}
