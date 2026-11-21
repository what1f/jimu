//! Cross-provider message handoff, vision downgrade, tool call ID normalization, and error image splitting.

use crate::{
    ApiKind, AssistantBlock, AssistantMessage, ContentPart, ImageContent, Message, Model,
    ResolvedTranscript, StopReason, TextContent, ToolCall, ToolResultMessage,
};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

const CONTEXT_SAFETY_TOKENS: u64 = 4_096;
const CONTEXT_SAFETY_RATIO_PCT: u64 = 15;
const MIN_OUTPUT_WITH_RATIO_MARGIN: u64 = 32_768;
const IMAGE_INPUT_TOKEN_ESTIMATE: u64 = 1_200;

/// Clamp `max_tokens` into the remaining context window while leaving a safety margin for
/// upstream tokenizer variance (matching Pi's `clampMaxTokensToContext`).
pub fn clamp_max_tokens_to_context(
    model: &Model,
    resolved: &ResolvedTranscript,
    max_tokens: u32,
) -> u32 {
    if model.context_window == 0 {
        return max_tokens.max(1);
    }
    let input = estimate_resolved_input_tokens(resolved);
    let ratio_margin = input.saturating_mul(CONTEXT_SAFETY_RATIO_PCT).div_ceil(100);
    let margin = CONTEXT_SAFETY_TOKENS.max(ratio_margin);
    let cw = u64::from(model.context_window);
    let mut room = cw.saturating_sub(input).saturating_sub(margin);
    if room < MIN_OUTPUT_WITH_RATIO_MARGIN {
        room = MIN_OUTPUT_WITH_RATIO_MARGIN.min(
            cw.saturating_sub(input)
                .saturating_sub(CONTEXT_SAFETY_TOKENS),
        );
    }
    let room_u32 = u32::try_from(room.max(1)).unwrap_or(u32::MAX);
    max_tokens.min(room_u32).max(1)
}

fn estimate_resolved_input_tokens(resolved: &ResolvedTranscript) -> u64 {
    let mut bytes = resolved.system_prompt.len() as u64;
    let mut images = 0u64;
    for tool in &resolved.tools {
        bytes = bytes
            .saturating_add(tool.name.len() as u64)
            .saturating_add(tool.description.len() as u64)
            .saturating_add(
                serde_json::to_string(&tool.parameters)
                    .map(|s| s.len() as u64)
                    .unwrap_or(64),
            );
    }
    for msg in &resolved.messages {
        match msg {
            Message::System(s) => {
                bytes = bytes.saturating_add(crate::render_system_message_text(s).len() as u64);
            }
            Message::User(u) => {
                for part in &u.content {
                    match part {
                        ContentPart::Text(t) => bytes = bytes.saturating_add(t.text.len() as u64),
                        ContentPart::Image(_) => images = images.saturating_add(1),
                    }
                }
            }
            Message::Assistant(a) => {
                for block in &a.content {
                    match block {
                        AssistantBlock::Text(t) => {
                            bytes = bytes.saturating_add(t.text.len() as u64)
                        }
                        AssistantBlock::Thinking(th) => {
                            bytes = bytes.saturating_add(th.thinking.len() as u64)
                        }
                        AssistantBlock::ToolCall(tc) => {
                            bytes = bytes.saturating_add(tc.name.len() as u64).saturating_add(
                                serde_json::to_string(&tc.arguments)
                                    .map(|s| s.len() as u64)
                                    .unwrap_or(32),
                            );
                        }
                    }
                }
            }
            Message::ToolResult(tr) => {
                for part in &tr.content {
                    match part {
                        ContentPart::Text(t) => bytes = bytes.saturating_add(t.text.len() as u64),
                        ContentPart::Image(_) => images = images.saturating_add(1),
                    }
                }
            }
        }
    }
    bytes
        .div_ceil(4)
        .saturating_add(images.saturating_mul(IMAGE_INPUT_TOKEN_ESTIMATE))
}

pub const NON_VISION_USER_IMAGE_PLACEHOLDER: &str =
    "(image omitted: model does not support images)";
pub const NON_VISION_TOOL_IMAGE_PLACEHOLDER: &str =
    "(tool image omitted: model does not support images)";
pub const ERROR_TOOL_IMAGE_PLACEHOLDER: &str = "(image attached after the tool results)";

/// An error tool result split into text-only result blocks plus trailing images.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitErrorToolResult {
    pub content: Vec<TextContent>,
    pub caption: String,
    pub images: Vec<ImageContent>,
}

/// Split images out of an `is_error: true` tool result for APIs (such as Anthropic) that
/// reject images inside error `tool_result` blocks.
pub fn split_error_tool_result_images(msg: &ToolResultMessage) -> Option<SplitErrorToolResult> {
    if !msg.is_error {
        return None;
    }
    let images: Vec<ImageContent> = msg
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Image(img) => Some(img.clone()),
            ContentPart::Text(_) => None,
        })
        .collect();
    if images.is_empty() {
        return None;
    }
    let content = replace_images_with_text_placeholder(&msg.content, ERROR_TOOL_IMAGE_PLACEHOLDER);
    let caption = format!(
        "Images from the error result of tool call {} ({}):",
        msg.tool_call_id, msg.tool_name
    );
    Some(SplitErrorToolResult {
        content,
        caption,
        images,
    })
}

