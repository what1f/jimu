//! Asynchronous bridge from `jimu-js` pure-data UI Mod nodes (`UiNode`) to `jimu-tui` / `ratatui`.
//!
//! - Zero background tasks when no extension registers `ui.render`.
//! - Synchronous TUI draw hooks (`ChromeSlots`) perform only O(1) memory reads.
//! - All QuickJS `render_ui` calls run asynchronously off the render thread with Latest-Only
//!   coalescing for chrome slots and bounded caches for per-item decorations.

use jimu_js::{
    DEFAULT_UI_RENDER_TIMEOUT_MS, JsExtensionRuntime, MAX_UI_BOX_GAP, MAX_UI_BOX_PADDING, UiBorder,
    UiComponent, UiDirection, UiNode, UiTextStyle,
};
use jimu_tui::{
    engine::theme::{Theme, parse_color},
    harness::{ChromeState, SurfaceDecoration, ToolRowView},
};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::{sync::Notify, task::JoinHandle};
use unicode_width::UnicodeWidthStr;

/// Hard upper bound on rendered lines produced from a single [`UiNode`] expansion.
pub const MAX_RENDERED_UI_LINES: usize = 64;

const MAX_ITEM_CACHE_ENTRIES: usize = 256;
const MAX_PENDING_ITEM_REQUESTS: usize = 64;

/// Resolve a UI Mod color specification: checks semantic `Theme` token names first, then falls
/// back to `jimu_tui::engine::theme::parse_color` (hex, `rgb(...)`, `ansi(n)`, standard names).
pub fn resolve_ui_color(spec: &str, theme: &Theme) -> Option<Color> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return None;
    }
    let normalized = trimmed.to_ascii_lowercase().replace(['-', ' '], "_");
    let token_color = match normalized.as_str() {
        "bg" | "bg_base" => Some(theme.bg_base),
        "bg_surface" | "surface" | "bg_light" => Some(theme.bg_light),
        "bg_dark" => Some(theme.bg_dark),
        "bg_highlight" => Some(theme.bg_highlight),
        "bg_hover" => Some(theme.bg_hover),
        "bg_terminal" => Some(theme.bg_terminal),
        "bg_visual" => Some(theme.bg_visual),
        "fg" | "text_primary" | "primary" => Some(theme.text_primary),
        "text_secondary" | "secondary" => Some(theme.text_secondary),
        "text_muted" | "muted" | "gray" => Some(theme.gray),
        "dim" | "gray_dim" => Some(theme.gray_dim),
        "gray_bright" => Some(theme.gray_bright),
        "accent" | "accent_primary" | "accent_user" => Some(theme.accent_user),
        "accent_assistant" => Some(theme.accent_assistant),
        "accent_thinking" => Some(theme.accent_thinking),
        "accent_tool" => Some(theme.accent_tool),
        "accent_system" => Some(theme.accent_system),
        "accent_error" | "error" => Some(theme.accent_error),
        "accent_success" | "success" => Some(theme.accent_success),
        "accent_running" | "running" => Some(theme.running),
        "accent_skill" => Some(theme.accent_skill),
        "accent_plan" => Some(theme.accent_plan),
        "accent_verify" => Some(theme.accent_verify),
        "accent_remember" => Some(theme.accent_remember),
        "accent_model" => Some(theme.accent_model),
        "warning" | "accent_warning" => Some(theme.warning),
        "command" => Some(theme.command),
        "path" => Some(theme.path),
        "border" | "prompt_border" => Some(theme.prompt_border),
        "border_focus" | "prompt_border_active" => Some(theme.prompt_border_active),
        _ => None,
    };
    token_color.or_else(|| parse_color(trimmed))
}

