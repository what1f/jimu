//! Reconciles delivered key events with physical input state for terminal/OS pairs that drop modifier bits the application needs.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

use crate::engine::input::terminal::ModifierDelivery;

/// Snapshot of physically-held modifier keys at a single point in time.
/// Future probes can populate more bits; consumers should only read what they need.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct ModifierState {
    pub command: bool,
    pub option: bool,
    pub shift: bool,
    pub control: bool,
}

/// OS-level probe of physical modifier state. One snapshot per call.
pub trait ModifierProbe {
    fn snapshot(&self) -> ModifierState;
}

/// Production probe: macOS reads CoreGraphics, other OSes return all-false.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsModifierProbe;

impl ModifierProbe for OsModifierProbe {
    #[cfg(target_os = "macos")]
    fn snapshot(&self) -> ModifierState {
        super::macos_modifiers::snapshot()
    }

    #[cfg(not(target_os = "macos"))]
    fn snapshot(&self) -> ModifierState {
        ModifierState::default()
    }
}

/// Reusable normalizer that upgrades incoming key events with modifiers the terminal failed to encode.
/// One instance lives on the pager's `AppView` and is invoked at the top of `handle_input`, so every downstream surface sees the rescued event.
/// Construct with [`KeyboardNormalizer::from_terminal_context`].
#[derive(Debug, Clone, Copy)]
pub struct KeyboardNormalizer<P: ModifierProbe = OsModifierProbe> {
    probe: P,
    delivery: ModifierDelivery,
}

impl<P: ModifierProbe> KeyboardNormalizer<P> {
    /// Upgrade a [`KeyEvent`] when a modifier is held but absent from the event.
    /// Returns `Some` only if the delivered event changed.
    pub fn rescue_key(&self, key: KeyEvent) -> Option<KeyEvent> {
        if key.code == KeyCode::Char('\u{0002}') && key.modifiers.is_empty() {
            let mut out = key;
            out.code = KeyCode::Char('b');
            out.modifiers = KeyModifiers::CONTROL;
            return Some(out);
        }
        if !self.delivery.benefits_from_rescue()
            || super::terminal_support::os_modifier_rescue_suppressed()
        {
            return None;
        }
        if !key.modifiers.is_empty() {
            return None;
        }
        if !matches!(key.code, KeyCode::Backspace | KeyCode::Delete) {
            return None;
        }
        let state = self.probe.snapshot();
        // Cmd wins per macOS convention: Cmd+Backspace (line-kill) is the stronger action; almost no one holds Cmd+Opt simultaneously
        let added = match (
            state.command && self.delivery.cmd.benefits_from_rescue(),
            state.option && self.delivery.opt.benefits_from_rescue(),
        ) {
            (true, _) => KeyModifiers::SUPER,
            (false, true) => KeyModifiers::ALT,
            _ => return None,
        };
        let mut out = key;
        out.modifiers |= added;
        Some(out)
    }

    /// Upgrade an [`Event`] in place, owning a fresh `Event::Key` only when a rescue actually fires.
    pub fn rescue<'a>(&self, ev: &'a Event) -> std::borrow::Cow<'a, Event> {
        if let Event::Key(k) = ev
            && let Some(upgraded) = self.rescue_key(*k)
        {
            return std::borrow::Cow::Owned(Event::Key(upgraded));
        }
        std::borrow::Cow::Borrowed(ev)
    }
}

impl KeyboardNormalizer<OsModifierProbe> {
    pub fn from_terminal_context() -> Self {
        Self {
            probe: OsModifierProbe,
            delivery: crate::engine::input::terminal::terminal_context()
                .keyboard_capabilities()
                .modifier_delivery,
        }
    }
}