fn replace_images_with_text_placeholder(
    content: &[ContentPart],
    placeholder: &str,
) -> Vec<TextContent> {
    let mut result = Vec::with_capacity(content.len());
    let mut prev_was_placeholder = false;

    for part in content {
        match part {
            ContentPart::Image(_) => {
                if !prev_was_placeholder {
                    result.push(TextContent::plain(placeholder));
                }
                prev_was_placeholder = true;
            }
            ContentPart::Text(text) => {
                prev_was_placeholder = text.text == placeholder;
                result.push(text.clone());
            }
        }
    }

    result
}

fn downgrade_parts(content: &[ContentPart], placeholder: &str) -> Vec<ContentPart> {
    replace_images_with_text_placeholder(content, placeholder)
        .into_iter()
        .map(ContentPart::Text)
        .collect()
}

/// Deterministic 16-character lowercase hex SHA-256 prefix.
pub fn short_hash(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    let mut out = String::with_capacity(16);
    for byte in &digest[..8] {
        use std::fmt::Write;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn is_valid_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn sanitize_id_chars(input: &str) -> String {
    input
        .chars()
        .map(|c| if is_valid_id_char(c) { c } else { '_' })
        .collect()
}

/// Normalize any tool call ID into Anthropic's `^[a-zA-Z0-9_-]{1,64}$` grammar deterministically.
/// Long or pipe-delimited IDs (such as 450+ char OpenAI Responses IDs) retain a readable prefix
/// plus a deterministic SHA-256 hash suffix so distinct IDs never collide after truncation.
pub fn normalize_tool_call_id_anthropic(id: &str) -> String {
    if !id.is_empty() && id.len() <= 64 && id.chars().all(is_valid_id_char) {
        return id.to_string();
    }
    let hash = short_hash(id);
    let sanitized = sanitize_id_chars(id);
    let max_prefix = 64 - 1 - hash.len(); // 47
    let prefix = sanitized
        .chars()
        .take(max_prefix)
        .collect::<String>()
        .trim_end_matches('_')
        .to_string();
    if prefix.is_empty() {
        format!("tc_{hash}")
    } else {
        format!("{prefix}_{hash}")
    }
}

/// Normalize tool call IDs for OpenAI Chat Completions (max 40 chars, `^[a-zA-Z0-9_-]+$`).
pub fn normalize_tool_call_id_openai_completions(id: &str) -> String {
    if !id.is_empty() && id.len() <= 40 && id.chars().all(is_valid_id_char) {
        return id.to_string();
    }
    let sanitized = sanitize_id_chars(id);
    let hash = &short_hash(id)[..8];
    let max_prefix = 40 - 1 - hash.len(); // 31
    let prefix = sanitized
        .chars()
        .take(max_prefix)
        .collect::<String>()
        .trim_end_matches('_')
        .to_string();
    if prefix.is_empty() {
        format!("tc_{hash}")
    } else {
        format!("{prefix}_{hash}")
    }
}

/// Normalize tool call IDs for OpenAI Responses (`^[a-zA-Z0-9_-]{1,64}$`).
pub fn normalize_tool_call_id_openai_responses(id: &str, _same_provider_api: bool) -> String {
    normalize_tool_call_id_anthropic(id)
}

/// Transform conversation messages for a target `model`:
/// - Downgrades images when `!model.supports_images`
/// - Preserves thinking blocks, signatures, and redacted payloads only for the exact same
///   `(provider, api, model)`; drops foreign thinking blocks and strips foreign signatures
/// - Normalizes foreign `tool_call_id`s and remaps matching `ToolResultMessage.tool_call_id`s
/// - Drops errored/aborted assistant turns and synthesizes error results for orphaned tool calls
pub fn transform_messages<F>(
    messages: &[Message],
    model: &Model,
    mut normalize_tool_call_id: F,
) -> Vec<Message>
where
    F: FnMut(&str, &Model, &AssistantMessage) -> String,
{
    let mut tool_call_id_map: HashMap<String, String> = HashMap::new();
    let mut first_pass: Vec<Message> = Vec::with_capacity(messages.len());

    for msg in messages {
        match msg {
            Message::System(sys) => first_pass.push(Message::System(sys.clone())),
            Message::User(user) => {
                let mut cloned = user.clone();
                if !model.supports_images {
                    cloned.content =
                        downgrade_parts(&cloned.content, NON_VISION_USER_IMAGE_PLACEHOLDER);
                }
                first_pass.push(Message::User(cloned));
            }
            Message::ToolResult(tr) => {
                let mut cloned = tr.clone();
                if !model.supports_images {
                    cloned.content =
                        downgrade_parts(&cloned.content, NON_VISION_TOOL_IMAGE_PLACEHOLDER);
                }
                if let Some(mapped) = tool_call_id_map.get(&cloned.tool_call_id) {
                    cloned.tool_call_id = mapped.clone();
                }
                first_pass.push(Message::ToolResult(cloned));
            }
            Message::Assistant(assistant) => {
                let is_same_model = assistant.provider == model.provider
                    && assistant.api == model.api
                    && assistant.model == model.id;

                let mut transformed_content = Vec::with_capacity(assistant.content.len());
                for block in &assistant.content {
                    match block {
                        AssistantBlock::Thinking(thinking) => {
                            if !is_same_model {
                                continue;
                            }
                            if thinking.redacted
                                || thinking.thinking_signature.is_some()
                                || !thinking.thinking.trim().is_empty()
                            {
                                transformed_content
                                    .push(AssistantBlock::Thinking(thinking.clone()));
                            }
                        }
                        AssistantBlock::Text(text) => {
                            if is_same_model {
                                transformed_content.push(AssistantBlock::Text(text.clone()));
                            } else {
                                transformed_content.push(AssistantBlock::Text(TextContent::plain(
                                    text.text.clone(),
                                )));
                            }
                        }
                        AssistantBlock::ToolCall(tc) => {
                            let mut cloned = tc.clone();
                            if !is_same_model {
                                cloned.thought_signature = None;
                                cloned.namespace = None;
                                let target_is_responses = matches!(
                                    model.api,
                                    ApiKind::OpenaiResponses | ApiKind::OpenaiCodexResponses
                                );
                                let is_foreign_provider_or_api = assistant.provider
                                    != model.provider
                                    || assistant.api != model.api;
                                if target_is_responses && is_foreign_provider_or_api {
                                    cloned.item_id = tc
                                        .item_id
                                        .as_deref()
                                        .filter(|s| !s.is_empty())
                                        .map(|item_id| format!("fc_{}", short_hash(item_id)));
                                } else {
                                    cloned.item_id = None;
                                }
                                let normalized = normalize_tool_call_id(&tc.id, model, assistant);
                                if normalized != tc.id {
                                    tool_call_id_map.insert(tc.id.clone(), normalized.clone());
                                    cloned.id = normalized;
                                }
                            }
                            transformed_content.push(AssistantBlock::ToolCall(cloned));
                        }
                    }
                }

                let mut cloned_msg = assistant.clone();
                cloned_msg.content = transformed_content;
                first_pass.push(Message::Assistant(cloned_msg));
            }
        }
    }

    // Second pass: drop errored/aborted turns and synthesize missing tool results for orphaned calls.
    let mut result = Vec::with_capacity(first_pass.len());
    let mut pending_tool_calls: Vec<ToolCall> = Vec::new();
    let mut seen_result_ids: HashSet<String> = HashSet::new();
    let mut held_system: Vec<Message> = Vec::new();

    let close_pending = |pending: &mut Vec<ToolCall>,
                         seen: &mut HashSet<String>,
                         held: &mut Vec<Message>,
                         out: &mut Vec<Message>| {
        if !pending.is_empty() {
            for tc in pending.drain(..) {
                if !seen.contains(&tc.id) {
                    out.push(Message::ToolResult(ToolResultMessage {
                        tool_call_id: tc.id,
                        tool_name: tc.name,
                        content: vec![ContentPart::Text(TextContent::plain("No result provided"))],
                        is_error: true,
                        timestamp: 0,
                    }));
                }
            }
            seen.clear();
        }
        out.append(held);
    };

    for msg in first_pass {
        match msg {
            Message::Assistant(assistant) => {
                close_pending(
                    &mut pending_tool_calls,
                    &mut seen_result_ids,
                    &mut held_system,
                    &mut result,
                );
                if matches!(
                    assistant.stop_reason,
                    StopReason::Error | StopReason::Aborted
                ) {
                    continue;
                }
                let calls: Vec<ToolCall> = assistant
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        AssistantBlock::ToolCall(tc) => Some(tc.clone()),
                        _ => None,
                    })
                    .collect();
                if !calls.is_empty() {
                    pending_tool_calls = calls;
                    seen_result_ids.clear();
                }
                result.push(Message::Assistant(assistant));
            }
            Message::ToolResult(tr) => {
                seen_result_ids.insert(tr.tool_call_id.clone());
                result.push(Message::ToolResult(tr));
            }
            Message::System(sys) => {
                if pending_tool_calls.is_empty() {
                    result.push(Message::System(sys));
                } else {
                    held_system.push(Message::System(sys));
                }
            }
            Message::User(user) => {
                close_pending(
                    &mut pending_tool_calls,
                    &mut seen_result_ids,
                    &mut held_system,
                    &mut result,
                );
                result.push(Message::User(user));
            }
        }
    }

    close_pending(
        &mut pending_tool_calls,
        &mut seen_result_ids,
        &mut held_system,
        &mut result,
    );

    result
}
