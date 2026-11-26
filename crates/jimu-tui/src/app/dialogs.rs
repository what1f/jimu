use crate::{
    engine::{
        input::key::KeyShortcut,
        render::SafeBuf,
        terminal::osc7501::{BlockedKind, ProgramStatus},
        theme::Theme,
    },
    harness::{ApprovalDecision, InteractionRequest, PlanDecision, QuestionAnswer, UiText},
    ui::{
        scrollback::{ToolCallBlock, blocks::EditToolCallBlock},
        views::{
            modal_window::{
                self, ModalSizing, ModalWindowConfig, ModalWindowOutcome, ModalWindowState,
                Shortcut,
            },
            permission_view::{
                self, PermissionInput, PermissionOption, PermissionOptionKind,
                PermissionRenderResult, PermissionViewState,
            },
            picker::{
                self, PickerConfig, PickerEntry, PickerHitAreas, PickerOutcome, PickerRow,
                PickerState,
            },
            plan_approval_view::{
                PlanApprovalInput, PlanApprovalLabels, PlanApprovalPane, PlanApprovalRequest,
                PlanApprovalViewState,
            },
            prompt_widget::{EnterOutcome, PromptStyle, PromptWidget},
            question_view::{
                self, Question, QuestionFocus, QuestionInput, QuestionSelection,
                QuestionViewRenderResult, QuestionViewState,
            },
            shortcuts_bar::HintItem,
            shortcuts_help as help,
        },
    },
};
use crossterm::event::{Event, KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use futures_channel::oneshot::Sender;
use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
};
use std::collections::HashSet;

use super::{Labels, transcript::tool_data, viewer::Viewer};

#[derive(Clone, Copy)]
pub(super) enum PickerTarget {
    Theme,
    Model,
    Effort,
    Permissions,
    Rewind,
    Tasks,
    ReadOnly,
}

pub(super) struct ModelEffortEntry {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub is_current: bool,
    pub thinking_levels: Vec<String>,
}

pub(super) enum DialogAction {
    Stay,
    Close,
    Pick(PickerTarget, String),
    PickModelEffort {
        model: String,
        effort: Option<String>,
    },
    Copy(String),
}

pub(super) enum Modal {
    Permission(Box<Permission>),
    Question(Box<QuestionDialog>),
    Plan(Box<Plan>),
    Picker(Box<Picker>),
    ModelEffort(Box<ModelEffortPicker>),
    Shortcuts(Box<Shortcuts>),
    Viewer(Box<Viewer>),
}

impl Modal {
    pub fn picker(title: String, target: PickerTarget, rows: Vec<(String, String)>) -> Self {
        Self::Picker(Box::new(Picker::new(title, rows, Choice::Local(target))))
    }

    pub fn picker_with_selected(
        title: String,
        target: PickerTarget,
        rows: Vec<(String, String)>,
        initial_selected: usize,
    ) -> Self {
        Self::Picker(Box::new(
            Picker::new(title, rows, Choice::Local(target)).with_selected(initial_selected),
        ))
    }

    pub fn model_effort(
        title: String,
        entries: Vec<ModelEffortEntry>,
        current_effort: Option<String>,
    ) -> Self {
        Self::ModelEffort(Box::new(ModelEffortPicker::new(
            title,
            entries,
            current_effort,
        )))
    }

    pub fn shortcuts(labels: &Labels) -> Self {
        Self::Shortcuts(Box::new(Shortcuts::new(labels)))
    }

    pub fn interaction(request: InteractionRequest, text: &UiText) -> Self {
        match request {
            InteractionRequest::ApproveTool {
                tool_name,
                summary,
                detail,
                reply,
            } => {
                let kinds = [
                    ("allow", &text.allow, PermissionOptionKind::AllowOnce),
                    (
                        "always",
                        &text.always_allow,
                        PermissionOptionKind::AllowAlways,
                    ),
                    ("deny", &text.deny, PermissionOptionKind::RejectOnce),
                ];
                let options = kinds
                    .into_iter()
                    .map(|(id, name, kind)| PermissionOption {
                        id: id.into(),
                        name: name.clone(),
                        kind,
                        scope_prefix: None,
                    })
                    .collect();
                let args = detail
                    .as_deref()
                    .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                    .filter(serde_json::Value::is_object);
                let command = args.as_ref().and_then(|args| {
                    args["cmd"]
                        .as_str()
                        .or_else(|| args["command"].as_str())
                        .map(str::to_owned)
                });
                let diff = args.as_ref().and_then(|args| {
                    if let ToolCallBlock::Edit(edit) =
                        tool_data(tool_name.clone(), args.clone(), None).block
                        && !edit.hunks.is_empty()
                    {
                        return Some(edit);
                    }
                    None
                });
                let mut description = Vec::new();
                if !summary.is_empty() && command.as_deref() != Some(summary.as_str()) {
                    description.push(summary);
                }
                if let Some(raw_detail) = detail.as_ref()
                    && !raw_detail.is_empty()
                    && command.as_deref() != Some(raw_detail.as_str())
                    && description.first().map(String::as_str) != Some(raw_detail.as_str())
                {
                    description.push(raw_detail.clone());
                }
                let state = PermissionViewState {
                    id: 0,
                    labels: Default::default(),
                    shell_layout: Default::default(),
                    focus: Default::default(),
                    options,
                    active_idx: 0,
                    bash_highlights: None,
                    bash_selection_count: 0,
                    bash_deny_selection_count: 0,
                    bash_command_raw: command,
                    mcp_scope: None,
                    title: tool_name,
                    description,
                    preview: Vec::new(),
                    args_expanded: false,
                    desc_scroll: 0,
                    subagent_label: None,
                    options_area_height: 0,
                    options_scroll_offset: 0,
                };
                Self::Permission(Box::new(Permission {
                    window: Default::default(),
                    state,
                    detail,
                    diff,
                    hits: None,
                    reply: Some(reply),
                }))
            }
            InteractionRequest::AskQuestion { prompt, reply } => {
                let question = Question {
                    question: prompt.question,
                    options: prompt
                        .options
                        .into_iter()
                        .map(|option| question_view::QuestionOption {
                            label: option.label,
                            description: option.description.unwrap_or_default(),
                            preview: None,
                            id: None,
                        })
                        .collect(),
                    multi_select: Some(prompt.multi_select),
                    id: None,
                };
                let mut state =
                    QuestionViewState::new(String::new(), vec![question], Default::default());
                if !prompt.allow_freeform {
                    state = state.with_no_freeform();
                }
                Self::Question(Box::new(QuestionDialog {
                    title: prompt.title,
                    state,
                    window: Default::default(),
                    hits: None,
                    area: Rect::default(),
                    editor_area: Rect::default(),
                    editor: PromptWidget::new(),
                    reply: Some(reply),
                }))
            }
            InteractionRequest::ApprovePlan {
                title,
                markdown,
                reply,
            } => {
                let request = PlanApprovalRequest {
                    tool_call_id: String::new(),
                    plan_content: Some(markdown),
                };
                let state = PlanApprovalViewState::new(request, Default::default());
                let pane = PlanApprovalPane::new(&state, &PlanApprovalLabels::default());
                Self::Plan(Box::new(Plan {
                    title,
                    state,
                    pane,
                    reply: Some(reply),
                }))
            }
            InteractionRequest::Select {
                title,
                options,
                reply,
            } => {
                let rows = options
                    .into_iter()
                    .enumerate()
                    .map(|(i, label)| (i.to_string(), label))
                    .collect();
                Self::Picker(Box::new(Picker::new(
                    title,
                    rows,
                    Choice::Harness(Some(reply)),
                )))
            }
        }
    }

