// Adapted from the Grok Build pager (Apache-2.0). Copyright 2023-2026 SpaceXAI.

use super::support::syntax;
use super::support::tool_label::titleize_segment;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use unicode_width::UnicodeWidthStr;

use crate::engine::theme::Theme;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PermissionFocus {
    #[default]
    Options,
    FollowupInput,
    PatternEdit,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PatternEditState {
    pub buffer: String,
    pub cursor: usize,
    dirty: bool,
    pub preview: Option<PatternPreview>,
}

impl PatternEditState {
    pub fn new(initial: impl Into<String>) -> Self {
        let buffer = initial.into();
        let cursor = buffer.len();
        Self {
            buffer,
            cursor,
            dirty: false,
            preview: None,
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn trimmed(&self) -> Option<&str> {
        let t = self.buffer.trim();
        (!t.is_empty()).then_some(t)
    }

    pub fn insert_char(&mut self, ch: char) {
        self.buffer.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        self.dirty = true;
        self.preview = None;
    }

    pub fn backspace(&mut self) {
        if let Some(ch) = self
            .buffer
            .get(..self.cursor)
            .and_then(|s| s.chars().next_back())
        {
            self.cursor -= ch.len_utf8();
            self.buffer.remove(self.cursor);
            self.dirty = true;
            self.preview = None;
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.buffer.len() {
            self.buffer.remove(self.cursor);
            self.dirty = true;
            self.preview = None;
        }
    }

    pub fn move_left(&mut self) {
        if let Some(ch) = self
            .buffer
            .get(..self.cursor)
            .and_then(|s| s.chars().next_back())
        {
            self.cursor -= ch.len_utf8();
        }
    }

    pub fn move_right(&mut self) {
        if let Some(ch) = self
            .buffer
            .get(self.cursor..)
            .and_then(|s| s.chars().next())
        {
            self.cursor += ch.len_utf8();
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.buffer.len();
    }

    pub fn clear(&mut self) {
        if !self.buffer.is_empty() {
            self.dirty = true;
            self.preview = None;
        }
        self.buffer.clear();
        self.cursor = 0;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpScope {
    Tool,
    Server,
}

#[derive(Debug, Clone)]
pub struct McpScopeState {
    pub tool_name: String,
    pub all_tools_label: String,
    pub server_prefix: Option<String>,
    pub selected: McpScope,
}

impl McpScopeState {
    pub fn action(&self) -> &str {
        self.server_prefix
            .as_deref()
            .and_then(|prefix| self.tool_name.strip_prefix(prefix))
            .map_or(self.tool_name.as_str(), |action| {
                action.trim_start_matches('_')
            })
    }

    pub fn display_name(&self) -> String {
        match self.server_prefix.as_deref() {
            Some(server) => format!(
                "({}) {}",
                titleize_segment(server),
                titleize_segment(self.action())
            ),
            None => titleize_segment(&self.tool_name),
        }
    }
}

#[derive(Default)]
pub struct PermissionViewState {
    pub labels: PermissionLabels,
    pub shell_layout: ShellLayout,
    pub id: usize,

    pub focus: PermissionFocus,

    pub options: Vec<PermissionOption>,

    pub active_idx: usize,

    pub bash_highlights: Option<BashCommandHighlights>,

    pub bash_selection_count: usize,

    pub bash_deny_selection_count: usize,

    pub bash_command_raw: Option<String>,

    pub mcp_scope: Option<McpScopeState>,

    pub title: String,

    pub description: Vec<String>,
    /// Optional themed preview, for example a colored diff supplied by the host.
    pub preview: Vec<Line<'static>>,

    pub args_expanded: bool,

    pub desc_scroll: u16,

    pub subagent_label: Option<String>,

    pub options_area_height: usize,

    pub options_scroll_offset: usize,
}

pub const ALLOW_ALWAYS_COMMAND_OPTION_ID: &str = "allow-always-command";
pub const REJECT_ALWAYS_COMMAND_OPTION_ID: &str = "reject-always-command";
pub const ALLOW_ALWAYS_MCP_OPTION_ID: &str = "allow-always-mcp";

impl PermissionViewState {
    pub fn has_adjustable_scope(&self) -> bool {
        let has_row = |id: &str| self.options.iter().any(|o| o.id.as_str() == id);
        let bash_adjustable = self.bash_highlights.as_ref().is_some_and(|h| {
            let len = h.highlighted_words.len();
            let allow_adjustable = has_row(ALLOW_ALWAYS_COMMAND_OPTION_ID)
                && (1..=len)
                    .filter(|&n| h.persisting_prefixes.contains(&n))
                    .nth(1)
                    .is_some();
            let deny_adjustable = has_row(REJECT_ALWAYS_COMMAND_OPTION_ID) && len > 1;
            allow_adjustable || deny_adjustable
        });
        bash_adjustable
            || self
                .mcp_scope
                .as_ref()
                .is_some_and(|s| s.server_prefix.is_some())
    }

    pub fn has_editable_bash_pattern(&self) -> bool {
        self.bash_highlights.is_some() && self.allow_always_command_idx().is_some()
    }

    pub fn is_scoped_option(&self, option: &PermissionOption) -> bool {
        let id = option.id.as_str();
        if self.mcp_scope.is_some() {
            id == ALLOW_ALWAYS_MCP_OPTION_ID
        } else {
            matches!(
                id,
                ALLOW_ALWAYS_COMMAND_OPTION_ID | REJECT_ALWAYS_COMMAND_OPTION_ID
            )
        }
    }

    pub fn scoped_row_jump_idx(&self) -> Option<usize> {
        if let Some(idx) = self.scoped_allow_row_idx() {
            return Some(idx);
        }
        if self.mcp_scope.is_some() {
            return None;
        }
        self.options
            .iter()
            .position(|o| o.id.as_str() == REJECT_ALWAYS_COMMAND_OPTION_ID)
    }

    pub fn scoped_allow_row_idx(&self) -> Option<usize> {
        let target = if self.mcp_scope.is_some() {
            ALLOW_ALWAYS_MCP_OPTION_ID
        } else {
            ALLOW_ALWAYS_COMMAND_OPTION_ID
        };
        self.options.iter().position(|o| o.id.as_str() == target)
    }

    pub fn allow_always_command_idx(&self) -> Option<usize> {
        self.options
            .iter()
            .position(|o| o.id.as_str() == ALLOW_ALWAYS_COMMAND_OPTION_ID)
    }

    pub fn step_persisting_allow_scope(&self, right: bool) -> usize {
        let current = self.bash_selection_count;
        let Some(h) = self.bash_highlights.as_ref() else {
            return current;
        };
        if right {
            (current + 1..=h.highlighted_words.len()).find(|&n| h.persisting_prefixes.contains(&n))
        } else {
            (1..current)
                .rev()
                .find(|&n| h.persisting_prefixes.contains(&n))
        }
        .unwrap_or(current)
    }

    pub fn has_collapsible_bash(&self, content_w: usize) -> bool {
        self.bash_command_raw.as_deref().is_some_and(|raw| {
            count_raw_bash_rows(
                raw,
                content_w,
                PERMISSION_COLLAPSED_ROWS + 1,
                &self.shell_layout,
            ) > PERMISSION_COLLAPSED_ROWS
        })
    }

    pub fn has_collapsible_display(&self, content_w: usize) -> bool {
        let (_, args_overflow) = mcp_args_visible_rows(self, content_w);
        args_overflow
            || self.preview.len() > PERMISSION_COLLAPSED_ROWS
            || self.has_collapsible_bash(content_w)
    }
}

fn shortcut_char(index: usize) -> char {
    if index < 9 {
        char::from(b'1' + index as u8)
    } else {
        ' '
    }
}

const SHORTCUT_LABELS: [&str; 10] = ["  ", "1 ", "2 ", "3 ", "4 ", "5 ", "6 ", "7 ", "8 ", "9 "];

fn shortcut_label(index: usize) -> &'static str {
    SHORTCUT_LABELS
        .get(index + 1)
        .copied()
        .unwrap_or(SHORTCUT_LABELS[0])
}

pub fn permission_chrome_height_pub(
    state: &PermissionViewState,
    content_w: usize,
    area_h: u16,
) -> u16 {
    let uncapped = permission_chrome_height(state, content_w);
    let options_and_pad = state.options.len() as u16 + 1;
    let max_chrome = area_h.saturating_sub(options_and_pad);
    uncapped.min(max_chrome)
}

fn permission_chrome_height(state: &PermissionViewState, content_w: usize) -> u16 {
    let (bash_rows, bash_indicator) = bash_visible_rows(state, content_w);
    let bash_line_count = bash_rows
        .saturating_add(bash_indicator as usize)
        .min(u16::MAX as usize) as u16;
    let mut h: u16 = 1;
    if state.subagent_label.is_some() {
        h += 1;
    }
    h += 1;
    h = h.saturating_add(bash_line_count);
    let (args_rows, indicator) = mcp_args_visible_rows(state, content_w);
    let args_rows = args_rows
        .saturating_add(indicator as usize)
        .min(u16::MAX as usize) as u16;
    h = h.saturating_add(args_rows);
    if state.focus == PermissionFocus::PatternEdit {
        h = h.saturating_add(2);
    } else if state.has_adjustable_scope() || state.has_editable_bash_pattern() {
        h = h.saturating_add(1);
    }
    h.saturating_add(1)
}

pub fn permission_view_height(state: &PermissionViewState, screen_h: u16, content_w: usize) -> u16 {
    let chrome_h = permission_chrome_height(state, content_w);
    let options_h = state.options.len() as u16;
    let vpad_bottom: u16 = 1;
    let total = chrome_h
        .saturating_add(options_h)
        .saturating_add(vpad_bottom);

    if state.args_expanded {
        return total.min(screen_h);
    }
    let cap = (screen_h as u32 / 2)
        .max(10)
        .min(screen_h as u32 * 80 / 100) as u16;
    total.min(cap)
}

pub const PERMISSION_COLLAPSED_ROWS: usize = 5;

fn mcp_args_visible_rows(state: &PermissionViewState, content_w: usize) -> (usize, bool) {
    let total: usize = state
        .description
        .iter()
        .map(|raw| char_wrap_row_count(raw, content_w))
        .sum::<usize>()
        .saturating_add(state.preview.len());
    if !state.args_expanded && total > PERMISSION_COLLAPSED_ROWS {
        (PERMISSION_COLLAPSED_ROWS - 1, true)
    } else {
        (total, false)
    }
}

fn bash_visible_rows(state: &PermissionViewState, content_w: usize) -> (usize, bool) {
    if state.bash_highlights.is_some() || state.bash_command_raw.is_some() {
        let Some(raw) = state.bash_command_raw.as_deref() else {
            return (0, false);
        };
        if state.args_expanded {
            return (
                count_raw_bash_rows(raw, content_w, usize::MAX, &state.shell_layout),
                false,
            );
        }
        let capped = count_raw_bash_rows(
            raw,
            content_w,
            PERMISSION_COLLAPSED_ROWS + 1,
            &state.shell_layout,
        );
        if capped > PERMISSION_COLLAPSED_ROWS {
            (PERMISSION_COLLAPSED_ROWS - 1, true)
        } else {
            (capped, false)
        }
    } else if state.mcp_scope.is_some() {
        (1, false)
    } else {
        (0, false)
    }
}

fn char_wrap_row_count(s: &str, width: usize) -> usize {
    let width = width.max(1);
    let mut rows = 1usize;
    let mut cur_w = 0usize;
    let mut cur_empty = true;
    for ch in s.chars() {
        let ch_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if cur_w + ch_w > width && !cur_empty {
            rows += 1;
            cur_w = 0;
        }
        cur_w += ch_w;
        cur_empty = false;
    }
    rows
}

fn hovered_bg(theme: &Theme) -> ratatui::style::Color {
    theme.bg_hover
}

pub struct PermissionRenderResult {
    pub inline_prompt: Option<InlinePromptArea>,
    pub preview: Option<Rect>,
    pub options: Vec<(usize, Rect)>,
}

pub struct InlinePromptArea {
    pub text_x: u16,
    pub y: u16,
    pub text_w: u16,
    pub content_x: u16,
    pub content_w: u16,
}

pub fn inline_text_width(area_width: u16) -> u16 {
    const LEFT_PAD: u16 = 3;
    const PREFIX_W: u16 = 8;
    area_width.saturating_sub(LEFT_PAD + PREFIX_W)
}

#[allow(clippy::too_many_arguments)]
pub fn render_permission_view(
    buf: &mut Buffer,
    area: Rect,
    state: &PermissionViewState,
    followup_text: &str,
    pattern_edit: Option<&PatternEditState>,
    hovered_item: Option<usize>,
    theme: &Theme,
    focused: bool,
) -> PermissionRenderResult {
    let area = area.intersection(buf.area);
    if area.height == 0 || area.width <= 5 {
        return PermissionRenderResult {
            inline_prompt: None,
            preview: None,
            options: Vec::new(),
        };
    }

    let is_followup = state.focus == PermissionFocus::FollowupInput;
    let pattern_edit = pattern_edit.filter(|_| state.focus == PermissionFocus::PatternEdit);

    let bg = Style::default().bg(theme.bg_light);
    buf.set_style(area, bg);

    let accent_style = Style::default().fg(theme.accent_user);
    for row in area.y..area.y + area.height {
        if let Some(cell) = buf.cell_mut((area.x, row)) {
            cell.set_symbol(crate::engine::render::glyphs::accent_bar());
            cell.set_style(accent_style);
        }
    }

    let content_x = area.x + 3;
    let content_width = area.width.saturating_sub(5);
    let mut y = area.y;

    y += 1;

    let area_bottom = area.y + area.height;

    if let Some(ref label) = state.subagent_label {
        if y < area_bottom {
            let prov_style = Style::default().fg(theme.gray);
            buf.set_line(
                content_x,
                y,
                &Line::from(Span::styled(label.clone(), prov_style)),
                content_width,
            );
        }
        y += 1;
    }

    if y < area_bottom {
        let title_style = Style::default()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD);
        buf.set_line(
            content_x,
            y,
            &Line::from(Span::styled(state.title.clone(), title_style)),
            content_width,
        );
    }
    y += 1;

    let (bash_rows, bash_indicator) = bash_visible_rows(state, content_width as usize);
    let mut bash_lines: Vec<Line<'_>> =
        if state.bash_highlights.is_some() || state.bash_command_raw.is_some() {
            build_permission_bash_lines(
                state.bash_command_raw.as_deref(),
                content_width as usize,
                bash_rows,
                &state.shell_layout,
            )
        } else if let Some(ref scope) = state.mcp_scope {
            build_mcp_scope_lines(scope, theme, content_width as usize)
        } else {
            Vec::new()
        };
    if bash_indicator {
        bash_lines.push(truncation_indicator_line(theme, state.labels));
    }
    {
        let (args_rows, indicator) = mcp_args_visible_rows(state, content_width as usize);
        let painted_preview = state.preview.len().min(args_rows);
        bash_lines.extend(state.preview.iter().take(painted_preview).cloned());
        bash_lines.extend(build_mcp_args_lines(
            &state.description,
            theme,
            content_width as usize,
            args_rows.saturating_sub(painted_preview),
        ));
        if indicator {
            bash_lines.push(truncation_indicator_line(theme, state.labels));
        }
    }

    let show_scope_hint = state.has_adjustable_scope();
    let show_edit_hint = state.has_editable_bash_pattern();
    let header_extra_h: u16 = if pattern_edit.is_some() {
        2
    } else if show_scope_hint || show_edit_hint {
        1
    } else {
        0
    };
    let options_reserve = header_extra_h + 1 + state.options.len() as u16 + 1;
    let max_bash_y = (area.y + area.height).saturating_sub(options_reserve);

    let mut last_drawn_bash: Option<usize> = None;
    let preview_top = y;
    let visible_preview_rows = max_bash_y.saturating_sub(preview_top) as usize;
    let preview_offset =
        usize::from(state.desc_scroll).min(bash_lines.len().saturating_sub(visible_preview_rows));
    for (li, bash_line) in bash_lines.iter().enumerate().skip(preview_offset) {
        if y >= max_bash_y {
            break;
        }
        buf.set_line(content_x, y, bash_line, content_width);
        last_drawn_bash = Some(li);
        y += 1;
    }
    let preview = (y > preview_top).then_some(Rect::new(
        content_x,
        preview_top,
        content_width,
        y.saturating_sub(preview_top),
    ));
    if let Some(last_idx) = last_drawn_bash
        && last_idx + 1 < bash_lines.len()
    {
        let text_w = bash_lines
            .get(last_idx)
            .map(|l| l.width() as u16)
            .unwrap_or(0);
        let ellipsis_x = content_x + text_w.min(content_width.saturating_sub(2));
        let ellipsis_style = Style::default().fg(theme.gray);
        buf.set_span(
            ellipsis_x,
            y - 1,
            &Span::styled(" \u{2026}", ellipsis_style),
            2,
        );
    }
    if let Some(edit) = pattern_edit {
        if y < area.y + area.height {
            render_pattern_editor_line(buf, content_x, y, content_width, edit, theme);
            y += 1;
        }
        if y < area.y + area.height {
            render_pattern_preview_line(
                buf,
                content_x,
                y,
                content_width,
                edit,
                theme,
                state.labels,
            );
            y += 1;
        }
    } else if (show_scope_hint || show_edit_hint) && y < area.y + area.height {
        let hint_style = Style::default()
            .fg(theme.text_secondary)
            .add_modifier(Modifier::DIM);
        let key_style = Style::default().fg(theme.accent_user);
        let mut spans: Vec<Span<'static>> = Vec::new();
        if show_scope_hint {
            spans.push(Span::styled("\u{2190} \u{2192}", key_style));
            spans.push(Span::styled(state.labels.narrow_scope, hint_style));
        }
        if show_edit_hint {
            if show_scope_hint {
                spans.push(Span::styled("  \u{00b7}  ", hint_style));
            }
            spans.push(Span::styled("e", key_style));
            spans.push(Span::styled(state.labels.edit_pattern, hint_style));
        }
        buf.set_line(content_x, y, &Line::from(spans), content_width);
        y += 1;
    }

    y += 1;

    let visible_bottom = area.y + area.height;
    let hover_bg = hovered_bg(theme);

    let selected_words: Option<String> = state.bash_highlights.as_ref().map(|h| {
        allow_scope_label(
            h,
            state.bash_command_raw.as_deref(),
            state.bash_selection_count,
        )
    });
    let deny_selected_words: Option<String> = state.bash_highlights.as_ref().map(|h| {
        h.highlighted_words
            .get(..state.bash_deny_selection_count)
            .unwrap_or(&[])
            .join(" ")
    });

    let mut options = Vec::new();
    let mut inline_prompt_result: Option<InlinePromptArea> = None;

    for (i, option) in state
        .options
        .iter()
        .enumerate()
        .skip(state.options_scroll_offset)
    {
        if y >= visible_bottom {
            break;
        }

        if is_followup && option.kind == PermissionOptionKind::RejectOnce {
            // Always-focused input row. No reverse-video overlay here: it is
            // an editable line (hardware cursor + accent bar carry focus),
            // so the terminal theme simply renders it bandless.
            let row_bg = theme.bg_visual;

            let full_row = Rect {
                x: area.x + 1,
                y,
                width: area.width.saturating_sub(1),
                height: 1,
            };
            options.push((i, full_row));
            buf.set_style(full_row, Style::default().bg(row_bg));

            if let Some(cell) = buf.cell_mut((area.x, y)) {
                cell.set_symbol(crate::engine::render::glyphs::accent_bar());
                cell.set_style(Style::default().fg(theme.accent_user).bg(row_bg));
            }

            let num_style = Style::default().fg(theme.accent_user).bg(row_bg);
            let marker_style = Style::default()
                .fg(theme.text_primary)
                .bg(row_bg)
                .add_modifier(Modifier::BOLD);
            let prompt_ind = Style::default().fg(theme.accent_user).bg(row_bg);
            buf.set_span(content_x, y, &Span::styled(shortcut_label(i), num_style), 2);
            buf.set_span(
                content_x + 2,
                y,
                &Span::styled(
                    format!("({}) ", crate::engine::render::glyphs::filled_dot()),
                    marker_style,
                ),
                4,
            );
            buf.set_span(
                content_x + 6,
                y,
                &Span::styled(crate::engine::render::glyphs::prompt_arrow(), prompt_ind),
                2,
            );

            let prefix_w: u16 = 8;
            let full_w = area.width.saturating_sub(3);
            inline_prompt_result = Some(InlinePromptArea {
                text_x: content_x + prefix_w,
                y,
                text_w: full_w.saturating_sub(prefix_w),
                content_x,
                content_w: full_w,
            });

            y += 1;
            continue;
        }

        let is_cursor = i == state.active_idx;
        let is_hovered = hovered_item == Some(i);
        // When the panel is unfocused, drop the cursor-row overlay so it
        // reads as "no active selection" - same rule as question_view.
        let row_bg = if is_hovered && !(is_cursor && focused) {
            hover_bg
        } else {
            theme.bg_light
        };

        let row_words = if option.id.as_str() == REJECT_ALWAYS_COMMAND_OPTION_ID {
            deny_selected_words.as_deref()
        } else {
            selected_words.as_deref()
        };
        let line = build_permission_option_line(
            option,
            i,
            is_cursor,
            row_bg,
            row_words,
            state.mcp_scope.as_ref(),
            followup_text,
            content_width,
            theme,
            state.labels,
        );

        let row_rect = Rect {
            x: content_x,
            y,
            width: content_width,
            height: 1,
        };
        options.push((i, row_rect));
        buf.set_style(row_rect, Style::default().bg(row_bg));
        buf.set_line(content_x, y, &line, content_width);
        // Band on RGB themes; reverse video on the terminal theme.
        if is_cursor && focused {
            buf.set_style(row_rect, theme.selection_overlay());
        } else if is_hovered {
            buf.set_style(row_rect, theme.hover_overlay());
        }
        y += 1;
    }

    if !focused {
        crate::engine::render::color::recede_area(buf, area, theme.bg_light, 0.66);
    }

    PermissionRenderResult {
        inline_prompt: inline_prompt_result,
        preview,
        options,
    }
}

pub fn preview_command_text(state: &PermissionViewState) -> String {
    match state.bash_highlights.as_ref() {
        Some(h) => h
            .command
            .clone()
            .unwrap_or_else(|| h.highlighted_words.join(" ")),
        None => state.bash_command_raw.clone().unwrap_or_default(),
    }
}

fn render_pattern_editor_line(
    buf: &mut Buffer,
    content_x: u16,
    y: u16,
    content_width: u16,
    edit: &PatternEditState,
    theme: &Theme,
) {
    let prompt_style = Style::default().fg(theme.accent_user);
    buf.set_span(content_x, y, &Span::styled("\u{276f} ", prompt_style), 2);

    let text_x = content_x + 2;
    let window = content_width.saturating_sub(2) as usize;
    if window == 0 {
        return;
    }

    let chars: Vec<char> = edit.buffer.chars().collect();
    let cursor_idx = edit
        .buffer
        .get(..edit.cursor)
        .map(|s| s.chars().count())
        .unwrap_or(0);
    let start = (cursor_idx + 1).saturating_sub(window);

    let text_style = Style::default().fg(theme.text_primary);
    // Accent-colored block on RGB themes; reverse video on the bandless
    // palette, whose Reset accent would paint no block at all.
    let caret_style = if theme.is_bandless() {
        theme.block_cursor_over(theme.bg_light)
    } else {
        Style::default().fg(theme.bg_light).bg(theme.accent_user)
    };

    let end = (start + window).min(chars.len());
    let mut col: u16 = 0;
    for (offset, ch) in chars.get(start..end).unwrap_or(&[]).iter().enumerate() {
        let idx = start + offset;
        let style = if idx == cursor_idx {
            caret_style
        } else {
            text_style
        };
        buf.set_span(text_x + col, y, &Span::styled(ch.to_string(), style), 1);
        col += 1;
    }
    if cursor_idx >= chars.len() && (col as usize) < window {
        buf.set_span(text_x + col, y, &Span::styled(" ", caret_style), 1);
    }
}

fn render_pattern_preview_line(
    buf: &mut Buffer,
    content_x: u16,
    y: u16,
    content_width: u16,
    edit: &PatternEditState,
    theme: &Theme,
    labels: PermissionLabels,
) {
    let dim = Style::default()
        .fg(theme.text_secondary)
        .add_modifier(Modifier::DIM);
    let sep = Span::styled("  \u{00b7}  ", dim);

    let mut spans: Vec<Span<'static>> = Vec::new();
    match edit.trimmed() {
        None => {
            spans.push(Span::styled(labels.pattern_help, dim));
        }
        Some(_) if edit.preview.is_some_and(|p| p.catch_all) => {
            spans.push(Span::styled(
                labels.pattern_rejected,
                Style::default().fg(theme.accent_error),
            ));
            spans.push(sep);
            spans.push(Span::styled("Esc", Style::default().fg(theme.accent_user)));
            spans.push(Span::styled(labels.cancel, dim));
        }
        Some(_) if edit.preview.is_some() => {
            if edit.preview.is_some_and(|p| p.matches) {
                spans.push(Span::styled(
                    labels.pattern_matches,
                    Style::default().fg(theme.accent_success),
                ));
            } else {
                spans.push(Span::styled(
                    labels.pattern_misses,
                    Style::default().fg(theme.accent_error),
                ));
            }
            if edit.preview.is_some_and(|p| p.broad) {
                spans.push(sep.clone());
                spans.push(Span::styled(
                    labels.pattern_broad,
                    Style::default().fg(theme.warning),
                ));
            }
            spans.push(sep);
            spans.push(Span::styled(
                "Enter",
                Style::default().fg(theme.accent_user),
            ));
            spans.push(Span::styled(labels.save, dim));
            spans.push(Span::styled("Esc", Style::default().fg(theme.accent_user)));
            spans.push(Span::styled(labels.cancel, dim));
        }
        Some(_) => {}
    }
    buf.set_line(content_x, y, &Line::from(spans), content_width);
}

pub fn render_bash_command_display_lines(
    command: &str,
    content_width: usize,
) -> Vec<Line<'static>> {
    build_raw_bash_lines(command, content_width, usize::MAX, &ShellLayout::default())
}

fn build_permission_bash_lines(
    raw: Option<&str>,
    content_width: usize,
    max_rows: usize,
    layout: &ShellLayout,
) -> Vec<Line<'static>> {
    match raw {
        Some(command) => build_raw_bash_lines(command, content_width, max_rows, layout),
        None => Vec::new(),
    }
}

fn prepare_bash_display_text(command: &str) -> String {
    let normalized = command.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::with_capacity(normalized.len());
    for (i, line) in normalized.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line.trim_end());
    }
    while out.ends_with('\n') {
        let Some(without) = out.len().checked_sub(1).and_then(|n| out.get(..n)) else {
            break;
        };
        if without.ends_with('\\') {
            out.pop();
            break;
        }
        if without.is_empty() || without.ends_with('\n') {
            out.pop();
            continue;
        }
        out.pop();
        break;
    }
    out
}

fn soft_wrap_row_texts<'a>(
    line: &'a str,
    line_start: usize,
    full_breaks: &[usize],
    heredoc_payload: &[(usize, usize)],
    content_width: usize,
    max_rows: usize,
) -> Vec<&'a str> {
    if max_rows == 0 {
        return Vec::new();
    }
    if content_width == 0 {
        return vec![line];
    }

    if line.len() <= content_width {
        return vec![line];
    }

    let line_end = line_start + line.len();
    if heredoc_payload
        .iter()
        .any(|&(start, end)| line_start >= start && line_end <= end)
    {
        return vec![line];
    }

    let first_inside = full_breaks.partition_point(|&b| b <= line_start);
    let mut bounds = full_breaks
        .get(first_inside..)
        .unwrap_or(&[])
        .iter()
        .copied()
        .take_while(|&b| b < line_end)
        .map(|b| b - line_start)
        .filter(|&b| line.is_char_boundary(b))
        .chain(std::iter::once(line.len()))
        .peekable();

    if bounds.peek().copied() == Some(line.len()) {
        return bash_quote_aware_wrap(line, content_width, max_rows);
    }

    let mut out: Vec<&'a str> = Vec::new();
    let mut pos = 0usize;
    let mut first_row = true;
    while pos < line.len() && out.len() < max_rows {
        let mut start = pos;
        if !first_row {
            while start < line.len()
                && line
                    .as_bytes()
                    .get(start)
                    .is_some_and(|b| b.is_ascii_whitespace())
            {
                start += 1;
            }
            while bounds.peek().is_some_and(|&b| b <= start) {
                bounds.next();
            }
            if start >= line.len() {
                break;
            }
        }
        first_row = false;

        let Some(mut end) = bounds.next() else {
            break;
        };
        if UnicodeWidthStr::width(line.get(start..end).unwrap_or("")) <= content_width {
            while let Some(&next_end) = bounds.peek() {
                if UnicodeWidthStr::width(line.get(start..next_end).unwrap_or("")) <= content_width
                {
                    end = next_end;
                    bounds.next();
                } else {
                    break;
                }
            }
            out.push(line.get(start..end).unwrap_or("").trim_end());
        } else {
            let row = line.get(start..end).unwrap_or("").trim_end();
            if UnicodeWidthStr::width(row) <= content_width {
                out.push(row);
            } else {
                out.extend(bash_quote_aware_wrap(
                    row,
                    content_width,
                    max_rows - out.len(),
                ));
            }
        }
        pos = end;
    }
    out
}

