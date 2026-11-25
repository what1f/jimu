// Adapted from the Grok Build pager (Apache-2.0). Copyright 2023-2026 SpaceXAI.

//! The bar accepts `&[HintItem]` from any source (action registry, prompt widget, scrollback state); each view builds its own hints dynamically.
//!
//! When a `PendingAction` is active (double-press confirmation), the bar replaces all hints with "press again to {label}".

use std::borrow::Cow;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

use crate::engine::input::key::KeyShortcut;
use crate::engine::theme::Theme;

/// A single hint for the shortcuts bar. It carries the keys as structured data; the bar decides how
/// they render. Views build these dynamically from the registry, widget keymaps, or local state.
#[derive(Debug, Clone)]
pub struct HintItem {
    /// Keys to display. Multiple keys are shown joined with "/" (e.g., j/k).
    pub keys: Vec<KeyShortcut>,
    /// Short label for the bottom bar (e.g., "send", "nav", "cancel").
    pub label: Cow<'static, str>,
    /// Optional custom key text, used when the bar paints a single key (and in the cheatsheet).
    /// With more than one key (`keys.len() > 1`) the bar ignores this and builds the text from the structured keys.
    pub custom_display: Option<&'static str>,
    /// Longer description for the all-shortcuts cheatsheet (e.g., "Send prompt to agent").
    /// When `None`, falls back to `label`.
    pub description: Option<Cow<'static, str>>,
    /// When true, the hint survives compact-mode truncation and renders regardless of `max_visible`.
    /// Use for hints that should be discoverable in every scrollback context (e.g. nav, turn, mode).
    pub pinned: bool,
}

impl HintItem {
    /// Single-key hint.
    pub fn new(key: KeyShortcut, label: impl Into<Cow<'static, str>>) -> Self {
        Self {
            keys: vec![key],
            label: label.into(),
            custom_display: None,
            description: None,
            pinned: false,
        }
    }

    /// Paired-key hint (e.g., j/k for nav, h/l for turn).
    pub fn paired(a: KeyShortcut, b: KeyShortcut, label: impl Into<Cow<'static, str>>) -> Self {
        Self {
            keys: vec![a, b],
            label: label.into(),
            custom_display: None,
            description: None,
            pinned: false,
        }
    }

    /// Mark this hint as pinned: it is always shown in the compact shortcuts bar, even when the hint list exceeds `max_visible`.
    pub fn pinned(mut self) -> Self {
        self.pinned = true;
        self
    }

    fn bar_key_display(&self) -> String {
        bar_key_segments(self)
            .into_iter()
            .map(|seg| seg.text)
            .collect()
    }
}

struct BarKeySeg {
    text: String,
    is_join: bool,
}

fn bar_key_segments(hint: &HintItem) -> Vec<BarKeySeg> {
    if hint.keys.len() <= 1 {
        let text = if let Some(d) = hint.custom_display {
            d.to_string()
        } else if let Some(k) = hint.keys.first() {
            k.display()
        } else {
            String::new()
        };
        if text.is_empty() {
            return Vec::new();
        }
        return vec![BarKeySeg {
            text,
            is_join: false,
        }];
    }

    let Some(first) = hint.keys.first() else {
        return Vec::new();
    };
    let shared = hint.keys.iter().all(|k| k.modifiers == first.modifiers);
    let mut segs = Vec::new();
    if shared {
        let prefix = first.modifiers_prefix();
        if !prefix.is_empty() {
            segs.push(BarKeySeg {
                text: prefix,
                is_join: false,
            });
        }
        for (i, k) in hint.keys.iter().enumerate() {
            if i > 0 {
                segs.push(BarKeySeg {
                    text: "/".into(),
                    is_join: true,
                });
            }
            segs.push(BarKeySeg {
                text: k.code_display(),
                is_join: false,
            });
        }
    } else {
        for (i, k) in hint.keys.iter().enumerate() {
            if i > 0 {
                segs.push(BarKeySeg {
                    text: "/".into(),
                    is_join: true,
                });
            }
            segs.push(BarKeySeg {
                text: k.display(),
                is_join: false,
            });
        }
    }
    segs
}

pub struct ShortcutsBar<'a> {
    hints: &'a [HintItem],
    /// If set, replaces all hints with "press again to {label}".
    pending_confirmation: Option<PendingHint>,
    /// Right-aligned text (e.g. team name).
    right_text: Option<&'a str>,
    /// Compact mode: render only the first `max_visible` hints from `hints`, then always append `help_hint` (e.g. the "all shortcuts" modal trigger).
    /// When None, all hints are rendered.
    compact: Option<CompactConfig>,
}

pub struct CompactConfig {
    /// Maximum number of items to render from the hint list before the trailing help hint.
    pub max_visible: usize,
    /// The trailing help hint (typically the binding for the all-shortcuts modal).
    /// It is always rendered when set, even if the hint list is empty.
    pub help_hint: Option<HintItem>,
}

/// Info needed to render the "press again" hint.
#[derive(Clone, Copy)]
pub struct PendingHint {
    pub shortcut: KeyShortcut,
    pub label: &'static str,
}

impl<'a> ShortcutsBar<'a> {
    pub fn new(hints: &'a [HintItem]) -> Self {
        Self {
            hints,
            pending_confirmation: None,
            right_text: None,
            compact: None,
        }
    }

    /// Render only the first `max_visible` hints, then append `help_hint` (typically the binding that opens the all-shortcuts modal).
    pub fn compact(mut self, max_visible: usize, help_hint: Option<HintItem>) -> Self {
        self.compact = Some(CompactConfig {
            max_visible,
            help_hint,
        });
        self
    }