    pub fn cancelled(&self) -> bool {
        match self {
            Self::Permission(dialog) => sender_closed(&dialog.reply),
            Self::Question(dialog) => sender_closed(&dialog.reply),
            Self::Plan(dialog) => sender_closed(&dialog.reply),
            Self::Picker(dialog) => {
                matches!(&dialog.choice, Choice::Harness(reply) if sender_closed(reply))
            }
            _ => false,
        }
    }

    pub fn is_interaction(&self) -> bool {
        match self {
            Self::Permission(_) | Self::Plan(_) | Self::Question(_) => true,
            Self::Picker(dialog) => matches!(dialog.choice, Choice::Harness(_)),
            _ => false,
        }
    }

    pub fn blocked_status(&self) -> Option<ProgramStatus> {
        let (kind, message) = match self {
            Self::Permission(dialog) => (BlockedKind::Permission, dialog.state.title.as_str()),
            Self::Plan(dialog) => (BlockedKind::Permission, dialog.title.as_str()),
            Self::Question(dialog) => (
                BlockedKind::Question,
                dialog.state.questions[0].question.as_str(),
            ),
            Self::Picker(dialog) if matches!(dialog.choice, Choice::Harness(_)) => {
                (BlockedKind::Question, dialog.title.as_str())
            }
            _ => return None,
        };
        Some(ProgramStatus::blocked(kind).with_msg(message))
    }

    pub fn retheme(&mut self) {
        if let Self::Plan(plan) = self {
            plan.pane
                .retheme(&plan.state, &PlanApprovalLabels::default());
        }
    }

    pub fn draw(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        theme: &Theme,
        text: &UiText,
        labels: &Labels,
        mouse: Option<(u16, u16)>,
    ) -> Option<(u16, u16)> {
        match self {
            Self::Permission(dialog) => dialog.draw(area, buf, theme, labels, mouse),
            Self::Question(dialog) => dialog.draw(area, buf, theme, labels, mouse),
            Self::Plan(dialog) => dialog.pane.render(
                area,
                buf,
                theme,
                &PlanApprovalLabels {
                    title: &dialog.title,
                    approve: &text.approve_plan,
                    reject: &text.reject_plan,
                    feedback: labels.get("Request changes"),
                    close: labels.get("Close"),
                    empty_plan: labels.get("No plan available"),
                    back: labels.get("Back"),
                    feedback_placeholder: Some(labels.get("What should change?")),
                },
            ),
            Self::Picker(dialog) => dialog.draw(area, buf, theme, labels),
            Self::ModelEffort(dialog) => dialog.draw(area, buf, theme, labels),
            Self::Shortcuts(dialog) => {
                dialog.draw(area, buf, theme);
                None
            }
            Self::Viewer(dialog) => dialog.draw(area, buf, theme, labels),
        }
    }

    pub fn input(&mut self, event: &Event) -> DialogAction {
        match self {
            Self::Permission(dialog) => dialog.input(event),
            Self::Question(dialog) => dialog.input(event),
            Self::Plan(dialog) => dialog.input(event),
            Self::Picker(dialog) => dialog.input(event),
            Self::ModelEffort(dialog) => dialog.input(event),
            Self::Shortcuts(dialog) => dialog.input(event),
            Self::Viewer(dialog) => dialog.input(event),
        }
    }

    pub fn pointer(&self, col: u16, row: u16) -> bool {
        let close = |window: &ModalWindowState| {
            let pos = Position::new(col, row);
            window.close_button_rect.is_some_and(|r| r.contains(pos))
                || window
                    .shortcut_hits
                    .iter()
                    .any(|hit| hit.clickable && hit.rect.contains(pos))
        };
        match self {
            Self::Permission(dialog) => {
                close(&dialog.window)
                    || dialog
                        .hits
                        .as_ref()
                        .and_then(|hits| hits.hit_test(col, row))
                        .is_some()
            }
            Self::Question(dialog) => {
                close(&dialog.window)
                    || dialog
                        .hits
                        .as_ref()
                        .and_then(|hits| hits.hit_test(col, row))
                        .is_some()
            }
            Self::Picker(dialog) => close(&dialog.window) || picker_row_at(&dialog.state, col, row),
            Self::ModelEffort(dialog) => {
                close(&dialog.window)
                    || picker_row_at(&dialog.state, col, row)
                    || dialog.pointer_on_effort(col, row)
            }
            Self::Shortcuts(dialog) => {
                close(&dialog.window) || picker_row_at(&dialog.picker, col, row)
            }
            Self::Plan(dialog) => dialog.pane.pointer_at(col, row),
            Self::Viewer(viewer) => viewer.pointer_at(col, row),
        }
    }
}