fn display_width_end(s: &str, start: usize, limit: usize, width: usize) -> usize {
    let Some(rest) = s.get(start..limit.min(s.len())) else {
        return start;
    };
    let mut used = 0usize;
    let mut end = start;
    for ch in rest.chars() {
        let ch_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_w > width && end > start {
            break;
        }
        used += ch_w;
        end += ch.len_utf8();
        if used >= width {
            break;
        }
    }
    end
}

fn extend_display_width_rows<'a>(
    rows: &mut Vec<&'a str>,
    line: &'a str,
    start: usize,
    end: usize,
    width: usize,
    max_rows: usize,
) {
    let end = end.min(line.len());
    let mut pos = start.min(end);
    while pos < end && rows.len() < max_rows {
        let chunk_end = display_width_end(line, pos, end, width);
        if chunk_end <= pos {
            break;
        }
        match line.get(pos..chunk_end) {
            Some(row) => rows.push(row),
            None => break,
        }
        pos = chunk_end;
    }
}

fn bash_quote_aware_wrap(line: &str, width: usize, max_rows: usize) -> Vec<&str> {
    if max_rows == 0 {
        return Vec::new();
    }
    if width == 0 || line.len() <= width {
        return vec![line];
    }

    let mut rows: Vec<&str> = Vec::new();
    let mut row_start = 0usize;
    let mut last_break = 0usize;

    let candidates = QuoteAwareBreakPoints::new(line).chain(std::iter::once(line.len()));

    for b in candidates {
        if b <= row_start {
            continue;
        }
        let candidate = line.get(row_start..b).unwrap_or("").trim_end();
        if UnicodeWidthStr::width(candidate) <= width {
            last_break = b;
            continue;
        }
        if last_break > row_start {
            let row = line.get(row_start..last_break).unwrap_or("").trim_end();
            if !row.is_empty() {
                rows.push(row);
                if rows.len() >= max_rows {
                    return rows;
                }
            }
            row_start = last_break;
            while row_start < line.len()
                && line
                    .as_bytes()
                    .get(row_start)
                    .is_some_and(|b| b.is_ascii_whitespace())
            {
                row_start += 1;
            }
            last_break = row_start;
            if b > row_start {
                let candidate = line.get(row_start..b).unwrap_or("").trim_end();
                if UnicodeWidthStr::width(candidate) <= width {
                    last_break = b;
                } else {
                    extend_display_width_rows(&mut rows, line, row_start, b, width, max_rows);
                    if rows.len() >= max_rows {
                        return rows;
                    }
                    row_start = b;
                    while row_start < line.len()
                        && line
                            .as_bytes()
                            .get(row_start)
                            .is_some_and(|b| b.is_ascii_whitespace())
                    {
                        row_start += 1;
                    }
                    last_break = row_start;
                }
            }
        } else {
            extend_display_width_rows(&mut rows, line, row_start, b, width, max_rows);
            if rows.len() >= max_rows {
                return rows;
            }
            row_start = b;
            while row_start < line.len()
                && line
                    .as_bytes()
                    .get(row_start)
                    .is_some_and(|b| b.is_ascii_whitespace())
            {
                row_start += 1;
            }
            last_break = row_start;
        }
    }
    if row_start < line.len() && rows.len() < max_rows {
        let row = line.get(row_start..).unwrap_or("").trim_end();
        if !row.is_empty() {
            rows.push(row);
        }
    }
    if rows.is_empty() { vec![line] } else { rows }
}

