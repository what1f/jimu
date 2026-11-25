//! Runtime scrollback appearance options.

use ratatui::style::Color;

/// Background style for block content area.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum BlockBackground {
    #[default]
    None,
    Light,
    Dark,
}

/// In-memory formatting and navigation options supplied by the host.
#[derive(Debug, Clone)]
pub struct AppearanceConfig {
    pub animation: AnimationConfig,
    pub prompt: PromptViewConfig,
    pub scrollback: ScrollbackConfig,
    pub show_timestamps: bool,
    pub show_thinking_blocks: bool,
    pub group_tool_verbs: bool,
    pub collapsed_edit_blocks: bool,
    pub render_mermaid: RenderMermaid,
    pub word_separators: String,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            animation: AnimationConfig::default(),
            prompt: PromptViewConfig::default(),
            scrollback: ScrollbackConfig::default(),
            show_timestamps: true,
            show_thinking_blocks: true,
            group_tool_verbs: true,
            collapsed_edit_blocks: false,
            render_mermaid: RenderMermaid::default(),
            word_separators: super::DEFAULT_WORD_SEPARATORS.to_owned(),
        }
    }
}

/// Prompt input view configuration (the editor widget, not the scrollback block).
#[derive(Debug, Clone, Copy)]
pub struct PromptViewConfig {
    /// When true, the prompt collapses to its minimum height (single-line) when focus is in the scrollback pane. Expands back when focused.
    pub collapse_unfocused: bool,
    /// Show hover highlight box when mousing over the prompt widget.
    pub mouse_hover: bool,
    /// Show the ❯ prefix character in the prompt editor.
    pub show_prefix: bool,
    /// Compact mode: remove top padding and reduce info block padding. Toggled at runtime via `/compact-mode`.
    pub compact: bool,
}