fn sender_closed<T>(reply: &Option<Sender<T>>) -> bool {
    reply.as_ref().is_none_or(Sender::is_canceled)
}

fn respond<T>(reply: &mut Option<Sender<T>>, value: T) -> DialogAction {
    if let Some(reply) = reply.take() {
        let _ = reply.send(value);
    }
    DialogAction::Close
}

fn strip_rewind_tree_prefix(label: &str) -> &str {
    let mut rest = label;
    loop {
        if let Some(next) = rest
            .strip_prefix("│  ")
            .or_else(|| rest.strip_prefix("├─ "))
            .or_else(|| rest.strip_prefix("└─ "))
            .or_else(|| rest.strip_prefix("   "))
        {
            rest = next;
        } else {
            return rest;
        }
    }
}

fn picker_row_at(state: &PickerState, col: u16, row: u16) -> bool {
    state.hit_areas.as_ref().is_some_and(|hits| {
        hits.item_rects
            .iter()
            .any(|rect| rect.contains((col, row).into()))
    })
}

fn window_config<'a>(title: &'a str, shortcuts: &'a [Shortcut<'a>]) -> ModalWindowConfig<'a> {
    ModalWindowConfig {
        title,
        subtitle: None,
        tabs: None,
        shortcuts,
        sizing: ModalSizing::medium(),
        fold_info: None,
    }
}

fn window_mouse(window: &mut ModalWindowState, mouse: &MouseEvent) -> ModalWindowOutcome {
    modal_window::handle_modal_mouse(window, mouse.kind, mouse.column, mouse.row)
}

pub(super) struct Permission {
    window: ModalWindowState,
    state: PermissionViewState,
    detail: Option<String>,
    diff: Option<EditToolCallBlock>,
    hits: Option<PermissionRenderResult>,
    reply: Option<Sender<ApprovalDecision>>,
}

impl Permission {
    fn draw(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        theme: &Theme,
        labels: &Labels,
        mouse: Option<(u16, u16)>,
    ) -> Option<(u16, u16)> {
        let previous_hits = self.hits.take();
        let content = modal_window::render_modal_window(
            buf,
            area,
            &mut self.window,
            &window_config(labels.get("Tool approval"), &[]),
            theme,
        )?
        .content;
        let detail = self.detail.as_deref().unwrap_or_default();
        let is_diff = detail
            .lines()
            .any(|line| line.starts_with("*** ") || line.starts_with("@@"));
        self.state.preview = if let Some(diff) = &self.diff {
            diff.render_diff_lines(theme, content.width.saturating_sub(5), &Default::default())
                .into_iter()
                .map(|row| row.line)
                .collect()
        } else if is_diff {
            detail
                .lines()
                .map(|line| {
                    let style = match line.as_bytes().first() {
                        Some(b'+') => ratatui::style::Style::new().fg(theme.accent_success),
                        Some(b'-') => ratatui::style::Style::new().fg(theme.accent_error),
                        _ => theme.primary(),
                    };
                    Line::styled(line.to_owned(), style)
                })
                .collect()
        } else {
            Vec::new()
        };
        let hovered = mouse.and_then(|(x, y)| previous_hits.as_ref()?.hit_test(x, y));
        self.hits = Some(permission_view::render_permission_view(
            buf,
            content,
            &self.state,
            "",
            None,
            hovered,
            theme,
            true,
        ));
        None
    }

    fn input(&mut self, event: &Event) -> DialogAction {
        let action = match event {
            Event::Key(key) => self.state.handle_key(key),
            Event::Mouse(mouse) => {
                // Only the close button dismisses an unanswered approval.
                let close = self
                    .window
                    .close_button_rect
                    .is_some_and(|r| r.contains((mouse.column, mouse.row).into()));
                let outcome = window_mouse(&mut self.window, mouse);
                if close && matches!(outcome, ModalWindowOutcome::CloseRequested) {
                    PermissionInput::Cancel
                } else {
                    self.hits
                        .as_ref()
                        .map_or(PermissionInput::Unhandled, |hits| hits.handle_mouse(mouse))
                }
            }
            _ => PermissionInput::Unhandled,
        };
        match action {
            PermissionInput::Select(i) => {
                let decision = match self.state.options.get(i).map(|option| option.kind) {
                    Some(PermissionOptionKind::AllowOnce) => ApprovalDecision::AllowOnce,
                    Some(PermissionOptionKind::AllowAlways) => ApprovalDecision::AllowSession,
                    _ => ApprovalDecision::Deny,
                };
                respond(&mut self.reply, decision)
            }
            PermissionInput::Cancel => respond(&mut self.reply, ApprovalDecision::Deny),
            PermissionInput::ExpandPreview => {
                self.state.args_expanded = !self.state.args_expanded;
                self.state.desc_scroll = 0;
                DialogAction::Stay
            }
            PermissionInput::ScrollPreview(delta) => {
                self.state.args_expanded = true;
                self.state.desc_scroll = self.state.desc_scroll.saturating_add_signed(delta);
                DialogAction::Stay
            }
            _ => DialogAction::Stay,
        }
    }
}

pub(super) struct QuestionDialog {
    title: String,
    state: QuestionViewState,
    window: ModalWindowState,
    hits: Option<QuestionViewRenderResult>,
    area: Rect,
    editor_area: Rect,
    editor: PromptWidget,
    reply: Option<Sender<Option<QuestionAnswer>>>,
}

impl QuestionDialog {
    fn editing(&self) -> bool {
        self.state.focus != QuestionFocus::Navigation
    }