struct QuoteAwareBreakPoints<'a> {
    bytes: &'a [u8],
    i: usize,
    in_single: bool,
    in_double: bool,
}

impl<'a> QuoteAwareBreakPoints<'a> {
    fn new(line: &'a str) -> Self {
        Self {
            bytes: line.as_bytes(),
            i: 0,
            in_single: false,
            in_double: false,
        }
    }
}

impl Iterator for QuoteAwareBreakPoints<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        while self.i < self.bytes.len() {
            let c = *self.bytes.get(self.i)?;
            if self.in_single {
                if c == b'\'' {
                    self.in_single = false;
                }
                self.i += 1;
                continue;
            }
            if self.in_double {
                if c == b'\\' && self.i + 1 < self.bytes.len() {
                    self.i += 2;
                    continue;
                }
                if c == b'"' {
                    self.in_double = false;
                }
                self.i += 1;
                continue;
            }
            match c {
                b'\'' => {
                    self.in_single = true;
                    self.i += 1;
                }
                b'"' => {
                    self.in_double = true;
                    self.i += 1;
                }
                b if b.is_ascii_whitespace() => {
                    let start = self.i;
                    while self.i < self.bytes.len()
                        && self
                            .bytes
                            .get(self.i)
                            .is_some_and(|b| b.is_ascii_whitespace())
                    {
                        self.i += 1;
                    }
                    if start > 0 {
                        return Some(start);
                    }
                }
                _ => self.i += 1,
            }
        }
        None
    }
}

