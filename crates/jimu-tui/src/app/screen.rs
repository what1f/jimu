use std::{
    io::{self, Stdout, Write},
    path::Path,
    sync::Arc,
};

use ratatui::{
    Viewport,
    backend::{Backend, CrosstermBackend},
    buffer::Buffer,
    layout::{Rect, Size},
    text::Line,
};

use crate::{
    engine::{
        inline::{
            LinkSpan, Terminal, emit_to_scrollback, resize_viewport_height,
            with_synchronized_output,
        },
        render::{SafeBuf, osc8::LinkTarget},
        terminal::{SetDefaultCursor, SetPointerCursor},
    },
    harness::{
        ChromeSlots, ChromeState, render_default_status_line,
        render_default_status_line_with_spinner,
    },
    ui::{
        scrollback::{
            ScratchBuffer, ScrollbackFrame, ScrollbackPane,
            inline::{InlineRow, inline_appearance, render_inline},
        },
        views::{
            completion_dropdown::{DropdownChrome, render_floating_dropdown},
            context_bar::context_bar_line_for_session,
            header::{HeaderButton, HeaderConfig, HeaderHits, render_header},
            location::location_parts,
            prompt_widget::{PromptFlag, PromptInfo, PromptPostFlush, PromptStyle},
            queue_pane::QueueLayout,
            welcome::{
                WelcomeConfig, WelcomeLogo, WelcomeMenuItem, WelcomePromptFocus,
                WelcomeRenderParams, WelcomeRenderResult, paint_welcome_toast, render_welcome,
            },
        },
    },
};

use super::{Focus, TuiApp, dialogs::Modal, runner::TerminalGuard};

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub enum ScreenMode {
    Inline,
    #[default]
    Fullscreen,
}

#[derive(Default)]
pub(super) struct FrameHits {
    pub area: Rect,
    pub prompt: Rect,
    pub queue: Rect,
    pub bg_tasks: Option<Rect>,
    pub header: HeaderHits,
    pub view_button: Option<(crate::ui::scrollback::EntryId, Rect)>,
    pub welcome: Option<WelcomeRenderResult>,
    pub transcript: Option<Arc<ScrollbackFrame>>,
    pub transcript_area: Rect,
    pub dropdown: Option<DropdownChrome>,
}

#[derive(Default)]
struct Drawing {
    cursor: Option<(u16, u16)>,
    post_flush: Option<PromptPostFlush>,
    links: Vec<LinkSpan>,
}

impl TuiApp {
    pub(super) fn welcome_actions(&self) -> Vec<(&'static str, &'static str, &'static str)> {
        let caps = self.harness.capabilities();
        let mut actions = Vec::new();
        if caps.session_picker.is_some() {
            actions.push(("resume", "Resume session", "/resume"));
        }
        if caps.model_picker.is_some() {
            actions.push(("model", "Switch model", "Alt+P"));
        }
        actions.extend([
            ("theme", "Switch theme", "/theme"),
            ("help", "Shortcuts", "?"),
            ("exit", "Exit", "Ctrl+Q"),
        ]);
        actions
    }