impl Default for PromptViewConfig {
    fn default() -> Self {
        Self {
            collapse_unfocused: true,
            mouse_hover: true,
            show_prefix: true,
            compact: false,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ScrollbackConfig {
    pub layout: LayoutConfig,
    pub scrollbar: ScrollbarConfig,
    pub scroll: ScrollConfig,
    pub blocks: BlocksConfig,
    pub display: ScrollbackDisplayConfig,
}

#[derive(Debug, Clone)]
pub struct ScrollbackDisplayConfig {
    /// Render a subtle horizontal line below the last entry to mark the end of content.
    pub line_under_last_entry: bool,
    /// Accent character for collapsed groupable blocks (default: "❙").
    /// Used instead of "┃" to prevent adjacent accents from merging visually.
    pub collapsed_accent_char: String,
    /// Blend factor for dimmed accents on collapsed groupable blocks (0.0-1.0).
    /// 0.0 is invisible (fully background), 1.0 is the full accent color. Default: 0.5.
    pub dim_accent: f32,
    /// `true`: selection wraps only the collapsed sub-group; expanded blocks get their own box. `false`: the whole group.
    pub group_selection_split: bool,
    /// When true, the active-block highlight within a group extends over the selection box border columns (│).
    /// When false (default), the highlight is inset by 1 column on each side so the borders remain uncolored.
    pub highlight_overlays_border: bool,
    /// When true, the selected entry's bullet is replaced with an expand indicator (e.g., "›") if the block is foldable and collapsed.
    /// Helps indicate which entries can be expanded with 'l' or 'e'.
    /// Default: true.
    pub expandable_indicator: bool,
    /// When true, the expand indicator also shows on running entries in their minimum fold mode (e.g., Truncated for execute/thinking blocks).
    /// The indicator inherits the block's animated accent style (blinking).
    /// Default: true.
    pub expandable_indicator_running: bool,
    /// Character to use as the expand indicator. Default: "›".
    pub expandable_indicator_char: String,
    /// Show ⧉ (copy) and ↗ (view) buttons on the selection box.
    /// Default: false (opt-in while testing).
    pub selection_buttons: bool,
    /// Pin user prompts as sticky headers when scrolled past.
    /// Default: true.
    pub sticky_headers: bool,
    /// Tabs (\t) in model output are replaced with this many spaces before rendering.
    /// Default: 4. Set to 0 to pass through tabs unchanged.
    pub tab_width: u8,
    /// Maximum number of visible entries in a group of consecutive collapsed tool-call / thinking blocks.
    /// Older entries beyond this limit are hidden behind a compact "╶╶ N more" header.
    /// 0 disables group truncation. Default: 10.
    pub group_max_visible: u16,
    /// Apply UAX #9 bidi reordering in the app before painting scrollback lines.
    /// **Default false**: many terminals already reorder; enabling both double-flips Arabic/Persian.
    /// Turn on only if your terminal does not handle RTL.
    pub rtl_bidi: bool,
}

impl Default for ScrollbackDisplayConfig {
    fn default() -> Self {
        Self {
            line_under_last_entry: false,
            collapsed_accent_char: crate::engine::render::glyphs::collapsed_accent().to_string(),
            dim_accent: 0.5,
            group_selection_split: true, // Mode B by default
            highlight_overlays_border: false,
            expandable_indicator: true,
            expandable_indicator_running: true,
            expandable_indicator_char: "›".to_string(),
            selection_buttons: false,
            sticky_headers: true,
            tab_width: 4,
            group_max_visible: 10,
            rtl_bidi: false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct LayoutConfig {
    /// Vertical padding (top/bottom) for outer viewport.
    pub outer_vpad: u16,
    /// Left horizontal padding for outer viewport.
    pub outer_hpad_left: u16,
    /// Right horizontal padding for outer viewport.
    pub outer_hpad_right: u16,
    /// Padding after accent line, before content (inside block bg).
    pub block_pad_left: u16,
    /// Padding after content, at right edge (inside block bg).
    pub block_pad_right: u16,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            outer_vpad: 1,
            outer_hpad_left: 2,
            outer_hpad_right: 2,
            block_pad_left: 2,
            block_pad_right: 2, // Match left padding for symmetry
        }
    }
}

impl LayoutConfig {
    /// Minimum value for horizontal padding (must have room for selection border).
    pub const MIN_HPAD: u16 = 1;

    /// Effective outer vertical padding (0 in compact mode).
    pub fn eff_outer_vpad(&self, compact: bool) -> u16 {
        if compact { 0 } else { self.outer_vpad }
    }

    /// Effective left horizontal padding (MIN_HPAD in compact mode).
    pub fn eff_hpad_left(&self, compact: bool) -> u16 {
        if compact {
            Self::MIN_HPAD
        } else {
            self.outer_hpad_left
        }
    }

    /// Effective right horizontal padding (MIN_HPAD in compact mode).
    pub fn eff_hpad_right(&self, compact: bool) -> u16 {
        if compact {
            Self::MIN_HPAD
        } else {
            self.outer_hpad_right
        }
    }

    pub fn validated(self) -> Self {
        Self {
            outer_vpad: self.outer_vpad,
            outer_hpad_left: self.outer_hpad_left.max(Self::MIN_HPAD),
            outer_hpad_right: self.outer_hpad_right.max(Self::MIN_HPAD),
            block_pad_left: self.block_pad_left,
            block_pad_right: self.block_pad_right,
        }
    }
}

/// Scrollbar layout: `scrollbar_x = screen_right - gap_right - 1`, and content ends at `scrollbar_x - gap_left`.
/// Content width is clamped so it never extends past the outer viewport padding, whatever the gaps are.
#[derive(Debug, Clone, Copy)]
pub struct ScrollbarConfig {
    pub enabled: bool,
    /// Gap between content/selection edge and scrollbar track.
    /// At 0 the track sits against the content; higher values add space.
    pub gap_left: u16,
    /// Gap between scrollbar track and screen edge.
    /// At 0 the scrollbar sits at the screen edge, inside outer_hpad_right when that padding is nonzero.
    pub gap_right: u16,
    /// Override scrollbar background color (None uses the theme default).
    pub scrollbar_bg: Option<Color>,
    /// Override scrollbar foreground/thumb color (None uses the theme default).
    pub scrollbar_fg: Option<Color>,
}

impl Default for ScrollbarConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            gap_left: 0,  // Content adjacent to scrollbar
            gap_right: 0, // Scrollbar at screen edge
            scrollbar_bg: None,
            scrollbar_fg: None,
        }
    }
}

impl ScrollbarConfig {
    /// Total width reserved for scrollbar (gap_left + track + gap_right).
    pub fn total_width(&self) -> u16 {
        if self.enabled {
            self.gap_left + 1 + self.gap_right
        } else {
            0
        }
    }