    /// Set the pending confirmation hint (replaces all normal hints).
    pub fn with_pending(mut self, pending: Option<PendingHint>) -> Self {
        self.pending_confirmation = pending;
        self
    }

    /// Set right-aligned text (e.g. team name).
    pub fn with_right_text(mut self, text: Option<&'a str>) -> Self {
        self.right_text = text;
        self
    }
}

impl Widget for ShortcutsBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }

        let theme = Theme::current();

        let bg_style = Style::default()
            .bg(theme.bg_base)
            .fg(theme.gray)
            .remove_modifier(Modifier::all());
        // Clear both content and style: set_style only patches style and would leave old text from previous renders
        for x in area.x..area.x + area.width {
            if let Some(cell) = buf.cell_mut((x, area.y)) {
                cell.reset();
                cell.set_style(bg_style);
            }
        }

        let key_style = Style::default()
            .fg(theme.text_secondary)
            .bg(theme.bg_base)
            .add_modifier(Modifier::BOLD);

        // muted(): DIM on the terminal theme, where `gray` is Reset and the
        // label would otherwise be indistinguishable from the bold key.
        let action_style = theme
            .muted()
            .bg(theme.bg_base)
            .remove_modifier(Modifier::BOLD);

        // If pending confirmation, show only "press again to {label}"
        if let Some(pending) = &self.pending_confirmation {
            let key_text = pending.shortcut.display();
            let label = format!("press again to {}", pending.label);

            let mut x = area.x;

            let key_span = Span::styled(&key_text, key_style);
            let key_width = key_text.width() as u16;
            buf.set_span(x, area.y, &key_span, key_width);
            x += key_width;

            let colon = Span::styled(":", action_style);
            buf.set_span(x, area.y, &colon, 1);
            x += 1;

            let action_span = Span::styled(&label, action_style);
            let action_width = label.width() as u16;
            buf.set_span(x, area.y, &action_span, action_width);
            let _ = x + action_width; // suppress unused
            return;
        }

        let sep_style = Style::default()
            .fg(theme.gray)
            .bg(theme.bg_base)
            .add_modifier(Modifier::DIM)
            .remove_modifier(Modifier::BOLD);

        let mut x = area.x;

        let effective = compute_effective_hints(self.hints, self.compact.as_ref());

        for (i, hint) in effective.iter().enumerate() {
            if i > 0 {
                let sep = Span::styled("  │  ", sep_style);
                let sep_width = 5u16;
                if x + sep_width > area.x + area.width {
                    break;
                }
                buf.set_span(x, area.y, &sep, sep_width);
                x += sep_width;
            }

            let key_width = hint.bar_key_display().width() as u16;
            if x + key_width > area.x + area.width {
                break;
            }
            x = paint_hint_keys(buf, x, area.y, hint, key_style, action_style);

            let colon = Span::styled(":", action_style);
            if x + 1 > area.x + area.width {
                break;
            }
            buf.set_span(x, area.y, &colon, 1);
            x += 1;

            let action_span = Span::styled(hint.label.as_ref(), action_style);
            let action_width = hint.label.width() as u16;
            if x + action_width > area.x + area.width {
                break;
            }
            buf.set_span(x, area.y, &action_span, action_width);
            x += action_width;
        }

        // Right-aligned text (team name etc.)
        if let Some(text) = self.right_text {
            let right_style = theme.muted().bg(theme.bg_base);
            let display = format!("{text} ");
            let rw = display.width() as u16;
            if rw > 0 && rw < area.width {
                let rx = area.x + area.width.saturating_sub(rw);
                if rx > x + 1 {
                    let right_span = Span::styled(display, right_style);
                    buf.set_span(rx, area.y, &right_span, rw);
                }
            }
        }
    }
}

fn paint_key_run(buf: &mut Buffer, x: u16, y: u16, text: &str, style: Style) -> u16 {
    if text.is_empty() {
        return x;
    }
    let w = text.width() as u16;
    let span = Span::styled(text, style);
    buf.set_span(x, y, &span, w);
    x + w
}

fn paint_hint_keys(
    buf: &mut Buffer,
    mut x: u16,
    y: u16,
    hint: &HintItem,
    key_style: Style,
    sep_style: Style,
) -> u16 {
    for seg in bar_key_segments(hint) {
        let style = if seg.is_join { sep_style } else { key_style };
        x = paint_key_run(buf, x, y, &seg.text, style);
    }
    x
}

/// Without `compact`: returns every hint from the input slice. With `compact`: pinned hints are
/// always included; the remaining `max_visible − pinned_count` slots take unpinned hints in their
/// original order.
pub fn compute_effective_hints<'a>(
    hints: &'a [HintItem],
    compact: Option<&'a CompactConfig>,
) -> Vec<&'a HintItem> {
    if let Some(cfg) = compact {
        let pinned_count = hints.iter().filter(|h| h.pinned).count();
        let unpinned_budget = cfg.max_visible.saturating_sub(pinned_count);
        let mut unpinned_used = 0;
        let mut v: Vec<&HintItem> = hints
            .iter()
            .filter(|h| {
                if h.pinned {
                    true
                } else if unpinned_used < unpinned_budget {
                    unpinned_used += 1;
                    true
                } else {
                    false
                }
            })
            .collect();
        if let Some(ref h) = cfg.help_hint {
            v.push(h);
        }
        v
    } else {
        hints.iter().collect()
    }
}