    fn draw(
        &mut self,
        buf: &mut Buffer,
        area: Rect,
        scratch: &mut ScratchBuffer,
        tail: &[InlineRow],
    ) -> Drawing {
        let previous_view_button = self.hits.view_button;
        self.hits = FrameHits {
            area,
            ..Default::default()
        };
        let mut drawing = Drawing::default();
        if area.is_empty() {
            return drawing;
        }
        let theme = &self.theme;
        buf.set_style(area, theme.primary().bg(theme.bg_base));
        let chrome = &self.config.chrome;
        let cwd = self
            .config
            .directory
            .as_deref()
            .unwrap_or_else(|| Path::new("."));
        let title = chrome
            .app_name
            .as_deref()
            .unwrap_or_else(|| self.config.labels.get("Terminal"));
        let caps = self.harness.capabilities();
        let bg_tasks = caps.tasks.map(|cap| cap.list_tasks()).unwrap_or_default();
        let snap = ChromeSnapshot::capture(self, cwd);
        let state = snap.as_state(self, cwd, area.width);
        let dynamic_flags = chrome.slots.status_flags(&state);
        let mut flags = Vec::new();
        flags.extend(chrome.status_flags.iter().map(|text| PromptFlag {
            text,
            color: None,
            bold: false,
        }));
        flags.extend(dynamic_flags.iter().map(|text| PromptFlag {
            text,
            color: None,
            bold: false,
        }));
        let prompt_info = PromptInfo {
            model_name: "",
            flags: &flags,
            ..Default::default()
        };
        let mouse = self.modals.is_empty().then_some(self.mouse.pos).flatten();
        let mut body = area;
        let header = chrome.slots.header(&state);
        let above_prompt = chrome.slots.above_prompt(&state);
        let status_lines = chrome
            .slots
            .status_line(&state)
            .unwrap_or_else(|| default_status_line(&state, chrome.slots.as_ref()));
        let mut footer = status_lines;
        footer.extend(chrome.slots.footer(&state));
        let welcome = if self.mode == ScreenMode::Fullscreen
            && self.history.state.is_empty()
            && self.queue.is_empty()
            && !self.queue_open
        {
            chrome.slots.welcome(&state)
        } else {
            Vec::new()
        };
        paint(buf, take_top(&mut body, row_count(header.len())), &header);
        paint(
            buf,
            take_bottom(&mut body, row_count(footer.len())),
            &footer,
        );
        if self.mode == ScreenMode::Fullscreen
            && self.history.state.is_empty()
            && self.queue.is_empty()
            && !self.queue_open
        {
            paint(buf, take_top(&mut body, row_count(welcome.len())), &welcome);
            let actions = self.welcome_actions();
            let menu = actions
                .iter()
                .map(|(_, label, shortcut)| WelcomeMenuItem {
                    label: self.config.labels.get(label),
                    shortcut,
                })
                .collect::<Vec<_>>();
            let config = WelcomeConfig {
                title,
                version: chrome.app_version.as_deref().unwrap_or_default(),
                logo: WelcomeLogo::default(),
                subtitle: chrome.welcome_subtitle.as_deref(),
                notes_title: chrome
                    .welcome_notes_title
                    .as_deref()
                    .unwrap_or_else(|| self.config.labels.get("What's new")),
                notes: &chrome.welcome_notes,
                notes_clickable: false,
                menu: &menu,
                footer_hint: chrome.footer_hint.as_deref(),
                placeholder: None,
            };
            let params = WelcomeRenderParams {
                config: &config,
                cwd,
                prompt_info,
                prompt_focus: if self.focus == Focus::Prompt {
                    WelcomePromptFocus::Focused
                } else {
                    WelcomePromptFocus::Unfocused
                },
                compact: area.width < 67,
                selected: if let Focus::Welcome(i) = self.focus {
                    Some(i)
                } else {
                    None
                },
                mouse_pos: mouse,
                notice: None,
                notice_expanded: false,
                notice_action: None,
                tip: None,
                error: None,
            };
            let mut result = render_welcome(body, buf, theme, &params, &mut self.composer);
            self.hits.prompt = result.prompt_rect.unwrap_or_default();
            drawing.cursor = result.cursor_pos;
            drawing.post_flush = result.post_flush_escapes.take();
            self.hits.welcome = Some(result);
        } else {
            if chrome.show_header && header.is_empty() {
                let header = take_top(&mut body, 1);
                let context = self.context.as_ref().map(|c| {
                    (
                        Some(c.tokens.unwrap_or(c.estimated_tokens)),
                        Some(u64::from(c.context_window)),
                    )
                });
                let context_line = context.and_then(|(used, cap)| {
                    context_bar_line_for_session(used, cap, false, theme, false)
                });
                let context_label = context_line.as_ref().map(ToString::to_string);
                let mut buttons = Vec::new();
                if let Some(label) = &context_label {
                    buttons.push(HeaderButton {
                        id: "context",
                        label,
                    });
                }
                buttons.extend([
                    HeaderButton {
                        id: "theme",
                        label: self.config.labels.get("Theme"),
                    },
                    HeaderButton {
                        id: "help",
                        label: "?",
                    },
                ]);
                self.hits.header = render_header(
                    buf,
                    header,
                    theme,
                    &HeaderConfig {
                        title: Some(title),
                        cwd,
                        buttons: &buttons,
                        pointer: mouse,
                        suppress_hits: false,
                    },
                );
                if let Some((used, cap)) = context
                    && let Some(&rect) = self.hits.header.buttons.get("context")
                    && let Some(line) = context_bar_line_for_session(
                        used,
                        cap,
                        mouse.is_some_and(|pos| rect.contains(pos.into())),
                        theme,
                        false,
                    )
                {
                    buf.set_line_safe(rect.x, rect.y, &line, rect.width);
                }
            }
            if !bg_tasks.is_empty() && body.height > 1 {
                let strip = take_bottom(&mut body, 1);
                let hovered = mouse.is_some_and(|pos| strip.contains(pos.into()));
                let summary = bg_tasks
                    .iter()
                    .map(|t| {
                        let cmd = t
                            .command
                            .split_whitespace()
                            .take(3)
                            .collect::<Vec<_>>()
                            .join(" ");
                        if cmd.is_empty() {
                            t.title.clone()
                        } else {
                            format!("{} ({cmd})", t.title)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("   ");
                let text_style = if hovered {
                    theme.primary()
                } else {
                    theme.muted()
                };
                let line = Line::from(vec![
                    ratatui::text::Span::styled(
                        "  ● ",
                        ratatui::style::Style::default().fg(theme.accent_success),
                    ),
                    ratatui::text::Span::styled(summary, text_style),
                ]);
                buf.set_line_safe(strip.x, strip.y, &line, strip.width);
                self.hits.bg_tasks = Some(strip);
            }
            let style = self.prompt_style(area);
            let height = self.composer.desired_height(
                body.width,
                &style,
                true,
                (body.height / 3).max(4).min(body.height),
            );
            let prompt = take_bottom(&mut body, height);
            self.hits.prompt = prompt;
            let queue_height = self
                .queue_pane
                .desired_height()
                .max(u16::from(self.queue_open));
            let queue = take_bottom(&mut body, queue_height);
            self.hits.queue = queue;
            if !queue.is_empty() {
                if self.queue.is_empty() {
                    buf.set_line_safe(
                        queue.x,
                        queue.y,
                        &Line::styled(self.config.labels.get("Queue is empty"), theme.dim()),
                        queue.width,
                    );
                } else {
                    self.queue_pane.render(
                        queue,
                        buf,
                        self.focus == Focus::Queue,
                        &QueueLayout { block_pad_left: 2 },
                        None,
                        false,
                    );
                }
            }
            if !above_prompt.is_empty() {
                paint(
                    buf,
                    take_bottom(&mut body, row_count(above_prompt.len())),
                    &above_prompt,
                );
            }
            if let Some(search) = &self.search {
                let row = take_top(&mut body, 1);
                if !row.is_empty() {
                    let label = self.config.labels.get("Find");
                    let offset =
                        unicode_width::UnicodeWidthStr::width(label).saturating_add(2) as u16;
                    let viewport = search.viewport(usize::from(row.width.saturating_sub(offset)));
                    let query = &search.text()[viewport.visible_byte_range];
                    buf.set_line_safe(
                        row.x,
                        row.y,
                        &Line::styled(format!("{label}  {query}"), theme.primary()),
                        row.width,
                    );
                    drawing.cursor = Some((
                        row.x
                            .saturating_add(offset)
                            .saturating_add(viewport.cursor_display_column as u16)
                            .min(row.right() - 1),
                        row.y,
                    ));
                }
            }
            if self.mode == ScreenMode::Fullscreen && !body.is_empty() {
                self.hits.transcript_area = body;
                let mut pane = ScrollbackPane::new().active(self.focus == Focus::Transcript);
                if let Some(pos) = mouse {
                    pane = pane.with_mouse_pos(pos);
                }
                let frame = pane.render(body, buf, &mut self.history.state, scratch);
                let hovered = mouse.and_then(|(x, y)| frame.block_at(x, y));
                let hovered_button = previous_view_button
                    .filter(|(_, rect)| mouse.is_some_and(|pos| rect.contains(pos.into())))
                    .and_then(|(id, _)| {
                        frame
                            .block_hit_rects
                            .iter()
                            .find(|hit| hit.entry_id == id)
                            .copied()
                    });
                let selected = self.history.state.selected().and_then(|index| {
                    frame
                        .block_hit_rects
                        .iter()
                        .find(|hit| hit.entry_index == index)
                        .copied()
                });
                let target = hovered
                    .or(hovered_button)
                    .or(selected)
                    .filter(|hit| hit.foldable && !hit.group_header);
                self.hits.view_button = target.and_then(|hit| {
                    let header = hit.header?;
                    let x = header.right();
                    let rect = Rect::new(x, header.y, 1, 1);
                    let usable = x < body.right() && frame.scrollbar_at(x, header.y).is_none();
                    usable.then_some((hit.entry_id, rect))
                });
                if let Some((_, rect)) = self.hits.view_button {
                    let style = if mouse.is_some_and(|pos| rect.contains(pos.into())) {
                        theme.primary()
                    } else {
                        theme.dim()
                    };
                    buf.set_string(rect.x, rect.y, "↗", style);
                }
                self.hits.transcript = Some(frame);
            } else {
                let from = tail.len().saturating_sub(usize::from(body.height));
                for (i, row) in tail[from..].iter().enumerate() {
                    let y = body
                        .bottom()
                        .saturating_sub(row_count(tail.len() - from))
                        .saturating_add(i as u16);
                    buf.set_line_safe(body.x, y, &row.line, body.width);
                }
            }
            let prompt = self
                .composer
                .draw(buf, prompt, None, &style, Some(&prompt_info), None);
            drawing.cursor = drawing.cursor.or(prompt.cursor_pos);
            drawing.post_flush = prompt.post_flush_escapes.map(Into::into);
        }
        if self.modals.is_empty() {
            let prompt = self.hits.prompt;
            self.hits.dropdown = render_floating_dropdown(
                buf,
                area,
                prompt,
                self.composer.completion_dropdown(),
                theme,
            );
            #[cfg(feature = "history-search")]
            if self.composer.history_search.is_active() {
                use crate::ui::views::history_search::{HistoryLabels, render_history};
                self.hits.dropdown = render_history(
                    buf,
                    area,
                    prompt,
                    &self.composer.history_search,
                    theme,
                    false,
                    HistoryLabels {
                        title: self.config.labels.get(" history "),
                        loading: self.config.labels.get("  Loading…"),
                        empty: self.config.labels.get("  No matching history"),
                    },
                );
            }
            if let Some(frame) = &self.hits.transcript {
                drawing.links = frame
                    .link_map
                    .links()
                    .iter()
                    .filter_map(|link| {
                        let url = match &link.target {
                            LinkTarget::Url(url) => url.clone(),
                            LinkTarget::File(path) => {
                                Arc::from(url::Url::from_file_path(path).ok()?.as_str())
                            }
                        };
                        Some(link.rects.iter().flat_map(move |rect| {
                            let url = url.clone();
                            (rect.y..rect.bottom()).map(move |row| LinkSpan {
                                row,
                                col_start: rect.x,
                                col_end: rect.right(),
                                url: url.clone(),
                                id: link.id,
                            })
                        }))
                    })
                    .flatten()
                    .filter(|link| {
                        self.hits.dropdown.as_ref().is_none_or(|dropdown| {
                            link.row < dropdown.panel.y
                                || link.row >= dropdown.panel.bottom()
                                || link.col_start >= dropdown.panel.right()
                                || link.col_end <= dropdown.panel.x
                        })
                    })
                    .collect();
            }
        } else if let Some(modal) = self.modals.front_mut() {
            if let Modal::Viewer(viewer) = modal {
                viewer.refresh(&mut self.history.state, area.width, theme);
            }
            drawing.cursor = modal.draw(
                area,
                buf,
                theme,
                &self.config.text,
                &self.config.labels,
                self.mouse.pos,
            );
            drawing.post_flush = None;
        }
        if let Some((text, _)) = &self.toast {
            paint_welcome_toast(
                buf,
                area,
                text,
                self.modals.is_empty().then_some(self.hits.prompt),
            );
        }
        drawing
    }

    fn prompt_style(&self, area: Rect) -> PromptStyle {
        PromptStyle {
            focused: self.focus == Focus::Prompt && self.search.is_none() && self.modals.is_empty(),
            compact: area.width < 67,
            prefix_override: Some(("> ", self.theme.accent_user)),
            ..Default::default()
        }
    }
}

fn row_count(n: usize) -> u16 {
    n.min(usize::from(u16::MAX)) as u16
}

fn take_top(area: &mut Rect, rows: u16) -> Rect {
    let height = rows.min(area.height);
    let out = Rect { height, ..*area };
    area.y = area.y.saturating_add(height);
    area.height -= height;
    out
}

fn take_bottom(area: &mut Rect, rows: u16) -> Rect {
    let height = rows.min(area.height);
    area.height -= height;
    Rect {
        height,
        y: area.bottom(),
        ..*area
    }
}

fn paint(buf: &mut Buffer, area: Rect, rows: &[Line<'static>]) {
    for (i, line) in rows.iter().take(usize::from(area.height)).enumerate() {
        buf.set_line_safe(area.x, area.y + i as u16, line, area.width);
    }
}

pub(super) struct Screen {
    mode: Option<ScreenMode>,
    main_area: Rect,
    committed: usize,
    generation: u64,
    scratch: ScratchBuffer,
    pointer: bool,
}

impl Default for Screen {
    fn default() -> Self {
        Self {
            mode: None,
            main_area: Rect::default(),
            committed: 0,
            generation: 0,
            scratch: ScratchBuffer::new(),
            pointer: false,
        }
    }
}

impl Screen {
    fn reset_commit_frontier(&mut self, app: &TuiApp) {
        if self.generation != app.generation {
            self.generation = app.generation;
            self.committed = 0;
        }
    }

    fn commit_inline(
        &mut self,
        app: &mut TuiApp,
        width: u16,
        mut emit: impl FnMut(&[InlineRow]) -> io::Result<()>,
    ) -> io::Result<()> {
        self.reset_commit_frontier(app);
        let mut count = 0;
        let result = app.history.state.commit_inline(
            width,
            0,
            app.running,
            true,
            |_| Line::default(),
            |_, rows| {
                emit(rows)?;
                count += 1;
                Ok(())
            },
        );
        self.committed += count;
        result.map(|_| ())
    }

    fn live_tail(&self, app: &TuiApp, size: Size) -> Vec<InlineRow> {
        let state = &app.history.state;
        let appearance = inline_appearance(state.appearance());
        state
            .iter_entries()
            .skip(self.committed)
            .flat_map(|(_, entry)| {
                render_inline(
                    entry,
                    &appearance,
                    state.cwd(),
                    size.width,
                    size.height,
                    |_| Line::default(),
                )
            })
            .collect()
    }

    fn inline_height(app: &TuiApp, size: Size, tail: &[InlineRow]) -> u16 {
        if !app.modals.is_empty() {
            return size.height;
        }
        let width = size.width;
        let style = app.prompt_style(Rect::new(0, 0, width, size.height));
        let prompt = app.composer.desired_height(
            width,
            &style,
            true,
            (size.height / 3).max(4).min(size.height),
        );
        let cwd = app
            .config
            .directory
            .as_deref()
            .unwrap_or_else(|| Path::new("."));
        let snap = ChromeSnapshot::capture(app, cwd);
        let chrome_state = snap.as_state(app, cwd, width);
        let slots = &app.config.chrome.slots;
        let status_rows = slots
            .status_line(&chrome_state)
            .map_or(1, |lines| lines.len());
        let custom_header_rows = slots.header(&chrome_state).len();
        let extra = custom_header_rows
            + slots.above_prompt(&chrome_state).len()
            + slots.footer(&chrome_state).len()
            + status_rows;
        let dropdown = if app.composer.any_dropdown_open() {
            9
        } else {
            0
        };
        let header_row = u16::from(app.config.chrome.show_header && custom_header_rows == 0);
        (header_row
            .saturating_add(prompt)
            .saturating_add(
                app.queue_pane
                    .desired_height()
                    .max(u16::from(app.queue_open)),
            )
            .saturating_add(row_count(extra + tail.len()))
            .saturating_add(dropdown))
        .min(size.height)
        .max(1)
    }

    pub fn backend<B: Backend>(
        &mut self,
        term: &mut Terminal<B>,
        app: &mut TuiApp,
    ) -> io::Result<()> {
        let size = term.size()?;
        if size.width == 0 || size.height == 0 {
            return Ok(());
        }
        let switched = self.mode != Some(app.mode);
        if switched {
            term.backend_mut().set_cursor_position((0, 0))?;
            term.set_viewport(if app.mode == ScreenMode::Inline {
                Viewport::Inline(1)
            } else {
                Viewport::Fullscreen
            })?;
            term.clear()?;
        }
        term.autoresize()?;
        let mut tail = Vec::new();
        if app.mode == ScreenMode::Inline {
            self.commit_inline(app, size.width, |rows| {
                for chunk in rows.chunks(usize::from(size.height)) {
                    term.insert_before(row_count(chunk.len()), |buf| {
                        for (i, row) in chunk.iter().enumerate() {
                            buf.set_line_safe(
                                buf.area.x,
                                buf.area.y + i as u16,
                                &row.line,
                                buf.area.width,
                            );
                        }
                    })?;
                }
                Ok(())
            })?;
            tail = self.live_tail(app, size);
            term.set_viewport_height(Self::inline_height(app, size, &tail))?;
        }
        self.mode = Some(app.mode);
        term.draw(|frame| {
            let area = frame.area();
            let drawing = app.draw(frame.buffer_mut(), area, &mut self.scratch, &tail);
            if let Some(cursor) = drawing.cursor {
                frame.set_cursor_position(cursor);
            }
        })?;
        Ok(())
    }

    pub fn native(
        &mut self,
        term: &mut Terminal<CrosstermBackend<Stdout>>,
        app: &mut TuiApp,
        guard: &mut TerminalGuard,
    ) -> io::Result<()> {
        let size = term.size()?;
        if size.width == 0 || size.height == 0 {
            return Ok(());
        }
        let full = app.mode == ScreenMode::Fullscreen;
        let switched = self.mode != Some(app.mode);
        if self.mode.is_none() {
            self.main_area = term.viewport_area();
        }
        if full && !guard.alternate() && self.mode.is_some() {
            term.clear()?;
        }
        guard.set_alternate(term.backend_mut(), full)?;
        if !full {
            self.pointer = false;
        }
        with_synchronized_output(term, |term| {
            let mut tail = Vec::new();
            if full {
                let area = Rect::new(0, 0, size.width, size.height);
                if switched || term.viewport_area() != area {
                    term.set_viewport(Viewport::Fixed(area))?;
                    term.clear()?;
                }
            } else {
                let mut area = self.main_area;
                area.width = size.width;
                area.height = area.height.clamp(1, size.height);
                area.y = area.y.min(size.height - area.height);
                if switched || term.viewport_area() != area {
                    term.set_viewport(Viewport::Fixed(area))?;
                }
                self.commit_inline(app, size.width, |rows| {
                    for row in rows {
                        emit_to_scrollback(term, &ansi_row(&row.line, size.width))?;
                    }
                    Ok(())
                })?;
                tail = self.live_tail(app, size);
                let height = Self::inline_height(app, size, &tail);
                if height < size.height {
                    resize_viewport_height(term, height)?;
                } else if term.viewport_area().height != size.height {
                    let origin = term.viewport_area().y;
                    if origin > 0 {
                        term.backend_mut()
                            .set_cursor_position((0, size.height - 1))?;
                        term.backend_mut().append_lines(origin)?;
                    }
                    term.set_viewport_area(Rect::new(0, 0, size.width, size.height));
                    term.clear()?;
                }
                self.main_area = term.viewport_area();
            }
            self.mode = Some(app.mode);
            let area = term.viewport_area();
            let drawing = app.draw(term.current_buffer_mut(), area, &mut self.scratch, &tail);
            term.set_frame_links(&drawing.links);
            term.flush_with_links()?;
            if let Some((x, y)) = drawing.cursor {
                term.show_cursor()?;
                term.set_cursor_position((x, y))?;
            } else {
                term.hide_cursor()?;
            }
            term.swap_buffers();
            let pointer = full && app.pointer_at_mouse();
            if pointer != self.pointer {
                if pointer {
                    crossterm::queue!(term.backend_mut(), SetPointerCursor)?;
                } else {
                    crossterm::queue!(
                        term.backend_mut(),
                        SetDefaultCursor,
                        crossterm::cursor::SetCursorStyle::DefaultUserShape
                    )?;
                }
                self.pointer = pointer;
            }
            if let Some(post_flush) = drawing.post_flush {
                post_flush.write_to(term.backend_mut())?;
            }
            Write::flush(term.backend_mut())
        })
    }
}

struct ChromeSnapshot {
    model: Option<String>,
    effort: Option<String>,
    permission_mode: Option<String>,
    fast_mode: bool,
    git_branch: Option<String>,
    session_id: Option<String>,
}

impl ChromeSnapshot {
    fn capture(app: &TuiApp, cwd: &Path) -> Self {
        let caps = app.harness.capabilities();
        let model_picker = caps.model_picker;
        let model = model_picker.map(|cap| cap.current_model().name);
        let effort = model_picker
            .and_then(|cap| cap.current_thinking_level())
            .filter(|lvl| !lvl.is_empty() && lvl != "off");
        let fast_mode = model_picker.is_some_and(|cap| cap.fast_mode());
        let permission_mode = caps.permission_mode.map(|cap| cap.current_mode());
        let session_id = caps.session_picker.and_then(|cap| cap.current_session_id());
        // Pure in-memory cached lookup; never spawns `git` on the draw hot path.
        let git_branch = location_parts(cwd).branch;
        Self {
            model,
            effort,
            permission_mode,
            fast_mode,
            git_branch,
            session_id,
        }
    }

    fn as_state<'a>(&'a self, app: &'a TuiApp, cwd: &'a Path, width: u16) -> ChromeState<'a> {
        ChromeState {
            running: app.running,
            usage: &app.usage,
            context: app.context.as_ref(),
            model_name: self.model.as_deref(),
            queued_count: app.queue.len(),
            theme: &app.theme,
            width,
            cwd,
            git_branch: self.git_branch.as_deref(),
            effort: self.effort.as_deref(),
            permission_mode: self.permission_mode.as_deref(),
            fast_mode: self.fast_mode,
            session_id: self.session_id.as_deref(),
        }
    }
}

/// Re-export of [`crate::harness::render_default_status_line`] for hosts importing from `app::screen`.
pub fn default_status_line_row(state: &ChromeState<'_>) -> Line<'static> {
    render_default_status_line(state)
}

fn default_status_line(state: &ChromeState<'_>, slots: &dyn ChromeSlots) -> Vec<Line<'static>> {
    if state.width == 0 {
        return Vec::new();
    }
    vec![render_default_status_line_with_spinner(
        state,
        slots.spinner_override(state),
    )]
}

fn ansi_row(row: &Line<'_>, width: u16) -> String {
    use crate::engine::render::line_utils::fit_line_to_width;
    use crossterm::style::{Attribute, ContentStyle};
    use ratatui::style::Modifier;
    use std::fmt::Write;
    let mut result = String::new();
    let row = if row.style.bg.is_some() {
        fit_line_to_width(row.clone(), usize::from(width))
    } else {
        row.clone()
    };
    for span in &row.spans {
        let s = row.style.patch(span.style);
        let mut style = ContentStyle {
            foreground_color: s.fg.map(Into::into),
            background_color: s.bg.map(Into::into),
            ..Default::default()
        };
        for (modifier, attr) in [
            (Modifier::BOLD, Attribute::Bold),
            (Modifier::DIM, Attribute::Dim),
            (Modifier::ITALIC, Attribute::Italic),
            (Modifier::UNDERLINED, Attribute::Underlined),
            (Modifier::REVERSED, Attribute::Reverse),
            (Modifier::CROSSED_OUT, Attribute::CrossedOut),
        ] {
            if s.add_modifier.contains(modifier) {
                style.attributes.set(attr);
            }
        }
        let _ = write!(result, "{}", style.apply(&span.content));
    }
    if result.is_empty() {
        result.push(' ');
    }
    result
}
