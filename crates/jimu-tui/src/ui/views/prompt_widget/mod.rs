// Adapted from the Grok Build pager (Apache-2.0). Copyright 2023-2026 SpaceXAI.

//! Reusable prompt input widget.
//!
//! [`PromptWidget`] is a text input component built on [`TextArea`] that handles text editing, multiline input, and submit.
//! It does NOT handle focus management (Esc, Tab) or render chrome (accent lines, selection boxes); those are the caller's responsibility.
//! That split keeps this widget reusable across the agent view, the welcome screen, overlays, etc.
//!
//! ## Visual (as rendered by agent view with chrome)
//!
//! ```text
//!                                            ← top vpad (configurable)
//!  ❯ type here, text wraps                   ← prefix + TextArea
//!    continuation of long input...            ← TextArea continuation
//!  grok-3 · yolo                             ← info line (optional)
//! ```
//!
//! The accent line (┃) and selection box are rendered by the caller.

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::StatefulWidgetRef;

use crate::engine::textarea::{ElementId, ElementKind, TextArea, TextAreaState, TextElement};

use super::support::preview_overlay::{PreviewConfig, PreviewStyle, render_preview_overlay};
use crate::engine::clipboard::{SystemClipboard, system_clipboard_get};
use crate::engine::input::key::key;
use crate::engine::render::SafeBuf;
use crate::engine::theme::Theme;
pub use images::{PastedImage, PromptImagePreview, PromptImagePreviewPreparation};

use self::suggestion::PromptSuggestionController;
use super::completion_dropdown::{DropdownChrome, DropdownMouse, scroll_offset_for};
use crate::ui::views::history_search::{HistoryEntry, HistorySearchState, history_scroll_offset};
use crate::ui::views::suggestion_controller::{
    AcceptMode, CompletionSplice, SuggestionController, SuggestionSource,
};

/// Element kind tag for pasted multi-line content.
pub const KIND_PASTE: ElementKind = ElementKind(1);
/// Element kind tag for @-file references (future use).
pub const KIND_FILE_REF: ElementKind = ElementKind(2);
/// Element kind tag for pasted image chips (`[Image #N]`).
pub const KIND_IMAGE: ElementKind = ElementKind(3);

/// Byte size at which a single-line paste is chipped (display only, not an offload threshold).
const PASTE_CHIP_DISPLAY_BYTES: usize = 10_000;

fn text_without_image_chips(
    text: &str,
    elements: impl Iterator<Item = (ElementKind, std::ops::Range<usize>)>,
) -> String {
    let mut output = String::with_capacity(text.len());
    let mut previous_end = 0usize;
    for (kind, range) in elements {
        if kind != KIND_IMAGE {
            continue;
        }
        let Some(gap) = text.get(previous_end..range.start) else {
            continue;
        };
        output.push_str(gap);
        previous_end = range.end;
    }
    if let Some(tail) = text.get(previous_end..) {
        output.push_str(tail);
    }
    output
}

/// What kind of element interaction occurred when pressing Enter on a chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElementInteraction {
    /// Paste or file-ref element was inlined (expanded).
    Inlined,
    /// Image chip was activated (caller should open preview).
    ImagePreview,
}

/// What happened after the widget processed an input event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptEvent {
    SearchHistory,
    ActivateStatus,
    ActivateElement(ElementId),
    /// Text was modified (char inserted, deleted, paste, newline). Redraw needed.
    Edited,
    /// The widget didn't handle this event. The caller should handle it (Esc, Tab, Ctrl-D, etc.).
    Ignored,
}

/// Result of routing an Enter-family key event through a submittable prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnterOutcome {
    PassThrough,
    NewlineInserted,
    Submit,
}

/// Request to open the line viewer, with optional initial line range.
pub struct ViewerRequest {
    pub path: std::path::PathBuf,
    /// Optional initial line range (1-based, exclusive end) to scroll to and select.
    pub initial_range: Option<std::ops::Range<usize>>,
}

/// Whether the in-app `Cmd+A` "select all in prompt" handler is supported for the given terminal
/// brand. Gated to Ghostty only. The rest have in-terminal "Select All" behaviour we don't want to
/// fight.
fn cmd_a_select_all_supported(brand: crate::engine::terminal::TerminalName) -> bool {
    matches!(brand, crate::engine::terminal::TerminalName::Ghostty)
}

/// Visual configuration for prompt rendering.
#[derive(Debug, Clone)]
pub struct PromptStyle {
    /// Whether the prompt is focused (affects prefix color, text dimming).
    pub focused: bool,
    /// Whether to show the ❯ prefix character.
    pub show_prefix: bool,
    /// Vertical padding above the text content.
    pub vpad_top: u16,
    /// Whether to render chrome (accent line + hpad + background fill).
    /// When true, the widget renders the full block layout (accent | hpad | content | hpad).
    /// When false, the widget renders only content (for use in overlays).
    pub chrome: bool,
    /// Layout config for chrome mode (accent + padding widths).
    /// Only used when `chrome` is true.
    pub chrome_pad_left: u16,
    pub chrome_pad_right: u16,
    /// Background surface for the prompt; see [`PromptBg`].
    pub bg: PromptBg,
    /// Override the accent line color.
    /// When `Some`, uses this color instead of the default `accent_user` / `gray_dim`. Used for plan mode (golden).
    pub accent_color_override: Option<ratatui::style::Color>,
    /// Override the border color (╭─╮│╰─╯).
    /// When `Some`, uses this color instead of `prompt_border_active` / `prompt_border`. Used for plan mode.
    pub border_color_override: Option<ratatui::style::Color>,
    /// Override the prefix character and its color.
    /// When `Some((str, color))`, replaces the default `❯` prefix.
    /// Used for bash mode (`"! "` in yellow).
    pub prefix_override: Option<(&'static str, ratatui::style::Color)>,
    /// Override the placeholder text shown when the textarea is empty (e.g. remember mode).
    /// When `Some(text)`, uses this instead of the default `"Build anything"`.
    pub placeholder_override: Option<&'static str>,
    /// The main composer hides the empty-textarea placeholder on focus; the feedback box keeps it visible.
    pub placeholder_when_focused: bool,
    /// Compact mode (currently unused for info_block sizing).
    pub compact: bool,
    /// Show the accent line (`┃`) on the left edge of the chrome.
    /// When false the line is hidden and its column reclaimed for content.
    pub show_accent_line: bool,
    /// Draw the prompt's border box (top `╭─╮` divider, side `│` borders, and bottom info divider).
    /// Only consulted when `chrome` is true.
    /// Defaults to `true` (the full-TUI boxed prompt); minimal mode sets it `false` for a cleaner, border-less input that still keeps the chrome padding.
    pub show_borders: bool,
    /// Session title inlined in the top border, aligned and styled like the bottom info line.
    /// None (default) keeps the plain border. Set only by the agent view.
    pub title: Option<String>,
    /// Paint image-chip overlay into `overlay_area` (default true).
    pub image_preview: bool,
    pub image_labels: ImagePreviewLabels,
}

/// Background for the prompt widget. Paste chips bake `theme.paste_bg` (a badge color tuned for the
/// default canvas) into their display `Line` at paste time. The background therefore says what kind
/// of surface the prompt sits on, not just its color.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PromptBg {
    /// The standalone prompt's default fill (`theme.bg_base`).
    #[default]
    Default,
    /// Explicit canvas color for prompts inlined in another widget whose surface matches the main prompt's (dashboard dispatch box, peek reply).
    /// Chips keep their badge background.
    Canvas(ratatui::style::Color),
    /// Inline panel color (question freeform input, permission follow-up).
    /// Chip cells are repainted to blend into the panel.
    Panel(ratatui::style::Color),
}

impl PromptBg {
    /// Effective fill color; `default` is the standalone prompt's.
    fn color(self, default: ratatui::style::Color) -> ratatui::style::Color {
        match self {
            Self::Default => default,
            Self::Canvas(c) | Self::Panel(c) => c,
        }
    }
}

impl Default for PromptStyle {
    fn default() -> Self {
        Self {
            focused: true,
            show_prefix: true,
            vpad_top: 1,
            chrome: true,
            chrome_pad_left: 2,
            chrome_pad_right: 1,
            bg: PromptBg::Default,
            accent_color_override: None,
            border_color_override: None,
            prefix_override: None,
            placeholder_when_focused: false,
            placeholder_override: None,
            compact: false,
            show_accent_line: false,
            show_borders: true,
            title: None,
            image_preview: true,
            image_labels: ImagePreviewLabels::default(),
        }
    }
}

impl PromptStyle {
    /// Style for overlays (no chrome, no vpad).
    pub fn overlay() -> Self {
        Self {
            chrome: false,
            vpad_top: 0,
            ..Default::default()
        }
    }

    /// Style for inline prompts in overlay panels (permission, plan approval, question).
    /// Focused, chromeless, no prefix, no vpad, bg_visual background.
    pub fn inline(bg: ratatui::style::Color) -> Self {
        Self {
            focused: true,
            show_prefix: false,
            vpad_top: 0,
            chrome: false,
            chrome_pad_left: 0,
            chrome_pad_right: 0,
            bg: PromptBg::Panel(bg),
            accent_color_override: None,
            border_color_override: None,
            prefix_override: None,
            placeholder_when_focused: false,
            placeholder_override: None,
            compact: false,
            show_accent_line: false,
            show_borders: false,
            title: None,
            image_preview: true,
            image_labels: ImagePreviewLabels::default(),
        }
    }

    /// Info block height: rows reserved for the bottom divider line.
    pub fn info_block(&self, has_info: bool) -> u16 {
        if has_info || (self.chrome && self.show_borders) {
            1
        } else {
            0
        } // bottom divider only
    }

    /// Mode-tinted accent: the override (e.g. plan mode) when set, else the focus-dependent default.
    /// Shared by the accent line and the prompt prefix so tints stay in lockstep.
    pub fn accent_color(&self, theme: &Theme) -> ratatui::style::Color {
        self.accent_color_override.unwrap_or(if self.focused {
            theme.accent_user
        } else {
            theme.gray_dim
        })
    }
}

/// A flag displayed in the prompt info line (e.g., "plan", "always-approve").
pub struct PromptFlag<'a> {
    /// Display text for the flag.
    pub text: &'a str,
    /// Optional color override.
    /// When `Some`, the flag renders in this color (blended toward bg for subtlety). When `None`, uses the default gray.
    pub color: Option<ratatui::style::Color>,
    /// Whether to render with bold modifier.
    pub bold: bool,
}

/// Info-line mode flags shared by the chat prompt and the dashboard peek badge: mode label, then permission.
/// Mode and permission are independent axes, so neither hides the other.
pub fn mode_flags<'a>(
    mode_label: Option<&'a str>,
    permission: PermissionLabel,
    theme: &Theme,
) -> Vec<PromptFlag<'a>> {
    let mut flags = Vec::new();
    if let Some(text) = mode_label {
        // Ask must stay distinct from the plan-family accent.
        let color = if text == "ask" {
            theme.accent_success
        } else {
            theme.accent_plan
        };

        flags.push(PromptFlag {
            text,
            color: Some(color),
            bold: false,
        });
    }
    if permission != PermissionLabel::Ask {
        flags.push(PromptFlag {
            text: permission.display_name(),
            // Blue `accent_system` reads as "system/automation", distinct from plan
            color: (permission == PermissionLabel::Auto).then_some(theme.accent_system),
            bold: false,
        });
    }
    flags
}

/// Optional info line rendered below the prompt text.
///
/// The default is blank: a caller that wants the bottom border without any info text passes it, and [`Self::is_blank`] then skips the text pass.
#[derive(Default)]
pub struct PromptInfo<'a> {
    /// Primary label to display on the info line (left side).
    pub model_name: &'a str,
    /// Flags to display on the left side, joined by " · " (e.g., "plan", "always-approve").
    pub flags: &'a [PromptFlag<'a>],
    /// Whether multiline mode is active (shown right-aligned).
    pub multiline: bool,
    /// Optional usage warning displayed right-aligned (e.g. "5% usage left").
    pub usage_warning: Option<&'a str>,
    /// When true the warning uses the yellow warning color (5% or less left); when false it uses dim grey text (5-10% left).
    pub usage_warning_critical: bool,
    pub multiline_label: Option<&'a str>,
}

impl PromptInfo<'_> {
    pub fn is_blank(&self) -> bool {
        self.model_name.is_empty()
            && self.flags.is_empty()
            && !self.multiline
            && self.usage_warning.is_none()
    }
}

/// Live voice-capture overlay for the prompt.
#[derive(Debug, Clone, Copy)]
pub struct VoicePromptOverlay<'a> {
    /// Latest interim transcript, if any.
    pub interim: Option<&'a str>,
    /// Theme accent associated with this overlay.
    pub color: ratatui::style::Color,
}

