use crate::AgentMessage;
use pikra_ai::{Message, SystemMessage, ToolDefinition, TranscriptContext, UserMessage};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Optional custom converter from `AgentMessage` slice to `TranscriptContext`.
pub type ConvertToLlmFn =
    Arc<dyn Fn(&str, &[ToolDefinition], &[AgentMessage]) -> TranscriptContext + Send + Sync>;

pub(crate) fn now_ms() -> u64 {
    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    {
        0
    }
    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// Converts [`AgentMessage`]s into a [`TranscriptContext`] for the LLM while preserving
/// a cache-stable leading [`Message::System`] with base `system_prompt` and resident `tools`.
///
/// - [`AgentMessage::Llm`] messages pass through directly.
/// - [`AgentMessage::Custom`] messages with `include_in_llm == true` are converted to [`Message::User`].
/// - [`AgentMessage::Custom`] messages with `include_in_llm == false` are omitted.
pub fn default_convert_to_llm(
    system_prompt: &str,
    tools: &[ToolDefinition],
    messages: &[AgentMessage],
) -> TranscriptContext {
    convert_to_llm_with_overrides(system_prompt, tools, messages, false, false)
}

pub(crate) fn convert_to_llm_with_overrides(
    system_prompt: &str,
    tools: &[ToolDefinition],
    messages: &[AgentMessage],
    system_overridden: bool,
    tools_overridden: bool,
) -> TranscriptContext {
    let mut llm_messages = Vec::with_capacity(messages.len() + 1);
    for msg in messages {
        match msg {
            AgentMessage::Llm(m) => llm_messages.push(m.clone()),
            AgentMessage::Custom {
                id,
                content,
                include_in_llm: true,
                timestamp,
                ..
            } => {
                llm_messages.push(Message::User(UserMessage {
                    id: id.clone(),
                    content: content.clone(),
                    timestamp: *timestamp,
                }));
            }
            AgentMessage::Custom {
                include_in_llm: false,
                ..
            } => {}
        }
    }

    if let Some(Message::System(first)) = llm_messages.first_mut() {
        if system_overridden || (first.content.is_empty() && !system_prompt.is_empty()) {
            first.content = system_prompt.to_string();
        }
        if tools_overridden || (first.tools_added.is_empty() && !tools.is_empty()) {
            first.tools_added = tools.to_vec();
        }
        TranscriptContext {
            messages: llm_messages,
        }
    } else {
        let mut all = Vec::with_capacity(llm_messages.len() + 1);
        all.push(Message::System(SystemMessage {
            content: system_prompt.to_string(),
            sections: Vec::new(),
            tools_added: tools.to_vec(),
            tools_removed: Vec::new(),
            timestamp: 0,
        }));
        all.extend(llm_messages);
        TranscriptContext { messages: all }
    }
}
