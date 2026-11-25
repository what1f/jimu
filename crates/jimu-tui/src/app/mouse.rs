// Mouse routing follows the Grok Build pager (Apache-2.0).
// Copyright 2023-2026 SpaceXAI.

use std::time::{Duration, Instant};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind as Kind};
use ratatui::layout::Position;

use crate::{
    engine::{
        clipboard,
        input::mouse::{MouseScrollState, ScrollConfig, ScrollDirection},
        input::terminal::hyperlinks::SchemeFilter,
        render::{link_opener, osc8::LinkTarget},
    },
    ui::views::{prompt_widget::PromptEvent, welcome::WelcomeTarget},
};

use super::{Focus, ScreenMode, TuiApp};

#[derive(Clone, Copy)]
enum Gesture {
    Prompt,
    Scrollbar,
    Transcript {
        start: (u16, u16),
        clicks: u8,
        moved: bool,
    },
}

#[derive(Default)]
pub(super) struct MouseState {
    pub pos: Option<(u16, u16)>,
    gesture: Option<Gesture>,
    last_click: Option<(Instant, (u16, u16), u8)>,
    scroll: MouseScrollState,
}

impl MouseState {
    pub fn reset(&mut self) {
        self.pos = None;
        self.cancel_gesture();
    }

    pub fn cancel_gesture(&mut self) {
        self.gesture = None;
        self.last_click = None;
        self.scroll.cancel_stream();
    }

    fn count_click(&mut self, position: (u16, u16)) -> u8 {
        let now = Instant::now();
        let clicks = self
            .last_click
            .filter(|(at, (col, row), _)| {
                now.duration_since(*at) <= Duration::from_millis(300)
                    && *row == position.1
                    && col.abs_diff(position.0) <= 1
            })
            .map_or(1, |(_, _, count)| count % 3 + 1);
        self.last_click = Some((now, position, clicks));
        clicks
    }
}

impl TuiApp {
    pub(super) fn pointer_at_mouse(&self) -> bool {
        let Some((col, row)) = self.mouse.pos else {
            return false;
        };
        let pos = Position::new(col, row);
        if let Some(modal) = self.modals.front() {
            return modal.pointer(col, row);
        }
        if self
            .hits
            .dropdown
            .as_ref()
            .is_some_and(|d| d.items.contains(pos))
        {
            return true;
        }
        if let Some(welcome) = &self.hits.welcome {
            return matches!(welcome.hit_test(col, row), Some(WelcomeTarget::Menu(_)));
        }
        self.queue_pane.delete_click(col, row).is_some()
            || self.queue_pane.edit_click(col, row).is_some()
            || self.hits.bg_tasks.is_some_and(|rect| rect.contains(pos))
            || self.hits.header.button_at(col, row).is_some()
            || self
                .hits
                .view_button
                .is_some_and(|(_, rect)| rect.contains(pos))
            || (self.harness.capabilities().model_picker.is_some()
                && self
                    .composer
                    .status_area()
                    .is_some_and(|rect| rect.contains(pos)))
            || self.hits.transcript.as_deref().is_some_and(|frame| {
                frame.link_target_at(col, row).is_some()
                    || frame.block_at(col, row).is_some_and(|hit| {
                        (hit.foldable || hit.group_header)
                            && hit.header.is_some_and(|area| area.contains(pos))
                    })
            })
    }