/// Greedy word-wrap the interim transcript into at most `max_rows` lines of `max_w` columns, appending an ellipsis to the last line when truncated.
fn wrap_voice_interim(text: &str, max_w: usize, max_rows: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthStr;
    if max_w == 0 || max_rows == 0 {
        return Vec::new();
    }
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for word in text.split_whitespace() {
        let ww = UnicodeWidthStr::width(word);
        if !cur.is_empty() && cur_w + 1 + ww > max_w {
            lines.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        if cur.is_empty() {
            if ww > max_w {
                cur = crate::engine::render::line_utils::truncate_str(word, max_w);
                cur_w = UnicodeWidthStr::width(cur.as_str());
            } else {
                cur.push_str(word);
                cur_w = ww;
            }
        } else {
            cur.push(' ');
            cur.push_str(word);
            cur_w += 1 + ww;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.len() > max_rows {
        lines.truncate(max_rows);
        if let Some(last) = lines.last_mut() {
            let trimmed =
                crate::engine::render::line_utils::truncate_str(last, max_w.saturating_sub(1));
            *last = format!("{trimmed}\u{2026}");
        }
    }
    lines
}

/// Result of rendering the prompt.
pub struct PromptRenderResult {
    /// Cursor position if the prompt wants a visible cursor.
    /// `None` when unfocused.
    pub cursor_pos: Option<(u16, u16)>,
    /// Terminal escape sequences to write after the ratatui cell flush (e.g. Kitty/iTerm2 inline image rendering for the image preview).
    pub post_flush_escapes: Option<PromptOverlayEscapes>,
}

/// Prompt state saved while another input context owns the composer.
#[derive(Debug, Default)]
pub struct StashedPrompt {
    pub text: String,
    pub cursor: usize,
    pub(crate) images: Vec<PastedImage>,
    pub(crate) chip_elements: Vec<ChipElement>,
    pub(crate) image_counter: usize,
    pub(crate) image_undo_stash: Vec<PastedImage>,
}

impl Drop for StashedPrompt {
    fn drop(&mut self) {
        images::drain_and_cleanup(images::SessionPathPolicy::Preserve, &mut self.images);
        images::drain_and_cleanup(
            images::SessionPathPolicy::Preserve,
            &mut self.image_undo_stash,
        );
    }
}

/// Feedback attachments in transit between the composer, the trace-consent card, and the send dispatch.
///
/// Owns its staged temp files: dropping without transferring them deletes them.
#[derive(Debug, Default)]
pub struct FeedbackImages(Vec<PastedImage>);

impl From<Vec<PastedImage>> for FeedbackImages {
    fn from(images: Vec<PastedImage>) -> Self {
        Self(images)
    }
}

impl Drop for FeedbackImages {
    fn drop(&mut self) {
        images::drain_and_cleanup(images::SessionPathPolicy::Delete, &mut self.0);
    }
}

impl FeedbackImages {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Read access for encoding; cleanup stays with this owner.
    pub fn as_slice(&self) -> &[PastedImage] {
        &self.0
    }

    /// Hand the records to a consumer that takes over cleanup (the prompt widget adopting them as live chips).
    pub fn take(&mut self) -> Vec<PastedImage> {
        std::mem::take(&mut self.0)
    }
}

impl StashedPrompt {
    pub fn from_submission(
        text: String,
        images: Vec<PastedImage>,
        chip_elements: Vec<ChipElement>,
    ) -> Self {
        Self {
            cursor: text.len(),
            image_counter: images_high_water(&images),
            text,
            images,
            chip_elements,
            image_undo_stash: Vec::new(),
        }
    }

    /// Clone for freeform prefill while this stash remains the session draft.
    /// Omits `staged_temp_path` so freeform Drop cannot delete session temps; display/send still use `encoded_bytes` / `session_image_path`.
    pub fn clone_for_live_prefill(&self) -> Self {
        let strip_temp = |img: &PastedImage| {
            let mut c = img.clone();
            c.staged_temp_path = None;
            c
        };
        Self {
            text: self.text.clone(),
            cursor: self.cursor,
            images: self.images.iter().map(strip_temp).collect(),
            chip_elements: self.chip_elements.clone(),
            image_counter: self.image_counter,
            image_undo_stash: self.image_undo_stash.iter().map(strip_temp).collect(),
        }
    }

    pub fn is_effectively_empty(&self) -> bool {
        self.text.trim().is_empty()
            && self.images.is_empty()
            && self.chip_elements.is_empty()
            && self.image_undo_stash.is_empty()
    }

    pub fn into_submission(mut self) -> (String, Vec<PastedImage>, Vec<ChipElement>) {
        (
            std::mem::take(&mut self.text),
            std::mem::take(&mut self.images),
            std::mem::take(&mut self.chip_elements),
        )
    }

    pub fn with_transformed_text(mut self, text: String) -> Self {
        if self.text == text {
            return self;
        }
        let Some(start) = self.text.rfind(&text) else {
            images::drain_and_cleanup(images::SessionPathPolicy::Preserve, &mut self.images);
            images::drain_and_cleanup(
                images::SessionPathPolicy::Preserve,
                &mut self.image_undo_stash,
            );
            self.text = text;
            self.cursor = self.text.len();
            self.chip_elements.clear();
            self.image_counter = 0;
            return self;
        };
        self.retain_transformed_range(text, start)
    }

    pub fn text_without_image_chips(&self) -> String {
        text_without_image_chips(
            &self.text,
            self.chip_elements
                .iter()
                .map(|element| (element.kind, element.range.clone())),
        )
    }

    /// Clear `staged_temp_path` only for images whose identity is still owned elsewhere.
    /// Edit-only pastes keep their paths so Drop / `drain_and_cleanup` can delete them.
    pub fn disarm_image_cleanup(&mut self, retained: &std::collections::HashSet<u64>) {
        for image in &mut self.images {
            if retained.contains(&image.preview.identity()) {
                image.staged_temp_path = None;
            }
        }
        for image in &mut self.image_undo_stash {
            if retained.contains(&image.preview.identity()) {
                image.staged_temp_path = None;
            }
        }
    }

    fn retain_transformed_range(mut self, text: String, start: usize) -> Self {
        let end = start + text.len();
        let mut image_numbers = std::collections::HashSet::new();
        self.chip_elements.retain_mut(|chip| {
            if chip.range.start < start || chip.range.end > end {
                return false;
            }
            chip.range = chip.range.start - start..chip.range.end - start;
            if chip.kind == KIND_IMAGE
                && let Some(number) = text
                    .get(chip.range.clone())
                    .and_then(parse_image_display_number)
            {
                image_numbers.insert(number);
            }
            true
        });
        self.images.retain(|image| {
            if image_numbers.contains(&image.display_number) {
                true
            } else {
                images::cleanup_image(images::SessionPathPolicy::Preserve, image);
                false
            }
        });
        images::drain_and_cleanup(
            images::SessionPathPolicy::Preserve,
            &mut self.image_undo_stash,
        );
        self.text = text;
        self.cursor = self.text.len();
        self.image_counter = images_high_water(&self.images);
        self
    }
}

/// Reusable text prompt component.
pub struct PromptWidget {
    pub textarea: TextArea,
    placeholder: Option<String>,
    textarea_state: TextAreaState,
    /// Cached textarea render area from last draw (for mouse coordinate mapping).
    textarea_area: Rect,
    status_area: Option<Rect>,
    status_hovered: bool,
    /// Whether typing `@` may activate file-reference completion on this surface.
    file_search_enabled: bool,
    /// Pending request to open the line viewer.
    /// Set by accept_file_search_for_viewer / Ctrl-L / : triggers, consumed by AgentView.
    pub(crate) pending_viewer_request: Option<ViewerRequest>,
    /// Prompt history panel state: search mode (`/history`) or browse mode (Up on an empty prompt).
    pub history_search: HistorySearchState,
    history_stash: Option<StashedPrompt>,
    slash: SlashChrome,

    /// Shell command suggestion controller (ghost text + progressive matching).
    pub(crate) suggestions: SuggestionController,

    /// Predicted-next-prompt controller (tab autocomplete ghost text).
    pub(crate) prompt_suggestion: PromptSuggestionController,
    /// Per-frame gate for the prompt-suggestion ghost, set by `AgentView` before each draw or key dispatch.
    /// False while a turn is running, in bash/remember input modes, or while editing a queued prompt.
    pub(crate) prompt_suggestion_active: bool,

    /// Images attached to the current prompt.
    pub images: Vec<PastedImage>,
    /// Images removed during undo that can be restored on redo.
    image_undo_stash: Vec<PastedImage>,
    /// Image element currently hovered by mouse hit-testing.
    hovered_image_element_id: Option<crate::engine::textarea::ElementId>,
    /// Image shown immediately after insertion without moving the edit cursor.
    post_insert_image_preview: Option<(crate::engine::textarea::ElementId, usize)>,
    #[cfg(feature = "inline-image")]
    overlay_image: Option<(
        u64,
        usize,
        u8,
        crate::engine::render::image_overlay::PastedImage,
    )>,
    /// Monotonic counter for image display numbering (1-based).
    /// Reset on prompt clear. Only increases within a single prompt lifetime.
    pub image_counter: usize,

    /// Compact mode lowers the paste-chip threshold from 4 lines to 2.
    compact: bool,

    /// Whether the in-app Cmd+A handler is enabled (gated to Ghostty). Only Ghostty is supported
    /// because. It's the only terminal in current use that can forward `Cmd+A`, needing just one
    /// documented terminal-side step (`keybind = cmd+a=unbind`).
    cmd_a_select_all_enabled: bool,
}

/// Prefix display width (`"❯ "` or `"> "`, both 2 columns).
const PREFIX_WIDTH: u16 = crate::engine::render::glyphs::PROMPT_ARROW_WIDTH;

impl PromptWidget {
    /// Create a new empty prompt widget with system clipboard support.
    pub fn new() -> Self {
        let theme = Theme::current();
        let mut textarea = TextArea::new();
        textarea.set_clipboard_provider(Box::new(SystemClipboard));
        // The prompt has a light bg, so the scrollbar track blends with it; the thumb uses the darker base bg for contrast
        textarea.scrollbar_track_style = Style::default().bg(theme.bg_base);
        textarea.scrollbar_thumb_style = Style::default()
            .fg(theme.selection_border)
            .bg(theme.bg_base);
        textarea.scrollbar_padding = 1;
        Self {
            textarea,
            placeholder: None,
            textarea_state: TextAreaState::default(),
            textarea_area: Rect::default(),
            status_area: None,
            status_hovered: false,
            file_search_enabled: true,
            pending_viewer_request: None,
            history_search: HistorySearchState::new(),
            history_stash: None,
            slash: SlashChrome::default(),
            suggestions: SuggestionController::new(),
            prompt_suggestion: PromptSuggestionController::new(),
            prompt_suggestion_active: false,
            images: Vec::new(),
            image_undo_stash: Vec::new(),
            hovered_image_element_id: None,
            post_insert_image_preview: None,
            #[cfg(feature = "inline-image")]
            overlay_image: None,
            image_counter: 0,
            compact: false,
            cmd_a_select_all_enabled: cmd_a_select_all_supported(
                crate::engine::terminal::terminal_context().brand,
            ),
        }
    }

    /// Set the ghost text for shell command suggestions.
    ///
    /// Source is set to [`SuggestionSource::None`]. Use `suggestions.set_ghost()` directly for source-aware callers.
    pub(crate) fn set_placeholder(&mut self, placeholder: String) {
        self.placeholder = Some(placeholder);
    }

    pub fn set_ghost_text(&mut self, text: Option<String>) {
        match text {
            Some(t) if !t.is_empty() => {
                self.suggestions.set_ghost(t, SuggestionSource::None);
            }
            _ => self.suggestions.clear_ghost(),
        }
    }

    /// Whether non-empty ghost text is set and would render.
    pub fn has_ghost_text(&self) -> bool {
        self.suggestions.has_ghost()
    }

    /// Accept ghost text (full or one word). Appends the accepted portion to the textarea.
    /// Returns `None` if no ghost is active or the cursor is not at end-of-text.
    pub fn accept_ghost(&mut self, mode: AcceptMode) -> Option<String> {
        if self.cursor() != self.text().len() {
            return None;
        }
        let accepted = self.suggestions.accept_ghost(mode)?;
        self.textarea.insert_str(&accepted);
        self.update_file_search_context();
        Some(accepted)
    }

    /// Dismiss ghost text without accepting it.
    pub fn clear_ghost(&mut self) {
        self.suggestions.clear_ghost();
    }

    /// The prompt-suggestion ghost to render for the current text, if the per-frame gate is open and no other completion UI owns the row.
    /// Requires the cursor at end-of-text so the ghost visually continues the typed text.
    pub fn prompt_suggestion_ghost(&self) -> Option<&str> {
        // Cheap checks first: most frames have no suggestion loaded
        if !self.prompt_suggestion_active
            || !self.prompt_suggestion.has_suggestion()
            || self.suggestions.has_ghost()
            || self.completion_dropdown_open()
            || self.history_search.is_active()
            || self.cursor() != self.text().len()
        {
            return None;
        }
        let slash = &self.slash;
        if slash.active || slash.inline_ghost.is_some() {
            return None;
        }
        self.prompt_suggestion.ghost_for(self.text())
    }

    /// Whether the prompt-suggestion ghost is currently visible (drives the key intercept and the shortcuts-bar hint).
    pub fn prompt_suggestion_visible(&self) -> bool {
        self.prompt_suggestion_ghost().is_some()
    }

    /// Accept the prompt suggestion: insert the remainder at end-of-text and move the cursor after it.
    /// Returns `true` when text was inserted.
    pub fn accept_prompt_suggestion(&mut self) -> bool {
        if self.prompt_suggestion_ghost().is_none() {
            return false;
        }
        let text = self.text().to_owned();
        let Some(rest) = self.prompt_suggestion.accept(&text) else {
            return false;
        };
        // Accepting rewrites the token; an unrelated highlight must not survive it
        self.textarea.clear_selection();
        self.textarea.insert_str(&rest);
        self.update_file_search_context();
        true
    }

    /// Try progressive matching against the new text.
    /// Returns `true` if the ghost was trimmed (no network request needed).
    pub fn try_progressive_match(&mut self, new_text: &str) -> bool {
        self.suggestions.try_progressive_match(new_text)
    }

    /// Whether the completion dropdown is currently open.
    pub fn completion_dropdown_open(&self) -> bool {
        self.suggestions.dropdown.open
    }

    /// An overlay gated above the prompt (e.g. the ephemeral tip) must therefore treat an open dropdown
    /// as covering it.
    pub fn any_dropdown_open(&self) -> bool {
        self.completion_dropdown_open()
            || self.slash_open()
            || self.completion_dropdown_open()
            || self.history_search.is_active()
    }

    /// Open the completion dropdown if items are available.
    pub fn completion_dropdown_open_if_available(&mut self) -> bool {
        if !self.suggestions.dropdown.items.is_empty() {
            self.suggestions.dropdown.open = true;
            true
        } else {
            false
        }
    }

    /// Close the completion dropdown.
    pub fn completion_dropdown_close(&mut self) {
        self.suggestions.dropdown.close();
    }

    /// Move the completion dropdown selection (Up = -1, Down = +1), wrapping.
    pub fn completion_dropdown_move(&mut self, delta: isize) {
        self.suggestions.dropdown.move_selection(delta);
    }

    /// Scroll the completion dropdown selection by `delta`, clamping at the ends (no wrap-around).
    /// Used for mouse-wheel and page-key scrolling.
    pub fn completion_dropdown_scroll(&mut self, delta: isize) {
        self.suggestions.dropdown.scroll_selection(delta);
    }

    /// Accept the selected completion, resolved against the current draft.
    /// Stale generations refuse and close; see [`SuggestionController::accept_completion`].
    /// Also clears ghost text since the accepted completion replaces it.
    pub fn completion_dropdown_accept(&mut self) -> Option<CompletionSplice> {
        let result = self.suggestions.accept_completion(self.textarea.text());
        if result.is_some() {
            self.suggestions.clear_ghost();
        }
        result
    }

    /// Write a resolved accept into the textarea.
    /// Returns whether text was written (`Stale` is a draft-preserving no-op); the cursor lands after the insert.
    pub fn apply_completion_splice(&mut self, splice: CompletionSplice) -> bool {
        // Accepting rewrites the token; an unrelated highlight must not survive it
        self.textarea.clear_selection();
        match splice {
            CompletionSplice::WholeLine(line) => {
                self.set_text(&line);
                self.set_cursor(line.len());
                true
            }
            CompletionSplice::Token(range, replacement) => {
                if self.completion_range_clips_element(&range) {
                    return false;
                }
                let start = range.start;
                self.textarea.replace_range(range, &replacement);
                self.textarea.set_cursor(start + replacement.len());
                self.update_file_search_context();
                true
            }
            CompletionSplice::Stale => false,
        }
    }

    /// Write a `TabAction::Fill` over the typed token (bash's first Tab). The caller must NOT treat a
    /// declined fill like typing (no re-fetch: that loop would spin on every Tab).
    pub fn apply_completion_fill(&mut self, range: std::ops::Range<usize>, fill: &str) -> bool {
        if self.completion_range_clips_element(&range) {
            return false;
        }
        let start = range.start;
        self.textarea.replace_range(range, fill);
        self.textarea.set_cursor(start + fill.len());
        self.update_file_search_context();
        true
    }

    /// Whether accepting the selected completion would splice a range that clips an atomic element.
    /// The write path would decline it AFTER the accept consumed the item, so callers probe first and degrade to opening the dropdown instead.
    pub fn completion_accept_would_clip_element(&self) -> bool {
        match self
            .suggestions
            .peek_completion_splice(self.textarea.text())
        {
            Some(CompletionSplice::Token(range, _)) => self.completion_range_clips_element(&range),
            _ => false,
        }
    }

    /// Whether a completion range overlaps an atomic prompt element (paste chip, file ref, image chip).
    /// Completion ranges are computed over the plain request text, but `TextArea::replace_range` expands any element overlap to the WHOLE element.
    /// Accepting such a range would swallow the entire chip (paste data loss). Treated as a stale no-op.
    fn completion_range_clips_element(&self, range: &std::ops::Range<usize>) -> bool {
        self.textarea
            .elements()
            .iter()
            .any(|e| e.range.start < range.end && e.range.end > range.start)
    }

    /// Set the hovered completion dropdown item. Returns `true` if changed.
    pub fn set_completion_hovered(&mut self, index: Option<usize>) -> bool {
        let clamped = index.filter(|&i| i < self.suggestions.dropdown.items.len());
        let changed = clamped != self.suggestions.dropdown.hovered;
        self.suggestions.dropdown.hovered = clamped;
        changed
    }

    /// Select the hovered completion item (keyboard selection follows the hover).
    pub fn select_completion_hovered(&mut self) -> bool {
        if let Some(idx) = self.suggestions.dropdown.hovered {
            self.suggestions.dropdown.selected = idx;
            true
        } else {
            false
        }
    }

    /// Get the current text.
    pub fn text(&self) -> &str {
        self.textarea.text()
    }

    /// Nothing recoverable remains: text, live images, chips, or Ctrl+U undo stash.
    /// Matches [`StashedPrompt::is_effectively_empty`].
    pub fn is_effectively_empty(&self) -> bool {
        self.text().trim().is_empty()
            && self.images.is_empty()
            && self.textarea.elements().is_empty()
            && self.image_undo_stash.is_empty()
    }

    /// Get the current cursor position (byte offset into text).
    pub fn cursor(&self) -> usize {
        self.textarea.cursor()
    }

    /// Active selection as a byte range (normalized, char-boundary safe), or `None` when nothing is selected.
    pub fn selection_range(&self) -> Option<std::ops::Range<usize>> {
        self.textarea.selection_range()
    }

    /// Clear the undo/redo history (see [`TextArea::clear_history`]).
    /// Pair with `set_text("")` when reusing the widget for a new logical context so an undo can't resurrect the previous draft.
    pub fn clear_history(&mut self) {
        self.textarea.clear_history();
    }

    /// Set the cursor position (byte offset), clamped to text length.
    pub fn set_cursor(&mut self, pos: usize) {
        self.post_insert_image_preview = None;
        self.textarea.set_cursor(pos);
    }

    /// Move the current prompt state into a snapshot for later restoration.
    pub fn stash(&mut self) -> StashedPrompt {
        // Bind orphan placeholders first so the chip snapshot and the drained images agree.
        self.rebind_image_placeholders();
        let chip_elements = self
            .textarea
            .elements()
            .iter()
            .map(|element| ChipElement {
                range: element.range.clone(),
                kind: element.kind,
                display: element.display.clone(),
            })
            .collect();
        let images = self.drain_images();
        StashedPrompt {
            text: self.text().to_owned(),
            cursor: self.cursor(),
            images,
            chip_elements,
            image_counter: self.image_counter,
            image_undo_stash: std::mem::take(&mut self.image_undo_stash),
        }
    }

    /// Restore a previous [`StashedPrompt`], including image ownership.
    pub fn restore(&mut self, mut stash: StashedPrompt) {
        let text = std::mem::take(&mut stash.text);
        let cursor = stash.cursor;
        let images = std::mem::take(&mut stash.images);
        let chip_elements = std::mem::take(&mut stash.chip_elements);
        let image_counter = stash.image_counter;
        let image_undo_stash = std::mem::take(&mut stash.image_undo_stash);
        if text.is_empty() {
            self.set_text("");
        } else {
            images::drain_and_cleanup(images::SessionPathPolicy::Preserve, &mut self.images);
            images::drain_and_cleanup(
                images::SessionPathPolicy::Preserve,
                &mut self.image_undo_stash,
            );
            images::reset_counter(&mut self.image_counter);
            self.set_text(&text);
        }
        self.restore_chip_elements(&chip_elements);
        self.set_images(images);
        self.image_counter = self.image_counter.max(image_counter);
        self.image_undo_stash = image_undo_stash;
        self.rechip_orphan_image_placeholders();
        self.set_cursor(cursor);
        self.update_file_search_context();
    }

    /// [`Self::set_text`] for text that is not this draft's: a recalled history line or a rewound
    /// prompt. Its `[Image #N]` placeholders name images from another send, so the records this
    /// composer still holds are released first instead of re-binding to a matching number.
    pub fn set_text_discarding_images(&mut self, text: &str) {
        images::drain_and_cleanup(images::SessionPathPolicy::Preserve, &mut self.images);
        images::drain_and_cleanup(
            images::SessionPathPolicy::Preserve,
            &mut self.image_undo_stash,
        );
        images::reset_counter(&mut self.image_counter);
        self.set_text(text);
    }

    /// Set the text content. A non-empty `text` that still names `[Image #N]` placeholders keeps the
    /// `PastedImage` records, unbound until the next sync, `set_images`, or `stash` re-chips them.
    /// No re-chip happens here: callers that restore an in-flight prompt follow with
    /// `restore_chip_elements` and `set_images`, and a re-chip first would bind a stale undo-stash
    /// record ahead of the restored one.
    pub fn set_text(&mut self, text: &str) {
        self.post_insert_image_preview = None;
        self.hovered_image_element_id = None;
        if text.is_empty() {
            // Revert any in-progress preview while the command text is still in the textarea (parse_invocation needs it)
        }
        self.textarea.set_text(text);
        // Suggestion state (ghost, dropdown items, pending fetches) is tied to a SPECIFIC draft
        // Every wholesale content swap (empty or not) invalidates it, so nothing stale survives into (or lands over) the new text
        self.suggestions.invalidate_draft();
        if text.is_empty() {
            // Split "drain Vec" from "reset counter" so the counter-reset is a single explicit line, not two redundant writes hidden in `clear()`
            images::drain_and_cleanup(images::SessionPathPolicy::Preserve, &mut self.images);
            images::drain_and_cleanup(
                images::SessionPathPolicy::Preserve,
                &mut self.image_undo_stash,
            );
            images::reset_counter(&mut self.image_counter);
            self.slash_close();
            // Safety net: ensure preview state is cleared even if slash_cancel_preview was a no-op (e.g. command not found after registry changes).
        } else if !chip_placeholder_regex().is_match(text) {
            // Non-empty replacement with no chip placeholder match: any retained `PastedImage` records are now orphans (no chip in the buffer references them)
            // Drop them to keep `drain_images()` honest and to release encoded bytes
            // The counter is reset since no chips survive
            images::drain_and_cleanup(images::SessionPathPolicy::Preserve, &mut self.images);
            images::drain_and_cleanup(
                images::SessionPathPolicy::Preserve,
                &mut self.image_undo_stash,
            );
            images::reset_counter(&mut self.image_counter);
        }
        self.update_file_search_context();
    }

    /// Disable `@` file references for a composer that does not render or drain file-search UI.
    pub fn disable_file_search(&mut self) {
        self.file_search_enabled = false;
        self.completion_dropdown_close();
        self.pending_viewer_request = None;
    }

    /// [`Self::set_text`] unless the buffer already holds exactly `text`. Skipping the no-op swap keeps
    /// chip elements, images, and undo history intact when a view reloads the same draft (the question
    /// view's freeform slots). Any real content change takes the normal reset path.
    pub fn set_text_preserving(&mut self, text: &str) {
        if self.text() != text {
            self.set_text(text);
        }
    }

    /// Append plain text at the end without replacing existing chip elements.
    pub fn append_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.post_insert_image_preview = None;
        self.hovered_image_element_id = None;
        let end = self.textarea.text().len();
        self.set_cursor(end);
        self.textarea.insert_str(text);
        self.update_file_search_context();
    }

    /// Whether the slash dropdown is currently open.
    pub fn slash_open(&self) -> bool {
        self.slash.open
    }

    /// Close the slash dropdown.
    pub fn slash_close(&mut self) {
        self.slash = SlashChrome::default();
    }

    /// Whether the cursor is at a file ref element boundary (for shortcuts bar hint).
    ///
    /// Matches the same positions where `:` or Ctrl-L would trigger: at element start, at element end, but NOT after the trailing space.
    pub fn file_ref_near_cursor(&self) -> bool {
        let cursor = self.textarea.cursor();
        self.textarea
            .elements()
            .iter()
            .any(|e| e.kind == KIND_FILE_REF && (cursor == e.range.start || cursor == e.range.end))
    }

    /// Parse a file ref element into (path, optional line range). Element text is `@path` or `@path:10`
    /// or `@path:10-12`. Returns `(path, None)` or `(path, Some(10..11))` or `(path, Some(10..13))`.
    fn parse_file_ref_element(text: &str) -> (String, Option<std::ops::Range<usize>>) {
        let text = text.strip_prefix('@').unwrap_or(text);
        if let Some(colon_pos) = text.rfind(':') {
            let Some(path) = text.get(..colon_pos) else {
                return (text.to_owned(), None);
            };
            let Some(range_str) = text.get(colon_pos + 1..) else {
                return (text.to_owned(), None);
            };
            if let Some(range) = parse_line_range(range_str) {
                return (path.to_owned(), Some(range));
            }
        }
        (text.to_owned(), None)
    }

    /// Find a file ref element when cursor is on or immediately after it. Used by. CtrlCtrl-L: works when
    /// the cursor is at element start, end, or the position right after element end (e.g., after the
    /// trailing space). Returns `(path, optional_line_range)`.
    pub fn file_ref_element_at_cursor(&self) -> Option<(String, Option<std::ops::Range<usize>>)> {
        let cursor = self.textarea.cursor();
        for elem in self.textarea.elements() {
            if elem.kind != KIND_FILE_REF {
                continue;
            }
            if cursor >= elem.range.start && cursor <= elem.range.end + 1 {
                let text = self.textarea.get_range(elem.range.clone())?;
                return Some(Self::parse_file_ref_element(text));
            }
        }
        None
    }

    /// Find a file ref element when cursor is at an element boundary. Used by the ':' trigger: fires
    /// when the cursor is at element.range.start (on the `@` char) or element.range.end (right after
    /// the element). Returns `(path, optional_line_range)`.
    fn file_ref_at_element_boundary(&self) -> Option<(String, Option<std::ops::Range<usize>>)> {
        let cursor = self.textarea.cursor();
        for elem in self.textarea.elements() {
            if elem.kind != KIND_FILE_REF {
                continue;
            }
            if cursor == elem.range.end || cursor == elem.range.start {
                let text = self.textarea.get_range(elem.range.clone())?;
                return Some(Self::parse_file_ref_element(text));
            }
        }
        None
    }

    /// Update file search context from current text and cursor position. Suppresses the context if the
    /// cursor is on or immediately after a file reference element. The `@` belongs to the atomic block
    /// and the dropdown can't edit it.
    fn update_file_search_context(&mut self) {
        let dropdown = &mut self.suggestions.dropdown;
        if dropdown.request_text != self.textarea.text()
            || dropdown.request_cursor != self.textarea.cursor()
        {
            dropdown.close();
            self.slash = SlashChrome::default();
        }
    }

    /// Return the active command or file token, in textarea byte coordinates.
    pub fn file_context(&self, directory: Option<&str>) -> Option<file_context::AtContext> {
        if !self.file_search_enabled {
            return None;
        }
        let context = file_context::detect_with_drill(self.text(), self.cursor(), directory)?;
        (!self.completion_range_clips_element(&context.range)).then_some(context)
    }

    pub fn completion_token(&self) -> Option<std::ops::Range<usize>> {
        if let Some(context) = self.file_context(None) {
            return Some(context.range);
        }
        let text = self.textarea.text();
        let cursor = self.cursor();
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
        let start = text[..cursor].rfind(is_boundary).map_or(0, |i| {
            i + text[i..].chars().next().map_or(0, char::len_utf8)
        });
        let end = text[cursor..]
            .find(is_boundary)
            .map_or(text.len(), |i| i + cursor);
        let token = text.get(start..end)?;
        let eligible = token.starts_with('/') || token.starts_with('$');
        (eligible && !self.completion_range_clips_element(&(start..end))).then_some(start..end)
    }

    /// Install completion rows for the current draft and cursor.
    pub fn set_completions(
        &mut self,
        items: Vec<super::suggestion_controller::CompletionItemParsed>,
    ) {
        self.suggestions.dropdown = super::suggestion_controller::CompletionDropdownState {
            open: !items.is_empty(),
            items,
            request_text: self.text().to_owned(),
            request_cursor: self.cursor(),
            generation: self.suggestions.generation(),
            ..Default::default()
        };
    }

    pub fn handle_completion_mouse(
        &mut self,
        mouse: &crossterm::event::MouseEvent,
        chrome: &DropdownChrome,
    ) -> bool {
        let state = self.completion_dropdown();
        if !state.open {
            return false;
        }
        match chrome.handle_mouse(
            mouse,
            state.items.len(),
            scroll_offset_for(state, chrome.items.height),
        ) {
            DropdownMouse::Hover(hit) => self.set_completion_hovered(hit),
            DropdownMouse::Select(index) => {
                self.suggestions.dropdown.selected = index;
                self.accept_completion()
            }
            DropdownMouse::Scroll(delta) => {
                self.completion_dropdown_scroll(delta);
                true
            }
            DropdownMouse::Unhandled => false,
        }
    }

    pub fn open_history(&mut self, entries: &[HistoryEntry], browse: bool) -> bool {
        if self.history_search.is_active() {
            return false;
        }
        let active = if browse {
            self.history_search
                .activate_browse(entries, self.textarea.text())
        } else {
            self.history_search.activate(entries, self.textarea.text())
        };
        if active {
            self.history_stash = Some(self.stash());
            self.completion_dropdown_close();
        }
        active
    }

    pub fn poll_history(&mut self) -> bool {
        let changed = self.history_search.poll();
        if changed && self.history_search.is_browse() {
            self.populate_history_selection();
        }
        changed
    }

    fn populate_history_selection(&mut self) {
        if let Some(text) = self.history_search.selected_text().map(str::to_owned) {
            self.set_text_discarding_images(&text);
            self.textarea.set_cursor(text.len());
        }
    }

    fn finish_history(&mut self, accept: bool) {
        let selection = accept
            .then(|| self.history_search.selected_text().map(str::to_owned))
            .flatten();
        self.history_search.deactivate();
        let stash = self.history_stash.take();
        if let Some(text) = selection {
            self.set_text_discarding_images(&text);
            self.textarea.set_cursor(text.len());
        } else if let Some(stash) = stash {
            self.restore(stash);
        }
    }

    fn handle_history_search_key(&mut self, key: &KeyEvent) -> PromptEvent {
        let browse = self.history_search.is_browse();
        if key!(Esc).matches(key)
            || key!('c', CONTROL).matches(key)
            || key!('r', CONTROL).matches(key)
        {
            self.finish_history(false);
        } else if key!(Enter).matches(key) || key!(Tab).matches(key) {
            self.finish_history(true);
        } else if key!(Up).matches(key)
            || key!('p', CONTROL).matches(key)
            || key!('k', CONTROL).matches(key)
        {
            if self.history_search.move_up() && browse {
                self.populate_history_selection();
            }
        } else if key!(Down).matches(key)
            || key!('n', CONTROL).matches(key)
            || key!('j', CONTROL).matches(key)
        {
            if self.history_search.move_down() {
                if browse {
                    self.populate_history_selection();
                }
            } else {
                self.finish_history(false);
            }
        } else if key!(PageUp).matches(key) || key!('u', CONTROL).matches(key) {
            self.history_search.page_move(-1, 8);
            if browse {
                self.populate_history_selection();
            }
        } else if key!(PageDown).matches(key) || key!('d', CONTROL).matches(key) {
            self.history_search.page_move(1, 8);
            if browse {
                self.populate_history_selection();
            }
        } else if browse {
            self.history_search.deactivate();
            self.history_stash = None;
            return self.handle_key_inner(key);
        } else {
            let event = self.handle_key_inner(key);
            self.history_search.update_query(self.textarea.text());
            return event;
        }
        PromptEvent::Edited
    }

    pub fn handle_history_mouse(
        &mut self,
        mouse: &crossterm::event::MouseEvent,
        chrome: &DropdownChrome,
    ) -> bool {
        if !self.history_search.is_active() {
            return false;
        }
        let state = &mut self.history_search;
        match chrome.handle_mouse(
            mouse,
            state.result_count(),
            history_scroll_offset(state, chrome.items.height),
        ) {
            DropdownMouse::Hover(hit) => state.set_hovered(hit),
            DropdownMouse::Select(index) => {
                state.selected = index;
                self.finish_history(true);
                true
            }
            DropdownMouse::Scroll(delta) => {
                if delta < 0 {
                    state.move_up();
                } else {
                    state.move_down();
                }
                if state.is_browse() {
                    self.populate_history_selection();
                }
                true
            }
            DropdownMouse::Unhandled => false,
        }
    }

    pub fn completion_dropdown(&self) -> &super::suggestion_controller::CompletionDropdownState {
        &self.suggestions.dropdown
    }

    pub fn set_slash_chrome(&mut self, chrome: SlashChrome) {
        self.slash = chrome;
    }

    pub fn take_viewer_request(&mut self) -> Option<ViewerRequest> {
        self.pending_viewer_request.take()
    }

    pub fn dismiss_prompt_suggestion(&mut self) {
        self.prompt_suggestion.dismiss();
    }

    pub fn reset_images(&mut self) {
        images::clear(&mut self.images, &mut self.image_counter);
    }

    pub fn set_prompt_suggestion(&mut self, suggestion: Option<String>) {
        self.prompt_suggestion.enabled = true;
        self.prompt_suggestion_active = true;
        let generation = self.prompt_suggestion.begin_fetch();
        self.prompt_suggestion.on_loaded(suggestion, generation);
    }

    /// How tall this widget wants to be, given the available width of the full prompt area (including
    /// chrome if enabled). Height = vpad_top + textarea rows + vpad_info(1) + info line. Clamped to
    /// max_height.
    pub fn desired_height(
        &self,
        area_width: u16,
        style: &PromptStyle,
        has_info: bool,
        max_height: u16,
    ) -> u16 {
        let content_width = self.content_width(area_width, style);
        let prefix_w = if style.show_prefix { PREFIX_WIDTH } else { 0 };
        let text_width = content_width.saturating_sub(prefix_w);
        // History browse live-populates the composer per selection move
        // Freeze the box at its pre-open one-row height so stepping through entries of different heights doesn't resize the layout per keypress
        // The first real edit detaches and the box resizes then
        let text_height = if self.history_search.is_browse() {
            1
        } else {
            self.textarea.desired_height(text_width).max(1)
        };
        let vpad_top = style.vpad_top;
        let info_block = style.info_block(has_info);
        let total = vpad_top + text_height + info_block;
        let min = vpad_top + 1 + info_block;
        total.clamp(min.min(max_height), max_height)
    }

    /// Current vertical scroll offset of the textarea.
    pub fn scroll(&self) -> u16 {
        self.textarea_state.scroll
    }

    /// Set the vertical scroll offset (e.g., restore after collapsed render).
    pub fn set_scroll(&mut self, scroll: u16) {
        self.textarea_state.scroll = scroll;
    }

    /// Last rendered textarea area for mouse hit-testing.
    pub fn status_area(&self) -> Option<Rect> {
        self.status_area
    }

    pub fn textarea_area(&self) -> Rect {
        self.textarea_area
    }

    /// Whether the caret sits on the first visual (wrap-aware) row of the text, per the last draw's
    /// textarea rect and scroll.
    pub fn caret_on_top_visual_row(&self) -> bool {
        let area = self.textarea_area;
        if area.height == 0 {
            return false;
        }
        // An empty draft has exactly one (top) row; its wrap ranges may be empty.
        if self.textarea.text().is_empty() {
            return true;
        }
        let caret = self
            .textarea
            .cursor_pos_with_state(area, self.textarea_state);
        let top = self
            .textarea
            .screen_position_of(0, area, self.textarea_state);
        match (caret, top) {
            (Some((_, caret_row)), Some((_, top_row))) => caret_row == top_row,
            // Caret or text top scrolled out of view: Up must move/scroll within the text.
            _ => false,
        }
    }

    /// Compute the content width inside the chrome (if any).
    fn content_width(&self, area_width: u16, style: &PromptStyle) -> u16 {
        if style.chrome {
            let accent_w = if style.show_accent_line { 1u16 } else { 0 };
            area_width.saturating_sub(accent_w + style.chrome_pad_left + style.chrome_pad_right)
        } else {
            area_width
        }
    }
}

impl PromptWidget {
    /// Route completion keys before handing ordinary editing to the textarea.
    pub fn handle_key(&mut self, key: &KeyEvent) -> PromptEvent {
        self.post_insert_image_preview = None;
        if self.history_search.is_active() {
            return self.handle_history_search_key(key);
        }
        if key!('r', CONTROL).matches(key) {
            return PromptEvent::SearchHistory;
        }
        if self.completion_dropdown_open() && key.modifiers.is_empty() {
            match key.code {
                KeyCode::Up => self.completion_dropdown_move(-1),
                KeyCode::Down => self.completion_dropdown_move(1),
                KeyCode::PageUp => self.completion_dropdown_scroll(-6),
                KeyCode::PageDown => self.completion_dropdown_scroll(6),
                KeyCode::Esc => self.completion_dropdown_close(),
                KeyCode::Enter | KeyCode::Tab => {
                    self.accept_completion();
                }
                _ => return self.handle_key_inner(key),
            }
            return PromptEvent::Edited;
        }
        self.handle_key_inner(key)
    }

    pub fn accept_completion(&mut self) -> bool {
        if self.completion_accept_would_clip_element() {
            return false;
        }
        let file_context = self.file_context(None);
        let Some(splice) = self.completion_dropdown_accept() else {
            return false;
        };
        match (splice, file_context) {
            (CompletionSplice::Token(range, replacement), Some(context))
                if range == context.range && replacement.starts_with('@') =>
            {
                if replacement.trim_end().ends_with('/') {
                    return self
                        .apply_completion_splice(CompletionSplice::Token(range, replacement));
                }
                let path_end = replacement.trim_end_matches(' ').len();
                let Some(path) = replacement.get(1..path_end) else {
                    return false;
                };
                if path.is_empty() {
                    return self
                        .apply_completion_splice(CompletionSplice::Token(range, replacement));
                }
                let display = file_ref_display(file_context::normalize_display_path(path));
                let start = range.start;
                self.textarea.begin_undo_group();
                self.textarea.replace_range_with_element(
                    range,
                    &replacement[..path_end],
                    KIND_FILE_REF,
                    Some(display),
                );
                self.textarea.set_cursor(start + path_end);
                self.textarea.insert_str(&replacement[path_end..]);
                self.textarea.end_undo_group();
                self.update_file_search_context();
                true
            }
            (other, _) => self.apply_completion_splice(other),
        }
    }

    fn handle_key_inner(&mut self, key: &KeyEvent) -> PromptEvent {
        // Esc/Tab: drop the highlight but decline the press so it still cancels or switches focus
        // Modified Esc has no structural consumer; it falls through to the textarea catch-all, which clears the highlight WITH a repaint
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab)
            || (key.code == KeyCode::Esc && key.modifiers.is_empty())
        {
            self.textarea.clear_selection();
            return PromptEvent::Ignored;
        }

        // Ctrl-L when the cursor is on or adjacent to a file ref element, or ':' typed right at an element boundary, opens the viewer
        if key!('l', CONTROL).matches(key)
            && let Some((path, initial_range)) = self.file_ref_element_at_cursor()
        {
            self.pending_viewer_request = Some(ViewerRequest {
                path: std::path::PathBuf::from(path),
                initial_range,
            });
            self.textarea.begin_undo_group();
            return PromptEvent::Edited;
        }
        if key!(':').matches(key)
            && let Some((path, initial_range)) = self.file_ref_at_element_boundary()
        {
            self.pending_viewer_request = Some(ViewerRequest {
                path: std::path::PathBuf::from(path),
                initial_range,
            });
            self.textarea.begin_undo_group();
            return PromptEvent::Edited;
        }
        // Not at an element boundary: fall through to type ':' normally

        // Newline: Shift/Alt+Enter, Apple Terminal CoreGraphics rescue (inside is_mod_enter),
        // or a delivered SUPER+Enter (Kitty). SUPER is excluded from is_mod_enter (fullscreen
        // on many terminals) and from bare-Enter send; insert here instead of textarea fallthrough.
        if crate::engine::input::is_mod_enter(key)
            || crate::engine::input::is_delivered_super_enter(key)
        {
            self.insert_replacing_selection("\n");
            return PromptEvent::Edited;
        }

        // Clear: Ctrl-C (empty yields Ignored so the caller can handle)
        if key!('c', CONTROL).matches(key) {
            return if self.textarea.text().is_empty() {
                PromptEvent::Ignored
            } else {
                self.set_text("");
                PromptEvent::Edited
            };
        }

        // Ctrl-V / Cmd-V: intercept to route through handle_paste() for element creation
        // Without this, textarea.input() would insert multi-line clipboard content inline instead of creating a paste element
        // Ghostty passes Cmd+V (SUPER) through as a key event when the clipboard has no text content
        if crate::engine::input::key::is_paste_key(key) {
            if let Some(text) = system_clipboard_get() {
                // handle_paste calls update_file_search_context internally.
                return self.handle_paste(&text);
            }
            return PromptEvent::Ignored;
        }

        // Ctrl-Shift-V: inline paste, inserting clipboard content directly without creating a [Pasted: N lines] element
        if crate::engine::input::key::is_inline_paste_key(key) {
            if let Some(text) = system_clipboard_get() {
                let text = normalize_line_breaks(&text);
                if text.is_empty() {
                    return PromptEvent::Ignored;
                }
                self.insert_replacing_selection(&text);
                return PromptEvent::Edited;
            }
            return PromptEvent::Ignored;
        }

        // Cmd+A (macOS, Ghostty only): select all text in the prompt textarea. Gated to Ghostty. Every
        // other macOS terminal binds `Cmd+A` to its own in-terminal "Select All" by default. We don't want
        // to compete with that, or surprise users on terminals where the key doesn't even arrive.
        if self.cmd_a_select_all_enabled && key!('a', SUPER).matches(key) {
            let len = self.textarea.text().len();
            if len == 0 {
                return PromptEvent::Ignored;
            }
            self.textarea.set_selection(0, len);
            // Move the cursor to the end of the selection so the visible caret matches the typical GUI "head at end" convention after Cmd+A
            self.textarea.set_cursor(len);
            return PromptEvent::Edited;
        }

        // Everything else: delegate to textarea.
        // Selection is rendered state: selection-only changes must report Edited or no frame is drawn.
        let old_text = self.textarea.text().to_owned();
        let old_cursor = self.textarea.cursor();
        let old_selection = self.textarea.selection_range();
        self.textarea.input(*key);
        let new_cursor = self.textarea.cursor();
        let new_selection = self.textarea.selection_range();
        let changed = self.textarea.text() != old_text
            || new_cursor != old_cursor
            || new_selection != old_selection;
        if changed {
            // Image chips only change with the text, so skip the resync on selection/cursor-only changes (held Shift+arrow extends)
            if self.textarea.text() != old_text {
                self.sync_images_with_textarea();
            }
            self.update_file_search_context();
            PromptEvent::Edited
        } else {
            PromptEvent::Ignored
        }
    }

    /// If the character immediately before the cursor is `\`, replace it with a newline and return `true` (continuation applied).
    /// Returns `false` when no continuation is needed.
    fn apply_backslash_continuation(&mut self) -> bool {
        let cursor = self.textarea.cursor();
        if cursor > 0 && self.textarea.text().as_bytes().get(cursor - 1) == Some(&b'\\') {
            self.textarea.delete_backward(1);
            self.textarea.insert_str("\n");
            return true;
        }
        false
    }

    /// Shared Enter-routing for any context where bare Enter submits. Apple Terminal CoreGraphics
    /// modifier rescue (Shift+Enter arrives as bare Enter on terminals without the Kitty protocol).
    pub fn route_enter(&mut self, key: &KeyEvent) -> EnterOutcome {
        self.route_enter_with_newline_modifier(
            key,
            crate::engine::input::is_apple_terminal_newline_modifier_held(),
        )
    }

    pub fn route_enter_with_newline_modifier(
        &mut self,
        key: &KeyEvent,
        is_newline_modifier_held: bool,
    ) -> EnterOutcome {
        // The file-search dropdown is open, so Enter accepts the selection, not submit. Let handle_key() deal with it.
        if self.completion_dropdown_open() || self.history_search.is_active() {
            return EnterOutcome::PassThrough;
        }

        // Apple Terminal: Shift+Enter arrives as bare Enter because there's no Kitty keyboard protocol.
        if key.code == KeyCode::Enter && is_newline_modifier_held {
            self.insert_replacing_selection("\n");
            return EnterOutcome::NewlineInserted;
        }

        // Backslash continuation: if the character before the cursor is `\`, replace it with a newline
        if key.code == KeyCode::Enter
            && key.modifiers.is_empty()
            && self.apply_backslash_continuation()
        {
            return EnterOutcome::NewlineInserted;
        }

        // Bare Enter submits
        if key.code == KeyCode::Enter && key.modifiers.is_empty() {
            return EnterOutcome::Submit;
        }

        // Not Enter, or Enter with modifiers (Shift/Alt): let handle_key() process it (Shift+Enter inserts a newline there)
        EnterOutcome::PassThrough
    }

    /// Attempt to send the prompt text. Empty/whitespace rejection: returns None.
    pub fn try_send(&mut self) -> Option<String> {
        if self.apply_backslash_continuation() {
            return None;
        }

        let text = self.textarea.text().to_string();

        // Reject empty/whitespace-only
        if text.trim().is_empty() {
            return None;
        }

        Some(text)
    }

    /// Whether the prompt is in a state where send would succeed.
    ///
    /// Used for dynamic hint visibility (show "Enter:send" only when sendable).
    pub fn can_send(&self) -> bool {
        let text = self.textarea.text();
        if text.trim().is_empty() {
            return false;
        }
        // Check backslash continuation: a trailing `\` means send would do continuation, not send
        let cursor = self.textarea.cursor();
        !(cursor > 0 && text.as_bytes().get(cursor - 1) == Some(&b'\\'))
    }

    /// Handle a mouse event.
    pub fn handle_mouse(&mut self, mouse: &crossterm::event::MouseEvent) -> PromptEvent {
        self.handle_mouse_inner(mouse).0
    }

    pub fn handle_mouse_scroll(&mut self, mouse: &crossterm::event::MouseEvent) -> bool {
        self.handle_mouse_inner(mouse).1
    }

    fn handle_mouse_inner(&mut self, mouse: &crossterm::event::MouseEvent) -> (PromptEvent, bool) {
        use crate::engine::textarea::{MouseAction, TextElementEventKind};

        self.post_insert_image_preview = None;
        let on_status = self
            .status_area
            .is_some_and(|r| r.contains((mouse.column, mouse.row).into()));
        match mouse.kind {
            MouseEventKind::Moved => self.status_hovered = on_status,
            MouseEventKind::Down(MouseButton::Left) if on_status => {
                return (PromptEvent::ActivateStatus, false);
            }
            _ => {}
        }
        let action = self
            .textarea
            .handle_mouse(*mouse, self.textarea_area, self.textarea_state);

        while let Some(event) = self.textarea.poll_element_event() {
            match event.kind {
                TextElementEventKind::HoverEnter => {
                    let is_image = self
                        .textarea
                        .elements()
                        .iter()
                        .any(|e| e.id == event.id && e.kind == KIND_IMAGE);
                    if is_image {
                        self.hovered_image_element_id = Some(event.id);
                    }
                }
                TextElementEventKind::HoverLeave => {
                    if self.hovered_image_element_id == Some(event.id) {
                        self.hovered_image_element_id = None;
                    }
                }
                TextElementEventKind::Click => {
                    return (PromptEvent::ActivateElement(event.id), false);
                }
            }
        }

        let did_scroll = matches!(action, MouseAction::Scrolled);
        let event = match action {
            MouseAction::CursorPlaced
            | MouseAction::SelectionUpdated
            | MouseAction::SelectionFinished
            | MouseAction::Scrolled => {
                // The cursor moved: update the file search context so the dropdown appears/disappears based on the new cursor position
                self.update_file_search_context();
                PromptEvent::Edited
            }
            MouseAction::Nothing => PromptEvent::Edited,
        };
        (event, did_scroll)
    }

    /// Trailing newlines in the inline path are preserved so that split batches (Windows) accumulate
    /// correctly. Repasting a chip's exact content while the cursor is on or right after it expands
    /// that chip instead of inserting a duplicate.
    pub fn handle_paste(&mut self, text: &str) -> PromptEvent {
        if text.is_empty() {
            return PromptEvent::Ignored;
        }
        self.post_insert_image_preview = None;

        let text = normalize_line_breaks(text);
        let text = &text;
        let replacing_selection = self.textarea.selection_range().is_some();

        // Repaste-to-expand: "paste didn't do what I want? paste again."
        // Requires exact byte equality after the original insertion's canonicalization (normalize_line_breaks above and the textarea's tab expansion)
        // E.g. a trailing-newline difference is a different paste and takes the normal path below.
        let expanded = self.textarea.expand_tabs(text);
        if !replacing_selection
            && let Some(elem) = self.paste_element_near_cursor()
            && self.textarea.get_range(elem.range.clone()) == Some(expanded.as_ref())
        {
            let id = elem.id;
            self.expand_element(id);
            return PromptEvent::Edited;
        }

        if replacing_selection {
            self.textarea.begin_undo_group();
            self.textarea.delete_selection();
        }

        // Count actual content lines
        // `.lines()` treats a trailing newline as optional, so "hello\n" counts as 1 line, exactly what we want
        let line_count = text.lines().count();

        let chip_min_lines = if self.compact { 2 } else { 4 };
        // Chip when the paste is multi-line past the threshold, or large by byte size
        // The label prefers size for large pastes (a 1 MB paste reads better as "1.0 MB" than "N lines"); smaller multi-line pastes keep the line count
        let by_lines = line_count >= chip_min_lines;
        let by_bytes = text.len() > PASTE_CHIP_DISPLAY_BYTES;
        if by_lines || by_bytes {
            let display = if by_bytes {
                paste_chip_display_bytes(text.len())
            } else {
                paste_chip_display(line_count)
            };
            self.textarea
                .insert_element(text, KIND_PASTE, Some(display));
        } else {
            self.textarea.insert_str(text);
        }
        if replacing_selection {
            self.textarea.end_undo_group();
            self.sync_images_with_textarea();
        }

        // Update the file search context so @-completion reacts to pasted text
        // Without this, bracketed paste (terminal-level paste) skips the per-keystroke update_file_search_context() that handle_key does
        // The fuzzy matcher query would stay stale until the next keystroke
        self.update_file_search_context();
        if self.history_search.is_active() && !self.history_search.is_browse() {
            self.history_search.update_query(self.textarea.text());
        }
        PromptEvent::Edited
    }

    /// Maximum number of image chips allowed in a single prompt (v1).
    pub const IMAGE_CAP: usize = 10;

    /// Toast text shown when an image insertion is rejected because the prompt already holds [`Self::IMAGE_CAP`] images.
    #[allow(dead_code)]
    pub fn cap_reached_toast() -> String {
        format!("Image limit reached (max {})", Self::IMAGE_CAP)
    }

    /// Insert a pasted image as an atomic `[Image #N]` chip. Returns `Err` with a user-facing message
    /// if the image is rejected (cap reached or too small). The caller should show a toast.
    pub fn insert_image(&mut self, image: PastedImage) -> Result<(), String> {
        if self.images.len() >= Self::IMAGE_CAP {
            return Err(Self::cap_reached_toast());
        }
        // Backend APIs reject images with either side < 8 px.
        const MIN_SIDE: u32 = 8;
        if let Some((w, h)) = image.preview_dimensions()
            && (w < MIN_SIDE || h < MIN_SIDE)
        {
            return Err(format!(
                "Image too small ({w}×{h}). Must be at least {MIN_SIDE}×{MIN_SIDE} pixels."
            ));
        }

        self.image_counter += 1;
        let display_number = self.image_counter;
        let placeholder = images::display_text(display_number);
        let display_line = chip_line(format!("Image #{display_number}"));

        self.textarea.begin_undo_group();
        let element_id = self
            .textarea
            .insert_element(&placeholder, KIND_IMAGE, Some(display_line));
        self.textarea.insert_str(" ");
        self.textarea.end_undo_group();
        self.post_insert_image_preview = Some((element_id, self.textarea.cursor()));

        let mut img = image;
        img.element_id = element_id;
        img.display_number = display_number;
        self.images.push(img);

        Ok(())
    }

    /// Insert text replacing the selection; resyncs images (a selection can swallow chips).
    pub fn insert_replacing_selection(&mut self, text: &str) {
        let replaced_selection = self.textarea.selection_range().is_some();
        self.textarea.insert_str_replacing_selection(text);
        if replaced_selection {
            self.sync_images_with_textarea();
        }
        self.update_file_search_context();
    }

    fn sync_images_with_textarea(&mut self) {
        use std::collections::HashMap;
        use std::collections::hash_map::Entry;

        self.rechip_orphan_image_placeholders();

        let live_image_elements: Vec<(ElementId, std::ops::Range<usize>)> = self
            .textarea
            .elements()
            .iter()
            .filter(|e| e.kind == KIND_IMAGE)
            .map(|e| (e.id, e.range.clone()))
            .collect();

        // Primary lookup keyed by `ElementId` (unique by construction).
        // Keying by `display_number` would silently drop one record when two images share a number
        // The warn branch below makes a future duplicate-id regression visible instead of hiding it
        let mut stored_by_id: HashMap<ElementId, PastedImage> =
            HashMap::with_capacity(self.images.len());
        for img in self.images.drain(..) {
            let id = img.element_id;
            match stored_by_id.entry(id) {
                Entry::Vacant(v) => {
                    v.insert(img);
                }
                Entry::Occupied(_) => {}
            }
        }

        // Fallback for elements restored by an undo/redo cycle. Keying by `display_number` is safe here
        // because the monotonic counter never recycles numbers within a prompt lifetime.
        // A same-number duplicate is no redo target but still owns files, so it stays in the stash.
        let mut stash_by_number: HashMap<usize, PastedImage> =
            HashMap::with_capacity(self.image_undo_stash.len());
        let mut stash_duplicates: Vec<PastedImage> = Vec::new();
        for img in self.image_undo_stash.drain(..) {
            let display_number = img.display_number;
            match stash_by_number.entry(display_number) {
                Entry::Vacant(v) => {
                    v.insert(img);
                }
                Entry::Occupied(_) => {
                    stash_duplicates.push(img);
                }
            }
        }

        let mut synced = Vec::with_capacity(live_image_elements.len());

        for (id, range) in &live_image_elements {
            // Primary: match by element_id (collision-free, stable for non-undo/redo edits)
            if let Some(mut img) = stored_by_id.remove(id) {
                // Refresh display_number from the live buffer text in case the chip was renumbered externally (sanity)
                let parsed = self
                    .textarea
                    .get_range(range.clone())
                    .and_then(parse_image_display_number)
                    .unwrap_or(img.display_number);
                img.element_id = *id;
                img.display_number = parsed;
                synced.push(img);
                continue;
            }
            // Fallback (redo path): match by display_number from the stash, then rebind to the freshly-issued `element_id`
            let display_number = self
                .textarea
                .get_range(range.clone())
                .and_then(parse_image_display_number)
                .unwrap_or(0);
            if let Some(mut img) = stash_by_number.remove(&display_number) {
                img.element_id = *id;
                img.display_number = display_number;
                synced.push(img);
            }
        }

        // Images whose elements are gone move to the undo stash so they can be recovered if the user
        // redoes. Cap the stash at `IMAGE_CAP * 2` so a long edit session can't unboundedly grow it.
        let mut new_stash: Vec<PastedImage> = stored_by_id
            .into_values()
            .chain(stash_by_number.into_values())
            .chain(stash_duplicates)
            .collect();
        let stash_cap = Self::IMAGE_CAP * 2;
        if new_stash.len() > stash_cap {
            // Oldest means the lowest `display_number` (monotonic within a prompt lifetime). Sort ascending, then drop the front.
            new_stash.sort_by_key(|img| img.display_number);
            let evict_count = new_stash.len() - stash_cap;
            for img in new_stash.drain(..evict_count) {
                // Stash eviction
                images::cleanup_image(images::SessionPathPolicy::Preserve, &img);
            }
        }
        self.image_undo_stash = new_stash;
        self.images = synced;
        if self
            .hovered_image_element_id
            .is_some_and(|id| !self.images.iter().any(|img| img.element_id == id))
        {
            self.hovered_image_element_id = None;
        }
        if self
            .post_insert_image_preview
            .is_some_and(|(id, _)| !self.images.iter().any(|img| img.element_id == id))
        {
            self.post_insert_image_preview = None;
        }
        // Monotonic high-water mark within a prompt lifetime: the counter only ever advances upward
        // Deleting a chip therefore never lets a later insert reuse a number that already appeared
        // The counter is reset only by explicit prompt clears (`set_text("")`, Ctrl+C) which call [`images::clear`]
        self.image_counter = self.image_counter.max(images_high_water(&self.images));
    }

    /// Look up the image record for the element the cursor is currently on. Positions at or past
    /// `range.end` (the trailing separator space and beyond) are intentionally excluded.
    pub fn image_at_cursor(&self) -> Option<&PastedImage> {
        let elem = self.textarea.element_at_cursor()?;
        if elem.kind != KIND_IMAGE {
            return None;
        }
        self.images.iter().find(|img| img.element_id == elem.id)
    }

    /// Image element the cursor is on, or exactly at its right edge. An on-chip match of any kind wins
    /// (the chip under the cursor owns the position). So a cursor sitting on a non-image chip yields
    /// `None` even when an image chip ends at the cursor.
    #[cfg(feature = "inline-image")]
    fn image_element_near_cursor(&self) -> Option<&crate::engine::textarea::TextElement> {
        if let Some(elem) = self.textarea.element_at_cursor() {
            return (elem.kind == KIND_IMAGE).then_some(elem);
        }
        let cursor = self.textarea.cursor();
        // Right edge only, not the spacer after (`range.end + 1`)
        self.textarea
            .elements()
            .iter()
            .filter(|e| e.kind == KIND_IMAGE)
            .find(|e| e.range.end == cursor)
    }

    /// Image record for the preview overlay: cursor, hover, or fresh insertion.
    #[cfg(feature = "inline-image")]
    fn image_for_preview(&self) -> Option<&PastedImage> {
        if let Some(elem) = self.image_element_near_cursor() {
            return self.images.iter().find(|img| img.element_id == elem.id);
        }
        if let Some(id) = self.hovered_image_element_id {
            return self.images.iter().find(|img| img.element_id == id);
        }
        let (id, cursor) = self.post_insert_image_preview?;
        if self.cursor() != cursor {
            return None;
        }
        self.images.iter().find(|img| img.element_id == id)
    }

    /// Drain all prompt-side images for submission. Re-binds orphan `[Image #N]` text to the records
    /// that name it, then reconciles against live `TextArea` elements so deleted chips are never
    /// included. Returns the drained images; the prompt-side storage is left empty.
    pub fn drain_images(&mut self) -> Vec<PastedImage> {
        self.post_insert_image_preview = None;
        self.reconciled_images();
        std::mem::take(&mut self.images)
    }

    pub fn has_live_image(&self) -> bool {
        self.images.iter().any(|image| {
            self.textarea
                .elements()
                .iter()
                .any(|element| element.kind == KIND_IMAGE && element.id == image.element_id)
        })
    }

    pub fn drop_image_preserving_session_file(&mut self, image_identity: u64) {
        let Some(position) = self
            .images
            .iter()
            .position(|image| image.preview.identity() == image_identity)
        else {
            return;
        };
        let Some(element_id) = self.images.get(position).map(|image| image.element_id) else {
            return;
        };
        if let Some(range) = self
            .textarea
            .elements()
            .iter()
            .find(|element| element.id == element_id)
            .map(|element| element.range.clone())
        {
            self.textarea.replace_range(range, "");
        }
        let image = self.images.remove(position);
        images::cleanup_image(images::SessionPathPolicy::Preserve, &image);
    }

    /// Re-bind orphan placeholders, then reconcile against live `TextArea` elements before a drain,
    /// dropping the records whose chip text is gone as well.
    pub fn reconciled_images(&mut self) -> &[PastedImage] {
        self.rebind_image_placeholders();
        let live_ids: std::collections::HashSet<_> = self
            .textarea
            .elements()
            .iter()
            .filter(|e| e.kind == KIND_IMAGE)
            .map(|e| e.id)
            .collect();
        images::reconcile(
            images::SessionPathPolicy::Preserve,
            &mut self.images,
            &live_ids,
        );
        &self.images
    }

    /// Buffer text with `[Image #N]` chip placeholders removed.
    /// For text-only consumers (e.g. question/permission feedback) that must not leak image tokens onto the wire.
    pub fn text_without_image_chips(&self) -> String {
        self.submitted_text_without_image_chips(self.textarea.text())
    }

    pub fn submitted_text_without_image_chips(&self, submitted: &str) -> String {
        if !submitted.starts_with(self.textarea.text()) {
            return submitted.to_owned();
        }
        text_without_image_chips(
            submitted,
            self.textarea
                .elements()
                .iter()
                .map(|element| (element.kind, element.range.clone())),
        )
    }

    /// Replace the prompt-side image list and restore `image_counter`. The caller must call
    /// `restore_chip_elements` beforehand to re-register the `KIND_IMAGE` textarea elements (with
    /// stored byte ranges). The identity lookup is therefore collision-free.
    pub fn set_images(&mut self, mut images: Vec<PastedImage>) {
        if images.len() > Self::IMAGE_CAP {
            images.truncate(Self::IMAGE_CAP);
        }

        // Emit a `tracing::warn!` with the existing image tracing target so the regression is visible
        // without crashing the TUI. The lookup below still picks the first match so the visible binding is
        // at least deterministic.
        let mut by_number: Vec<(usize, ElementId)> = Vec::new();
        for elem in self.textarea.elements() {
            if elem.kind != KIND_IMAGE {
                continue;
            }
            if let Some(dn) = self
                .textarea
                .get_range(elem.range.clone())
                .and_then(parse_image_display_number)
            {
                if by_number.iter().any(|(seen_dn, _)| *seen_dn == dn) {
                    continue;
                }
                by_number.push((dn, elem.id));
            }
        }

        for img in &mut images {
            if let Some(&(_, eid)) = by_number.iter().find(|(dn, _)| *dn == img.display_number) {
                img.element_id = eid;
            }
        }
        // Align with the monotonic contract in `sync_images_with_textarea`: the counter only ever advances upward within a prompt lifetime
        self.image_counter = self.image_counter.max(images_high_water(&images));
        self.images = images;
        // A record whose stored chip range went stale still binds to the placeholder text that names it.
        self.rechip_orphan_image_placeholders();
    }

    /// Re-register all chip elements (paste blocks, @-file refs, image chips) after a `set_text` restore.
    /// Uses the byte ranges stored at capture time; no buffer re-scanning. A range an element of the
    /// same kind already covers is skipped, so a re-chipped image never gets a second element.
    pub fn restore_chip_elements(&mut self, elems: &[ChipElement]) {
        let covered: Vec<(ElementKind, std::ops::Range<usize>)> = self
            .textarea
            .elements()
            .iter()
            .map(|e| (e.kind, e.range.clone()))
            .collect();
        self.textarea.restore_elements(
            elems
                .iter()
                .filter(|e| {
                    !covered.iter().any(|(kind, range)| {
                        *kind == e.kind && range.start < e.range.end && e.range.start < range.end
                    })
                })
                .map(|e| (e.range.clone(), e.kind, e.display.clone())),
        );
    }

    /// Reconcile modal-owned images with live chips, deleting removed session copies.
    pub fn reconcile_feedback_images_on_teardown(&mut self) {
        let live_ids: std::collections::HashSet<_> = self
            .textarea
            .elements()
            .iter()
            .filter(|element| element.kind == KIND_IMAGE)
            .map(|element| element.id)
            .collect();
        images::reconcile(
            images::SessionPathPolicy::Delete,
            &mut self.images,
            &live_ids,
        );
    }

    /// Keep disk-backed sources whose off-thread rehydration has not completed.
    pub fn preserve_pending_feedback_image_sources(&mut self) {
        for image in self.images.iter_mut().chain(&mut self.image_undo_stash) {
            if image.encoded_bytes.is_none() {
                image.session_image_path = None;
            }
        }
    }

    /// Drain and delete every image this composer still owns (live chips and undo stash).
    /// Teardown for throwaway composers (overlay modals); the main prompt lives for the whole session and never needs this.
    pub fn cleanup_images_on_teardown(&mut self) {
        images::drain_and_cleanup(images::SessionPathPolicy::Delete, &mut self.images);
        images::drain_and_cleanup(
            images::SessionPathPolicy::Delete,
            &mut self.image_undo_stash,
        );
    }

    /// Seed a fresh composer through the normal image insertion and reconciliation path.
    pub fn seed_images(&mut self, images: Vec<PastedImage>) -> usize {
        let mut rejected = 0;
        self.set_cursor(self.text().len());
        for image in images {
            self.image_counter = self
                .image_counter
                .max(image.display_number.saturating_sub(1));
            if self.insert_image(image.clone()).is_err() {
                images::cleanup_image(images::SessionPathPolicy::Delete, &image);
                rejected += 1;
            }
        }
        rejected
    }

    /// Expose the underlying textarea for element access.
    pub fn textarea(&self) -> &crate::engine::textarea::TextArea {
        &self.textarea
    }

    /// Buffer text of `elem` if it is a paste chip (`KIND_PASTE`).
    fn paste_text(&self, elem: &TextElement) -> Option<&str> {
        (elem.kind == KIND_PASTE)
            .then_some(elem.range.clone())
            .and_then(|range| self.textarea.get_range(range))
    }

    /// Check if the cursor is currently on a paste element. The match is strict on-chip, mirroring the
    /// Enter-to-expand targeting in [`Self::try_element_interaction`]. Returns the element's buffer
    /// text if so.
    pub fn paste_element_at_cursor(&self) -> Option<&str> {
        self.paste_text(self.textarea.element_at_cursor()?)
    }

    /// Paste element the cursor is on or immediately to the right of (where
    /// [`TextArea::insert_element`] leaves it right after a paste).
    fn paste_element_near_cursor(&self) -> Option<&TextElement> {
        let elem = self.textarea.element_at_cursor().or_else(|| {
            let cursor = self.textarea.cursor();
            self.textarea
                .elements()
                .iter()
                .find(|e| e.range.end == cursor)
        })?;
        (elem.kind == KIND_PASTE).then_some(elem)
    }

    /// Paste element text for the preview overlay. Display-only: Enter handling keeps the strict
    /// on-chip match.
    fn paste_element_for_preview(&self) -> Option<&str> {
        self.paste_text(self.paste_element_near_cursor()?)
    }

    /// Expand hint for the paste preview overlay, honest per position.
    /// ON the chip Enter expands it; right after the chip Enter submits, so the hint there advertises pasting the content again.
    /// Double-click expands from either position (a click moves the cursor onto the chip first).
    fn paste_preview_hint(&self, theme: &Theme) -> Line<'static> {
        let dim = theme.muted();
        // The chord deliberately deviates from the tips' text_secondary to a real accent
        // Oscura/rosepine alias text_secondary and gray to the box's own content/border colors, so only an accent pops
        let chord = Style::default()
            .fg(theme.fuzzy_accent)
            .add_modifier(Modifier::BOLD);
        let action = if self.paste_element_at_cursor().is_some() {
            "enter"
        } else {
            "paste again"
        };
        Line::from(vec![
            Span::styled(action, chord),
            Span::styled(" or ", dim),
            Span::styled("double-click", chord),
            Span::styled(" to expand", dim),
        ])
    }

    /// Inline (expand) element `id` into plain buffer text and refresh the @-completion context.
    /// A single undoable step (see [`TextArea::inline_element`]).
    fn expand_element(&mut self, id: ElementId) {
        self.textarea.inline_element(id);
        self.update_file_search_context();
    }

    /// Expand the paste chip under the cursor (mouse double-click path). Strict on-chip match: a cursor
    /// merely adjacent to a chip (e.g. right after it) must not expand it. Returns `true` if a paste
    /// chip was expanded.
    pub fn expand_paste_element_at_cursor(&mut self) -> bool {
        let Some(elem) = self.textarea.element_at_cursor() else {
            return false;
        };
        if elem.kind != KIND_PASTE {
            return false;
        }
        let id = elem.id;
        self.expand_element(id);
        true
    }

    /// Try to interact with an element at the cursor. Enter on a paste/file-ref element inlines
    /// (expands) it. Enter on an image chip signals the caller to open the preview (it does NOT inline
    /// the placeholder text). Returns `Some(interaction)` if handled, `None` otherwise.
    pub fn try_element_interaction(&mut self, key: &KeyEvent) -> Option<ElementInteraction> {
        let elem = self.textarea.element_at_cursor()?;
        if !key!(Enter).matches(key) {
            return None;
        }
        match elem.kind {
            k if k == KIND_PASTE || k == KIND_FILE_REF => {
                let id = elem.id;
                self.expand_element(id);
                Some(ElementInteraction::Inlined)
            }
            k if k == KIND_IMAGE => Some(ElementInteraction::ImagePreview),
            _ => None,
        }
    }
}

