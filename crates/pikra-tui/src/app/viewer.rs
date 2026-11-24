use crate::{
    engine::theme::Theme,
    ui::{
        scrollback::{DisplayMode, EntryId, RenderBlock, ScrollbackState, ToolCallBlock},
        views::block_viewer::{
            BlockViewerPane, EditContent, EditRow, MarkdownContent, OutputContent, ReadContent,
            ViewerContent, ViewerEntry, ViewerKind,
        },
    },
};
use crossterm::event::{Event, MouseButton, MouseEventKind};
use ratatui::{buffer::Buffer, layout::Rect};

use super::{
    Labels, TuiApp,
    dialogs::{DialogAction, Modal},
};

pub(super) struct Viewer {
    id: EntryId,
    pane: BlockViewerPane,
    entry: ViewerEntry,
    close: Rect,
}

impl TuiApp {
    pub(super) fn open_block(&mut self, id: EntryId) -> bool {
        let Some(viewer) = Viewer::new(id, &self.history.state, self.hits.area.width, &self.theme)
        else {
            return false;
        };
        self.history.state.clear_text_selection();
        self.modals.push_back(Modal::Viewer(Box::new(viewer)));
        true
    }
}

impl Viewer {
    fn new(id: EntryId, state: &ScrollbackState, width: u16, theme: &Theme) -> Option<Self> {
        let entry = Self::snapshot(state, id, width, theme)?;
        let value = id.value();
        let pane = match entry.block {
            ViewerContent::Markdown(_) => BlockViewerPane::for_markdown(value, &entry),
            ViewerContent::Execute(_) => BlockViewerPane::for_execute(value, &entry),
            ViewerContent::Read(_) => BlockViewerPane::for_read(value, &entry),
            ViewerContent::WebFetch(_) => BlockViewerPane::for_web_fetch(value, &entry),
            ViewerContent::Edit(_) => BlockViewerPane::for_edit(value, &entry),
            _ => None,
        }
        .or_else(|| {
            let source = state.get_by_id(id)?;
            let ctx = source.context_with_mode(
                width.max(1),
                DisplayMode::Expanded,
                state.appearance(),
                state.cwd(),
            );
            let output = source.output_uncached(&ctx);
            let lines = output
                .lines
                .into_iter()
                .map(|row| row.content)
                .collect::<Vec<_>>();
            (!lines.is_empty())
                .then(|| BlockViewerPane::for_expanded_lines(value, ViewerKind::PlainText, lines))
        })?;
        Some(Self {
            id,
            pane,
            entry,
            close: Rect::default(),
        })
    }

    fn snapshot(
        state: &ScrollbackState,
        id: EntryId,
        width: u16,
        theme: &Theme,
    ) -> Option<ViewerEntry> {
        let source = state.get_by_id(id)?;
        let width = width.max(1);
        let context = || {
            source.context_with_mode(
                width,
                DisplayMode::Expanded,
                state.appearance(),
                state.cwd(),
            )
        };
        let tool = match &source.block {
            RenderBlock::ToolCall(tool) => Some(tool),
            _ => None,
        };
        let block = match (&source.block, tool) {
            (RenderBlock::AgentMessage(_) | RenderBlock::Thinking(_), _) => {
                let output = source.output_uncached(&context());
                ViewerContent::Markdown(MarkdownContent {
                    lines: output.lines.into_iter().map(|row| row.content).collect(),
                    source_lines: Vec::new(),
                    generation: state.content_generation(),
                })
            }
            (_, Some(ToolCallBlock::Execute(tool))) => ViewerContent::Execute(OutputContent {
                output: tool.output.clone(),
            }),
            (_, Some(ToolCallBlock::Read(tool))) => ViewerContent::Read(ReadContent {
                path: tool.path.clone(),
                first_line: tool.line_range.map_or(1, |range| range.start),
                content: tool.content.clone(),
                error: tool.error.clone(),
            }),
            (_, Some(ToolCallBlock::WebFetch(tool))) => ViewerContent::WebFetch(OutputContent {
                output: tool.output.clone(),
            }),
            (_, Some(ToolCallBlock::Edit(tool))) => ViewerContent::Edit(EditContent {
                path: tool.path.clone(),
                rows: tool
                    .render_diff_lines(theme, width, &Default::default())
                    .into_iter()
                    .map(|row| EditRow {
                        line: row.line,
                        background: row.background,
                        source: None,
                    })
                    .collect(),
            }),
            _ => ViewerContent::Text,
        };
        Some(ViewerEntry {
            block,
            is_running: source.is_running,
            copy_text: source.block.copy_text(false),
            copy_meta: source.block.copy_meta(),
        })
    }

    pub(super) fn refresh(&mut self, state: &mut ScrollbackState, width: u16, theme: &Theme) {
        if std::mem::take(&mut self.pane.raw_toggle_pending)
            && let Some(source) = state.get_by_id_mut(self.id)
        {
            source.toggle_raw();
            state.mark_structurally_dirty(self.id);
        }
        if let Some(entry) = Self::snapshot(state, self.id, width, theme) {
            self.pane.tick(&entry);
            self.entry = entry;
        }
    }

    pub(super) fn draw(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        theme: &Theme,
        labels: &Labels,
    ) -> Option<(u16, u16)> {
        if area.is_empty() {
            return None;
        }
        buf.reset();
        buf.set_style(area, theme.primary().bg(theme.bg_base));
        let title = labels.get("Block details");
        let close = labels.get("Esc Close");
        let header = Rect { height: 1, ..area };
        let mut status = crate::ui::views::header::AgentStatusBar::new(theme);
        status.push(
            "close",
            ratatui::text::Line::styled(close.to_owned(), theme.dim()),
        );
        self.close = status
            .render(buf, header)
            .get("close")
            .copied()
            .unwrap_or_default();
        buf.set_stringn(
            header.x,
            header.y,
            title,
            usize::from(header.width.saturating_sub(close.len() as u16)),
            theme.primary(),
        );
        let area = Rect {
            y: area.y + 1,
            height: area.height.saturating_sub(1),
            ..area
        };
        self.pane.render_content(area, buf, &self.entry, true, &[]);
        self.pane.render_text_drag_overlay(buf);
        self.pane.list_state.cursor_position()
    }

    pub(super) fn pointer_at(&self, col: u16, row: u16) -> bool {
        self.close.contains((col, row).into())
    }

    pub(super) fn input(&mut self, event: &Event) -> DialogAction {
        match event {
            Event::Key(key) if self.pane.is_close_key(key) => return DialogAction::Close,
            Event::Key(key) => {
                self.pane.handle_key(key);
            }
            Event::Paste(text) => {
                self.pane.handle_paste(text);
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::Down(MouseButton::Left)
                    if self.pointer_at(mouse.column, mouse.row) =>
                {
                    return DialogAction::Close;
                }
                MouseEventKind::ScrollUp => self.pane.handle_scroll(-3),
                MouseEventKind::ScrollDown => self.pane.handle_scroll(3),
                _ => {
                    self.pane.handle_mouse(mouse.kind, mouse.column, mouse.row);
                    if mouse.kind == MouseEventKind::Up(MouseButton::Left)
                        && let Some(text) = self.pane.drag_copy_text.take()
                    {
                        return DialogAction::Copy(text);
                    }
                }
            },
            _ => {}
        }
        self.pane
            .process_pending_copy(&self.entry)
            .map_or(DialogAction::Stay, DialogAction::Copy)
    }
}