    pub(super) fn mouse_event(&mut self, mouse: MouseEvent) {
        let (col, row) = (mouse.column, mouse.row);
        let position = Position::new(col, row);
        let left_down = mouse.kind == Kind::Down(MouseButton::Left);
        let wheel = ScrollDirection::from_mouse_event(&mouse);
        if wheel.is_none() && !matches!(mouse.kind, Kind::Moved) {
            self.mouse.scroll.cancel_stream();
        }

        if let Some(gesture) = self.mouse.gesture
            && matches!(
                mouse.kind,
                Kind::Drag(MouseButton::Left) | Kind::Up(MouseButton::Left)
            )
        {
            self.drag(mouse, gesture);
            return;
        }

        if let Some(dropdown) = &self.hits.dropdown {
            let inside = dropdown.panel.contains(position);
            #[cfg(feature = "history-search")]
            let history = self.composer.history_search.is_active();
            #[cfg(not(feature = "history-search"))]
            let history = false;
            if history {
                self.composer.handle_history_mouse(&mouse, dropdown);
            } else {
                self.composer.handle_completion_mouse(&mouse, dropdown);
            }
            if inside {
                self.mouse.scroll.cancel_stream();
                return;
            }
        }

        if let Some(welcome) = &self.hits.welcome
            && left_down
            && let Some(WelcomeTarget::Menu(index)) = welcome.hit_test(col, row)
        {
            self.focus = Focus::Welcome(index);
            if let Some((id, _, _)) = self.welcome_actions().get(index) {
                self.welcome_action(id);
            }
            return;
        }
        if left_down
            && let Some((id, rect)) = self.hits.view_button
            && rect.contains(position)
        {
            self.open_block(id);
            return;
        }
        if left_down
            && let Some(rect) = self.hits.bg_tasks
            && rect.contains(position)
        {
            self.open_background_tasks();
            return;
        }
        if left_down && let Some(button) = self.hits.header.button_at(col, row) {
            self.command(button);
            return;
        }
        if self.hits.prompt.contains(position) {
            self.mouse.scroll.cancel_stream();
            if left_down {
                self.focus_prompt();
                self.history.state.clear_text_selection();
                self.mouse.gesture = Some(Gesture::Prompt);
            }
            if mouse.kind != Kind::Moved {
                let before = self.composer.cursor();
                if self.composer.handle_mouse(&mouse) == PromptEvent::ActivateStatus
                    && self.harness.capabilities().model_picker.is_some()
                {
                    self.mouse.gesture = None;
                    self.command("model");
                } else if before != self.composer.cursor() {
                    self.refresh_completion();
                }
            }
            return;
        }
        if self.hits.queue.contains(position) {
            self.mouse.scroll.cancel_stream();
            if left_down {
                if let Some(id) = self.queue_pane.delete_click(col, row) {
                    self.remove_queued_row(id, false);
                    return;
                }
                if let Some(id) = self.queue_pane.edit_click(col, row) {
                    self.remove_queued_row(id, true);
                    return;
                }
                self.focus = Focus::Queue;
            }
            if let Some(direction) = wheel {
                self.queue_pane.handle_scroll(
                    if direction == ScrollDirection::Up {
                        -3
                    } else {
                        3
                    },
                    col,
                    row,
                );
            } else {
                self.queue_pane
                    .handle_mouse(mouse.kind, col, row, self.hits.queue);
                if mouse.kind == Kind::Moved {
                    self.queue_pane.update_delete_hover(col, row);
                    self.queue_pane.update_edit_hover(col, row);
                    self.queue_pane.update_row_hover(col, row);
                }
            }
            return;
        } else if mouse.kind == Kind::Moved {
            self.queue_pane.clear_delete_hover();
            self.queue_pane.clear_edit_hover();
            self.queue_pane.clear_row_hover();
        }
        if self.mode != ScreenMode::Fullscreen {
            return;
        }
        let Some(frame) = self.hits.transcript.clone() else {
            return;
        };
        if let Some(direction) = wheel {
            if self.hits.transcript_area.contains(position) {
                let config = ScrollConfig::from_settings()
                    .with_viewport_height(frame.scrollback_rect.height);
                let lines = self.mouse.scroll.on_scroll_event(direction, config).lines;
                self.scroll_lines(lines);
            } else {
                self.mouse.scroll.cancel_stream();
            }
            return;
        }
        if !left_down {
            return;
        }
        if frame.scrollbar_at(col, row).is_some() {
            self.mouse.gesture = Some(Gesture::Scrollbar);
            frame.drag_scrollbar(&mut self.history.state, row);
            return;
        }
        if !frame.scrollback_rect.contains(position) {
            return;
        }
        let clicks = self.mouse.count_click((col, row));
        self.mouse.gesture = Some(Gesture::Transcript {
            start: (col, row),
            clicks,
            moved: false,
        });
        self.history.state.clear_text_selection();
        if clicks == 1
            && frame.link_target_at(col, row).is_some()
            && mouse.modifiers.intersects(
                crossterm::event::KeyModifiers::CONTROL
                    | crossterm::event::KeyModifiers::SUPER
                    | crossterm::event::KeyModifiers::META,
            )
        {
            self.mouse.gesture = None;
            self.open_link(frame.link_target_at(col, row).cloned());
        } else {
            self.history.state.start_selection(col, row);
        }
    }