fn text_style_to_ratatui(style: &UiTextStyle, theme: &Theme) -> Style {
    let mut out = Style::default();
    if let Some(fg) = style
        .color
        .as_deref()
        .and_then(|c| resolve_ui_color(c, theme))
    {
        out = out.fg(fg);
    }
    if let Some(bg) = style.bg.as_deref().and_then(|c| resolve_ui_color(c, theme)) {
        out = out.bg(bg);
    }
    if style.bold == Some(true) {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.dim == Some(true) {
        out = out.add_modifier(Modifier::DIM);
    }
    if style.italic == Some(true) {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if style.underline == Some(true) {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    out
}

/// Translate a pure-data [`UiNode`] tree into styled `ratatui` lines, capped at
/// [`MAX_RENDERED_UI_LINES`] to bound allocations on deep or wide trees.
///
/// Any [`UiNode::Native`] node expands into `native_lines`, supporting onion-style wrapping
/// (`box([next(e), text(...)])`) as well as complete replacements.
pub fn render_node_to_lines(
    node: &UiNode,
    native_lines: &[Line<'static>],
    theme: &Theme,
    width: u16,
) -> Vec<Line<'static>> {
    render_node_to_lines_capped(node, native_lines, theme, width, MAX_RENDERED_UI_LINES)
}

fn render_node_to_lines_capped(
    node: &UiNode,
    native_lines: &[Line<'static>],
    theme: &Theme,
    width: u16,
    max_lines: usize,
) -> Vec<Line<'static>> {
    if max_lines == 0 {
        return Vec::new();
    }
    match node {
        UiNode::Native => native_lines.iter().take(max_lines).cloned().collect(),
        UiNode::Text { text, style } => {
            let ratatui_style = text_style_to_ratatui(style, theme);
            if text.is_empty() {
                return Vec::new();
            }
            text.split('\n')
                .take(max_lines)
                .map(|segment| Line::from(Span::styled(segment.to_owned(), ratatui_style)))
                .collect()
        }
        UiNode::Divider { ch, color } => {
            let w = usize::from(width);
            if w == 0 {
                return Vec::new();
            }
            let unit = ch.as_deref().filter(|s| !s.is_empty()).unwrap_or("─");
            let unit_w = UnicodeWidthStr::width(unit).max(1);
            let repeats = (w / unit_w).max(1);
            let mut line_text = String::with_capacity(unit.len() * repeats);
            for _ in 0..repeats {
                line_text.push_str(unit);
            }
            let fg = color
                .as_deref()
                .and_then(|c| resolve_ui_color(c, theme))
                .unwrap_or(theme.prompt_border);
            vec![Line::from(Span::styled(line_text, Style::default().fg(fg)))]
        }
        UiNode::Box {
            direction,
            gap,
            border,
            border_color,
            bg,
            padding_left,
            padding_right,
            children,
        } => {
            let clamped_gap = (*gap).min(MAX_UI_BOX_GAP);
            let clamped_pad_l = (*padding_left).min(MAX_UI_BOX_PADDING);
            let clamped_pad_r = (*padding_right).min(MAX_UI_BOX_PADDING);
            let border_overhead: u16 = match border {
                UiBorder::Left => 2,
                UiBorder::All => 4,
                UiBorder::None | UiBorder::Top | UiBorder::Bottom => 0,
            };
            let inner_width = width
                .saturating_sub(border_overhead)
                .saturating_sub(clamped_pad_l)
                .saturating_sub(clamped_pad_r);

            let mut lines: Vec<Line<'static>> = match direction {
                UiDirection::Column => {
                    let mut out = Vec::new();
                    let gap_lines = usize::from(clamped_gap);
                    for child in children {
                        if out.len() >= max_lines {
                            break;
                        }
                        let remaining = max_lines - out.len();
                        let rendered = render_node_to_lines_capped(
                            child,
                            native_lines,
                            theme,
                            inner_width,
                            remaining,
                        );
                        if rendered.is_empty() {
                            continue;
                        }
                        if !out.is_empty() && gap_lines > 0 {
                            for _ in 0..gap_lines {
                                if out.len() >= max_lines {
                                    break;
                                }
                                out.push(Line::default());
                            }
                        }
                        let take = max_lines.saturating_sub(out.len());
                        out.extend(rendered.into_iter().take(take));
                    }
                    out
                }
                UiDirection::Row => {
                    let mut row_spans: Vec<Span<'static>> = Vec::new();
                    let gap_str = " ".repeat(usize::from(clamped_gap));
                    let mut has_prev = false;
                    for child in children {
                        let rendered = render_node_to_lines_capped(
                            child,
                            native_lines,
                            theme,
                            inner_width,
                            max_lines,
                        );
                        if rendered.is_empty() {
                            continue;
                        }
                        if has_prev && !gap_str.is_empty() {
                            row_spans.push(Span::raw(gap_str.clone()));
                        }
                        for (idx, line) in rendered.into_iter().enumerate() {
                            if idx > 0 {
                                row_spans.push(Span::raw(" "));
                            }
                            row_spans.extend(line.spans);
                        }
                        has_prev = true;
                    }
                    if row_spans.is_empty() {
                        Vec::new()
                    } else {
                        vec![Line::from(row_spans)]
                    }
                }
            };

            let bg_color = bg.as_deref().and_then(|c| resolve_ui_color(c, theme));
            let pad_l = usize::from(clamped_pad_l);
            let pad_r = usize::from(clamped_pad_r);

            if pad_l > 0 || pad_r > 0 || bg_color.is_some() {
                let bg_style = bg_color.map_or_else(Style::default, |c| Style::default().bg(c));
                for line in &mut lines {
                    let mut spans = Vec::with_capacity(line.spans.len() + 2);
                    if pad_l > 0 {
                        spans.push(Span::styled(" ".repeat(pad_l), bg_style));
                    }
                    for mut span in std::mem::take(&mut line.spans) {
                        if let Some(c) = bg_color
                            && span.style.bg.is_none()
                        {
                            span.style = span.style.bg(c);
                        }
                        spans.push(span);
                    }
                    if pad_r > 0 {
                        spans.push(Span::styled(" ".repeat(pad_r), bg_style));
                    }
                    line.spans = spans;
                }
            }

            let mut bordered = apply_box_border(
                lines,
                *border,
                border_color.as_deref(),
                bg_color,
                theme,
                width,
            );
            bordered.truncate(max_lines);
            bordered
        }
    }
}

fn apply_box_border(
    mut lines: Vec<Line<'static>>,
    border: UiBorder,
    border_color: Option<&str>,
    bg_color: Option<Color>,
    theme: &Theme,
    width: u16,
) -> Vec<Line<'static>> {
    if border == UiBorder::None {
        return lines;
    }
    let fg = border_color
        .and_then(|c| resolve_ui_color(c, theme))
        .unwrap_or(theme.prompt_border);
    let mut bstyle = Style::default().fg(fg);
    if let Some(bg) = bg_color {
        bstyle = bstyle.bg(bg);
    }

    match border {
        UiBorder::None => lines,
        UiBorder::Left => {
            for line in &mut lines {
                let mut spans = Vec::with_capacity(line.spans.len() + 1);
                spans.push(Span::styled("│ ", bstyle));
                spans.extend(std::mem::take(&mut line.spans));
                line.spans = spans;
            }
            lines
        }
        UiBorder::Top => {
            let rule = Line::from(Span::styled("─".repeat(usize::from(width).max(1)), bstyle));
            let mut out = Vec::with_capacity(lines.len() + 1);
            out.push(rule);
            out.extend(lines);
            out
        }
        UiBorder::Bottom => {
            lines.push(Line::from(Span::styled(
                "─".repeat(usize::from(width).max(1)),
                bstyle,
            )));
            lines
        }
        UiBorder::All => {
            let inner = usize::from(width).saturating_sub(2).max(1);
            let top = Line::from(Span::styled(format!("┌{}┐", "─".repeat(inner)), bstyle));
            let bottom = Line::from(Span::styled(format!("└{}┘", "─".repeat(inner)), bstyle));
            let mut out = Vec::with_capacity(lines.len() + 2);
            out.push(top);
            for mut line in lines {
                let used: usize = line
                    .spans
                    .iter()
                    .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                    .sum();
                let target_content = inner.saturating_sub(2);
                let pad = target_content.saturating_sub(used);
                let mut spans = Vec::with_capacity(line.spans.len() + 3);
                spans.push(Span::styled("│ ", bstyle));
                spans.extend(std::mem::take(&mut line.spans));
                if pad > 0 {
                    let fill_style =
                        bg_color.map_or_else(Style::default, |c| Style::default().bg(c));
                    spans.push(Span::styled(" ".repeat(pad), fill_style));
                }
                spans.push(Span::styled(" │", bstyle));
                out.push(Line::from(spans));
            }
            out.push(bottom);
            out
        }
    }
}

fn contains_native(node: &UiNode) -> bool {
    match node {
        UiNode::Native => true,
        UiNode::Text { .. } | UiNode::Divider { .. } => false,
        UiNode::Box { children, .. } => children.iter().any(contains_native),
    }
}

/// Translate a [`UiNode`] returned for `UserMessage` or `ToolRow` into a [`SurfaceDecoration`].
///
/// - Plain `UiNode::Native` returns `None` (zero overhead).
/// - A `UiNode::Box` wrapping `UiNode::Native` (onion middleware `box([next(e), ...])`) splits
///   cleanly into `prefix_lines`, `suffix_lines`, and `left_border`, preserving native syntax
///   highlighting, image chips, and diff rendering.
/// - Any tree without `UiNode::Native` returns `replace: Some(...)`.
pub fn render_node_to_decoration(
    node: &UiNode,
    theme: &Theme,
    width: u16,
) -> Option<SurfaceDecoration> {
    if matches!(node, UiNode::Native) {
        return None;
    }
    if !contains_native(node) {
        return Some(SurfaceDecoration {
            replace: Some(render_node_to_lines(node, &[], theme, width)),
            ..Default::default()
        });
    }

    let mut dec = SurfaceDecoration::default();
    extract_native_decoration(node, theme, width, &mut dec);
    if dec.prefix_lines.is_empty()
        && dec.suffix_lines.is_empty()
        && dec.left_border.is_none()
        && dec.replace.is_none()
    {
        None
    } else {
        Some(dec)
    }
}

fn extract_native_decoration(
    node: &UiNode,
    theme: &Theme,
    width: u16,
    out: &mut SurfaceDecoration,
) {
    let UiNode::Box {
        direction,
        gap,
        border,
        border_color,
        padding_left,
        padding_right,
        children,
        ..
    } = node
    else {
        return;
    };

    let border_fg = border_color
        .as_deref()
        .and_then(|c| resolve_ui_color(c, theme))
        .unwrap_or(theme.prompt_border);
    let rule_line = || {
        Line::from(Span::styled(
            "─".repeat(usize::from(width).max(1)),
            Style::default().fg(border_fg),
        ))
    };

    match border {
        UiBorder::Left => {
            if out.left_border.is_none() {
                out.left_border = Some(border_fg);
            }
        }
        UiBorder::Top => {
            out.prefix_lines.push(rule_line());
        }
        UiBorder::Bottom => {}
        UiBorder::All => {
            if out.left_border.is_none() {
                out.left_border = Some(border_fg);
            }
            out.prefix_lines.push(rule_line());
        }
        UiBorder::None => {}
    }

    let inner_width = width
        .saturating_sub((*padding_left).min(MAX_UI_BOX_PADDING))
        .saturating_sub((*padding_right).min(MAX_UI_BOX_PADDING));

    let native_indices: Vec<usize> = children
        .iter()
        .enumerate()
        .filter_map(|(idx, child)| contains_native(child).then_some(idx))
        .collect();

    if let Some(&native_idx) = native_indices.first() {
        let gap_lines = if *direction == UiDirection::Column {
            usize::from((*gap).min(MAX_UI_BOX_GAP))
        } else {
            0
        };

        let mut prefix_added = false;
        for child in &children[..native_idx] {
            let rendered = render_node_to_lines(child, &[], theme, inner_width);
            if !rendered.is_empty() {
                if prefix_added && gap_lines > 0 {
                    for _ in 0..gap_lines {
                        out.prefix_lines.push(Line::default());
                    }
                }
                out.prefix_lines.extend(rendered);
                prefix_added = true;
            }
        }
        if prefix_added && gap_lines > 0 {
            for _ in 0..gap_lines {
                out.prefix_lines.push(Line::default());
            }
        }

        extract_native_decoration(&children[native_idx], theme, inner_width, out);

        let mut suffix_group = Vec::new();
        for child in &children[native_idx + 1..] {
            let rendered = render_node_to_lines(child, &[], theme, inner_width);
            if !rendered.is_empty() {
                if !suffix_group.is_empty() && gap_lines > 0 {
                    for _ in 0..gap_lines {
                        suffix_group.push(Line::default());
                    }
                }
                suffix_group.extend(rendered);
            }
        }
        if !suffix_group.is_empty() {
            if gap_lines > 0 {
                for _ in 0..gap_lines {
                    out.suffix_lines.push(Line::default());
                }
            }
            out.suffix_lines.extend(suffix_group);
        }
    }

    if matches!(border, UiBorder::Bottom | UiBorder::All) {
        out.suffix_lines.push(rule_line());
    }
    out.prefix_lines.truncate(MAX_RENDERED_UI_LINES);
    out.suffix_lines.truncate(MAX_RENDERED_UI_LINES);
}

/// Convert a [`UiNode`] returned for `UiComponent::Spinner` into an inline `Span`.
pub fn render_node_to_spinner_span(node: &UiNode, theme: &Theme) -> Option<Span<'static>> {
    if matches!(node, UiNode::Native) {
        return None;
    }
    let lines = render_node_to_lines(node, &[], theme, 32);
    let first = lines.into_iter().find(|l| !l.spans.is_empty())?;
    let mut style = Style::default();
    let mut text = String::new();
    for (idx, span) in first.spans.into_iter().enumerate() {
        if idx == 0 {
            style = span.style;
        }
        text.push_str(span.content.as_ref());
    }
    (!text.is_empty()).then_some(Span::styled(text, style))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct UserMessageKey {
    text_hash: u64,
    image_count: usize,
    width: u16,
    generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ToolRowKey {
    name: String,
    running: bool,
    is_error: bool,
    width: u16,
    generation: u64,
}

#[derive(Default)]
struct UiModCache {
    slots: HashMap<UiComponent, (u64, UiNode)>,
    user_messages: HashMap<UserMessageKey, UiNode>,
    user_order: VecDeque<UserMessageKey>,
    tool_rows: HashMap<ToolRowKey, UiNode>,
    tool_order: VecDeque<ToolRowKey>,
}

impl UiModCache {
    fn clear(&mut self) {
        self.slots.clear();
        self.user_messages.clear();
        self.user_order.clear();
        self.tool_rows.clear();
        self.tool_order.clear();
    }

    fn insert_user(&mut self, key: UserMessageKey, node: UiNode) -> bool {
        let changed = self.user_messages.get(&key) != Some(&node);
        if !self.user_messages.contains_key(&key) {
            if self.user_order.len() >= MAX_ITEM_CACHE_ENTRIES
                && let Some(old) = self.user_order.pop_front()
            {
                self.user_messages.remove(&old);
            }
            self.user_order.push_back(key.clone());
        }
        self.user_messages.insert(key, node);
        changed
    }

    fn insert_tool(&mut self, key: ToolRowKey, node: UiNode) -> bool {
        let changed = self.tool_rows.get(&key) != Some(&node);
        if !self.tool_rows.contains_key(&key) {
            if self.tool_order.len() >= MAX_ITEM_CACHE_ENTRIES
                && let Some(old) = self.tool_order.pop_front()
            {
                self.tool_rows.remove(&old);
            }
            self.tool_order.push_back(key.clone());
        }
        self.tool_rows.insert(key, node);
        changed
    }
}

enum PendingItemWork {
    UserMessage { key: UserMessageKey, props: Value },
    ToolRow { key: ToolRowKey, props: Value },
}

#[derive(Default)]
struct PendingUiWork {
    /// Latest-Only map for chrome slots (`StatusLine`, `AbovePrompt`, `Header`, `Footer`, `Spinner`).
    slots: HashMap<UiComponent, (u64, Value)>,
    slot_queued_keys: HashMap<UiComponent, u64>,
    /// Bounded queue for per-item decorations (`UserMessage`, `ToolRow`).
    items: VecDeque<PendingItemWork>,
    queued_users: HashSet<UserMessageKey>,
    queued_tools: HashSet<ToolRowKey>,
}

/// Asynchronous cache bridge between [`JsExtensionRuntime`] UI Mods and `jimu-tui` [`ChromeSlots`].
pub struct UiModBridge {
    runtime: Arc<RwLock<Option<Arc<JsExtensionRuntime>>>>,
    on_chrome_update: Arc<dyn Fn() + Send + Sync>,
    on_history_update: Arc<dyn Fn() + Send + Sync>,
    cache: Arc<RwLock<UiModCache>>,
    pending: Arc<Mutex<PendingUiWork>>,
    notify: Arc<Notify>,
    epoch: Arc<AtomicU64>,
    stopped: Arc<AtomicBool>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl UiModBridge {
    /// Create a bridge for `runtime` using a single update callback for both chrome and history.
    /// Spawns a background worker task ONLY when `runtime` has at least one registered `ui.render`
    /// interceptor (zero threads/tasks otherwise).
    pub fn new(runtime: Arc<JsExtensionRuntime>, on_update: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self::with_callbacks(Some(runtime), Arc::clone(&on_update), on_update)
    }

    /// Create a bridge with separate callbacks for lightweight chrome redraws (`StatusLine`,
    /// `AbovePrompt`, `Header`, `Footer`, `Spinner`) and scrollback history invalidations
    /// (`UserMessage`, `ToolRow`), supporting an optional or hot-swappable runtime.
    pub fn with_callbacks(
        runtime: Option<Arc<JsExtensionRuntime>>,
        on_chrome_update: Arc<dyn Fn() + Send + Sync>,
        on_history_update: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let bridge = Self {
            runtime: Arc::new(RwLock::new(runtime)),
            on_chrome_update,
            on_history_update,
            cache: Arc::new(RwLock::new(UiModCache::default())),
            pending: Arc::new(Mutex::new(PendingUiWork::default())),
            notify: Arc::new(Notify::new()),
            epoch: Arc::new(AtomicU64::new(0)),
            stopped: Arc::new(AtomicBool::new(false)),
            task: Mutex::new(None),
        };
        bridge.sync_worker_state();
        bridge
    }

    fn current_runtime(&self) -> Option<Arc<JsExtensionRuntime>> {
        self.runtime
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// True if any loaded JS extension registered a `ui.render` interceptor.
    pub fn is_active(&self) -> bool {
        self.current_runtime().is_some_and(|rt| rt.has_ui_render())
    }

    /// Replace (or clear) the underlying JS runtime, flushing cached nodes and starting or
    /// stopping the background render worker accordingly.
    pub fn set_runtime(&self, runtime: Option<Arc<JsExtensionRuntime>>) {
        *self.runtime.write().unwrap_or_else(|e| e.into_inner()) = runtime;
        self.sync_after_reload();
    }

    /// Clear cached nodes after an extension reload and ensure the worker matches `has_ui_render`.
    pub fn sync_after_reload(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.cache
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        {
            let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            p.slots.clear();
            p.slot_queued_keys.clear();
            p.items.clear();
            p.queued_users.clear();
            p.queued_tools.clear();
        }
        self.sync_worker_state();
    }

    fn sync_worker_state(&self) {
        let active = self.is_active() && !self.stopped.load(Ordering::Acquire);
        let mut slot = self.task.lock().unwrap_or_else(|e| e.into_inner());
        if !active {
            if let Some(handle) = slot.take() {
                handle.abort();
            }
            return;
        }
        if slot.is_some() {
            return;
        }
        let runtime = Arc::clone(&self.runtime);
        let on_chrome_update = Arc::clone(&self.on_chrome_update);
        let on_history_update = Arc::clone(&self.on_history_update);
        let cache = Arc::clone(&self.cache);
        let pending = Arc::clone(&self.pending);
        let notify = Arc::clone(&self.notify);
        let epoch = Arc::clone(&self.epoch);
        let stopped = Arc::clone(&self.stopped);

        *slot = Some(tokio::spawn(async move {
            worker_loop(
                runtime,
                on_chrome_update,
                on_history_update,
                cache,
                pending,
                notify,
                epoch,
                stopped,
            )
            .await;
        }));
    }

    /// O(1) lookup for a chrome slot (`StatusLine`, `AbovePrompt`, `Header`, `Footer`, `Spinner`).
    /// Schedules an asynchronous QuickJS render when stale/missing and returns the last cached
    /// node immediately so drawing never blocks or flickers.
    pub fn slot_node(&self, component: UiComponent, state: &ChromeState<'_>) -> Option<UiNode> {
        let rt = self.current_runtime()?;
        if !rt.has_ui_render() {
            return None;
        }
        let generation = rt.ui_generation();
        let key = chrome_state_fingerprint(component, state, generation);

        let cached_entry = {
            let guard = self.cache.read().unwrap_or_else(|e| e.into_inner());
            guard.slots.get(&component).cloned()
        };

        if let Some((cached_key, node)) = &cached_entry
            && *cached_key == key
        {
            return Some(node.clone());
        }

        // Enqueue Latest-Only update if not already queued for this key.
        {
            let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            if p.slot_queued_keys.get(&component).copied() != Some(key) {
                p.slot_queued_keys.insert(component, key);
                p.slots.insert(component, (key, chrome_state_props(state)));
                drop(p);
                self.notify.notify_one();
            }
        }

        cached_entry.map(|(_, node)| node)
    }

    /// Render a chrome slot through `UiModBridge`. Returns `None` when inactive, uncached, or
    /// when the extension returned plain `UiNode::Native`.
    pub fn render_chrome_slot(
        &self,
        component: UiComponent,
        state: &ChromeState<'_>,
        native_lines: &[Line<'static>],
    ) -> Option<Vec<Line<'static>>> {
        let node = self.slot_node(component, state)?;
        if matches!(node, UiNode::Native) {
            return None;
        }
        Some(render_node_to_lines(
            &node,
            native_lines,
            state.theme,
            state.width,
        ))
    }

    /// Render a `Spinner` override span if registered and non-`Native`.
    pub fn spinner_override(&self, state: &ChromeState<'_>) -> Option<Span<'static>> {
        let node = self.slot_node(UiComponent::Spinner, state)?;
        render_node_to_spinner_span(&node, state.theme)
    }

    /// O(1) lookup for a `UserMessage` surface decoration; schedules async render on cache miss.
    pub fn user_message_decoration(
        &self,
        text: &str,
        image_count: usize,
        width: u16,
        theme: &Theme,
    ) -> Option<SurfaceDecoration> {
        let rt = self.current_runtime()?;
        if !rt.has_ui_render() {
            return None;
        }
        let key = UserMessageKey {
            text_hash: hash_str(text),
            image_count,
            width,
            generation: rt.ui_generation(),
        };

        if let Some(node) = self
            .cache
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .user_messages
            .get(&key)
            .cloned()
        {
            return render_node_to_decoration(&node, theme, width);
        }

        {
            let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            if p.items.len() < MAX_PENDING_ITEM_REQUESTS && p.queued_users.insert(key.clone()) {
                let props = json!({
                    "text": text,
                    "image_count": image_count,
                    "width": width,
                });
                p.items
                    .push_back(PendingItemWork::UserMessage { key, props });
                drop(p);
                self.notify.notify_one();
            }
        }
        None
    }

    /// O(1) lookup for a `ToolRow` surface decoration; schedules async render on cache miss.
    pub fn tool_row_decoration(
        &self,
        row: &ToolRowView<'_>,
        theme: &Theme,
    ) -> Option<SurfaceDecoration> {
        let rt = self.current_runtime()?;
        if !rt.has_ui_render() {
            return None;
        }
        let key = ToolRowKey {
            name: row.name.to_owned(),
            running: row.running,
            is_error: row.is_error,
            width: row.width,
            generation: rt.ui_generation(),
        };

        if let Some(node) = self
            .cache
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .tool_rows
            .get(&key)
            .cloned()
        {
            return render_node_to_decoration(&node, theme, row.width);
        }

        {
            let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            if p.items.len() < MAX_PENDING_ITEM_REQUESTS && p.queued_tools.insert(key.clone()) {
                let props = json!({
                    "name": row.name,
                    "running": row.running,
                    "is_error": row.is_error,
                    "width": row.width,
                });
                p.items.push_back(PendingItemWork::ToolRow { key, props });
                drop(p);
                self.notify.notify_one();
            }
        }
        None
    }
}

impl Drop for UiModBridge {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        self.notify.notify_one();
        if let Some(handle) = self.task.lock().unwrap_or_else(|e| e.into_inner()).take() {
            handle.abort();
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn worker_loop(
    runtime_slot: Arc<RwLock<Option<Arc<JsExtensionRuntime>>>>,
    on_chrome_update: Arc<dyn Fn() + Send + Sync>,
    on_history_update: Arc<dyn Fn() + Send + Sync>,
    cache: Arc<RwLock<UiModCache>>,
    pending: Arc<Mutex<PendingUiWork>>,
    notify: Arc<Notify>,
    epoch: Arc<AtomicU64>,
    stopped: Arc<AtomicBool>,
) {
    loop {
        if stopped.load(Ordering::Acquire) {
            break;
        }

        let batch_epoch = epoch.load(Ordering::Acquire);
        // Drain all latest-only chrome slots first, then pending item decorations.
        let (slot_batch, item_batch) = {
            let mut p = pending.lock().unwrap_or_else(|e| e.into_inner());
            let slots: Vec<(UiComponent, u64, Value)> = p
                .slots
                .drain()
                .map(|(comp, (key, props))| (comp, key, props))
                .collect();
            let items: Vec<PendingItemWork> = p.items.drain(..).collect();
            (slots, items)
        };

        if slot_batch.is_empty() && item_batch.is_empty() {
            notify.notified().await;
            continue;
        }

        let Some(runtime) = runtime_slot
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        else {
            continue;
        };

        let mut chrome_changed = false;
        let mut history_changed = false;

        for (component, key, props) in slot_batch {
            if stopped.load(Ordering::Acquire) || epoch.load(Ordering::Acquire) != batch_epoch {
                break;
            }
            let node = runtime
                .render_ui(component, props, DEFAULT_UI_RENDER_TIMEOUT_MS)
                .await
                .unwrap_or(UiNode::Native);
            if epoch.load(Ordering::Acquire) != batch_epoch {
                break;
            }
            let changed = {
                let mut guard = cache.write().unwrap_or_else(|e| e.into_inner());
                let prev_node = guard.slots.get(&component).map(|(_, n)| n);
                let changed = prev_node != Some(&node);
                guard.slots.insert(component, (key, node));
                changed
            };
            if changed {
                chrome_changed = true;
            }
        }

        for item in item_batch {
            if stopped.load(Ordering::Acquire) || epoch.load(Ordering::Acquire) != batch_epoch {
                break;
            }
            match item {
                PendingItemWork::UserMessage { key, props } => {
                    let node = runtime
                        .render_ui(
                            UiComponent::UserMessage,
                            props,
                            DEFAULT_UI_RENDER_TIMEOUT_MS,
                        )
                        .await
                        .unwrap_or(UiNode::Native);
                    pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .queued_users
                        .remove(&key);
                    if epoch.load(Ordering::Acquire) != batch_epoch {
                        break;
                    }
                    let changed = cache
                        .write()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert_user(key, node);
                    if changed {
                        history_changed = true;
                    }
                }
                PendingItemWork::ToolRow { key, props } => {
                    let node = runtime
                        .render_ui(UiComponent::ToolRow, props, DEFAULT_UI_RENDER_TIMEOUT_MS)
                        .await
                        .unwrap_or(UiNode::Native);
                    pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .queued_tools
                        .remove(&key);
                    if epoch.load(Ordering::Acquire) != batch_epoch {
                        break;
                    }
                    let changed = cache
                        .write()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert_tool(key, node);
                    if changed {
                        history_changed = true;
                    }
                }
            }
        }

        if !stopped.load(Ordering::Acquire) && epoch.load(Ordering::Acquire) == batch_epoch {
            if history_changed {
                on_history_update();
            } else if chrome_changed {
                on_chrome_update();
            }
        }
    }
}

fn hash_str(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn chrome_state_fingerprint(
    component: UiComponent,
    state: &ChromeState<'_>,
    generation: u64,
) -> u64 {
    let mut h = DefaultHasher::new();
    component.hash(&mut h);
    generation.hash(&mut h);
    state.running.hash(&mut h);
    state.model_name.hash(&mut h);
    state.queued_count.hash(&mut h);
    state.width.hash(&mut h);
    state.cwd.hash(&mut h);
    state.git_branch.hash(&mut h);
    state.effort.hash(&mut h);
    state.permission_mode.hash(&mut h);
    state.fast_mode.hash(&mut h);
    state.session_id.hash(&mut h);
    state.usage.input_tokens.hash(&mut h);
    state.usage.output_tokens.hash(&mut h);
    state.usage.cache_read_tokens.hash(&mut h);
    state.usage.cache_write_tokens.hash(&mut h);
    ((state.usage.cost_usd * 10_000.0).round() as i64).hash(&mut h);
    if let Some(ctx) = state.context {
        ctx.context_window.hash(&mut h);
        ctx.tokens.unwrap_or(ctx.estimated_tokens).hash(&mut h);
        ctx.percent.map(|p| (p * 10.0).round() as i64).hash(&mut h);
    }
    h.finish()
}

fn chrome_state_props(state: &ChromeState<'_>) -> Value {
    let context_json = state.context.map(|ctx| {
        let used = ctx.tokens.unwrap_or(ctx.estimated_tokens);
        let pct = ctx.percent.unwrap_or_else(|| {
            if ctx.context_window > 0 {
                (used as f64 / f64::from(ctx.context_window)) * 100.0
            } else {
                0.0
            }
        });
        json!({
            "tokens": used,
            "context_window": ctx.context_window,
            "percent": (pct * 10.0).round() / 10.0,
        })
    });
    json!({
        "running": state.running,
        "model": state.model_name,
        "queued_count": state.queued_count,
        "width": state.width,
        "cwd": state.cwd.to_string_lossy(),
        "git_branch": state.git_branch,
        "effort": state.effort,
        "permission_mode": state.permission_mode,
        "fast_mode": state.fast_mode,
        "session_id": state.session_id,
        "usage": {
            "input_tokens": state.usage.input_tokens,
            "output_tokens": state.usage.output_tokens,
            "cache_read_tokens": state.usage.cache_read_tokens,
            "cache_write_tokens": state.usage.cache_write_tokens,
            "total_tokens": state.usage.total_tokens,
            "cost_usd": state.usage.cost_usd,
        },
        "context": context_json,
    })
}