/// Paint slash/skill accent color on a byte range, using textarea screen coords.
/// Highlighting thus works on any visual row, including the continuation rows of a token that soft-wraps at the line end.
fn paint_slash_token_highlight(
    textarea: &TextArea,
    state: TextAreaState,
    ta_area: Rect,
    buf: &mut Buffer,
    range: std::ops::Range<usize>,
    fg: ratatui::style::Color,
) {
    // One row-span per visual row, from the textarea's own wrap ranges (the source of truth for where it broke the text)
    // Invalid ranges yield no spans
    for span in textarea.screen_spans_of_range(range, ta_area, state) {
        for x in span.left()..span.right() {
            if let Some(cell) = buf.cell_mut((x, span.y))
                && !cell.symbol().trim().is_empty()
            {
                cell.set_fg(fg);
            }
        }
    }
}

impl PromptWidget {
    /// Render the prompt widget. When `style.chrome` is true, renders the full block layout: `accent(┃)
    /// | hpad | content | hpad` with background fill. When false, renders only the content (prefix +
    /// textarea + info).
    pub fn draw(
        &mut self,
        buf: &mut Buffer,
        area: Rect,
        overlay_area: Option<Rect>,
        style: &PromptStyle,
        info: Option<&PromptInfo>,
        voice: Option<VoicePromptOverlay>,
    ) -> PromptRenderResult {
        self.status_area = None;
        self.textarea_area = Rect::default();
        let area = area.intersection(buf.area);
        if area.height == 0 || area.width < 4 {
            return PromptRenderResult {
                cursor_pos: None,
                post_flush_escapes: None,
            };
        }

        let theme = Theme::current();
        let bg = style.bg.color(theme.bg_base);

        let border_color = style.border_color_override.unwrap_or(if style.focused {
            theme.prompt_border_active
        } else {
            theme.prompt_border
        });

        // Fill the entire area with fg and bg so every cell has RGB colors (needed for blending)
        buf.set_style(area, Style::default().fg(theme.text_primary).bg(bg));

        // Compute content area (inside chrome if enabled).
        let content_area = if style.chrome {
            let accent_w = if style.show_accent_line { 1u16 } else { 0 };
            if style.show_accent_line {
                let accent_color = style.accent_color(&theme);
                for y in area.y..area.y + area.height {
                    if let Some(cell) = buf.cell_mut((area.x, y)) {
                        cell.set_symbol(crate::engine::render::glyphs::accent_bar());
                        cell.set_style(Style::default().fg(accent_color).bg(bg));
                    }
                }
            }
            Rect {
                x: area.x + accent_w + style.chrome_pad_left,
                y: area.y,
                width: area
                    .width
                    .saturating_sub(accent_w + style.chrome_pad_left + style.chrome_pad_right),
                height: area.height,
            }
        } else {
            area
        };

        // Split content: vpad_top, text, info_block
        let vpad_top = style.vpad_top;
        let info_block = style.info_block(info.is_some());
        let chunks = Layout::vertical([
            Constraint::Length(vpad_top),
            Constraint::Min(1),
            Constraint::Length(info_block),
        ])
        .split(content_area);

        let Some(&text_area_rect) = chunks.get(1) else {
            return PromptRenderResult {
                cursor_pos: None,
                post_flush_escapes: None,
            };
        };

        // Top divider: ╭──────────╮
        if vpad_top > 0 && style.chrome && style.show_borders {
            let div_style = Style::default().fg(border_color).bg(bg);
            let div_y = chunks.first().map(|c| c.y).unwrap_or(area.y);
            let left_x = area.x;
            let right_x = area.x + area.width.saturating_sub(1);
            for x in area.x..area.x + area.width {
                if let Some(cell) = buf.cell_mut((x, div_y)) {
                    let ch = if x == left_x {
                        '\u{256d}' // ╭
                    } else if x == right_x {
                        '\u{256e}' // ╮
                    } else {
                        '\u{2500}' // ─
                    };
                    cell.set_char(ch);
                    cell.set_style(div_style);
                }
            }

            // Caption inlined in the divider, ending on the same column as the info line below.
            // The pad spaces blank the adjacent `─`.
            let caption = style
                .title
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty());
            let caption_right = content_area.x + content_area.width;
            let max_w = caption_right.saturating_sub(area.x + 3);
            if let Some(caption) = caption
                && max_w >= 6
            {
                let label = format!(" {caption} ");
                let trunc = crate::engine::render::line_utils::truncate_str(&label, max_w as usize);
                let label_w = unicode_width::UnicodeWidthStr::width(trunc.as_str()) as u16;
                buf.set_string(
                    caption_right.saturating_sub(label_w),
                    div_y,
                    &trunc,
                    Self::chrome_caption_style(bg, &theme, style.focused),
                );
            }
        }