fn build_raw_bash_lines(
    command: &str,
    content_width: usize,
    max_rows: usize,
    layout: &ShellLayout,
) -> Vec<Line<'static>> {
    let text = prepare_bash_display_text(command);
    if text.is_empty() || max_rows == 0 {
        return Vec::new();
    }

    let full_breaks = &layout.operator_breaks;
    let heredoc_payload = &layout.heredoc_payload;

    let fallback = Style::default().fg(Theme::current().command);
    let grammar = if cfg!(windows) { "powershell" } else { "bash" };
    let mut hl = syntax::by_token(grammar);

    let mut out = Vec::new();
    let mut offset = 0usize;
    for (idx, physical) in text.split('\n').enumerate() {
        if out.len() >= max_rows {
            break;
        }
        if idx > 0 {
            offset += 1;
        }
        let spans = syntax::paint(physical, &mut hl, fallback);
        if physical.is_empty() {
            out.push(Line::default());
            continue;
        }
        debug_assert_eq!(
            spans.iter().map(|s| s.content.as_ref()).collect::<String>(),
            physical,
            "highlight spans must flatten back to the physical line"
        );
        for row in soft_wrap_row_texts(
            physical,
            offset,
            full_breaks,
            heredoc_payload,
            content_width,
            max_rows - out.len(),
        ) {
            let start = (row.as_ptr() as usize) - (physical.as_ptr() as usize);
            out.push(Line::from(slice_highlighted_spans(
                &spans,
                start,
                start + row.len(),
            )));
        }
        offset += physical.len();
    }
    out
}

