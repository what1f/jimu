use crate::engine::textarea::{
    EditBuffer, EditCommand, EditOutcome, SingleLineViewport, classify_key_event,
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEditOutcome {
    Unhandled,
    /// The key was recognized and consumed, but text and cursor stayed unchanged.
    HandledNoChange,
    CursorChanged,
    TextChanged,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineEditor {
    buffer: EditBuffer,
}

impl LineEditor {
    pub fn text(&self) -> &str {
        self.buffer.text()
    }

    pub fn cursor_byte(&self) -> usize {
        self.buffer.cursor_byte()
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.buffer = EditBuffer::from_text(sanitize_single_line(text));
    }

    pub fn reset(&mut self) {
        self.buffer = EditBuffer::new();
    }

    pub fn delete_last_grapheme(&mut self) -> LineEditOutcome {
        let _ = self.buffer.set_cursor_byte(self.buffer.text().len());
        Self::from_edit_outcome(self.buffer.apply(EditCommand::DeleteGraphemeBackward))
    }

    pub fn insert_paste(&mut self, text: &str) -> LineEditOutcome {
        self.insert_paste_with_policy(text, |_| true, usize::MAX)
    }

    pub fn insert_paste_with_policy(
        &mut self,
        text: &str,
        mut allow_insert: impl FnMut(char) -> bool,
        max_chars: usize,
    ) -> LineEditOutcome {
        let cleaned = sanitize_single_line(text);
        let accepted = cleaned
            .chars()
            .filter(|character| allow_insert(*character))
            .take(max_chars)
            .collect::<String>();
        if accepted.is_empty() {
            return LineEditOutcome::HandledNoChange;
        }
        Self::from_edit_outcome(self.buffer.insert_str(&accepted))
    }

    pub fn insert_paste_with_byte_limit(
        &mut self,
        text: &str,
        max_total_bytes: usize,
    ) -> LineEditOutcome {
        let cleaned = sanitize_single_line(text);
        let remaining = max_total_bytes.saturating_sub(self.buffer.text().len());
        let mut accepted_bytes = 0usize;
        let accepted = cleaned
            .chars()
            .take_while(|character| {
                let next = accepted_bytes + character.len_utf8();
                if next > remaining {
                    return false;
                }
                accepted_bytes = next;
                true
            })
            .collect::<String>();
        if accepted.is_empty() {
            return LineEditOutcome::HandledNoChange;
        }
        Self::from_edit_outcome(self.buffer.insert_str(&accepted))
    }

    pub fn handle_key(&mut self, key: &KeyEvent) -> LineEditOutcome {
        self.handle_key_with_insert_policy(key, |_| true)
    }

    pub fn handle_key_with_insert_policy(
        &mut self,
        key: &KeyEvent,
        allow_insert: impl FnOnce(char) -> bool,
    ) -> LineEditOutcome {
        if key.kind == KeyEventKind::Release {
            return LineEditOutcome::Unhandled;
        }
        let command = match key {
            KeyEvent {
                code: KeyCode::Home,
                ..
            }
            | KeyEvent {
                code: KeyCode::Left,
                modifiers: KeyModifiers::SUPER,
                ..
            } => Some(EditCommand::MoveLogicalLineStart),
            KeyEvent {
                code: KeyCode::End, ..
            }
            | KeyEvent {
                code: KeyCode::Right,
                modifiers: KeyModifiers::SUPER,
                ..
            } => Some(EditCommand::MoveLogicalLineEnd),
            _ => classify_key_event(key),
        };
        let Some(command) = command else {
            return LineEditOutcome::Unhandled;
        };
        if let EditCommand::Insert(character) = command
            && !allow_insert(character)
        {
            return LineEditOutcome::HandledNoChange;
        }
        Self::from_edit_outcome(self.buffer.apply(command))
    }

    pub fn viewport(&self, width: usize) -> SingleLineViewport {
        self.buffer.single_line_viewport(width)
    }

    fn from_edit_outcome(outcome: EditOutcome) -> LineEditOutcome {
        match outcome {
            EditOutcome::Unchanged => LineEditOutcome::HandledNoChange,
            EditOutcome::CursorOnly => LineEditOutcome::CursorChanged,
            EditOutcome::TextOnly(_) | EditOutcome::TextAndCursor(_) => {
                LineEditOutcome::TextChanged
            }
        }
    }
}

pub fn sanitize_single_line(text: impl Into<String>) -> String {
    let mut text = text.into();
    text.retain(|character| !matches!(character, '\r' | '\n'));
    text
}
