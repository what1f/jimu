// Adapted from the Grok Build pager (Apache-2.0). Copyright 2023-2026 SpaceXAI.

/// Controller for the predicted-next-prompt ghost text.
#[derive(Debug, Default)]
pub struct PromptSuggestionController {
    /// Full suggestion text from the model. Empty means no suggestion.
    full_text: String,
    /// Request generation counter; responses carrying a stale generation are discarded (a newer turn ended, or the suggestion was invalidated).
    generation: u64,
    /// Set when the user dismissed the current suggestion (Esc). Cleared by the next loaded suggestion.
    dismissed: bool,
    /// Whether the host enables prompt suggestions.
    pub enabled: bool,
}

impl PromptSuggestionController {
    pub fn new() -> Self {
        Self {
            full_text: String::new(),
            generation: 0,
            dismissed: false,
            enabled: false,
        }
    }

    /// Begin a new fetch: invalidates any in-flight request and returns the generation to thread through the effect pipeline.
    pub fn begin_fetch(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    /// A suggestion arrived from the shell. Discards stale generations and empty payloads.
    /// Returns `true` when the suggestion was installed.
    pub fn on_loaded(&mut self, suggestion: Option<String>, generation: u64) -> bool {
        if generation != self.generation {
            return false;
        }
        match suggestion {
            Some(text) if !text.trim().is_empty() && !text.contains('\n') => {
                self.full_text = text;
                self.dismissed = false;
                true
            }
            _ => {
                self.full_text.clear();
                false
            }
        }
    }

    /// The ghost text to render for the current prompt text, if any.
    pub fn ghost_for(&self, text: &str) -> Option<&str> {
        if !self.enabled || self.dismissed || self.full_text.is_empty() {
            return None;
        }
        let rest = self.full_text.strip_prefix(text)?;
        if rest.is_empty() { None } else { Some(rest) }
    }

    /// Accept the suggestion against the current prompt text. Returns the remainder to insert and clears the suggestion.
    pub fn accept(&mut self, text: &str) -> Option<String> {
        let rest = self.ghost_for(text)?.to_owned();
        self.clear();
        Some(rest)
    }

    /// Dismiss the current suggestion (Esc) until a new one loads.
    pub fn dismiss(&mut self) {
        self.dismissed = true;
    }

    /// Drop the suggestion and invalidate any in-flight fetch (turn started, prompt sent, session switched...).
    pub fn clear(&mut self) {
        self.full_text.clear();
        self.generation = self.generation.wrapping_add(1);
    }

    /// Whether a (non-dismissed) suggestion is loaded, regardless of the current prompt text.
    pub fn has_suggestion(&self) -> bool {
        self.enabled && !self.dismissed && !self.full_text.is_empty()
    }
}
