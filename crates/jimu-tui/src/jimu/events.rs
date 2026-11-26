//! Lossless translation between native Jimu types (`KitEvent`, `AgentMessage`,
//! `ContextUsage`, `QueuedInputSummary`) and harness-agnostic `crate::harness` types.

use crate::harness::{
    ContextMeterSnapshot, HistoryMessage, NoticeLevel, QueuedItem, ToolOutput, TuiEvent,
    UsageStats, UserInput,
};
use jimu_agent::{AgentEvent, AgentMessage, AgentToolResult, QueuedInputKind, QueuedInputSummary};
use jimu_ai::{
    AssistantBlock, AssistantMessageEvent, ContentPart, ImageContent, Message, TextContent, Usage,
};
use jimu_kit::KitEvent;
use jimu_session::{ContextUsage, SessionEvent};
use serde_json::Value;
use std::collections::HashMap;

pub(crate) fn user_input_to_parts(input: UserInput) -> Vec<ContentPart> {
    let mut parts = Vec::new();
    if !input.text.is_empty() {
        parts.push(ContentPart::Text(TextContent::plain(input.text)));
    }
    for img in input.images {
        parts.push(ContentPart::Image(ImageContent {
            data: encode_base64(&img.data),
            mime_type: img.mime_type,
        }));
    }
    if parts.is_empty() {
        parts.push(ContentPart::Text(TextContent::plain("")));
    }
    parts
}