    /// Whether scrollbar fits entirely within outer_hpad_right.
    pub fn is_outside(&self, outer_hpad_right: u16) -> bool {
        self.gap_right < outer_hpad_right
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ScrollConfig {
    /// Minimum lines of context to keep visible above/below the selected entry when navigating.
    /// 0 scrolls to the edge (default).
    pub margin: u16,
    /// Minimum scroll as a fraction of viewport height (0-100).
    /// If a scroll would be less than this percentage of the viewport, scroll by this amount instead.
    /// 0 means minimal scroll (default); 100 always scrolls a full page.
    pub min_page_fraction: u8,
    /// Follow indicator style in the gap row below scrollback.
    pub follow_indicator: FollowIndicator,
    /// When follow mode scrolls to new content, auto-select the latest entry.
    pub follow_auto_select: bool,
    /// Scrolling past the bottom (j, Ctrl-D, page-down, mousewheel) engages follow mode.
    pub follow_by_overscroll: bool,
    /// When true (default), expanding/collapsing a block adjusts scroll_offset so the block's header line stays at the same screen position.
    /// When false, uses ensure_selected_visible (the block may shift on screen).
    pub anchor_on_fold: bool,
    pub respect_manual_folds: bool,
}

impl Default for ScrollConfig {
    fn default() -> Self {
        Self {
            margin: 0,
            min_page_fraction: 0,
            follow_indicator: FollowIndicator::Center,
            follow_auto_select: true,
            follow_by_overscroll: true,
            anchor_on_fold: true,
            respect_manual_folds: false,
        }
    }
}

/// Scroll indicator display mode: the ▼ jump-to-bottom arrow below scrollback and its ▲ jump-to-response-top mirror under the sticky prompt header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FollowIndicator {
    /// No scroll indicators.
    None,
    /// Show ▼ centered in the gap row below scrollback when not following and there's content below the viewport.
    /// Show ▲ centered under the sticky prompt header while the answer being read starts above the viewport top.
    #[default]
    Center,
}

impl ScrollConfig {
    pub fn min_scroll_lines(&self, viewport_height: u16) -> u16 {
        if self.min_page_fraction == 0 {
            0
        } else {
            let fraction =
                (self.min_page_fraction.min(100) as u32) * (viewport_height as u32) / 100;
            fraction as u16
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AnimationConfig {
    /// Animation frame rate (ticks per second).
    /// Higher is smoother but uses more CPU. Default: 30.
    pub fps: u8,
    /// Rows per wave cycle for accent line animation.
    /// Lower is a faster wave; higher is slower and smoother. Default: 32.
    pub wave_rows: u16,
    /// Show an FPS counter overlay in the top-right corner (debug/dev builds only).
    pub show_fps: bool,
}

impl Default for AnimationConfig {
    fn default() -> Self {
        Self {
            fps: 30,
            wave_rows: 32,
            show_fps: false,
        }
    }
}

impl AnimationConfig {
    pub fn tick_interval(&self) -> std::time::Duration {
        let fps = self.fps.max(1) as u64;
        std::time::Duration::from_millis(1000 / fps)
    }
}

#[derive(Debug, Clone, Default)]
pub struct BlocksConfig {
    pub edit: EditBlockConfig,
    pub prompt: PromptConfig,
    pub thinking: ThinkingConfig,
    pub tool: ToolConfig,
    pub list_dir: ListDirConfig,
    pub execute: ExecuteConfig,
}

#[derive(Debug, Clone)]
pub struct EditBlockConfig {
    pub indent: bool,
    pub vpad: bool,
    pub bg: BlockBackground,
    pub accent_bg: bool,
    pub accent: Option<Color>,
    pub gutter_bg: bool,
    pub indent_bg: bool,
    /// Show the +N/-M line summary in the collapsed header.
    /// `None` (default) follows the shell-owned `collapsed_edit_blocks` flag; an explicit pager.toml value pins the shape regardless of the flag.
    pub line_summary: Option<bool>,
    /// Whether Edit blocks start expanded. Fold shape is [`Self::effective_expanded`].
    pub expanded_by_default: Option<bool>,
    /// Separator between diff hunks.
    /// Options: "…" (ellipsis, default), "───" (line), "⋯" (midline), "" (none).
    pub hunk_separator: String,
    /// Show two line-number columns (old and new) like GitHub's unified diff.
    /// When false (default), show a single column with the new-file line number.
    pub dual_line_numbers: bool,
}

impl Default for EditBlockConfig {
    fn default() -> Self {
        Self {
            indent: true,
            vpad: false,
            bg: BlockBackground::None,
            accent_bg: false,
            accent: None,
            gutter_bg: false,
            indent_bg: false,
            line_summary: None,
            expanded_by_default: None,
            hunk_separator: "…".to_string(),
            dual_line_numbers: false,
        }
    }
}

impl EditBlockConfig {
    /// A true flag collapses even when `expanded_by_default` is `Some(true)`.
    /// Flag off returns `expanded_by_default.unwrap_or(true)`, so an explicit false still collapses.
    pub fn effective_expanded(&self, collapsed_edit_blocks: bool) -> bool {
        !collapsed_edit_blocks && self.expanded_by_default.unwrap_or(true)
    }

    /// Effective collapsed-header `+N/-M` diffstat toggle. An explicit `line_summary` wins; unset follows the flag.
    /// Unset shows the diffstat exactly when the flag collapses Edits (the one-liner view is what the summary exists for).
    pub fn effective_line_summary(&self, collapsed_edit_blocks: bool) -> bool {
        self.line_summary.unwrap_or(collapsed_edit_blocks)
    }
}

/// Runtime config for user prompt block (rendered inside scrollback).
#[derive(Debug, Clone)]
pub struct PromptConfig {
    /// Whether to apply vertical padding (blank lines above/below).
    pub vpad: bool,
    /// Block background color.
    pub bg: BlockBackground,
    /// Whether accent column gets block's background.
    pub accent_bg: bool,
    /// Minimum content lines to show in truncated/sticky header mode.
    /// This is the number of actual content lines, not including vpad.
    pub min_lines: u16,
    /// Show the ❯ prefix character before the prompt text.
    pub show_prefix: bool,
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            vpad: true,
            bg: BlockBackground::Light,
            accent_bg: false,
            min_lines: 2,
            show_prefix: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ThinkingConfig {
    /// Accent color for the thinking block.
    pub accent: Color,
    /// Whether accent line is enabled. When false, no accent in any mode.
    pub accent_enabled: bool,
    /// How much to blend markdown colors with background (0.0-1.0).
    /// 0.8 means 80% original color, 20% background.
    pub bg_blend: f32,
    /// Number of visual lines to show in truncated mode (before and after ellipsis).
    pub truncated_lines: u16,
    /// Whether the accent line animates (traveling wave) while thinking is active.
    pub animate: bool,
    /// Show header line ("Thinking..." / "Thought for Xs") in all display modes.
    /// When false (default), the header only appears in collapsed mode.
    /// When true, it appears as the first line in truncated and expanded modes too.
    pub header: bool,
    /// When true, the header uses brighter styling in non-collapsed modes (matching tool block title style).
    /// It respects muted_collapsed when collapsed. When false (default), the header is always dim/muted gray.
    pub header_bright: bool,
    /// Render the reasoning body de-emphasized (SGR dim and italic) on top of the `bg_blend` fade.
    /// **Not a TOML key**: minimal mode sets it because there the fade alone cannot separate reasoning from the answer.
    pub body_dim_italic: bool,
    /// Append a dim "(ctrl+e to expand)" hint to the *collapsed* header when it fits on the same row (never adds a row).
    /// **Not a TOML key**: minimal mode sets it, the only mode where a folded block cannot be unfolded in place.
    pub collapsed_expand_hint: bool,
    /// Draw the reasoning rail inside the body lines (a `┃ ` prefix directly below the header's bullet) instead of painting the reserved accent column.
    /// The rail then sits under the diamond rather than beside it.
    /// **Not a TOML key**: minimal mode sets it so every block's bullet stays flush at column 0.
    pub rail_under_bullet: bool,
}

impl Default for ThinkingConfig {
    fn default() -> Self {
        Self {
            accent: crate::engine::theme::Theme::current().gray_dim,
            accent_enabled: true,
            bg_blend: 0.7,
            truncated_lines: 3,
            animate: true,
            header: true,
            header_bright: false,
            body_dim_italic: false,
            collapsed_expand_hint: false,
            rail_under_bullet: false,
        }
    }
}

/// Runtime config for tool call blocks (Read, Search, ListDir, etc).
#[derive(Debug, Clone)]
pub struct ToolConfig {
    /// When true, collapsed tool calls render entirely in muted gray.
    /// When false, collapsed tool calls show normal colors (paths, patterns, etc).
    pub muted_collapsed: bool,
    /// When true, parenthetical details use gray_dim (dimmest gray):
    /// Read "(1-50)", Search "(N matches)", Edit "(N edits)", Thinking "for Xs".
    /// When false, they use the normal muted gray.
    pub dim_details: bool,
    /// Bullet/icon character rendered before tool call headers.
    pub bullet: ToolBullet,
    // Bullet color is decided by BlockContent::bullet(), not config. Collapsed dimming is EntryRenderer.
    // TODO(dim_muted): add a dim factor for collapsed text styling (not just bullet/accent).
}

impl Default for ToolConfig {
    fn default() -> Self {
        Self {
            muted_collapsed: true,
            dim_details: true,
            bullet: ToolBullet::Diamond,
        }
    }
}

/// Header icon. When collapsed and `muted_collapsed` is on, the color blends with the muted palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolBullet {
    /// No bullet (default).
    #[default]
    None,
    /// `·` (middle dot, smallest).
    Dot,
    /// `•` (bullet, between dot and circle).
    SmallCircle,
    /// `●` (filled circle).
    Circle,
    /// `▸` (right-pointing small triangle).
    SmallTriangle,
    /// `▶` (right-pointing triangle).
    Triangle,
    /// `◆` (filled diamond).
    Diamond,
}

impl ToolBullet {
    /// The display character for this bullet, or `None` for no bullet.
    pub fn char(&self) -> Option<&'static str> {
        match self {
            Self::None => Option::None,
            Self::Dot => Some("·"),
            Self::SmallCircle => Some("•"),
            Self::Circle => Some(crate::engine::render::glyphs::filled_dot()),
            Self::SmallTriangle => Some("▸"),
            Self::Triangle => Some("▶"),
            // `glyphs` degrades the default scrollback bullet to the CP437 `♦` on legacy Windows consoles that can't render U+25C6
            // Tool calls, thinking, the running-subagent block, etc. all use it.
            Self::Diamond => Some(crate::engine::render::glyphs::diamond_filled()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ListDirConfig {
    /// When true, output has terminal-style dark background.
    /// When false, output has no background (default).
    pub terminal_bg: bool,
}

impl Default for ListDirConfig {
    fn default() -> Self {
        Self {
            terminal_bg: true, // Default: dark background for output
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExecuteHeaderStyle {
    /// Shell style: `$ command` (default). The `$` prompt is dim/muted.
    #[default]
    Shell,
    /// Label style: `Run command` (like Edit/Search blocks).
    /// "Run" is bold (muted when collapsed, primary when expanded).
    Label,
}

#[derive(Debug, Clone)]
pub struct ExecuteConfig {
    /// Number of output lines to show at the start in truncated mode.
    pub first_lines: u16,
    /// Number of output lines to show at the end in truncated mode.
    pub last_lines: u16,
    /// Whether accent line is enabled. When false, no accent (running/success/error).
    pub accent_enabled: bool,
    /// Accent color for running execute blocks (animated).
    pub running_accent: Color,
    /// Header display style (shell `$` vs label `Run`).
    pub header_style: ExecuteHeaderStyle,
    /// When true, command text is muted/uncolored when collapsed.
    pub muted_command_collapsed: bool,
}

impl Default for ExecuteConfig {
    fn default() -> Self {
        Self {
            first_lines: 2,
            last_lines: 3,
            accent_enabled: true,
            running_accent: crate::engine::theme::Theme::current().accent_running,
            header_style: ExecuteHeaderStyle::Label,
            muted_command_collapsed: true,
        }
    }
}

/// User preference for rendering Mermaid diagrams.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum RenderMermaid {
    /// Show the diagram's clickable affordance row. The default.
    #[default]
    Auto,
    /// Explicit opt-in; behaves identically to [`Auto`](Self::Auto).
    /// Terminal capability is never consulted; the affordance row is text and hit-rects.
    On,
    /// Show the inline diagram art alone, without the affordance row.
    Off,
}

impl RenderMermaid {
    /// Canonical persisted string (matches the settings-registry choices).
    pub fn as_canonical(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::On => "on",
            Self::Off => "off",
        }
    }

    /// Parse a canonical string, returning `None` for unrecognized input so callers can fall back to the default rather than guess.
    pub fn from_canonical(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "on" => Some(Self::On),
            "off" => Some(Self::Off),
            _ => None,
        }
    }
}

use std::sync::atomic::{AtomicU8, Ordering};

static TAB_WIDTH: AtomicU8 = AtomicU8::new(4);

/// Current tab expansion width (number of spaces per `\t`).
pub fn tab_width() -> u8 {
    TAB_WIDTH.load(Ordering::Relaxed)
}

/// Update the global tab width (called when config is loaded/reloaded).
pub fn set_tab_width(w: u8) {
    TAB_WIDTH.store(w, Ordering::Relaxed);
}

/// Replaces tabs with spaces at [`tab_width`].
/// This sits beside the width because every caller that paints a tab has to agree on it.
/// ratatui drops a cluster holding a control character, so a tab left in the text is deleted rather than drawn.
pub fn expand_tabs(text: &str) -> std::borrow::Cow<'_, str> {
    let width = tab_width();
    if width == 0 || !text.contains('\t') {
        return std::borrow::Cow::Borrowed(text);
    }
    std::borrow::Cow::Owned(text.replace('\t', &" ".repeat(usize::from(width))))
}