    fn draw(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        theme: &Theme,
        labels: &Labels,
        mouse: Option<(u16, u16)>,
    ) -> Option<(u16, u16)> {
        let submit = Shortcut {
            label: labels.get(if self.editing() {
                "Enter Save"
            } else {
                "Submit"
            }),
            clickable: true,
            id: 0,
        };
        let footer = [submit];
        let mut config = window_config(&self.title, &footer);
        if self.state.fullscreen {
            config.sizing = ModalSizing::large();
        }
        self.area = Rect::default();
        self.editor_area = Rect::default();
        let previous_hits = self.hits.take();
        let content =
            modal_window::render_modal_window(buf, area, &mut self.window, &config, theme)?.content;
        self.area = content;
        self.editor_area = Rect::default();
        let mut cursor = None;
        if self.editing() && content.height >= 4 {
            let style = PromptStyle::default();
            let height = self.editor.desired_height(
                content.width,
                &style,
                false,
                content.height.saturating_sub(2).min(8),
            );
            self.area.height = self.area.height.saturating_sub(height);
            self.editor_area = Rect {
                y: self.area.bottom(),
                height,
                ..content
            };
            cursor = self
                .editor
                .draw(buf, self.editor_area, None, &style, None, None)
                .cursor_pos;
        }
        let hovered = mouse.and_then(|(x, y)| previous_hits.as_ref()?.hit_test(x, y));
        self.hits = Some(question_view::render_question_view(
            buf,
            self.area,
            &self.state,
            hovered,
            theme,
            !self.editing(),
        ));
        cursor
    }

    fn answer(&mut self) -> DialogAction {
        let mut selected_indices = match &self.state.selections[0] {
            QuestionSelection::Single(index) => index.iter().copied().collect(),
            QuestionSelection::Multi(indices) => indices.iter().copied().collect::<Vec<_>>(),
        };
        selected_indices.sort_unstable();
        let freeform = self.state.per_question_freeform_selected[0]
            .then(|| self.state.per_question_freeform[0].clone())
            .filter(|s| !s.trim().is_empty());
        if selected_indices.is_empty() && freeform.is_none() {
            DialogAction::Stay
        } else {
            respond(
                &mut self.reply,
                Some(QuestionAnswer {
                    selected_indices,
                    freeform,
                }),
            )
        }
    }

    fn save(&mut self) -> DialogAction {
        self.state
            .save_freeform(self.editor.text_without_image_chips());
        self.state.focus = QuestionFocus::Navigation;
        if self.state.questions[0].multi_select != Some(true) {
            return self.answer();
        }
        DialogAction::Stay
    }

    fn input(&mut self, event: &Event) -> DialogAction {
        let action = match event {
            Event::Key(key) if self.editing() => {
                if key.code == KeyCode::Esc {
                    self.state.focus = QuestionFocus::Navigation;
                } else if self.editor.route_enter(key) == EnterOutcome::Submit {
                    return self.save();
                } else {
                    self.editor.handle_key(key);
                }
                QuestionInput::Changed
            }
            Event::Key(key) => {
                let rows = self
                    .hits
                    .as_ref()
                    .map_or(0, |r| r.options_end_y.saturating_sub(r.options_start_y));
                self.state.handle_navigation_key(
                    key,
                    rows,
                    usize::from(
                        self.area
                            .width
                            .saturating_sub(question_view::QUESTION_VIEW_HPAD),
                    ),
                )
            }
            Event::Paste(text) if self.editing() => {
                self.editor.handle_paste(text);
                QuestionInput::Changed
            }
            Event::Mouse(mouse) => {
                match window_mouse(&mut self.window, mouse) {
                    ModalWindowOutcome::CloseRequested => return respond(&mut self.reply, None),
                    ModalWindowOutcome::ShortcutActivated(0) => {
                        return if self.editing() {
                            self.save()
                        } else {
                            self.answer()
                        };
                    }
                    _ => {}
                }
                if self.editing() && self.editor_area.contains((mouse.column, mouse.row).into()) {
                    self.editor.handle_mouse(mouse);
                    QuestionInput::Changed
                } else {
                    self.hits.as_ref().map_or(QuestionInput::Unhandled, |hits| {
                        hits.handle_mouse(&mut self.state, self.area, mouse)
                    })
                }
            }
            _ => QuestionInput::Unhandled,
        };
        match action {
            QuestionInput::Cancel => respond(&mut self.reply, None),
            QuestionInput::Submit => self.answer(),
            QuestionInput::EditFreeform => {
                let previous = self.state.activate_freeform_input();
                self.editor.set_text_discarding_images(&previous);
                if let Event::Key(key) = event
                    && matches!(key.code, KeyCode::Char(ch) if ch != ' ' && ch != 'z')
                {
                    self.editor.handle_key(key);
                }
                DialogAction::Stay
            }
            QuestionInput::ChangeTab(index) => {
                self.state.change_tab(index);
                DialogAction::Stay
            }
            _ => DialogAction::Stay,
        }
    }
}

pub(super) struct Plan {
    title: String,
    state: PlanApprovalViewState,
    pane: PlanApprovalPane,
    reply: Option<Sender<PlanDecision>>,
}

impl Plan {
    fn input(&mut self, event: &Event) -> DialogAction {
        let action = match event {
            Event::Key(key) => self.pane.handle_key(key),
            Event::Mouse(mouse) => self.pane.handle_mouse(mouse),
            Event::Paste(text) => self.pane.handle_paste(text),
            _ => return DialogAction::Stay,
        };
        match action {
            PlanApprovalInput::Approve => respond(&mut self.reply, PlanDecision::Approve),
            PlanApprovalInput::Reject | PlanApprovalInput::Close => {
                respond(&mut self.reply, PlanDecision::Reject { feedback: None })
            }
            PlanApprovalInput::SubmitFeedback(text) => respond(
                &mut self.reply,
                PlanDecision::Reject {
                    feedback: Some(text),
                },
            ),
            _ => DialogAction::Stay,
        }
    }
}

