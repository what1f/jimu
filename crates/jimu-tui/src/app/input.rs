use super::{Focus, ScreenMode, TuiApp, TuiInput, dialogs::DialogAction};
use crate::{
    engine::{
        input::{key::KeyShortcut, line_editor::LineEditOutcome},
        theme::ThemeKind,
        x10_filter::TimedInputEvent,
    },
    harness::{AutocompleteProvider, UserInput, is_valid_slash_query},
    ui::views::{
        prompt_widget::{EnterOutcome, SlashChrome},
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
                DialogAction::PickModelEffort { model, effort } => {
                    self.modals.pop_front();
                    self.pick_model_and_effort(model, effort);
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
                self.composer.completion_dropdown_close();
                #[cfg(feature = "clipboard-image")]
                {
                    let remaining = crate::ui::views::prompt_widget::PromptWidget::IMAGE_CAP
                        .saturating_sub(self.composer.reconciled_images().len());
                    if let Some(batch) = super::image_paste::try_load_pasted_image_paths(
                        &text,
                        self.config.directory.as_deref(),
                        remaining,
                    ) {
                        self.insert_pasted_images(batch);
                        self.refresh_completion();
                        return;
                    }
                }
                self.composer.handle_paste(&text);
                self.refresh_slash_highlight();
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
        let text = self.composer.text().to_owned();
        let cursor = self.composer.cursor();

        // Inline argument autocomplete for leading slash commands (e.g. `/model `, `/effort `,
        // `/permissions `, `/theme `, `/fast `).
        if text.starts_with('/')
            && !text.contains('\n')
            && let Some(ws) = text.find(char::is_whitespace)
            && cursor > ws
        {
            let cmd_name = &text[1..ws];
            let mut arg_start = ws;
            while let Some(ch) = text[arg_start..].chars().next() {
                if ch.is_whitespace() && arg_start < cursor {
                    arg_start += ch.len_utf8();
                } else {
                    break;
                }
            }
            let arg_query = &text[arg_start..cursor];
            if !arg_query.contains(char::is_whitespace) {
                let arg_end = text[cursor..]
                    .find(char::is_whitespace)
                    .map_or(text.len(), |i| cursor + i);
                let current_arg = &text[arg_start..arg_end];
                let arg_items = self.command_arg_suggestions(cmd_name, arg_query);
                if !arg_items.is_empty() {
                    if cursor == arg_end
                        && !current_arg.is_empty()
                        && arg_items.iter().any(|item| item.token == current_arg)
                    {
                        self.composer.completion_dropdown_close();
                        self.refresh_slash_highlight();
                        return;
                    }
                    let replace_range = arg_start..arg_end;
                    let parsed = arg_items
                        .into_iter()
                        .filter_map(|item| {
                            if item.token == current_arg {
                                return None;
                            }
                            let mut insert_text = text.clone();
                            insert_text.replace_range(replace_range.clone(), &item.token);
                            Some(CompletionItemParsed {
                                display: item.token.clone(),
                                description: item.description.unwrap_or_default(),
                                insert_text,
                                source: SuggestionSource::None,
                                priority: 0,
                                replace_range: Some(replace_range.clone()),
                                token_text: Some(item.token),
                                truncated: false,
                            })
                        })
                        .take(100)
                        .collect();
                    self.composer.set_completions(parsed);
                    self.refresh_slash_highlight();
                    return;
                }
            }
        }

        let Some(range) = self.composer.completion_token() else {
            self.composer.completion_dropdown_close();
            self.refresh_slash_highlight();
            return;
        };
        let full_token = &text[range.clone()];
        let token = &text[range.start..cursor];

        // Refresh skills when opening a slash or `$` completion menu so hot-reloaded skills appear.
        if (token.starts_with('/') || token.starts_with('$'))
            && (!self.composer.completion_dropdown_open() || token.len() == 1)
        {
            self.sync_skills();
        }

        let is_leading_slash = range.start == 0 && full_token.starts_with('/');
        let is_mid_skill =
            (range.start > 0 && full_token.starts_with('/')) || full_token.starts_with('$');

        if is_leading_slash {
            if let Some(query) = token.strip_prefix('/') {
                if !is_valid_slash_query(query) {
                    self.composer.completion_dropdown_close();
                    self.refresh_slash_highlight();
                    return;
                }
                if cursor == range.end && self.is_known_slash_name(query) {
                    self.composer.completion_dropdown_close();
                    self.refresh_slash_highlight();
                    return;
                }
            }
        } else if is_mid_skill {
            let query = &token[1..];
            if !is_valid_slash_query(query)
                || (cursor == range.end && self.is_known_skill_name(query))
            {
                self.composer.completion_dropdown_close();
                self.refresh_slash_highlight();
                return;
            }
        } else if !full_token.starts_with('@') {
            self.composer.completion_dropdown_close();
            self.refresh_slash_highlight();
            return;
        }

        let suggestions = if is_mid_skill {
            self.completion.skill_suggestions(token)
        } else {
            self.completion.suggestions(token)
        };
        let items = suggestions
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
        self.refresh_slash_highlight();
    }

    pub(super) fn refresh_slash_highlight(&mut self) {
        let text = self.composer.text();
        let mut chrome = SlashChrome::default();
        let mut leading_end = 0usize;

        // Highlight recognized leading `/command` or `/skill` (strictly column 0, no leading spaces).
        if text.starts_with('/') {
            let end = text.find(char::is_whitespace).unwrap_or(text.len());
            let name = &text[1..end];
            let recognized = self.is_known_slash_name(name);
            let leading_open = self.composer.completion_dropdown_open()
                && self.composer.cursor() <= end
                && is_valid_slash_query(name);
            if recognized || leading_open {
                chrome.active = true;
                chrome.open = leading_open;
                chrome.command_recognized = recognized;
                chrome.command_range = Some(0..end);
                leading_end = end;
            }
        }

        // Highlight recognized `/skill` and `$skill` mentions anywhere else in the draft.
        let is_boundary = |c: char| {
            c.is_whitespace()
                || matches!(
                    c,
                    '(' | '['
                        | '{'
                        | '"'
                        | '\''
                        | '。'
                        | '、'
                        | '？'
                        | '！'
                        | '，'
                        | '；'
                        | '：'
                        | '（'
                        | '【'
                )
        };
        let is_name_char = |c: char| c.is_alphanumeric() || matches!(c, '-' | '_' | ':');

        for (idx, ch) in text.char_indices() {
            if idx < leading_end || (ch != '/' && ch != '$') {
                continue;
            }
            let valid_before = match text[..idx].chars().next_back() {
                None => true,
                Some(prev) if ch == '$' => {
                    !(prev.is_ascii_alphanumeric() || matches!(prev, '_' | '$' | '\\'))
                }
                Some(prev) => is_boundary(prev),
            };
            if !valid_before {
                continue;
            }
            let tail = &text[idx + 1..];
            let len = tail.find(|c| !is_name_char(c)).unwrap_or(tail.len());
            if len == 0 {
                continue;
            }
            // Do not highlight path segments like `/Users/foo` or `/foo.txt`.
            if ch == '/'
                && tail[len..]
                    .chars()
                    .next()
                    .is_some_and(|next| matches!(next, '/' | '.'))
            {
                continue;
            }
            let candidate = tail[..len].trim_end_matches(':');
            if self.is_known_skill_name(candidate) {
                chrome.recognized_tokens.push(idx..idx + 1 + len);
            }
        }

        self.composer.set_slash_chrome(chrome);
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
        let next = if current == ThemeKind::Light {
            ThemeKind::Dark
        } else {
            ThemeKind::Light
        };
        self.apply_theme(next);
        if let Some(cb) = &self.config.on_theme_change {
            cb(next);
        }
        if let Some(cb) = &self.config.on_theme_name_change {
            cb(next.display_name());
        }
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
            _ if self.try_clipboard_image_paste_key(&key) => {}
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

    #[cfg(feature = "clipboard-image")]
    fn insert_pasted_images(&mut self, batch: super::image_paste::PastedImageBatch) {
        let mut toast = batch.warning;
        for img in batch.images {
            if let Err(err) = self.composer.insert_image(img) {
                if toast.is_none() {
                    toast = Some(err);
                }
                if self.composer.reconciled_images().len()
                    >= crate::ui::views::prompt_widget::PromptWidget::IMAGE_CAP
                {
                    break;
                }
            }
        }
        if let Some(msg) = toast {
            self.show_toast(&msg);
        }
    }

    fn try_clipboard_image_paste_key(&mut self, key: &KeyEvent) -> bool {
        #[cfg(not(feature = "clipboard-image"))]
        {
            let _ = key;
            false
        }
        #[cfg(feature = "clipboard-image")]
        {
            if crate::engine::input::key::is_inline_paste_key(key) {
                return false;
            }
            let is_image_paste_chord = crate::engine::input::key::is_paste_key(key)
                || KeyShortcut::new(K::Char('v'), M::ALT).matches(key);
            if !is_image_paste_chord {
                return false;
            }
            let clipboard_text = crate::engine::clipboard::system_clipboard_get();
            let remaining = crate::ui::views::prompt_widget::PromptWidget::IMAGE_CAP
                .saturating_sub(self.composer.reconciled_images().len());
            if let Some(batch) = super::image_paste::probe_clipboard_images(
                clipboard_text.as_deref(),
                self.config.directory.as_deref(),
                remaining,
            ) {
                self.composer.completion_dropdown_close();
                self.insert_pasted_images(batch);
                self.refresh_completion();
                return true;
            }
            if let Some(text) = clipboard_text {
                self.composer.completion_dropdown_close();
                self.composer.handle_paste(&text);
                self.refresh_completion();
                return true;
            }
            // No image and no text on clipboard: swallow Alt+V/Ctrl+V cleanly without inserting a character.
            true
        }
    }

    fn send_prompt(&mut self, follow: bool) {
        if let Some(raw_text) = self.composer.try_send() {
            let mut clean_text = self.composer.submitted_text_without_image_chips(&raw_text);
            let images: Vec<crate::harness::InputImage> = self
                .composer
                .drain_images()
                .into_iter()
                .filter_map(|img| {
                    let data = img.encoded_bytes.map(|b| b.to_vec()).or_else(|| {
                        img.session_image_path
                            .as_ref()
                            .or(img.source_path.as_ref())
                            .and_then(|p| std::fs::read(p).ok())
                    })?;
                    Some(crate::harness::InputImage {
                        mime_type: img.mime_type,
                        data,
                        label: img.source_path.map(|p| p.display().to_string()),
                    })
                })
                .collect();
            let display_override = if !images.is_empty() {
                clean_text = clean_text.trim().to_owned();
                let raw_trimmed = raw_text.trim();
                if raw_trimmed.is_empty() {
                    Some(super::image_paste::format_user_display(
                        &clean_text,
                        images.len(),
                    ))
                } else {
                    Some(raw_trimmed.to_owned())
                }
            } else {
                None
            };
            self.composer.set_text_discarding_images("");
            self.composer.clear_history();
            self.refresh_completion();
            self.submit_with_display(
                UserInput {
                    text: clean_text,
                    images,
                },
                display_override,
                follow,
            );
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