    fn drag(&mut self, mouse: MouseEvent, gesture: Gesture) {
        let (col, row) = (mouse.column, mouse.row);
        let released = matches!(mouse.kind, Kind::Up(_));
        if released {
            self.mouse.gesture = None;
        }
        match gesture {
            Gesture::Prompt => {
                let before = self.composer.cursor();
                self.composer.handle_mouse(&mouse);
                if before != self.composer.cursor() {
                    self.refresh_completion();
                }
            }
            Gesture::Scrollbar => {
                if let Some(frame) = &self.hits.transcript {
                    frame.drag_scrollbar(&mut self.history.state, row);
                }
            }
            Gesture::Transcript {
                start,
                clicks,
                moved,
            } => {
                if !released {
                    self.mouse.last_click = None;
                    self.mouse.gesture = Some(Gesture::Transcript {
                        start,
                        clicks,
                        moved: moved || start != (col, row),
                    });
                    self.history.state.update_selection(col, row);
                    return;
                }
                if moved || start != (col, row) {
                    self.history.state.update_selection(col, row);
                    if self.history.state.finish_selection() {
                        self.copy_selection();
                    }
                    return;
                }
                self.history.state.finish_selection();
                self.transcript_click(col, row, clicks);
            }
        }
    }

    fn transcript_click(&mut self, col: u16, row: u16, clicks: u8) {
        let Some(frame) = self.hits.transcript.clone() else {
            return;
        };
        if let Some(target) = frame.link_target_at(col, row) {
            self.open_link(Some(target.clone()));
            return;
        }
        let state = &mut self.history.state;
        if let Some(hit) = frame.block_at(col, row)
            && (hit.foldable || hit.group_header)
            && hit
                .header
                .is_some_and(|header| header.contains((col, row).into()))
        {
            self.focus = Focus::Transcript;
            if clicks == 2 {
                self.open_block(hit.entry_id);
            } else {
                frame.toggle_block(state, col, row);
            }
        } else if clicks == 2 && state.select_word(col, row)
            || clicks == 3 && state.select_line(col, row)
        {
            self.copy_selection();
        }
    }

    fn open_link(&self, target: Option<LinkTarget>) {
        if !self.native {
            return;
        }
        match target {
            Some(LinkTarget::Url(url)) => {
                link_opener::open_url_if_safe(&url, SchemeFilter::Standard);
            }
            Some(LinkTarget::File(path)) => {
                link_opener::open_path(&path);
            }
            None => {}
        }
    }

    fn copy_selection(&mut self) {
        if let Some(text) = self.history.state.selected_text() {
            self.copy(&text);
        }
    }

    pub(super) fn copy(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let succeeded = if let Some(copy) = &self.config.clipboard {
            copy(text).is_ok()
        } else {
            self.native && clipboard::copy_text_to_clipboard(text).reported_success()
        };
        self.show_toast(if succeeded {
            "Copied"
        } else {
            "Clipboard unavailable"
        });
    }

    pub(super) fn show_toast(&mut self, message: &str) {
        self.toast = Some((
            self.config.labels.get(message).to_owned(),
            Instant::now() + Duration::from_secs(2),
        ));
    }

    fn scroll_lines(&mut self, lines: i32) -> bool {
        if lines == 0 {
            return false;
        }
        let lines = lines.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
        self.history.state.scroll_by(lines);
        true
    }

    pub(super) fn pointer_tick(&mut self) -> bool {
        let update = self.mouse.scroll.on_tick();
        let mut changed = self.scroll_lines(update.lines);
        if matches!(
            self.mouse.gesture,
            Some(Gesture::Transcript { moved: true, .. })
        ) {
            changed |= self.history.state.tick_selection_autoscroll();
        }
        if self
            .toast
            .as_ref()
            .is_some_and(|(_, until)| Instant::now() >= *until)
        {
            self.toast = None;
            changed = true;
        }
        changed
    }
}