enum Choice {
    Local(PickerTarget),
    Harness(Option<Sender<Option<usize>>>),
}

pub(super) struct Picker {
    title: String,
    rows: Vec<(String, String)>,
    filtered: Vec<usize>,
    choice: Choice,
    state: PickerState,
    window: ModalWindowState,
    scrollbar: Rect,
    dragging: bool,
}

impl Picker {
    fn new(title: String, rows: Vec<(String, String)>, choice: Choice) -> Self {
        Self {
            title,
            rows,
            choice,
            filtered: Vec::new(),
            state: PickerState::input_active(),
            window: Default::default(),
            scrollbar: Rect::default(),
            dragging: false,
        }
    }

    fn with_selected(mut self, idx: usize) -> Self {
        self.state.selected = idx.min(self.rows.len().saturating_sub(1));
        self
    }

    fn draw(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        theme: &Theme,
        labels: &Labels,
    ) -> Option<(u16, u16)> {
        self.state.hit_areas = None;
        self.scrollbar = Rect::default();
        let config = window_config(&self.title, &[]);
        let content =
            modal_window::render_modal_window(buf, area, &mut self.window, &config, theme)?.content;
        if content.is_empty() {
            return None;
        }
        let search = Rect::new(content.x, content.y, content.width, 1);
        let viewport = self
            .state
            .query_viewport(usize::from(search.width.saturating_sub(2)));
        let prompt = if self.state.query().is_empty() {
            labels.get("Filter")
        } else {
            &self.state.query()[viewport.visible_byte_range]
        };
        buf.set_line_safe(
            search.x,
            search.y,
            &Line::styled(format!("⌕ {prompt}"), theme.dim()),
            search.width,
        );
        let list = Rect::new(
            content.x,
            content.y + 1,
            content.width,
            content.height.saturating_sub(1),
        );
        let query = self.state.query().to_lowercase();
        self.filtered = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(i, (_, label))| label.to_lowercase().contains(&query).then_some(i))
            .collect();
        self.state.selected = self
            .state
            .selected
            .min(self.filtered.len().saturating_sub(1));
        let strip_tree_prefix =
            matches!(self.choice, Choice::Local(PickerTarget::Rewind)) && !query.is_empty();
        let display_labels: Vec<String> = self
            .filtered
            .iter()
            .map(|&index| {
                let raw = self.rows[index].1.as_str();
                if strip_tree_prefix {
                    strip_rewind_tree_prefix(raw).to_owned()
                } else {
                    raw.to_owned()
                }
            })
            .collect();
        let entries = display_labels
            .iter()
            .enumerate()
            .map(|(i, label)| {
                PickerEntry::Row(PickerRow {
                    label: label.as_str(),
                    right_label: "",
                    selected: i == self.state.selected,
                    expanded: false,
                    fields: &[],
                    description_lines: &[],
                    summary_lines: &[],
                    dimmed: false,
                    indent: 0,
                    badge: "",
                    badge_color: None,
                    collapsible: false,
                    underline_last_desc: false,
                })
            })
            .collect::<Vec<_>>();
        let hits = picker::render_picker_content(
            buf,
            list,
            theme,
            &mut self.state,
            &entries,
            &[],
            &[],
            Some(theme.bg_base),
            false,
        );
        self.scrollbar = if self.filtered.len() > usize::from(list.height) {
            Rect::new(list.right().saturating_sub(1), list.y, 1, list.height)
        } else {
            Rect::default()
        };
        self.state.hit_areas = Some(PickerHitAreas {
            close_button: self.window.close_button_rect.unwrap_or_default(),
            search_bar: search,
            item_rects: hits.item_rects,
            entry_indices: hits.entry_indices,
            tab_rects: Vec::new(),
            filter_rect: None,
        });
        self.state.search_active.then(|| {
            (
                search
                    .x
                    .saturating_add(2)
                    .saturating_add(viewport.cursor_display_column as u16)
                    .min(search.right().saturating_sub(1)),
                search.y,
            )
        })
    }

    fn input(&mut self, event: &Event) -> DialogAction {
        if let Event::Mouse(mouse) = event {
            let outcome = window_mouse(&mut self.window, mouse);
            if matches!(outcome, ModalWindowOutcome::CloseRequested) {
                return self.close();
            }
            let pos = Position::new(mouse.column, mouse.row);
            if mouse.kind == MouseEventKind::Up(MouseButton::Left) && self.dragging {
                self.dragging = false;
                return DialogAction::Stay;
            }
            if (mouse.kind == MouseEventKind::Down(MouseButton::Left)
                && self.scrollbar.contains(pos))
                || (mouse.kind == MouseEventKind::Drag(MouseButton::Left) && self.dragging)
            {
                self.dragging = true;
                let max = self
                    .filtered
                    .len()
                    .saturating_sub(usize::from(self.scrollbar.height));
                let travel = self.scrollbar.height.saturating_sub(1).max(1);
                let row = mouse.row.saturating_sub(self.scrollbar.y).min(travel);
                self.state.scroll_offset = Some(usize::from(row) * max / usize::from(travel));
                return DialogAction::Stay;
            }
            if !matches!(mouse.kind, MouseEventKind::Moved)
                && self
                    .window
                    .popup_area
                    .is_none_or(|area| !area.contains(pos))
            {
                return DialogAction::Stay;
            }
        }
        let outcome = picker::handle_picker_input(
            event,
            &mut self.state,
            self.filtered.len(),
            &PickerConfig::default(),
        );
        match outcome {
            PickerOutcome::Closed => self.close(),
            PickerOutcome::Selected(i) => {
                let Some(&index) = self.filtered.get(i) else {
                    return DialogAction::Stay;
                };
                match &mut self.choice {
                    Choice::Harness(reply) => respond(reply, Some(index)),
                    Choice::Local(PickerTarget::ReadOnly) => DialogAction::Stay,
                    Choice::Local(target) => {
                        DialogAction::Pick(*target, self.rows[index].0.clone())
                    }
                }
            }
            _ => DialogAction::Stay,
        }
    }

    fn close(&mut self) -> DialogAction {
        match &mut self.choice {
            Choice::Harness(reply) => respond(reply, None),
            Choice::Local(_) => DialogAction::Close,
        }
    }
}