        // Render prefix on first text row: search icon when history search is active, else ❯.
        let prefix_w = if style.show_prefix { PREFIX_WIDTH } else { 0 };
        if style.show_prefix && text_area_rect.width > PREFIX_WIDTH {
            let (prefix_str, accent_color) =
                if self.history_search.is_active() && !self.history_search.is_browse() {
                    // Search-mode indicator, the highest prefix priority
                    // Browse mode keeps the normal prefix: the composer holds the populated entry (a real draft), not a filter query
                    ("? ", theme.accent_user)
                } else if let Some((p, c)) = style.prefix_override {
                    // Caller override (e.g., bash mode `! ` in yellow).
                    (p, c)
                } else {
                    (
                        crate::engine::render::glyphs::prompt_arrow(),
                        style.accent_color(&theme),
                    )
                };
            buf.set_string(
                text_area_rect.x,
                text_area_rect.y,
                prefix_str,
                Style::default().fg(accent_color).bg(bg),
            );
        }

        // TextArea content
        let ta_area = Rect {
            x: text_area_rect.x + prefix_w,
            y: text_area_rect.y,
            width: text_area_rect.width.saturating_sub(prefix_w),
            height: text_area_rect.height,
        };
        self.textarea_area = ta_area;

        (&self.textarea).render_ref(ta_area, buf, &mut self.textarea_state);

