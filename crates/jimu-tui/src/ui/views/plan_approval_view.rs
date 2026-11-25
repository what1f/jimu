// Adapted from the Grok Build pager (Apache-2.0). Copyright 2023-2026 SpaceXAI.

use super::{
    block_viewer::{BlockViewerPane, MarkdownContent, ViewerContent, ViewerEntry},
    modal_window::{
        ModalSizing, ModalWindowConfig, ModalWindowOutcome, ModalWindowState, Shortcut,
        handle_modal_mouse, render_modal_window,
    },
    support::markdown_syntax,
};
use crate::engine::{
    markdown::StreamingMarkdownRenderer,
    theme::{Theme, md_style},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::{buffer::Buffer, layout::Rect, text::Line};

use crate::ui::views::prompt_widget::{EnterOutcome, PromptStyle, PromptWidget, StashedPrompt};

/// Placeholder body for the plan-approval preview when `exit_plan_mode` parks with no plan content.
/// No content means a missing/empty `plan.md`, or a whitespace-only body. Must be non-empty after
/// trim so `LineViewerState::open_markdown_content` accepts it; empty bodies are rejected there.
pub const EMPTY_PLAN_PLACEHOLDER: &str = "\
# No plan written yet

The agent exited plan mode without writing a plan.

- **Approve**: leave plan mode and start implementing
- **Request changes**: send the agent back to planning
- **Quit**: abandon and turn plan mode off
";

/// Status-line label while plan approval is parked.
///
/// Empty plans use an active decision prompt instead of "Waiting…", so the UI doesn't look stuck when there is no preview body to open.
pub fn plan_approval_status_label(has_plan: bool) -> &'static str {
    if has_plan {
        "Waiting on plan approval"
    } else {
        "No plan written: approve or request changes"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanApprovalFocus {
    Preview,
    Prompt,
    Commenting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanReviewSource {
    Inline,
    FileBacked,
}

/// A revision request is not an outcome: the review reopens and plan mode stays on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanReviewOutcome {
    Approved,
    Abandoned,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewOrigin {
    InTurn,
    AfterTurn,
}

#[derive(Debug, Clone)]
pub struct PlanComment {
    pub id: u64,
    pub line_range: std::ops::Range<usize>,
    pub text: String,
}

pub struct PlanApprovalViewState {
    pub tool_call_id: String,
    pub has_plan: bool,
    pub plan_content: Option<String>,
    pub source: PlanReviewSource,
    pub stashed_prompt: StashedPrompt,
    pub origin: ReviewOrigin,

    pub focus: PlanApprovalFocus,
    pub comments: Vec<PlanComment>,
    pub next_comment_id: u64,
    pub editing_comment_id: Option<u64>,
    pub commenting_range: Option<std::ops::Range<usize>>,

    pub stashed_feedback_prompt: Option<StashedPrompt>,
}

impl PlanApprovalViewState {
    pub fn new(request: PlanApprovalRequest, stashed_prompt: StashedPrompt) -> Self {
        Self::with_source(request, PlanReviewSource::Inline, stashed_prompt)
    }

    pub fn with_source(
        request: PlanApprovalRequest,
        source: PlanReviewSource,
        stashed_prompt: StashedPrompt,
    ) -> Self {
        let plan_content = request.plan_content.filter(|s| !s.trim().is_empty());
        let has_plan = plan_content.is_some();
        Self {
            tool_call_id: request.tool_call_id,
            has_plan,
            plan_content,
            source,
            stashed_prompt,
            origin: ReviewOrigin::InTurn,
            focus: PlanApprovalFocus::Preview,
            comments: Vec::new(),
            next_comment_id: 0,
            editing_comment_id: None,
            commenting_range: None,
            stashed_feedback_prompt: None,
        }
    }

    /// CreatePlan already ended; no ext method to answer.
    pub fn after_turn(
        tool_call_id: String,
        plan_content: String,
        stashed_prompt: StashedPrompt,
    ) -> Self {
        let plan_content = (!plan_content.trim().is_empty()).then_some(plan_content);
        let has_plan = plan_content.is_some();
        PlanApprovalViewState {
            tool_call_id,
            has_plan,
            plan_content,
            source: PlanReviewSource::Inline,
            stashed_prompt,
            origin: ReviewOrigin::AfterTurn,
            focus: PlanApprovalFocus::Preview,
            comments: Vec::new(),
            next_comment_id: 0,
            editing_comment_id: None,
            commenting_range: None,
            stashed_feedback_prompt: None,
        }
    }

    pub fn format_feedback(&self, freeform: Option<&str>) -> String {
        let mut parts: Vec<String> = self
            .comments
            .iter()
            .map(|comment| match self.source {
                PlanReviewSource::Inline => {
                    let label = if comment.line_range.len() == 1 {
                        format!("Proposed plan line {}:", comment.line_range.start)
                    } else {
                        format!(
                            "Proposed plan lines {}-{}:",
                            comment.line_range.start,
                            comment.line_range.end - 1
                        )
                    };
                    let snippets =
                        inline_plan_snippets(self.plan_content.as_deref(), &comment.line_range);
                    format!("{label}\n{snippets}\n\nComment:\n{}", comment.text)
                }
                PlanReviewSource::FileBacked => format_file_backed_plan_comment(comment),
            })
            .collect();

        if let Some(text) = freeform
            && !text.trim().is_empty()
        {
            let text = match (self.source, self.comments.is_empty()) {
                (PlanReviewSource::Inline, false) => format!("Additional feedback:\n{text}"),
                _ => text.to_owned(),
            };
            parts.push(text);
        }

        parts.join("\n\n")
    }
}

impl PlanApprovalViewState {
    pub fn is_after_turn(&self) -> bool {
        self.origin == ReviewOrigin::AfterTurn
    }
    pub fn is_in_turn(&self) -> bool {
        self.origin == ReviewOrigin::InTurn
    }
}

pub struct PlanApprovalRequest {
    pub tool_call_id: String,
    pub plan_content: Option<String>,
}

fn format_file_backed_plan_comment(comment: &PlanComment) -> String {
    let range = if comment.line_range.len() == 1 {
        format!("@plan.md:{}", comment.line_range.start)
    } else {
        format!(
            "@plan.md:{}-{}",
            comment.line_range.start,
            comment.line_range.end - 1
        )
    };
    format!("{range}\n{}", comment.text)
}

pub fn inline_plan_snippets(plan_content: Option<&str>, range: &std::ops::Range<usize>) -> String {
    let Some(plan_content) = plan_content else {
        return "> [plan content unavailable]".to_owned();
    };
    let lines: Vec<&str> = plan_content.lines().collect();
    if range.start == 0 || range.start >= range.end || range.start > lines.len() {
        return "> [selected lines unavailable]".to_owned();
    }

    let Some(start) = range.start.checked_sub(1) else {
        return "> [selected lines unavailable]".to_owned();
    };
    let end = range.end.saturating_sub(1).min(lines.len());
    if end < range.start {
        return "> [selected lines unavailable]".to_owned();
    }

    let Some(snippet) = lines.get(start..end) else {
        return "> [selected lines unavailable]".to_owned();
    };
    snippet
        .iter()
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn format_plan_comments(comments: &[PlanComment], plan_content: Option<&str>) -> String {
    comments
        .iter()
        .map(|comment| {
            let label = if comment.line_range.len() == 1 {
                format!("Proposed plan line {}:", comment.line_range.start)
            } else {
                format!(
                    "Proposed plan lines {}-{}:",
                    comment.line_range.start,
                    comment.line_range.end - 1
                )
            };
            let snippets = inline_plan_snippets(plan_content, &comment.line_range);
            format!("{label}\n{snippets}\n\nComment:\n{}", comment.text)
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[derive(Clone, Copy)]
pub struct PlanApprovalLabels<'a> {
    pub title: &'a str,
    pub approve: &'a str,
    pub reject: &'a str,
    pub feedback: &'a str,
    pub close: &'a str,
    pub empty_plan: &'a str,
    pub back: &'a str,
    pub feedback_placeholder: Option<&'a str>,
}

impl Default for PlanApprovalLabels<'_> {
    fn default() -> Self {
        Self {
            title: "Review plan",
            approve: "Approve",
            reject: "Reject",
            feedback: "Request changes",
            close: "Close",
            empty_plan: EMPTY_PLAN_PLACEHOLDER,
            back: "Back",
            feedback_placeholder: Some("What should change?"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanApprovalInput {
    Changed,
    Approve,
    Reject,
    StartFeedback,
    SubmitFeedback(String),
    Close,
    Unhandled,
}

/// The plan's Markdown uses the same fullscreen list renderer, search and selection as other detail views.
pub struct PlanApprovalPane {
    viewer: BlockViewerPane,
    entry: ViewerEntry,
    chrome: ModalWindowState,
    feedback_prompt: PromptWidget,
    feedback_area: Rect,
    focus: PlanApprovalFocus,
}

const APPROVE: usize = 0;
const REJECT: usize = 1;
const FEEDBACK: usize = 2;
const CLOSE: usize = 3;

impl PlanApprovalPane {
    pub fn new(state: &PlanApprovalViewState, labels: &PlanApprovalLabels<'_>) -> Self {
        let entry = Self::render_entry(state, labels);
        let viewer = BlockViewerPane::for_markdown(0, &entry)
            .expect("plan preview always contains Markdown");
        Self {
            viewer,
            entry,
            chrome: ModalWindowState::new(),
            feedback_prompt: PromptWidget::new(),
            feedback_area: Rect::default(),
            focus: state.focus,
        }
    }

    fn render_entry(state: &PlanApprovalViewState, labels: &PlanApprovalLabels<'_>) -> ViewerEntry {
        let text = state.plan_content.as_deref().unwrap_or(labels.empty_plan);
        let mut renderer = StreamingMarkdownRenderer::new(md_style::style(), true);
        renderer.push(text);
        renderer.finish(markdown_syntax());
        let lines = renderer
            .view()
            .lines
            .iter()
            .map(crate::engine::render::line_utils::line_to_static)
            .collect();
        ViewerEntry {
            block: ViewerContent::Markdown(MarkdownContent {
                lines,
                source_lines: Vec::new(),
                generation: 0,
            }),
            is_running: false,
            copy_text: Some(text.to_owned()),
            copy_meta: None,
        }
    }

    pub(crate) fn retheme(
        &mut self,
        state: &PlanApprovalViewState,
        labels: &PlanApprovalLabels<'_>,
    ) {
        self.entry = Self::render_entry(state, labels);
        self.viewer.rebuild_items(&self.entry);
    }

    pub(crate) fn pointer_at(&self, col: u16, row: u16) -> bool {
        let pos = (col, row).into();
        self.chrome
            .close_button_rect
            .is_some_and(|rect| rect.contains(pos))
            || self
                .chrome
                .shortcut_hits
                .iter()
                .any(|hit| hit.clickable && hit.rect.contains(pos))
    }

    pub(crate) fn handle_paste(&mut self, text: &str) -> PlanApprovalInput {
        if self.focus == PlanApprovalFocus::Prompt {
            self.feedback_prompt.handle_paste(text);
        } else if !self.viewer.handle_paste(text) {
            return PlanApprovalInput::Unhandled;
        }
        PlanApprovalInput::Changed
    }

    pub fn render(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        theme: &Theme,
        labels: &PlanApprovalLabels<'_>,
    ) -> Option<(u16, u16)> {
        let writing_feedback = self.focus == PlanApprovalFocus::Prompt;
        let labels_text = [
            if writing_feedback {
                format!("Enter {}", labels.feedback)
            } else {
                format!("a {}", labels.approve)
            },
            format!("q {}", labels.reject),
            format!(
                "Tab {}",
                if writing_feedback {
                    labels.back
                } else {
                    labels.feedback
                }
            ),
            format!(
                "Esc {}",
                if writing_feedback {
                    labels.back
                } else {
                    labels.close
                }
            ),
        ];
        let shortcuts = std::array::from_fn::<_, 4, _>(|id| Shortcut {
            label: &labels_text[id],
            clickable: true,
            id,
        });
        let config = ModalWindowConfig {
            title: labels.title,
            subtitle: None,
            tabs: None,
            shortcuts: &shortcuts,
            sizing: ModalSizing::large(),
            fold_info: None,
        };
        let areas = render_modal_window(buf, area, &mut self.chrome, &config, theme)?;
        self.feedback_area = Rect::default();
        let mut body = areas.content;
        let mut cursor = None;
        if writing_feedback && body.height >= 3 {
            self.feedback_prompt
                .set_placeholder(labels.feedback_placeholder.unwrap_or_default().to_owned());
            let style = PromptStyle {
                placeholder_when_focused: true,
                ..PromptStyle::default()
            };
            let height =
                self.feedback_prompt
                    .desired_height(body.width, &style, false, body.height.min(8));
            body.height = body.height.saturating_sub(height);
            self.feedback_area = Rect {
                y: body.bottom(),
                height,
                ..body
            };
            cursor = self
                .feedback_prompt
                .draw(buf, self.feedback_area, None, &style, None, None)
                .cursor_pos;
        }
        self.viewer.render_content(
            body,
            buf,
            &self.entry,
            !writing_feedback,
            &[] as &[Line<'static>],
        );
        cursor.or_else(|| {
            (!writing_feedback)
                .then(|| self.viewer.list_state.cursor_position())
                .flatten()
        })
    }

    fn toggle_feedback(&mut self) -> PlanApprovalInput {
        if self.focus == PlanApprovalFocus::Prompt {
            self.focus = PlanApprovalFocus::Preview;
            PlanApprovalInput::Changed
        } else {
            self.focus = PlanApprovalFocus::Prompt;
            PlanApprovalInput::StartFeedback
        }
    }

    fn feedback_outcome(&self) -> PlanApprovalInput {
        let text = self.feedback_prompt.text_without_image_chips();
        if text.trim().is_empty() {
            PlanApprovalInput::Changed
        } else {
            PlanApprovalInput::SubmitFeedback(text)
        }
    }

    pub fn handle_key(&mut self, key: &KeyEvent) -> PlanApprovalInput {
        if self.focus == PlanApprovalFocus::Prompt {
            if key.modifiers.is_empty() && matches!(key.code, KeyCode::Esc | KeyCode::Tab) {
                return self.toggle_feedback();
            }
            return match self.feedback_prompt.route_enter(key) {
                EnterOutcome::Submit => self.feedback_outcome(),
                EnterOutcome::NewlineInserted => PlanApprovalInput::Changed,
                EnterOutcome::PassThrough => {
                    self.feedback_prompt.handle_key(key);
                    PlanApprovalInput::Changed
                }
            };
        }
        if self.viewer.list_state.input_mode().is_none() && key.modifiers == KeyModifiers::NONE {
            match key.code {
                KeyCode::Enter | KeyCode::Char('a') => return PlanApprovalInput::Approve,
                KeyCode::Char('q' | 'r') => return PlanApprovalInput::Reject,
                KeyCode::Tab => return self.toggle_feedback(),
                KeyCode::Esc => return PlanApprovalInput::Close,
                _ => {}
            }
        }
        if self.viewer.handle_key(key) {
            PlanApprovalInput::Changed
        } else {
            PlanApprovalInput::Unhandled
        }
    }

    pub fn handle_mouse(&mut self, mouse: &MouseEvent) -> PlanApprovalInput {
        match handle_modal_mouse(&mut self.chrome, mouse.kind, mouse.column, mouse.row) {
            ModalWindowOutcome::ShortcutActivated(APPROVE)
                if self.focus == PlanApprovalFocus::Prompt =>
            {
                self.feedback_outcome()
            }
            ModalWindowOutcome::ShortcutActivated(APPROVE) => PlanApprovalInput::Approve,
            ModalWindowOutcome::ShortcutActivated(REJECT) => PlanApprovalInput::Reject,
            ModalWindowOutcome::ShortcutActivated(FEEDBACK) => self.toggle_feedback(),
            ModalWindowOutcome::ShortcutActivated(CLOSE)
                if self.focus == PlanApprovalFocus::Prompt =>
            {
                self.toggle_feedback()
            }
            ModalWindowOutcome::ShortcutActivated(CLOSE) | ModalWindowOutcome::CloseRequested => {
                PlanApprovalInput::Close
            }
            ModalWindowOutcome::Unhandled => {
                use crossterm::event::MouseEventKind::{ScrollDown, ScrollUp};
                if self
                    .feedback_area
                    .contains((mouse.column, mouse.row).into())
                {
                    self.feedback_prompt.handle_mouse(mouse);
                    return PlanApprovalInput::Changed;
                }
                if self
                    .chrome
                    .popup_area
                    .is_some_and(|area| area.contains((mouse.column, mouse.row).into()))
                {
                    if matches!(mouse.kind, ScrollUp | ScrollDown) {
                        self.viewer
                            .handle_scroll(if mouse.kind == ScrollUp { -3 } else { 3 });
                        return PlanApprovalInput::Changed;
                    }
                    if self
                        .viewer
                        .handle_mouse(mouse.kind, mouse.column, mouse.row)
                    {
                        return PlanApprovalInput::Changed;
                    }
                }
                PlanApprovalInput::Unhandled
            }
            _ => PlanApprovalInput::Changed,
        }
    }

    pub fn submit_feedback(state: &PlanApprovalViewState, text: Option<&str>) -> PlanApprovalInput {
        PlanApprovalInput::SubmitFeedback(state.format_feedback(text))
    }
}
