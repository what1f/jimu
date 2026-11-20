//! Deterministic transcript replay for system prompts, named sections, and tool deltas.

use crate::{Message, SystemMessage, ToolDefinition, TranscriptContext};

/// Effective prompt state after replaying every `Message::System` entry in order.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedTranscript {
    pub system_prompt: String,
    pub tools: Vec<ToolDefinition>,
    pub messages: Vec<Message>,
}

/// Render a single `SystemMessage` into prompt text (`content` followed by active `sections`).
pub fn render_system_message_text(message: &SystemMessage) -> String {
    let mut parts = Vec::new();
    if !message.content.is_empty() {
        parts.push(message.content.as_str());
    }
    for (_, value) in &message.sections {
        if let Some(text) = value
            && !text.is_empty()
        {
            parts.push(text.as_str());
        }
    }
    parts.join("\n\n")
}

impl TranscriptContext {
    /// Replay every `Message::System` in order to compute the effective base instructions +
    /// sections, deterministic tool definitions, and conversation messages.
    pub fn resolve(&self) -> ResolvedTranscript {
        let mut base_parts: Vec<String> = Vec::new();
        let mut sections: Vec<(String, String)> = Vec::new();
        let mut tools: Vec<ToolDefinition> = Vec::new();
        let mut conversation: Vec<Message> = Vec::new();

        for msg in &self.messages {
            match msg {
                Message::System(sys) => {
                    if !sys.content.is_empty() {
                        base_parts.push(sys.content.clone());
                    }
                    for (name, value) in &sys.sections {
                        match value {
                            Some(text) => {
                                if let Some(existing) = sections.iter_mut().find(|(k, _)| k == name) {
                                    existing.1 = text.clone();
                                } else {
                                    sections.push((name.clone(), text.clone()));
                                }
                            }
                            None => {
                                sections.retain(|(k, _)| k != name);
                            }
                        }
                    }
                    for removed in &sys.tools_removed {
                        tools.retain(|t| t.name != *removed);
                    }
                    for added in &sys.tools_added {
                        if let Some(existing) = tools.iter_mut().find(|t| t.name == added.name) {
                            *existing = added.clone();
                        } else {
                            tools.push(added.clone());
                        }
                    }
                }
                other => conversation.push(other.clone()),
            }
        }

        let mut prompt_parts = base_parts;
        for (_, text) in sections {
            if !text.is_empty() {
                prompt_parts.push(text);
            }
        }

        ResolvedTranscript {
            system_prompt: prompt_parts.join("\n\n"),
            tools,
            messages: conversation,
        }
    }

    /// Return the effective system prompt after replaying all `Message::System` entries.
    pub fn current_system_prompt(&self) -> String {
        self.resolve().system_prompt
    }

    /// Return the effective ordered tool list after replaying all `Message::System` entries.
    pub fn current_tools(&self) -> Vec<ToolDefinition> {
        self.resolve().tools
    }

    /// Collapse all `Message::System` entries into a single leading `SystemMessage`, if any.
    pub fn collapsed_system_message(&self) -> Option<SystemMessage> {
        let mut base_parts: Vec<String> = Vec::new();
        let mut sections: Vec<(String, String)> = Vec::new();
        let mut tools: Vec<ToolDefinition> = Vec::new();
        let mut first_timestamp: Option<u64> = None;

        for msg in &self.messages {
            if let Message::System(sys) = msg {
                if first_timestamp.is_none() {
                    first_timestamp = Some(sys.timestamp);
                }
                if !sys.content.is_empty() {
                    base_parts.push(sys.content.clone());
                }
                for (name, value) in &sys.sections {
                    match value {
                        Some(text) => {
                            if let Some(existing) = sections.iter_mut().find(|(k, _)| k == name) {
                                existing.1 = text.clone();
                            } else {
                                sections.push((name.clone(), text.clone()));
                            }
                        }
                        None => sections.retain(|(k, _)| k != name),
                    }
                }
                for removed in &sys.tools_removed {
                    tools.retain(|t| t.name != *removed);
                }
                for added in &sys.tools_added {
                    if let Some(existing) = tools.iter_mut().find(|t| t.name == added.name) {
                        *existing = added.clone();
                    } else {
                        tools.push(added.clone());
                    }
                }
            }
        }

        if first_timestamp.is_none() && tools.is_empty() {
            return None;
        }

        Some(SystemMessage {
            content: base_parts.join("\n\n"),
            sections: sections.into_iter().map(|(k, v)| (k, Some(v))).collect(),
            tools_added: tools,
            tools_removed: Vec::new(),
            timestamp: first_timestamp.unwrap_or(0),
        })
    }

    /// Rebuild this transcript with all system messages collapsed into a single leading entry.
    pub fn collapse_system_messages(&self) -> TranscriptContext {
        let head = self.collapsed_system_message();
        let mut messages = Vec::with_capacity(self.messages.len());
        if let Some(sys) = head {
            messages.push(Message::System(sys));
        }
        for msg in &self.messages {
            if !matches!(msg, Message::System(_)) {
                messages.push(msg.clone());
            }
        }
        TranscriptContext { messages }
    }
}