fn count_raw_bash_rows(
    command: &str,
    content_width: usize,
    max_rows: usize,
    layout: &ShellLayout,
) -> usize {
    let text = prepare_bash_display_text(command);
    if text.is_empty() {
        return 0;
    }
    let full_breaks = &layout.operator_breaks;
    let heredoc_payload = &layout.heredoc_payload;
    let mut rows = 0usize;
    let mut offset = 0usize;
    for (idx, physical) in text.split('\n').enumerate() {
        if rows >= max_rows {
            return rows;
        }
        if idx > 0 {
            offset += 1;
        }
        if physical.is_empty() {
            rows += 1;
        } else {
            rows += soft_wrap_row_texts(
                physical,
                offset,
                full_breaks,
                heredoc_payload,
                content_width,
                max_rows - rows,
            )
            .len();
        }
        offset += physical.len();
    }
    rows
}

fn slice_highlighted_spans(
    spans: &[Span<'static>],
    start: usize,
    end: usize,
) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    for span in spans {
        let span_start = pos;
        let span_end = pos + span.content.len();
        pos = span_end;
        if span_end <= start {
            continue;
        }
        if span_start >= end {
            break;
        }
        let lo = start.max(span_start) - span_start;
        let hi = end.min(span_end) - span_start;
        if lo >= hi {
            continue;
        }
        let Some(slice) = span.content.get(lo..hi) else {
            debug_assert!(false, "row boundary off a char boundary");
            continue;
        };
        out.push(Span::styled(slice.to_owned(), span.style));
    }
    out
}

