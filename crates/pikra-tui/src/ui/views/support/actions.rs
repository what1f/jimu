// Adapted from the Grok Build pager (Apache-2.0). Copyright 2023-2026 SpaceXAI.

use crate::engine::input::key::KeyShortcut;
use crate::ui::views::shortcuts_bar::HintItem;
pub type ActionId = &'static str;

/// When an action is available / visible.
/// Used for **exact** matching in `registry.lookup()`.
/// Each layer in the input chain queries its own context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    /// Global; checked at the app level after all views.
    Always,
    /// Only when prompt pane is focused.
    PromptFocused,
    /// Only when scrollback pane is focused.
    ScrollbackFocused,
    /// Agent-level; checked after pane routing, before global.
    AgentScreen,
    /// Only on the welcome screen.
    WelcomeScreen,
    /// Only when the Agent Dashboard view is focused.
    DashboardFocused,
    /// Only inside the dashboard's session overlay (a dashboard-spawned agent rendered fullscreen).
    /// Distinguishes the detail-view shortcuts (back to dashboard, prev/next session) from the dashboard LIST shortcuts.
    /// The cheatsheet can then dim whichever set isn't applicable to the current view.
    DashboardOverlay,
}

/// Action category (for grouping in command palette).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    GettingStarted,
    Input,
    ConversationNav,
    ConversationAction,
    Panels,
    Session,
    /// Agent Dashboard shortcuts.
    Dashboard,
}

#[derive(Debug, Clone)]
pub struct ActionDef {
    pub id: ActionId,
    /// Short label for shortcuts bar: "Send", "Quit", "nav"
    pub label: &'static str,
    /// Longer description for command palette
    pub description: &'static str,
    /// Optional man-style help for the shortcuts cheatsheet detail/expand UI.
    /// Consumers should fall back to `description` when this is `None`.
    pub long_help: Option<&'static str>,
    pub default_key: KeyShortcut,
    /// Optional second key binding (e.g., j/k both shown as "j/k:nav")
    pub alt_keys: Vec<KeyShortcut>,
    pub category: Category,
    /// When this action is available
    pub context: When,
    /// Priority for shortcuts bar. `None` means don't show; `Some(0)` is the highest priority.
    pub hint_priority: Option<u8>,
    /// Combined display for shortcuts bar (e.g., "j/k" for the SelectNext/SelectPrev pair).
    /// If set, overrides default_key.display().
    pub hint_key_display: Option<&'static str>,
    /// If true, requires double-press (1000ms TTL) to execute.
    /// The first press sets a `PendingAction`; the second press confirms.
    pub requires_confirmation: bool,
}

impl ActionDef {
    /// Convert this action def into a [`HintItem`] for the shortcuts bar.
    /// Uses `default_key` only.
    /// For paired hints (j/k, h/l), the view should use [`HintItem::paired`] with keys from two related action defs.
    pub fn hint(&self) -> HintItem {
        let mut item = HintItem::new(self.default_key, self.label);
        item.custom_display = self.hint_key_display;
        item.description = Some(std::borrow::Cow::Borrowed(self.description));
        item
    }
}

#[derive(Default)]
pub struct ActionRegistry {
    actions: Vec<ActionDef>,
}
impl ActionRegistry {
    pub fn new(actions: Vec<ActionDef>) -> Self {
        Self { actions }
    }
    pub fn all(&self) -> &[ActionDef] {
        &self.actions
    }
}