        // Chip bg remap (see `PromptBg::Panel`): chip `Line`s bake in `paste_bg` at paste time
        // The same element can render on multiple surfaces, so restyle at paint time
        if matches!(style.bg, PromptBg::Panel(_)) && bg != theme.paste_bg {
            for y in ta_area.top()..ta_area.bottom() {
                for x in ta_area.left()..ta_area.right() {
                    if let Some(cell) = buf.cell_mut((x, y))
                        && cell.bg == theme.paste_bg
                    {
                        cell.bg = bg;
                    }
                }
            }
        }

        // Slash overlays: teal command name and args ghost text
        // Both use the same snapshot, so clone once
        // Capture flags for later ghost text suppression to avoid a second clone
        let (slash_active, slash_has_inline_ghost) = {
            let snap = self.slash.clone();

            // Slash command-name highlight color
            // Minimal mode is monochrome, so recolor to primary text (no visible accent); typed `/commands` then match the rest of the input
            let slash_hl = if crate::ui::views::modal_window::embedded() {
                theme.text_primary
            } else {
                theme.accent_skill
            };

            let leading_invocation = snap
                .command_range
                .as_ref()
                .is_some_and(|range| range.start == 0);

            // Leading `/` teals on dropdown or a recognized name. Mid-text teals only when
            // the token actually runs (hoist) or is a skill/plugin mention.
            if snap.active
                && (snap.command_recognized || (leading_invocation && snap.open))
                && let Some(cmd_range) = &snap.command_range
            {
                paint_slash_token_highlight(
                    &self.textarea,
                    self.textarea_state,
                    ta_area,
                    buf,
                    cmd_range.clone(),
                    slash_hl,
                );
            }

            if let Some(ref ghost) = snap.inline_ghost
                && ghost.highlight
            {
                paint_slash_token_highlight(
                    &self.textarea,
                    self.textarea_state,
                    ta_area,
                    buf,
                    ghost.token_range.clone(),
                    slash_hl,
                );
            }

            // Args ghost text: show expected arguments after the cursor for skills and commands that declare an arg_placeholder
            if snap.args_query_is_empty
                && let Some(ref placeholder) = snap.args_placeholder
                && let Some((start_x, row_y)) = self.textarea.screen_position_of(
                    self.textarea.cursor(),
                    ta_area,
                    self.textarea_state,
                )
            {
                let avail = (ta_area.x + ta_area.width).saturating_sub(start_x) as usize;
                if avail > 0 {
                    let truncated =
                        crate::engine::render::line_utils::truncate_str(placeholder, avail);
                    buf.set_string(start_x, row_y, &truncated, theme.muted().bg(bg));
                }
            }

            // Mid-text inline ghost text: show the completion suffix for a partial `/command` token under the cursor
            if let Some(ref ghost) = snap.inline_ghost
                && ghost.token_range.end <= self.textarea.text().len()
                && let Some((screen_x, screen_y)) = self.textarea.screen_position_of(
                    ghost.token_range.end,
                    ta_area,
                    self.textarea_state,
                )
            {
                let avail = (ta_area.x + ta_area.width).saturating_sub(screen_x) as usize;
                if avail > 0 {
                    let truncated =
                        crate::engine::render::line_utils::truncate_str(&ghost.text, avail);
                    buf.set_string(screen_x, screen_y, &truncated, theme.muted().bg(bg));
                }
            }

            // Teal highlighting for recognized mid-text `/command` tokens.
            for token_range in &snap.recognized_tokens {
                paint_slash_token_highlight(
                    &self.textarea,
                    self.textarea_state,
                    ta_area,
                    buf,
                    token_range.clone(),
                    slash_hl,
                );
            }

            (snap.active, snap.inline_ghost.is_some())
        };