fn build_mcp_scope_lines(
    scope: &McpScopeState,
    theme: &Theme,
    _content_w: usize,
) -> Vec<Line<'static>> {
    let active_style = Style::default()
        .fg(theme.accent_user)
        .add_modifier(Modifier::BOLD);
    let inactive_style = Style::default().fg(theme.gray).add_modifier(Modifier::DIM);

    let spans: Vec<Span<'static>> = match (scope.selected, scope.server_prefix.as_deref()) {
        (_, None) => vec![Span::styled(scope.display_name(), active_style)],
        (McpScope::Tool, Some(_)) => vec![Span::styled(scope.display_name(), active_style)],
        (McpScope::Server, Some(prefix)) => vec![
            Span::styled(format!("({}) ", titleize_segment(prefix)), active_style),
            Span::styled(titleize_segment(scope.action()), inactive_style),
        ],
    };
    vec![Line::from(spans)]
}

fn build_mcp_args_lines(
    description: &[String],
    theme: &Theme,
    content_w: usize,
    max_rows: usize,
) -> Vec<Line<'static>> {
    if description.is_empty() || max_rows == 0 {
        return Vec::new();
    }
    let fallback = Style::default().fg(theme.text_secondary);
    let mut hl = syntax::by_token("json");
    let mut out: Vec<Line<'static>> = Vec::new();
    for raw in description {
        if out.len() >= max_rows {
            break;
        }
        let spans = syntax::paint(raw, &mut hl, fallback);
        out.extend(char_wrap_spans(spans, content_w));
    }
    out.truncate(max_rows);
    out
}