pub(crate) fn parts_to_text(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .map(|part| match part {
            ContentPart::Text(t) => t.text.as_str(),
            ContentPart::Image(_) => "[image]",
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Format user message content parts for transcript display, preserving numbered `[Image #N]` chips.
pub(crate) fn user_parts_to_display(parts: &[ContentPart]) -> String {
    let mut image_idx = 0usize;
    let mut chips = Vec::new();
    let mut texts = Vec::new();
    for part in parts {
        match part {
            ContentPart::Text(t) if !t.text.is_empty() => texts.push(t.text.as_str()),
            ContentPart::Text(_) => {}
            ContentPart::Image(_) => {
                image_idx += 1;
                chips.push(format!("[Image #{image_idx}]"));
            }
        }
    }
    let body = texts.join("\n");
    if chips.is_empty() {
        body
    } else if body.trim().is_empty() {
        chips.join(" ")
    } else {
        format!("{} {}", chips.join(" "), body.trim())
    }
}

pub(crate) fn tool_result_to_output(result: AgentToolResult, is_error: bool) -> ToolOutput {
    ToolOutput {
        text: parts_to_text(&result.content),
        images: Vec::new(),
        structured: result.structured_content,
        details: result.details,
        is_error: result.is_error || is_error,
    }
}

pub(crate) fn delegated_tool_name(
    name: &str,
    args: Option<&Value>,
    details: Option<&Value>,
) -> Option<String> {
    if let Some(d) = details
        && let Some(inner) = d.get("delegated_tool").and_then(|v| v.get("name"))
        && let Some(s) = inner.as_str()
    {
        return Some(s.to_owned());
    }
    if name == "call_tool"
        && let Some(a) = args
        && let Some(s) = a.get("name").and_then(|v| v.as_str())
    {
        return Some(s.to_owned());
    }
    None
}

pub(crate) fn usage_to_stats(u: &Usage) -> UsageStats {
    UsageStats {
        input_tokens: u.input,
        output_tokens: u.output,
        cache_read_tokens: u.cache_read,
        cache_write_tokens: u.cache_write,
        reasoning_tokens: u.reasoning,
        total_tokens: u.total_tokens,
        cost_usd: u.cost_total,
    }
}

pub(crate) fn context_usage_to_snapshot(u: ContextUsage) -> ContextMeterSnapshot {
    ContextMeterSnapshot {
        tokens: u.tokens,
        estimated_tokens: u.estimated_tokens,
        context_window: u.context_window,
        percent: u.percent,
        compaction_threshold: u.compaction_threshold,
    }
}

pub(crate) fn queued_summary_to_item(q: QueuedInputSummary) -> QueuedItem {
    QueuedItem {
        id: q.id,
        is_steering: q.kind == QueuedInputKind::Steering,
        text: q.text,
    }
}

pub(crate) fn restore_history(messages: Vec<AgentMessage>) -> (Vec<HistoryMessage>, UsageStats) {
    let mut out = Vec::new();
    let mut total = UsageStats::default();
    let mut call_args: HashMap<String, Value> = HashMap::new();

    for msg in messages {
        if msg.is_meta() {
            continue;
        }
        match msg {
            AgentMessage::Llm(Message::User(u)) => {
                out.push(HistoryMessage::User {
                    text: user_parts_to_display(&u.content),
                    steered: false,
                });
            }
            AgentMessage::Llm(Message::Assistant(a)) => {
                total.input_tokens = total.input_tokens.saturating_add(a.usage.input);
                total.output_tokens = total.output_tokens.saturating_add(a.usage.output);
                total.cache_read_tokens =
                    total.cache_read_tokens.saturating_add(a.usage.cache_read);
                total.cache_write_tokens =
                    total.cache_write_tokens.saturating_add(a.usage.cache_write);
                total.total_tokens = total.total_tokens.saturating_add(a.usage.total_tokens);
                total.cost_usd += a.usage.cost_total;
                if let Some(r) = a.usage.reasoning {
                    *total.reasoning_tokens.get_or_insert(0) += r;
                }
                let mut thinking_parts = Vec::new();
                let mut text_parts = Vec::new();
                for block in &a.content {
                    match block {
                        AssistantBlock::Thinking(t) if !t.redacted && !t.thinking.is_empty() => {
                            thinking_parts.push(t.thinking.as_str());
                        }
                        AssistantBlock::Text(t) if !t.text.is_empty() => {
                            text_parts.push(t.text.as_str());
                        }
                        AssistantBlock::ToolCall(tc) => {
                            call_args.insert(tc.id.clone(), Value::Object(tc.arguments.clone()));
                        }
                        _ => {}
                    }
                }
                if !thinking_parts.is_empty() || !text_parts.is_empty() || a.error_message.is_some()
                {
                    out.push(HistoryMessage::Assistant {
                        thinking: (!thinking_parts.is_empty()).then(|| thinking_parts.join("\n")),
                        text: text_parts.join("\n"),
                        error: a.error_message,
                    });
                }
            }
            AgentMessage::Llm(Message::ToolResult(r)) => {
                let args = call_args.remove(&r.tool_call_id).unwrap_or(Value::Null);
                let effective_name = delegated_tool_name(&r.tool_name, Some(&args), None);
                out.push(HistoryMessage::Tool {
                    call_id: r.tool_call_id,
                    name: r.tool_name,
                    effective_name,
                    args,
                    output: ToolOutput {
                        text: parts_to_text(&r.content),
                        is_error: r.is_error,
                        ..ToolOutput::default()
                    },
                });
            }
            AgentMessage::Custom { content, .. } => {
                out.push(HistoryMessage::Notice {
                    level: NoticeLevel::Info,
                    message: parts_to_text(&content),
                });
            }
            AgentMessage::Llm(Message::System(_)) => {}
        }
    }

    (out, total)
}

/// Stateful per-stream translator that tracks steered IDs and assistant streaming deltas.
#[derive(Default)]
pub(crate) struct KitEventTranslator {
    steered_id: Option<String>,
    seen_delta: bool,
}

impl KitEventTranslator {
    pub(crate) fn translate(&mut self, event: KitEvent, out: &mut Vec<TuiEvent>) {
        match event {
            KitEvent::Agent(AgentEvent::AgentStart) => {
                self.seen_delta = false;
                out.push(TuiEvent::TurnStarted);
            }
            KitEvent::Agent(AgentEvent::AgentEnd { .. }) => {
                self.seen_delta = false;
                out.push(TuiEvent::TurnEnded);
            }
            KitEvent::Agent(AgentEvent::Steered { id, .. }) => {
                self.steered_id = Some(id);
            }
            KitEvent::Agent(AgentEvent::MessageStart { message }) => {
                if !message.is_meta()
                    && let AgentMessage::Llm(Message::Assistant(_)) = message
                {
                    self.seen_delta = false;
                }
            }
            KitEvent::Agent(AgentEvent::MessageUpdate {
                assistant_message_event,
                ..
            }) => match assistant_message_event {
                AssistantMessageEvent::TextDelta {
                    content_index,
                    delta,
                } => {
                    self.seen_delta = true;
                    out.push(TuiEvent::TextDelta {
                        block_index: content_index,
                        delta,
                    });
                }
                AssistantMessageEvent::ThinkingDelta {
                    content_index,
                    delta,
                } => {
                    self.seen_delta = true;
                    out.push(TuiEvent::ThinkingDelta {
                        block_index: content_index,
                        delta,
                    });
                }
                _ => {}
            },
            KitEvent::Agent(AgentEvent::MessageEnd { message }) => {
                if message.is_meta() {
                    return;
                }
                match message {
                    AgentMessage::Llm(Message::User(u)) => {
                        let steered = self.steered_id.is_some() && u.id == self.steered_id;
                        if steered {
                            self.steered_id = None;
                        }
                        out.push(TuiEvent::UserMessage {
                            id: u.id,
                            text: user_parts_to_display(&u.content),
                            steered,
                        });
                    }
                    AgentMessage::Llm(Message::Assistant(a)) => {
                        if !self.seen_delta {
                            for (idx, block) in a.content.iter().enumerate() {
                                match block {
                                    AssistantBlock::Text(t) if !t.text.is_empty() => {
                                        out.push(TuiEvent::TextDelta {
                                            block_index: idx,
                                            delta: t.text.clone(),
                                        });
                                    }
                                    AssistantBlock::Thinking(t) => {
                                        if t.redacted {
                                            out.push(TuiEvent::ThinkingRedacted {
                                                block_index: idx,
                                            });
                                        } else if !t.thinking.is_empty() {
                                            out.push(TuiEvent::ThinkingDelta {
                                                block_index: idx,
                                                delta: t.thinking.clone(),
                                            });
                                        }
                                    }
                                    AssistantBlock::ToolCall(_) => {}
                                    _ => {}
                                }
                            }
                        }
                        self.seen_delta = false;
                        out.push(TuiEvent::AssistantMessageEnd {
                            error: a.error_message,
                        });
                    }
                    AgentMessage::Custom { content, .. } => {
                        out.push(TuiEvent::SessionNotice {
                            level: NoticeLevel::Info,
                            message: parts_to_text(&content),
                        });
                    }
                    _ => {}
                }
            }
            KitEvent::Agent(AgentEvent::RequestUsage { usage, .. }) => {
                out.push(TuiEvent::UsageUpdate(usage_to_stats(&usage)));
            }
            KitEvent::Agent(AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            }) => {
                let effective_name = delegated_tool_name(&tool_name, Some(&args), None);
                out.push(TuiEvent::ToolStart {
                    call_id: tool_call_id,
                    name: tool_name,
                    effective_name,
                    args,
                });
            }
            KitEvent::Agent(AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                partial_result,
                ..
            }) => {
                out.push(TuiEvent::ToolUpdate {
                    call_id: tool_call_id,
                    output: tool_result_to_output(partial_result, false),
                });
            }
            KitEvent::Agent(AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            }) => {
                let effective_name = delegated_tool_name(&tool_name, None, Some(&result.details));
                out.push(TuiEvent::ToolEnd {
                    call_id: tool_call_id,
                    name: tool_name,
                    effective_name,
                    output: tool_result_to_output(result, is_error),
                });
            }
            KitEvent::Session(SessionEvent::CompactionStart { .. }) => {
                out.push(TuiEvent::SessionNotice {
                    level: NoticeLevel::Info,
                    message: "Summarizing conversation…".into(),
                });
            }
            KitEvent::Session(SessionEvent::CompactionEnd { .. }) => {
                out.push(TuiEvent::SessionNotice {
                    level: NoticeLevel::Success,
                    message: "Conversation summarized".into(),
                });
            }
            KitEvent::Session(SessionEvent::Forked { .. }) => {
                out.push(TuiEvent::SessionNotice {
                    level: NoticeLevel::Info,
                    message: "Switched branch".into(),
                });
            }
            KitEvent::Error(err) => {
                out.push(TuiEvent::Error(err));
            }
            _ => {}
        }
    }
}

fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((triple >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(triple & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}