pub(super) struct ModelEffortPicker {
    title: String,
    entries: Vec<ModelEffortEntry>,
    filtered: Vec<usize>,
    state: PickerState,
    window: ModalWindowState,
    scrollbar: Rect,
    dragging: bool,
    effort_name: Option<String>,
    effort_idx: usize,
    track_rect: Rect,
    effort_hits: Vec<(Rect, usize)>,
}

impl ModelEffortPicker {
    fn new(title: String, entries: Vec<ModelEffortEntry>, current_effort: Option<String>) -> Self {
        let initial_selected = entries.iter().position(|e| e.is_current).unwrap_or(0);
        let filtered: Vec<usize> = (0..entries.len()).collect();
        let mut state = PickerState::input_active();
        state.selected = initial_selected;
        let mut picker = Self {
            title,
            entries,
            filtered,
            state,
            window: Default::default(),
            scrollbar: Rect::default(),
            dragging: false,
            effort_name: current_effort,
            effort_idx: 0,
            track_rect: Rect::default(),
            effort_hits: Vec::new(),
        };
        picker.sync_effort_for_selected();
        picker
    }

    fn selected_entry(&self) -> Option<&ModelEffortEntry> {
        let idx = *self.filtered.get(self.state.selected)?;
        self.entries.get(idx)
    }

    fn current_levels(&self) -> &[String] {
        self.selected_entry()
            .map(|e| e.thinking_levels.as_slice())
            .unwrap_or(&[])
    }

    fn sync_effort_for_selected(&mut self) {
        let len = self.current_levels().len();
        if len == 0 {
            self.effort_idx = 0;
            return;
        }
        if let Some(name) = self.effort_name.as_deref()
            && let Some(pos) = self.current_levels().iter().position(|l| l == name)
        {
            self.effort_idx = pos;
            return;
        }
        let clamped = self.effort_idx.min(len.saturating_sub(1));
        self.effort_idx = clamped;
        self.effort_name = self.current_levels().get(clamped).cloned();
    }

    fn step_effort(&mut self, delta: isize) {
        let len = self.current_levels().len();
        if len == 0 {
            return;
        }
        let next = (self.effort_idx as isize + delta).clamp(0, (len - 1) as isize) as usize;
        self.effort_idx = next;
        self.effort_name = self.current_levels().get(next).cloned();
    }

    fn set_effort_index(&mut self, idx: usize) {
        let len = self.current_levels().len();
        if idx < len {
            self.effort_idx = idx;
            self.effort_name = self.current_levels().get(idx).cloned();
        }
    }

    fn pointer_on_effort(&self, col: u16, row: u16) -> bool {
        let pos = Position::new(col, row);
        self.track_rect.contains(pos) || self.effort_hits.iter().any(|(rect, _)| rect.contains(pos))
    }