fn truncation_indicator_line(theme: &Theme, labels: PermissionLabels) -> Line<'static> {
    let style = Style::default().fg(theme.gray).bg(theme.bg_light);
    Line::from(vec![
        Span::styled("... ", style),
        Span::styled(
            "Ctrl-F",
            Style::default().fg(theme.accent_user).bg(theme.bg_light),
        ),
        Span::styled(labels.expand, style),
    ])
}

fn char_wrap_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut line_spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_style = Style::default();
    let mut col = 0usize;

    for span in spans {
        let style = span.style;
        for ch in span.content.chars() {
            let ch_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            let line_has_content = !run.is_empty() || !line_spans.is_empty();
            if col + ch_w > width && line_has_content {
                if !run.is_empty() {
                    line_spans.push(Span::styled(std::mem::take(&mut run), run_style));
                }
                lines.push(Line::from(std::mem::take(&mut line_spans)));
                col = 0;
            }
            if style != run_style && !run.is_empty() {
                line_spans.push(Span::styled(std::mem::take(&mut run), run_style));
            }
            run_style = style;
            run.push(ch);
            col += ch_w;
        }
    }
    if !run.is_empty() {
        line_spans.push(Span::styled(run, run_style));
    }
    if !line_spans.is_empty() || lines.is_empty() {
        lines.push(Line::from(line_spans));
    }
    lines
}

#[allow(clippy::too_many_arguments)]
fn build_permission_option_line<'a>(
    option: &PermissionOption,
    index: usize,
    is_cursor: bool,
    row_bg: ratatui::style::Color,
    selected_words: Option<&str>,
    mcp_scope: Option<&McpScopeState>,
    followup_text: &str,
    row_width: u16,
    theme: &Theme,
    labels: PermissionLabels,
) -> Line<'a> {
    let num_style = Style::default().fg(theme.accent_user).bg(row_bg);

    let sc = shortcut_char(index);

    if option.kind == PermissionOptionKind::RejectOnce {
        let deny = if option.name.is_empty() {
            labels.deny
        } else {
            &option.name
        };
        return build_reject_once_line(sc, is_cursor, row_bg, followup_text, theme, deny);
    }

    let (label_prefix, scope_words) = dynamic_option_label(option, selected_words, mcp_scope);
    let scope_is_mcp = mcp_scope.is_some();

    let marker = if is_cursor {
        format!("({})", crate::engine::render::glyphs::filled_dot())
    } else {
        "(\u{25cb})".to_string()
    };
    let marker_style = if is_cursor {
        Style::default()
            .fg(theme.text_primary)
            .bg(row_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.gray).bg(row_bg)
    };
    let label_style = Style::default()
        .fg(theme.text_primary)
        .bg(row_bg)
        .add_modifier(if is_cursor {
            Modifier::BOLD
        } else {
            Modifier::empty()
        });

    let mut spans = vec![
        Span::styled(format!("{sc} "), num_style),
        Span::styled(format!("{marker} "), marker_style),
        Span::styled(label_prefix, label_style),
    ];

    if let Some(scope) = scope_words {
        let prefix_w: usize = spans.iter().map(|s| s.width()).sum();
        let max_scope = (row_width as usize).saturating_sub(prefix_w + 1);
        let truncated = if scope.width() > max_scope {
            crate::engine::render::line_utils::truncate_str(&scope, max_scope)
        } else {
            scope
        };
        if scope_is_mcp {
            spans.push(Span::styled(truncated, label_style));
        } else {
            for s in syntax::paint(
                &truncated,
                &mut syntax::by_token("bash"),
                Style::default().fg(theme.command),
            ) {
                spans.push(Span::styled(s.content.into_owned(), s.style.bg(row_bg)));
            }
        }
    }

    Line::from(spans).style(Style::default().bg(row_bg))
}

fn build_reject_once_line<'a>(
    shortcut_ch: char,
    is_cursor: bool,
    row_bg: ratatui::style::Color,
    followup_text: &str,
    theme: &Theme,
    deny: &str,
) -> Line<'a> {
    let num_style = Style::default().fg(theme.accent_user).bg(row_bg);
    let has_text = !followup_text.trim().is_empty();

    let (marker, marker_style) = if is_cursor {
        (
            format!("({})", crate::engine::render::glyphs::filled_dot()),
            Style::default()
                .fg(theme.text_primary)
                .bg(row_bg)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        (
            "(\u{25cb})".to_string(),
            Style::default().fg(theme.gray).bg(row_bg),
        )
    };

    let prompt_indicator = Style::default().fg(theme.accent_user).bg(row_bg);

    let (label, label_style) = if has_text {
        let first_line = followup_text.lines().next().unwrap_or("");
        let preview = crate::engine::render::line_utils::truncate_str(first_line, 50);
        (preview, Style::default().fg(theme.text_primary).bg(row_bg))
    } else {
        (deny.to_owned(), Style::default().fg(theme.gray).bg(row_bg))
    };

    let mut spans = vec![
        Span::styled(format!("{shortcut_ch} "), num_style),
        Span::styled(format!("{marker} "), marker_style),
    ];
    if has_text {
        spans.push(Span::styled(
            crate::engine::render::glyphs::prompt_arrow(),
            prompt_indicator,
        ));
    }
    spans.push(Span::styled(label, label_style));

    Line::from(spans).style(Style::default().bg(row_bg))
}