        // Interim STT: muted italic overlay (not in the textarea)
        // Finalized text remains the real, editable draft
        // While it shows, the caret is drawn after the ghost words, where the final will leave it
        let mut interim_caret: Option<(u16, u16)> = None;
        let voice_interim_shown = if let Some(v) = voice
            && let Some(interim) = v.interim.filter(|t| !t.trim().is_empty())
            && ta_area.width > 0
            && ta_area.height > 0
        {
            // muted() fallback where the blend is inexpressible (terminal
            // theme): DIM instead of gray (Reset — full default fg).
            let interim_style =
                match crate::engine::render::color::blend_color(bg, theme.text_secondary, 0.7) {
                    Some(fg) => Style::default().fg(fg).bg(bg),
                    None => theme.muted().bg(bg),
                }
                .add_modifier(Modifier::ITALIC);
            if prompt_blank_for_voice(self.textarea.text()) {
                let lines =
                    wrap_voice_interim(interim, ta_area.width as usize, ta_area.height as usize);
                for (i, line) in lines.iter().enumerate() {
                    buf.set_string(ta_area.x, ta_area.y + i as u16, line, interim_style);
                }
                if let Some(last) = lines.last() {
                    let end_x =
                        ta_area.x + unicode_width::UnicodeWidthStr::width(last.as_str()) as u16;
                    let last_y = ta_area.y + (lines.len() - 1) as u16;
                    // Mirror the textarea: a full row wraps the caret to the next row, or pins it to the last cell on the last row
                    interim_caret = Some(if end_x < ta_area.x + ta_area.width {
                        (end_x, last_y)
                    } else if last_y + 1 < ta_area.y + ta_area.height {
                        (ta_area.x, last_y + 1)
                    } else {
                        (end_x - 1, last_y)
                    });
                }
            } else {
                // Ghost preview of the interim inserted at the caret, or replacing the active selection.
                // Uses the same selection-aware insertion point and spacing rule
                // (space_voice_fragment) as the real insertion so preview and result cannot drift.
                let text = self.textarea.text();
                let (base, at, tail_at) = match self.textarea.selection_range() {
                    Some(sel) => (
                        std::borrow::Cow::Owned(format!(
                            "{}{}",
                            text.get(..sel.start).unwrap_or(""),
                            text.get(sel.end..).unwrap_or("")
                        )),
                        sel.start,
                        sel.end,
                    ),
                    None => {
                        let cursor = self.textarea.cursor();
                        (std::borrow::Cow::Borrowed(text), cursor, cursor)
                    }
                };
                let (display, _) = space_voice_fragment(&base, at, interim);

                if let Some((start_x, row_y)) =
                    self.textarea
                        .screen_position_of(at, ta_area, self.textarea_state)
                {
                    let row_right = ta_area.x + ta_area.width;
                    let avail = row_right.saturating_sub(start_x) as usize;
                    if avail > 0 {
                        // The textarea already painted the full draft: snapshot the cells after the insertion
                        // point (selection end, or caret), paint the interim over the span, then re-blit that tail
                        // shifted right by the ghost's width, so the prompt reads as pushed aside and a selection as replaced; a multi-row selection falls back to the caret row.
                        let tail_x = self
                            .textarea
                            .screen_position_of(tail_at, ta_area, self.textarea_state)
                            .filter(|(_, ty)| *ty == row_y)
                            .map_or(start_x, |(tx, _)| tx.max(start_x));
                        let saved: Vec<ratatui::buffer::Cell> = (tail_x..row_right)
                            .map(|x| buf.cell((x, row_y)).cloned().unwrap_or_default())
                            .collect();
                        let truncated =
                            crate::engine::render::line_utils::truncate_str(&display, avail);
                        let ghost_w =
                            unicode_width::UnicodeWidthStr::width(truncated.as_str()) as u16;
                        buf.set_string(start_x, row_y, &truncated, interim_style);
                        // trim_end: the caret sits before the spacing added for the text that follows
                        let words_w =
                            unicode_width::UnicodeWidthStr::width(truncated.trim_end()) as u16;
                        interim_caret =
                            Some(((start_x + words_w).min(row_right.saturating_sub(1)), row_y));
                        let mut x = start_x.saturating_add(ghost_w);
                        for cell in saved {
                            if x >= row_right {
                                break;
                            }
                            if let Some(dst) = buf.cell_mut((x, row_y)) {
                                *dst = cell;
                            }
                            x = x.saturating_add(1);
                        }
                    }
                }
            }
            true
        } else {
            false
        };