    fn draw(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        theme: &Theme,
        labels: &Labels,
    ) -> Option<(u16, u16)> {
        self.state.hit_areas = None;
        self.scrollbar = Rect::default();
        self.track_rect = Rect::default();
        self.effort_hits.clear();

        let shortcuts = [
            Shortcut {
                label: "↑↓ Model",
                clickable: false,
                id: 0,
            },
            Shortcut {
                label: "←→ Effort",
                clickable: false,
                id: 0,
            },
            Shortcut {
                label: "Enter Apply",
                clickable: false,
                id: 0,
            },
            Shortcut {
                label: "Esc Cancel",
                clickable: false,
                id: 0,
            },
        ];
        let config = window_config(&self.title, &shortcuts);
        let content =
            modal_window::render_modal_window(buf, area, &mut self.window, &config, theme)?.content;
        if content.is_empty() {
            return None;
        }

        let search = Rect::new(content.x, content.y, content.width, 1);
        let viewport = self
            .state
            .query_viewport(usize::from(search.width.saturating_sub(2)));
        let prompt = if self.state.query().is_empty() {
            labels.get("Filter")
        } else {
            &self.state.query()[viewport.visible_byte_range]
        };
        buf.set_line_safe(
            search.x,
            search.y,
            &Line::styled(format!("⌕ {prompt}"), theme.dim()),
            search.width,
        );

        let query = self.state.query().to_lowercase();
        self.filtered = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                let hay = format!("{} {} {}", e.name, e.id, e.provider).to_lowercase();
                hay.contains(&query).then_some(i)
            })
            .collect();
        self.state.selected = self
            .state
            .selected
            .min(self.filtered.len().saturating_sub(1));
        self.sync_effort_for_selected();

        let has_effort = !self.current_levels().is_empty() && content.height >= 5;
        let effort_rows: u16 = if has_effort { 3 } else { 0 };
        let list_height = content.height.saturating_sub(1 + effort_rows);
        let list = Rect::new(content.x, content.y + 1, content.width, list_height);

        let entries = self
            .filtered
            .iter()
            .enumerate()
            .map(|(i, &index)| {
                let entry = &self.entries[index];
                PickerEntry::Row(PickerRow {
                    label: &entry.name,
                    right_label: &entry.provider,
                    selected: i == self.state.selected,
                    expanded: false,
                    fields: &[],
                    description_lines: &[],
                    summary_lines: &[],
                    dimmed: false,
                    indent: 0,
                    badge: if entry.is_current { "●" } else { "" },
                    badge_color: entry.is_current.then_some(theme.accent_skill),
                    collapsible: false,
                    underline_last_desc: false,
                })
            })
            .collect::<Vec<_>>();

        let hits = picker::render_picker_content(
            buf,
            list,
            theme,
            &mut self.state,
            &entries,
            &[],
            &[],
            Some(theme.bg_base),
            false,
        );
        self.scrollbar = if self.filtered.len() > usize::from(list.height) {
            Rect::new(list.right().saturating_sub(1), list.y, 1, list.height)
        } else {
            Rect::default()
        };
        self.state.hit_areas = Some(PickerHitAreas {
            close_button: self.window.close_button_rect.unwrap_or_default(),
            search_bar: search,
            item_rects: hits.item_rects,
            entry_indices: hits.entry_indices,
            tab_rects: Vec::new(),
            filter_rect: None,
        });

        if has_effort {
            let effort_y = list.bottom();
            // 1px hairline divider
            let divider = "─".repeat(usize::from(content.width));
            buf.set_line_safe(
                content.x,
                effort_y,
                &Line::styled(divider, Style::default().fg(theme.panel_border_fg())),
                content.width,
            );

            let levels = self.current_levels().to_vec();
            let active_idx = self.effort_idx.min(levels.len().saturating_sub(1));
            let active_label = levels
                .get(active_idx)
                .map(String::as_str)
                .unwrap_or_default();

            // Row 1: `  Effort   ━━━━━━●──────   high`
            let bar_y = effort_y + 1;
            let prefix = "  Effort   ";
            let suffix = format!("   {active_label}");
            let used = prefix.chars().count() + suffix.chars().count();
            let track_len = usize::from(content.width).saturating_sub(used).clamp(6, 28);
            let knob = if levels.len() <= 1 {
                0
            } else {
                active_idx * (track_len - 1) / (levels.len() - 1)
            };
            let filled = "━".repeat(knob);
            let remaining = "─".repeat(track_len.saturating_sub(knob + 1));
            let bar_line = Line::from(vec![
                Span::styled(prefix, theme.dim()),
                Span::styled(filled, Style::default().fg(theme.accent_skill)),
                Span::styled(
                    "●",
                    Style::default()
                        .fg(theme.accent_skill)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(remaining, Style::default().fg(theme.panel_border_fg())),
                Span::styled(
                    suffix,
                    Style::default()
                        .fg(theme.text_primary)
                        .add_modifier(Modifier::BOLD),
                ),
            ]);
            buf.set_line_safe(content.x, bar_y, &bar_line, content.width);
            let track_x = content.x.saturating_add(prefix.chars().count() as u16);
            self.track_rect = Rect::new(
                track_x,
                bar_y,
                (track_len as u16).min(content.right().saturating_sub(track_x)),
                1,
            );

            // Row 2: spaced scale ticks (`off  low  medium  high  max`) — clickable
            let scale_y = effort_y + 2;
            let mut spans = vec![Span::raw(" ".repeat(prefix.chars().count()))];
            let mut cursor_x = track_x;
            for (idx, level) in levels.iter().enumerate() {
                if idx > 0 {
                    spans.push(Span::raw("  "));
                    cursor_x = cursor_x.saturating_add(2);
                }
                let width = level.chars().count() as u16;
                let style = if idx == active_idx {
                    Style::default()
                        .fg(theme.accent_skill)
                        .add_modifier(Modifier::BOLD)
                } else {
                    theme.dim()
                };
                spans.push(Span::styled(level.clone(), style));
                if cursor_x < content.right() {
                    self.effort_hits.push((
                        Rect::new(
                            cursor_x,
                            scale_y,
                            width.min(content.right().saturating_sub(cursor_x)),
                            1,
                        ),
                        idx,
                    ));
                }
                cursor_x = cursor_x.saturating_add(width);
            }
            buf.set_line_safe(content.x, scale_y, &Line::from(spans), content.width);
        }

        self.state.search_active.then(|| {
            (
                search
                    .x
                    .saturating_add(2)
                    .saturating_add(viewport.cursor_display_column as u16)
                    .min(search.right().saturating_sub(1)),
                search.y,
            )
        })
    }

    fn input(&mut self, event: &Event) -> DialogAction {
        match event {
            Event::Key(key) if key.code == KeyCode::Left => {
                self.step_effort(-1);
                return DialogAction::Stay;
            }
            Event::Key(key) if key.code == KeyCode::Right => {
                self.step_effort(1);
                return DialogAction::Stay;
            }
            Event::Mouse(mouse) => {
                let outcome = window_mouse(&mut self.window, mouse);
                if matches!(outcome, ModalWindowOutcome::CloseRequested) {
                    return DialogAction::Close;
                }
                let pos = Position::new(mouse.column, mouse.row);
                if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                    if let Some((_, idx)) = self
                        .effort_hits
                        .iter()
                        .find(|(rect, _)| rect.contains(pos))
                        .copied()
                    {
                        self.set_effort_index(idx);
                        return DialogAction::Stay;
                    }
                    if self.track_rect.contains(pos) && !self.current_levels().is_empty() {
                        let len = self.current_levels().len();
                        let rel = pos.x.saturating_sub(self.track_rect.x) as usize;
                        let max_x = usize::from(self.track_rect.width.saturating_sub(1)).max(1);
                        let idx = if len <= 1 {
                            0
                        } else {
                            (rel * (len - 1) + max_x / 2) / max_x
                        };
                        self.set_effort_index(idx.min(len - 1));
                        return DialogAction::Stay;
                    }
                }
                if mouse.kind == MouseEventKind::Up(MouseButton::Left) && self.dragging {
                    self.dragging = false;
                    return DialogAction::Stay;
                }
                if (mouse.kind == MouseEventKind::Down(MouseButton::Left)
                    && self.scrollbar.contains(pos))
                    || (mouse.kind == MouseEventKind::Drag(MouseButton::Left) && self.dragging)
                {
                    self.dragging = true;
                    let max = self
                        .filtered
                        .len()
                        .saturating_sub(usize::from(self.scrollbar.height));
                    let travel = self.scrollbar.height.saturating_sub(1).max(1);
                    let row = mouse.row.saturating_sub(self.scrollbar.y).min(travel);
                    self.state.scroll_offset = Some(usize::from(row) * max / usize::from(travel));
                    return DialogAction::Stay;
                }
                if !matches!(mouse.kind, MouseEventKind::Moved)
                    && self
                        .window
                        .popup_area
                        .is_none_or(|area| !area.contains(pos))
                {
                    return DialogAction::Stay;
                }
            }
            _ => {}
        }

        let prev_selected = self.state.selected;
        let outcome = picker::handle_picker_input(
            event,
            &mut self.state,
            self.filtered.len(),
            &PickerConfig::default(),
        );
        if self.state.selected != prev_selected {
            self.sync_effort_for_selected();
        }
        match outcome {
            PickerOutcome::Closed => DialogAction::Close,
            PickerOutcome::Selected(i) => {
                let Some(&index) = self.filtered.get(i) else {
                    return DialogAction::Stay;
                };
                self.state.selected = i;
                self.sync_effort_for_selected();
                let model = self.entries[index].id.clone();
                let effort = (!self.entries[index].thinking_levels.is_empty())
                    .then(|| self.effort_name.clone())
                    .flatten();
                DialogAction::PickModelEffort { model, effort }
            }
            _ => DialogAction::Stay,
        }
    }
}