fn dynamic_option_label(
    option: &PermissionOption,
    selected_words: Option<&str>,
    mcp_scope: Option<&McpScopeState>,
) -> (String, Option<String>) {
    if matches!(
        option.kind,
        PermissionOptionKind::AllowAlways | PermissionOptionKind::RejectAlways
    ) && let Some(prefix) = &option.scope_prefix
    {
        if let Some(scope) = mcp_scope {
            let scope_text = match scope.selected {
                McpScope::Tool => scope.display_name(),
                McpScope::Server => match scope.server_prefix.as_deref() {
                    Some(server) => scope
                        .all_tools_label
                        .replace("{server}", &titleize_segment(server)),
                    None => scope.display_name(),
                },
            };
            return (format!("{prefix} "), Some(scope_text));
        }
        if let Some(words) = selected_words {
            return (format!("{prefix} "), Some(words.to_owned()));
        }
    }
    (option.name.clone(), None)
}

pub fn allow_scope_label(
    h: &BashCommandHighlights,
    raw_command: Option<&str>,
    count: usize,
) -> String {
    let words = &h.highlighted_words;
    let n = count.min(words.len());
    let uses_raw_key = n == words.len()
        && n > 0
        && h.prefix.is_empty()
        && h.suffix.is_empty()
        && words
            .get(..n)
            .is_some_and(|w| w.iter().any(|w| w.chars().any(char::is_whitespace)));
    match raw_command.filter(|_| uses_raw_key) {
        Some(raw) => raw.to_owned(),
        None => words.get(..n).unwrap_or(&[]).join(" "),
    }
}

pub fn option_label_for_selection(
    option: &PermissionOption,
    selected_words: Option<&str>,
    mcp_scope: Option<&McpScopeState>,
) -> String {
    let (prefix, scope_text) = dynamic_option_label(option, selected_words, mcp_scope);
    match scope_text {
        Some(scope) => format!("{prefix}{scope}"),
        None => prefix,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

#[derive(Debug, Clone)]
pub struct PermissionOption {
    pub id: String,
    pub name: String,
    pub kind: PermissionOptionKind,
    pub scope_prefix: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct BashCommandHighlights {
    pub highlighted_words: Vec<String>,
    pub prefix: String,
    pub suffix: String,
    pub command: Option<String>,
    pub persisting_prefixes: Vec<usize>,
}

/// Optional display offsets in the normalized preview. The harness owns shell parsing and policy.
#[derive(Debug, Default)]
pub struct ShellLayout {
    pub operator_breaks: Vec<usize>,
    pub heredoc_payload: Vec<(usize, usize)>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PatternPreview {
    pub catch_all: bool,
    pub matches: bool,
    pub broad: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionInput {
    Changed,
    Select(usize),
    Cancel,
    AdjustScope(bool),
    EditPattern,
    ExpandPreview,
    ScrollPreview(i16),
    Unhandled,
}

impl PermissionViewState {
    pub fn handle_key(&mut self, key: &KeyEvent) -> PermissionInput {
        let len = self.options.len();
        if key.code == KeyCode::Esc
            || key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c')
        {
            return PermissionInput::Cancel;
        }
        if len == 0 || self.focus != PermissionFocus::Options {
            return PermissionInput::Unhandled;
        }
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('f') {
            return PermissionInput::ExpandPreview;
        }
        if !(key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT) {
            return PermissionInput::Unhandled;
        }
        match key.code {
            KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => {
                self.active_idx = (self.active_idx + len - 1) % len
            }
            KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
                self.active_idx = (self.active_idx + 1) % len
            }
            KeyCode::Enter if self.active_idx < len => {
                return PermissionInput::Select(self.active_idx);
            }
            KeyCode::Left if self.has_adjustable_scope() => {
                return PermissionInput::AdjustScope(false);
            }
            KeyCode::Right if self.has_adjustable_scope() => {
                return PermissionInput::AdjustScope(true);
            }
            KeyCode::Char('e') if self.has_editable_bash_pattern() => {
                return PermissionInput::EditPattern;
            }
            KeyCode::Char(ch @ '1'..='9') if (ch as usize - '1' as usize) < len => {
                self.active_idx = ch as usize - '1' as usize;
                return PermissionInput::Select(self.active_idx);
            }
            _ => return PermissionInput::Unhandled,
        }
        PermissionInput::Changed
    }
}

impl PermissionRenderResult {
    pub fn hit_test(&self, col: u16, row: u16) -> Option<usize> {
        self.options
            .iter()
            .find(|(_, rect)| rect.contains((col, row).into()))
            .map(|(index, _)| *index)
    }

    pub fn handle_mouse(&self, mouse: &MouseEvent) -> PermissionInput {
        let hit = self.hit_test(mouse.column, mouse.row);
        match (mouse.kind, hit) {
            (MouseEventKind::Down(MouseButton::Left), Some(index)) => {
                PermissionInput::Select(index)
            }
            (MouseEventKind::Moved, _) => PermissionInput::Changed,
            (MouseEventKind::ScrollUp | MouseEventKind::ScrollDown, _)
                if self
                    .preview
                    .is_some_and(|rect| rect.contains((mouse.column, mouse.row).into())) =>
            {
                PermissionInput::ScrollPreview(if mouse.kind == MouseEventKind::ScrollUp {
                    -3
                } else {
                    3
                })
            }
            _ => PermissionInput::Unhandled,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PermissionLabels {
    pub deny: &'static str,
    pub narrow_scope: &'static str,
    pub edit_pattern: &'static str,
    pub expand: &'static str,
    pub pattern_help: &'static str,
    pub pattern_rejected: &'static str,
    pub pattern_matches: &'static str,
    pub pattern_misses: &'static str,
    pub pattern_broad: &'static str,
    pub cancel: &'static str,
    pub save: &'static str,
}

impl Default for PermissionLabels {
    fn default() -> Self {
        Self {
            deny: "No, reject (type to add feedback)",
            narrow_scope: " narrow scope",
            edit_pattern: " edit pattern",
            expand: " to expand",
            pattern_help: "Type a command pattern to allow",
            pattern_rejected: "✗ Matches everything",
            pattern_matches: "✓ Matches this command",
            pattern_misses: "✗ Does not match this command",
            pattern_broad: "⚠ Very broad",
            cancel: " cancel",
            save: " save  ",
        }
    }
}