        // Placeholder text when empty (unfocused, or opted in while focused).
        if self.textarea.text().is_empty()
            && ta_area.width > 0
            && (!style.focused || style.placeholder_when_focused)
            && !voice_interim_shown
        {
            let placeholder = style
                .placeholder_override
                .or(self.placeholder.as_deref())
                .unwrap_or("Build anything");
            // `set_string` clips at the buffer edge, not at the textarea, so a placeholder longer than the box would paint over its border.
            let truncated = crate::engine::render::line_utils::truncate_str(
                placeholder,
                ta_area.width as usize,
            );
            buf.set_string(ta_area.x, ta_area.y, &truncated, theme.muted().bg(bg));
        }

        // Side borders: │ on left and right of each text row.
        if style.chrome && style.show_borders && area.width >= 2 {
            let div_style = Style::default().fg(border_color).bg(bg);
            let left_x = area.x;
            let right_x = area.x + area.width.saturating_sub(1);
            for y in text_area_rect.y..text_area_rect.y + text_area_rect.height {
                if let Some(cell) = buf.cell_mut((left_x, y)) {
                    cell.set_char('\u{2502}'); // │
                    cell.set_style(div_style);
                }
                if let Some(cell) = buf.cell_mut((right_x, y)) {
                    cell.set_char('\u{2502}'); // │
                    cell.set_style(div_style);
                }
            }
        }

        // Bottom divider: ╰──────────grok-3 · flags──╯
        // Guard on actual allocated height, not requested `info_block`
        // During resize the layout may squeeze the info block to 0 rows, leaving chunks[2].y past the buffer boundary
        if info_block > 0
            && style.chrome
            && style.show_borders
            && let Some(info_chunk) = chunks.get(2)
            && info_chunk.height > 0
        {
            let div_y = info_chunk.y;
            let div_style = Style::default().fg(border_color).bg(bg);
            let left_x = area.x;
            let right_x = area.x + area.width.saturating_sub(1);
            for x in area.x..area.x + area.width {
                if let Some(cell) = buf.cell_mut((x, div_y)) {
                    let ch = if x == left_x {
                        '\u{2570}' // ╰
                    } else if x == right_x {
                        '\u{256f}' // ╯
                    } else {
                        '\u{2500}' // ─
                    };
                    cell.set_char(ch);
                    cell.set_style(div_style);
                }
            }
            // A blank info line still writes its padding spaces, which would punch holes in the divider it sits on.
            if let Some(info) = info.filter(|i| !i.is_blank()) {
                let info_rect = Rect {
                    x: content_area.x,
                    y: div_y,
                    width: content_area.width,
                    height: 1,
                };
                self.status_area =
                    self.render_info_line(buf, info_rect, info, bg, &theme, style.focused);
            }
        }

        // Unfocused dimming: blend fg toward bg (bg already precomputed above).
        // Skip when the bg is overridden: the prompt is inline in another widget
        if !style.focused && style.bg == PromptBg::Default {
            // Dim only the content inside the box (skip all border chars).
            let dim_area = Rect {
                x: area.x + 1,
                y: content_area.y + vpad_top,
                width: area.width.saturating_sub(2),
                height: content_area.height.saturating_sub(vpad_top + info_block),
            };
            crate::engine::render::color::recede_area(buf, dim_area, bg, 0.66);
        }

        // Finalized draft stays editable during voice; while interim words show, the caret follows their end
        let cursor_pos = if !style.focused {
            None
        } else {
            // `interim_caret` is only set while the interim shows
            interim_caret.or_else(|| {
                self.textarea
                    .cursor_pos_with_state(ta_area, self.textarea_state)
            })
        };

        // Ghost suffixes (shell completion / predicted prompt)
        // Voice interim owns the end-of-text cells when shown, so skip both ghosts then
        if !voice_interim_shown {
            if let Some(ghost) = self.suggestions.ghost_text()
                && self.textarea.cursor() == self.textarea.text().len()
                && !slash_active
                && !slash_has_inline_ghost
                && let Some((cx, cy)) = cursor_pos
            {
                let avail = (ta_area.x + ta_area.width).saturating_sub(cx) as usize;
                if avail > 0 {
                    let truncated = crate::engine::render::line_utils::truncate_str(ghost, avail);
                    buf.set_string(cx, cy, &truncated, theme.ghost_text_style().bg(bg));
                }
            }

            if let Some(ghost) = self.prompt_suggestion_ghost()
                && let Some((cx, cy)) = cursor_pos
            {
                let avail = (ta_area.x + ta_area.width).saturating_sub(cx) as usize;
                if avail > 0 {
                    let truncated = crate::engine::render::line_utils::truncate_str(ghost, avail);
                    buf.set_string(cx, cy, &truncated, theme.ghost_text_style().bg(bg));
                }
            }
        }

        // Paste preview overlay: show when cursor is on or right after a paste element
        if style.focused
            && let Some(overlay) = overlay_area
            && let Some(paste_text) = self.paste_element_for_preview()
        {
            let preview_style = PreviewStyle::new(theme.paste_bg, theme.paste_fg, theme.paste_dim);
            let config = PreviewConfig {
                hint: Some(self.paste_preview_hint(&theme)),
                ..Default::default()
            };
            render_preview_overlay(buf, overlay, paste_text, preview_style, config);
        }

        let post_flush_escapes = self.render_image_preview(buf, overlay_area, style, &theme);