pub(super) struct Shortcuts {
    entries: Vec<help::ShortcutsHelpEntry>,
    picker: PickerState,
    window: ModalWindowState,
    collapsed: HashSet<usize>,
    expanded: HashSet<help::ExpandKey>,
    mode: help::ShortcutsHelpMode,
    filter: bool,
}

impl Shortcuts {
    fn new(labels: &Labels) -> Self {
        use KeyCode::*;
        use KeyModifiers as M;
        #[allow(unused_mut)]
        let mut shortcuts = vec![
            (Enter, M::NONE, "Send or steer"),
            (Enter, M::SHIFT, "New line (or Alt+Enter / Ctrl+J)"),
            (Enter, M::CONTROL, "Queue follow-up"),
            (Up, M::NONE, "Previous input / focus queue"),
            (Down, M::NONE, "Next input"),
            (Char('s'), M::CONTROL, "Stash / restore draft (or Alt+S)"),
            (Char('g'), M::CONTROL, "Edit prompt in $EDITOR"),
            (Char('m'), M::ALT, "Toggle multiline mode"),
            (Char('!'), M::NONE, "Local shell mode (on empty prompt)"),
            (Char('z'), M::CONTROL, "Undo (restores cleared draft)"),
            (Esc, M::NONE, "Stop / double-Esc rewind"),
            (Char('c'), M::CONTROL, "Stop / clear / twice to exit"),
            (Char('d'), M::CONTROL, "Exit on empty prompt"),
            (Char('q'), M::CONTROL, "Exit"),
            (BackTab, M::NONE, "Cycle permission mode"),
            (Char('t'), M::ALT, "Cycle effort level"),
            (Char('p'), M::ALT, "Switch model"),
            (Char('o'), M::CONTROL, "Expand or collapse blocks"),
            (Tab, M::NONE, "Complete"),
            (PageUp, M::NONE, "Scroll up"),
            (PageDown, M::NONE, "Scroll down"),
        ];
        #[cfg(feature = "history-search")]
        shortcuts.push((Char('r'), M::CONTROL, "Input history"));
        let entries = shortcuts
            .into_iter()
            .map(|(key, mods, text)| help::ShortcutsHelpEntry::Hint {
                item: HintItem::new(KeyShortcut::new(key, mods), labels.get(text).to_owned()),
                dimmed: false,
                action_id: None,
                long_help: None,
            })
            .collect::<Vec<_>>();
        Self {
            picker: help::build_initial_picker_state(&entries),
            entries,
            window: Default::default(),
            collapsed: HashSet::new(),
            expanded: HashSet::new(),
            mode: help::ShortcutsHelpMode::Browse,
            filter: false,
        }
    }

    fn draw(&mut self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        help::render_modal(
            buf,
            area,
            &self.entries,
            &mut self.picker,
            &mut self.window,
            self.filter,
            &self.collapsed,
            &self.expanded,
            &self.mode,
            theme,
            area.width < 65,
        );
    }

    fn input(&mut self, event: &Event) -> DialogAction {
        use help::{ModalKeyOutcome as Key, ShortcutsHelpOutcome as Mouse};
        let outcome = match event {
            Event::Key(key) => help::handle_modal_key(
                key,
                &self.entries,
                &mut self.picker,
                &mut self.window,
                self.filter,
                &self.collapsed,
                &self.expanded,
                &mut self.mode,
                false,
            ),
            Event::Mouse(mouse) => {
                match window_mouse(&mut self.window, mouse) {
                    ModalWindowOutcome::CloseRequested => return DialogAction::Close,
                    ModalWindowOutcome::ShortcutActivated(1) => self.filter = !self.filter,
                    _ => {}
                }
                match help::handle_mouse(
                    mouse,
                    &self.entries,
                    &mut self.picker,
                    self.filter,
                    &self.collapsed,
                    &mut self.mode,
                ) {
                    Mouse::Close => Key::Close,
                    Mouse::ToggleFilter => Key::ToggleFilter,
                    Mouse::ToggleSection(i) => Key::ToggleSection(i),
                    Mouse::ToggleExpand(id) => Key::ToggleExpand(id),
                    _ => Key::Unchanged,
                }
            }
            Event::Paste(text) => {
                help::handle_paste(text, &mut self.picker, &self.mode);
                Key::Changed
            }
            _ => Key::Unchanged,
        };
        match outcome {
            Key::Close => return DialogAction::Close,
            Key::ToggleFilter => self.filter = !self.filter,
            Key::ToggleSection(i) => help::toggle_membership(&mut self.collapsed, i),
            Key::ToggleExpand(id) => help::toggle_membership(&mut self.expanded, id),
            _ => {}
        }
        DialogAction::Stay
    }
}