        PromptRenderResult {
            cursor_pos,
            post_flush_escapes,
        }
    }

    /// Caption style for text embedded in the prompt's border chrome (the info line's model name on `╰─╯` and the session title on `╭─╮`).
    /// Dimmed secondary text over the prompt bg, fading further when unfocused, so both borders read as one chrome.
    fn chrome_caption_style(bg: ratatui::style::Color, theme: &Theme, focused: bool) -> Style {
        let opacity = if focused { 0.6 } else { 0.4 };
        match crate::engine::render::color::blend_color(bg, theme.text_secondary, opacity) {
            Some(fg) => Style::default().fg(fg).bg(bg),
            // Profile palettes can't blend: the caption stays dim instead of
            // dropping to gray, with the fg pinned over the border cells
            // (accent-colored in plan mode) by muted_over_chrome.
            None => theme.muted_over_chrome().bg(bg),
        }
    }

    /// Render the info line: left-aligned `model_name · flag1 · flag2`, right-aligned `multiline`.
    /// `pub(crate)` so the dashboard's dispatch box (which draws its own chrome) can paint an identical
    /// model and mode indicator on its bottom border. Does not read `self`.
    pub fn render_info_line(
        &self,
        buf: &mut Buffer,
        area: Rect,
        info: &PromptInfo,
        bg: ratatui::style::Color,
        theme: &Theme,
        focused: bool,
    ) -> Option<Rect> {
        if area.height == 0 || area.width == 0 {
            return None;
        }

        // When the prompt is unfocused, fade info-line content further toward bg so it follows the same focused/unfocused dimming as the prompt border
        // Model name and flag color use a higher opacity than the separator so they remain readable
        let sep_opacity = if focused { 1.0 } else { 0.6 };
        let flag_opacity = if focused { 0.75 } else { 0.5 };

        let model_style = if self.status_hovered {
            Style::default().fg(theme.text_primary).bg(bg)
        } else {
            Self::chrome_caption_style(bg, theme, focused)
        };
        let sep_fg = if focused {
            theme.gray_dim
        } else {
            crate::engine::render::color::blend_color(bg, theme.gray_dim, sep_opacity)
                .unwrap_or(theme.gray_dim)
        };
        let sep_style = Style::default().fg(sep_fg).bg(bg);
        let flag_style = theme.muted().bg(bg);

        // Left side: model name and flags. Wrap with leading/trailing spaces so the cells adjacent to the
        // corner borders (╰ / ╯) are blanked out. They would otherwise show the underlying `─` glyphs from
        // the bottom-border fill. That gives 1 cell of visual padding on each side.
        let pad_style = Style::default().bg(bg);
        let mut left_spans = vec![Span::styled(" ", pad_style)];
        if let Some(warning) = info.usage_warning {
            let fg = if info.usage_warning_critical {
                theme.warning
            } else {
                sep_fg
            };
            let warning_style = Style::default().fg(fg).bg(bg);
            left_spans.push(Span::styled(warning.to_owned(), warning_style));
        }
        let mut needs_sep = info.usage_warning.is_some();
        if !info.model_name.is_empty() {
            if needs_sep {
                left_spans.push(Span::styled("  ", sep_style));
            }
            left_spans.push(Span::styled(info.model_name, model_style));
            needs_sep = true;
        }
        for flag in info.flags {
            if needs_sep {
                left_spans.push(Span::styled("  ", sep_style));
            }
            needs_sep = true;
            let mut style = if let Some(color) = flag.color {
                if flag.bold {
                    // Bold flags use full color for visibility.
                    Style::default().fg(color).bg(bg)
                } else {
                    // No dimmed variant on profile palettes (Reset bg): keep
                    // the flag's own color rather than dropping to gray.
                    let dimmed = crate::engine::render::color::blend_color(bg, color, flag_opacity)
                        .unwrap_or(color);
                    Style::default().fg(dimmed).bg(bg)
                }
            } else if flag.bold {
                Style::default().fg(theme.text_primary).bg(bg)
            } else if focused {
                flag_style
            } else {
                // Unfocused colorless flag: fade further toward bg; muted()
                // where the blend is inexpressible (terminal theme).
                match crate::engine::render::color::blend_color(bg, theme.gray, flag_opacity) {
                    Some(dimmed) => Style::default().fg(dimmed).bg(bg),
                    None => theme.muted().bg(bg),
                }
            };
            if flag.bold {
                style = style.add_modifier(Modifier::BOLD);
            }
            left_spans.push(Span::styled(flag.text, style));
        }
        // Trailing pad mirrors the leading pad above.
        left_spans.push(Span::styled(" ", pad_style));

        // Build right-side spans: "multiline" indicator.
        let mut right_spans = Vec::new();
        if info.multiline {
            right_spans.push(Span::styled(
                info.multiline_label.unwrap_or("multiline"),
                flag_style,
            ));
        }

        if !right_spans.is_empty() {
            // Pad the right side so it doesn't sit flush against the ╯ corner.
            right_spans.push(Span::styled(" ", pad_style));
            let right_line = Line::from(right_spans);
            let right_w = (right_line.width() as u16).min(area.width);
            let left_line = Line::from(left_spans);
            let left_w = (left_line.width() as u16).min(area.width.saturating_sub(right_w + 1));
            // Right-align both parts: [left][gap][right]
            let total_w = left_w + 1 + right_w; // 1 for gap
            let x = area.x + area.width.saturating_sub(total_w);
            buf.set_line_safe(x, area.y, &left_line, left_w);
            let rx = area.x + area.width.saturating_sub(right_w);
            buf.set_line_safe(rx, area.y, &right_line, right_w);
            Some(Rect::new(x, area.y, area.right().saturating_sub(x), 1))
        } else {
            // Right-align: clamp to area width so text doesn't overflow borders.
            let line = Line::from(left_spans);
            let text_w = (line.width() as u16).min(area.width);
            let x = area.x + area.width.saturating_sub(text_w);
            buf.set_line_safe(x, area.y, &line, text_w);
            Some(Rect::new(x, area.y, text_w, 1))
        }
    }
}

impl Default for PromptWidget {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse a line range string like "10" or "10-12" into a Range<usize> (1-based, exclusive end).
fn parse_line_range(s: &str) -> Option<std::ops::Range<usize>> {
    if let Some((start_s, end_s)) = s.split_once('-') {
        let start: usize = start_s.parse().ok()?;
        let end: usize = end_s.parse().ok()?;
        if start > 0 && end >= start {
            Some(start..end + 1)
        } else {
            None
        }
    } else {
        let line: usize = s.parse().ok()?;
        if line > 0 { Some(line..line + 1) } else { None }
    }
}

/// Build the styled display `Line` for a file reference element. Renders as: `@foo/bar.rs` or
/// `@foo/bar.rs:10-12`. Style: `@` and `:` in gray, path in theme.path, numbers in text_primary.
pub fn file_ref_display(path: &str) -> Line<'static> {
    let theme = Theme::current();
    let (file_part, line_part) = if let Some(colon_pos) = path.rfind(':') {
        (
            path.get(..colon_pos).unwrap_or(path),
            path.get(colon_pos + 1..),
        )
    } else {
        (path, None)
    };
    styled_file_ref(file_part, line_part, &theme, true)
}

/// Extract the display number from an `[Image #N]` or `[Image #N: <path>]` chip placeholder.
/// Returns `None` when the input doesn't match either form. The canonical emitter never produces
/// that form; the permissiveness is intentional defence in depth.
fn parse_image_display_number(placeholder: &str) -> Option<usize> {
    let inner = placeholder.strip_prefix("[Image #")?.strip_suffix(']')?;
    // Split at the first `:` (path-suffix form) or use the whole string.
    let number_str = inner.split_once(':').map(|(n, _)| n).unwrap_or(inner);
    number_str.trim().parse().ok()
}

/// Regex matching a canonical image chip placeholder anywhere in a buffer string: `[Image
/// #<digits>]` or `[Image #<digits>:`.
fn chip_placeholder_regex() -> &'static regex::Regex {
    use std::sync::LazyLock;
    static RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\[Image #\d+(?::|\])").unwrap());
    &RE
}

/// Build a `[<label>]` chip line with the shared chip styling (dim brackets, paste foreground/background).
fn chip_line(label: String) -> Line<'static> {
    let theme = Theme::current();
    let bracket = Style::default().fg(theme.paste_dim).bg(theme.paste_bg);
    let text = Style::default().fg(theme.paste_fg).bg(theme.paste_bg);
    Line::from(vec![
        Span::styled("[", bracket),
        Span::styled(label, text),
        Span::styled("]", bracket),
    ])
}

/// Normalize bare `\r`, U+2028, and U+2029 to `\n`, leaving `\r\n` intact. Rust's `str::lines()`
/// splits on none of these, so without this normalization such pastes count as a single line and
/// the invisible separators break the textarea's width model.
fn normalize_line_breaks(text: &str) -> String {
    let mut s = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' if chars.peek() != Some(&'\n') => s.push('\n'),
            '\u{2028}' | '\u{2029}' => s.push('\n'),
            _ => s.push(c),
        }
    }
    s
}

/// Build the styled display `Line` for a paste element chip.
///
/// Renders as: `[Pasted: N lines]`
fn paste_chip_display(line_count: usize) -> Line<'static> {
    chip_line(format!(
        "Pasted: {} line{}",
        line_count,
        if line_count != 1 { "s" } else { "" }
    ))
}

/// Display `Line` for a byte-triggered paste chip, e.g. `[Pasted: 12 KB]` or `[Pasted: 1.0 MB]` (size, not a line count).
/// Decimal (1000-based) units.
fn paste_chip_display_bytes(byte_len: usize) -> Line<'static> {
    let size = if byte_len >= 1_000_000 {
        format!("{:.1} MB", byte_len as f64 / 1_000_000.0)
    } else if byte_len >= 1000 {
        format!("{} KB", byte_len / 1000)
    } else {
        format!("{byte_len} bytes")
    };
    chip_line(format!("Pasted: {size}"))
}

/// Highest `display_number` present in `images`, or `0` when the slice is empty.
fn images_high_water(images: &[PastedImage]) -> usize {
    images.iter().map(|i| i.display_number).max().unwrap_or(0)
}

pub mod file_context;
mod image_state;

mod images;
mod suggestion;

#[derive(Debug, Clone)]
pub struct ChipElement {
    pub range: std::ops::Range<usize>,
    pub kind: ElementKind,
    pub display: Option<Line<'static>>,
}

#[derive(Clone, Debug, Default)]
pub struct SlashChrome {
    pub open: bool,
    pub active: bool,
    pub command_recognized: bool,
    pub command_range: Option<std::ops::Range<usize>>,
    pub args_query_is_empty: bool,
    pub args_placeholder: Option<String>,
    pub recognized_tokens: Vec<std::ops::Range<usize>>,
    pub inline_ghost: Option<SlashGhost>,
}

#[derive(Clone, Debug)]
pub struct SlashGhost {
    pub token_range: std::ops::Range<usize>,
    pub text: String,
    pub highlight: bool,
}

#[derive(Clone, Debug)]
pub struct ImagePreviewLabels {
    pub image: &'static str,
    pub format: &'static str,
    pub dimensions: &'static str,
    pub size: &'static str,
    pub path: &'static str,
    pub unavailable: &'static str,
    pub pending: &'static str,
    pub loading: &'static str,
}

impl Default for ImagePreviewLabels {
    fn default() -> Self {
        Self {
            image: "Image",
            format: "Format",
            dimensions: "Dimensions",
            size: "Size",
            path: "Path",
            unavailable: "Preview unavailable",
            pending: "Preview pending",
            loading: "Loading…",
        }
    }
}

#[cfg(feature = "inline-image")]
pub use crate::engine::render::image_overlay::overlay::{
    Escapes as PromptOverlayEscapes, PostFlush as PromptPostFlush,
};
#[cfg(not(feature = "inline-image"))]
pub use crate::engine::terminal::overlay::{
    Escapes as PromptOverlayEscapes, PostFlush as PromptPostFlush,
};

impl PromptWidget {
    #[cfg(not(feature = "inline-image"))]
    fn render_image_preview(
        &mut self,
        _buf: &mut Buffer,
        _area: Option<Rect>,
        _style: &PromptStyle,
        _theme: &Theme,
    ) -> Option<PromptOverlayEscapes> {
        None
    }

    #[cfg(feature = "inline-image")]
    fn render_image_preview(
        &mut self,
        buf: &mut Buffer,
        area: Option<Rect>,
        style: &PromptStyle,
        theme: &Theme,
    ) -> Option<PromptOverlayEscapes> {
        use crate::engine::render::image_overlay::{self, ImageLabels, Preview};
        let image = if style.image_preview && style.focused {
            self.image_for_preview()
        } else {
            None
        };
        if let (Some(area), Some(image)) = (area, image) {
            let identity = image.preview.identity();
            let display = image.display_number;
            let status = if image.preview.prepared().is_some() {
                2
            } else {
                u8::from(!image.preview.is_pending())
            };
            if self
                .overlay_image
                .as_ref()
                .is_none_or(|cache| (cache.0, cache.1, cache.2) != (identity, display, status))
            {
                let preview = match image.preview.prepared() {
                    Some((bytes, _)) => Preview::Ready(bytes.to_vec()),
                    None if image.preview.is_pending() => Preview::Pending,
                    None => Preview::Failed,
                };
                self.overlay_image = Some((
                    identity,
                    display,
                    status,
                    image_overlay::PastedImage::new(
                        display,
                        image.mime_type.clone(),
                        image.source_path.clone(),
                        image.byte_len,
                        image.preview_dimensions(),
                        preview,
                    ),
                ));
            }
            let labels = &style.image_labels;
            if let Some((_, _, _, preview)) = &self.overlay_image
                && let Some(escapes) = image_overlay::render_image_overlay(
                    buf,
                    area,
                    preview,
                    theme.paste_bg,
                    theme.paste_fg,
                    theme.paste_dim,
                    &ImageLabels {
                        image: labels.image,
                        format: labels.format,
                        dimensions: labels.dimensions,
                        size: labels.size,
                        path: labels.path,
                        unavailable: labels.unavailable,
                        pending: labels.pending,
                        loading: labels.loading,
                    },
                )
            {
                return Some(escapes);
            }
        }
        self.overlay_image = None;
        image_overlay::overlay::clear()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PermissionLabel {
    #[default]
    Ask,
    Auto,
    Custom(&'static str),
}

impl PermissionLabel {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Ask => "Ask",
            Self::Auto => "Auto",
            Self::Custom(label) => label,
        }
    }
}
pub(crate) fn prompt_blank_for_voice(text: &str) -> bool {
    text.trim().is_empty()
}
pub(crate) fn space_voice_fragment(text: &str, at: usize, fragment: &str) -> (String, bool) {
    let needs_leading = at > 0
        && text
            .get(..at)
            .is_some_and(|prefix| !prefix.ends_with(char::is_whitespace));
    let needs_trailing = at < text.len()
        && text
            .get(at..)
            .is_some_and(|suffix| !suffix.starts_with(char::is_whitespace));
    let mut out = String::with_capacity(fragment.len() + 2);
    if needs_leading {
        out.push(' ');
    }
    out.push_str(fragment);
    if needs_trailing {
        out.push(' ');
    }
    (out, needs_trailing)
}
pub fn styled_file_ref<'a>(
    path: &str,
    line_range: Option<&str>,
    theme: &Theme,
    at_prefix: bool,
) -> Line<'a> {
    let dim = theme.muted();
    let path_style = Style::default().fg(theme.path);
    let num_style = Style::default().fg(theme.gray_bright);

    let mut spans = Vec::new();
    if at_prefix {
        spans.push(Span::styled("@", dim));
    }
    spans.push(Span::styled(path.to_owned(), path_style));
    if let Some(range) = line_range {
        spans.push(Span::styled(":", dim));
        spans.push(Span::styled(range.to_owned(), num_style));
    }
    Line::from(spans)
}
