//! Google Generative AI (`POST {base_url}/models/{model.id}:streamGenerateContent?alt=sse`) streaming adapter.

use crate::providers::{
    drive_event_stream, emit_protocol_error, emit_transport_error, new_assistant_message, now_ms,
    send_cancellable,
};
use crate::sse::SseStream;
use crate::transform::{normalize_tool_call_id_anthropic, transform_messages};
use crate::usage::{calculate_cost, google_stream_url, has_header, merge_headers};
use crate::{
    AssistantBlock, AssistantEventStream, AssistantMessageEvent, ContentPart, HttpRequest,
    HttpTransport, Message, Model, SimpleStreamOptions, StopReason, TextContent, ThinkingContent,
    ThinkingLevel, ToolCall, ToolDefinition, TranscriptContext, Usage,
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use std::sync::Arc;

fn convert_tools(tools: &[ToolDefinition]) -> Value {
    let declarations: Vec<Value> = tools
        .iter()
        .map(|tool| {
            let mut decl = Map::new();
            decl.insert("name".into(), Value::String(tool.name.clone()));
            decl.insert("description".into(), Value::String(tool.description.clone()));
            decl.insert("parametersJsonSchema".into(), tool.parameters.clone());
            Value::Object(decl)
        })
        .collect();
    json!([{ "functionDeclarations": declarations }])
}

fn convert_messages(model: &Model, messages: &[Message]) -> Vec<Value> {
    let transformed = transform_messages(messages, model, |id, _, _| {
        normalize_tool_call_id_anthropic(id)
    });

    let mut contents: Vec<Value> = Vec::new();
    let mut i = 0;

    while i < transformed.len() {
        match &transformed[i] {
            Message::System(_) => {
                i += 1;
            }
            Message::User(user) => {
                let mut parts = Vec::new();
                for part in &user.content {
                    match part {
                        ContentPart::Text(t) => {
                            parts.push(json!({ "text": t.text }));
                        }
                        ContentPart::Image(img) => {
                            parts.push(json!({
                                "inlineData": {
                                    "mimeType": img.mime_type,
                                    "data": img.data,
                                }
                            }));
                        }
                    }
                }
                if !parts.is_empty() {
                    contents.push(json!({
                        "role": "user",
                        "parts": parts,
                    }));
                }
                i += 1;
            }
            Message::Assistant(assistant) => {
                let is_same_model = assistant.provider == model.provider
                    && assistant.api == model.api
                    && assistant.model == model.id;
                let mut parts = Vec::new();

                for block in &assistant.content {
                    match block {
                        AssistantBlock::Text(t) => {
                            let sig = if is_same_model {
                                t.text_signature.as_deref()
                            } else {
                                None
                            };
                            if t.text.trim().is_empty() && sig.is_none() {
                                continue;
                            }
                            let mut part = Map::new();
                            part.insert("text".into(), Value::String(t.text.clone()));
                            if let Some(sig) = sig {
                                part.insert("thoughtSignature".into(), Value::String(sig.into()));
                            }
                            parts.push(Value::Object(part));
                        }
                        AssistantBlock::Thinking(th) => {
                            if !is_same_model {
                                continue;
                            }
                            let sig = th.thinking_signature.as_deref();
                            if th.thinking.trim().is_empty() && sig.is_none() {
                                continue;
                            }
                            let mut part = Map::new();
                            part.insert("thought".into(), Value::Bool(true));
                            part.insert("text".into(), Value::String(th.thinking.clone()));
                            if let Some(sig) = sig {
                                part.insert("thoughtSignature".into(), Value::String(sig.into()));
                            }
                            parts.push(Value::Object(part));
                        }
                        AssistantBlock::ToolCall(tc) => {
                            let mut fc = Map::new();
                            fc.insert("name".into(), Value::String(tc.name.clone()));
                            fc.insert("args".into(), Value::Object(tc.arguments.clone()));
                            if !tc.id.is_empty() {
                                fc.insert("id".into(), Value::String(tc.id.clone()));
                            }
                            let mut part = Map::new();
                            part.insert("functionCall".into(), Value::Object(fc));
                            if is_same_model && let Some(sig) = &tc.thought_signature {
                                part.insert(
                                    "thoughtSignature".into(),
                                    Value::String(sig.clone()),
                                );
                            }
                            parts.push(Value::Object(part));
                        }
                    }
                }

                if !parts.is_empty() {
                    contents.push(json!({
                        "role": "model",
                        "parts": parts,
                    }));
                }
                i += 1;
            }
            Message::ToolResult(_) => {
                // Merge consecutive tool results into a single user turn with functionResponse parts.
                let mut fr_parts = Vec::new();
                while i < transformed.len() {
                    let Message::ToolResult(tr) = &transformed[i] else {
                        break;
                    };
                    let text_result = tr
                        .content
                        .iter()
                        .filter_map(|p| match p {
                            ContentPart::Text(t) => Some(t.text.as_str()),
                            ContentPart::Image(_) => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let image_parts: Vec<Value> = if model.supports_images {
                        tr.content
                            .iter()
                            .filter_map(|p| match p {
                                ContentPart::Image(img) => Some(json!({
                                    "inlineData": {
                                        "mimeType": img.mime_type,
                                        "data": img.data,
                                    }
                                })),
                                ContentPart::Text(_) => None,
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };

                    let response_str = if !text_result.trim().is_empty() {
                        text_result
                    } else if !image_parts.is_empty() {
                        "(see attached image)".into()
                    } else {
                        String::new()
                    };

                    let mut fr = Map::new();
                    fr.insert("name".into(), Value::String(tr.tool_name.clone()));
                    if !tr.tool_call_id.is_empty() {
                        fr.insert("id".into(), Value::String(tr.tool_call_id.clone()));
                    }
                    fr.insert(
                        "response".into(),
                        if tr.is_error {
                            json!({ "error": response_str })
                        } else {
                            json!({ "output": response_str })
                        },
                    );
                    if !image_parts.is_empty() {
                        fr.insert("parts".into(), Value::Array(image_parts));
                    }
                    fr_parts.push(json!({ "functionResponse": Value::Object(fr) }));
                    i += 1;
                }

                contents.push(json!({
                    "role": "user",
                    "parts": fr_parts,
                }));
            }
        }
    }

    contents
}

fn uses_thinking_level(model_id: &str) -> bool {
    let lower = model_id.to_ascii_lowercase();
    lower.contains("gemini-3") || lower.contains("gemini-4")
}

fn build_thinking_config(model: &Model, level: ThinkingLevel) -> Option<Value> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::None => {
            if uses_thinking_level(&model.id) {
                Some(json!({ "thinkingLevel": "MINIMAL" }))
            } else if model.id.to_ascii_lowercase().contains("pro") {
                None
            } else {
                Some(json!({ "thinkingBudget": 0 }))
            }
        }
        other => {
            if uses_thinking_level(&model.id) {
                let lvl = match other {
                    ThinkingLevel::Minimal => "MINIMAL",
                    ThinkingLevel::Low => "LOW",
                    ThinkingLevel::Medium => "MEDIUM",
                    ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => "HIGH",
                    ThinkingLevel::Off | ThinkingLevel::None => "MINIMAL",
                };
                Some(json!({
                    "includeThoughts": true,
                    "thinkingLevel": lvl,
                }))
            } else {
                let budget = match other {
                    ThinkingLevel::Minimal => 128,
                    ThinkingLevel::Low => 2_048,
                    ThinkingLevel::Medium => 8_192,
                    ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => 24_576,
                    ThinkingLevel::Off | ThinkingLevel::None => 0,
                };
                Some(json!({
                    "includeThoughts": true,
                    "thinkingBudget": budget,
                }))
            }
        }
    }
}

pub fn build_request_body(
    model: &Model,
    context: &TranscriptContext,
    options: &SimpleStreamOptions,
) -> Value {
    let resolved = context.resolve();
    let contents = convert_messages(model, &resolved.messages);

    let mut body = Map::new();
    if !resolved.system_prompt.is_empty() {
        body.insert(
            "systemInstruction".into(),
            json!({
                "parts": [{ "text": resolved.system_prompt }],
            }),
        );
    }

    body.insert("contents".into(), Value::Array(contents));

    if !resolved.tools.is_empty() {
        body.insert("tools".into(), convert_tools(&resolved.tools));
    }

    let mut gen_cfg = Map::new();
    if let Some(temperature) = options.temperature {
        gen_cfg.insert("temperature".into(), json!(temperature));
    }
    if let Some(max_tokens) = options.max_tokens {
        gen_cfg.insert("maxOutputTokens".into(), json!(max_tokens));
    }
    if model.supports_reasoning
        && let Some(level) = options.reasoning
        && let Some(tc) = build_thinking_config(model, level)
    {
        gen_cfg.insert("thinkingConfig".into(), tc);
    }
    for (k, v) in &options.sampling_params {
        gen_cfg.insert(k.clone(), v.clone());
    }
    if !gen_cfg.is_empty() {
        body.insert("generationConfig".into(), Value::Object(gen_cfg));
    }

    Value::Object(body)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActiveGoogleBlock {
    Text(usize),
    Thinking(usize),
}

pub fn stream(
    transport: Arc<dyn HttpTransport>,
    model: Model,
    context: TranscriptContext,
    options: SimpleStreamOptions,
) -> AssistantEventStream {
    drive_event_stream(move |sink| async move {
        let started_at = now_ms();
        let cancel = options.cancel.clone();
        let mut output = new_assistant_message(&model, options.reasoning);

        let body_val = build_request_body(&model, &context, &options);
        let body_bytes = match serde_json::to_vec(&body_val) {
            Ok(v) => Bytes::from(v),
            Err(err) => {
                emit_protocol_error(
                    &sink,
                    output,
                    "serialization_error",
                    format!("failed to serialize Google Generative AI request: {err}"),
                    &cancel,
                    started_at,
                );
                return;
            }
        };

        let mut base_headers = vec![
            ("content-type".into(), "application/json".into()),
            ("accept".into(), "text/event-stream".into()),
        ];
        if let Some(api_key) = options.api_key.as_deref()
            && !api_key.is_empty()
            && !has_header(&model.headers, "x-goog-api-key")
            && !has_header(&model.headers, "authorization")
        {
            base_headers.push(("x-goog-api-key".into(), api_key.to_string()));
        }
        let headers = merge_headers(base_headers, &model.headers, &options.headers);

        let request = HttpRequest {
            method: "POST".into(),
            url: google_stream_url(&model.base_url, &model.id),
            headers,
            body: body_bytes,
            timeout_ms: None,
        };

        let response = match send_cancellable(transport.as_ref(), request, cancel.clone()).await {
            Ok(resp) => resp,
            Err(err) => {
                emit_transport_error(&sink, output, err, &cancel, started_at);
                return;
            }
        };

        sink.send(AssistantMessageEvent::Start {
            partial: output.clone(),
        });

        let mut sse = SseStream::new(response.body, cancel.clone());
        let mut active: Option<ActiveGoogleBlock> = None;
        let mut tool_call_counter = 0u64;

        let close_active = |active: &mut Option<ActiveGoogleBlock>,
                            output: &crate::AssistantMessage,
                            sink: &crate::providers::EventSink| {
            if let Some(cur) = active.take() {
                match cur {
                    ActiveGoogleBlock::Text(idx) => {
                        if let Some(AssistantBlock::Text(b)) = output.content.get(idx) {
                            sink.send(AssistantMessageEvent::TextEnd {
                                content_index: idx,
                                text: b.text.clone(),
                            });
                        }
                    }
                    ActiveGoogleBlock::Thinking(idx) => {
                        if let Some(AssistantBlock::Thinking(b)) = output.content.get(idx) {
                            sink.send(AssistantMessageEvent::ThinkingEnd {
                                content_index: idx,
                                thinking: b.thinking.clone(),
                            });
                        }
                    }
                }
            }
        };

        while let Some(frame) = sse.next().await {
            let ev = match frame {
                Ok(ev) => ev,
                Err(err) => {
                    emit_transport_error(&sink, output, err, &cancel, started_at);
                    return;
                }
            };
            if ev.data.trim().is_empty() || ev.is_done() {
                continue;
            }

            let chunk: Value = match serde_json::from_str(&ev.data) {
                Ok(v) => v,
                Err(err) => {
                    emit_protocol_error(
                        &sink,
                        output,
                        "sse_parse_error",
                        format!("invalid Google SSE JSON: {err}"),
                        &cancel,
                        started_at,
                    );
                    return;
                }
            };

            if let Some(err_obj) = chunk.get("error") {
                let msg = err_obj
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Google Generative AI stream error");
                emit_protocol_error(&sink, output, "google_error", msg, &cancel, started_at);
                return;
            }

            if output.response_id.is_none()
                && let Some(id) = chunk.get("responseId").and_then(Value::as_str)
            {
                output.response_id = Some(id.to_string());
            }

            if let Some(candidate) = chunk
                .get("candidates")
                .and_then(Value::as_array)
                .and_then(|arr| arr.first())
            {
                if let Some(parts) = candidate
                    .get("content")
                    .and_then(|c| c.get("parts"))
                    .and_then(Value::as_array)
                {
                    for part in parts {
                        let thought_sig = part
                            .get("thoughtSignature")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty());

                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            let is_thinking =
                                part.get("thought").and_then(Value::as_bool) == Some(true);
                            let need_switch = match active {
                                None => true,
                                Some(ActiveGoogleBlock::Thinking(_)) => !is_thinking,
                                Some(ActiveGoogleBlock::Text(_)) => is_thinking,
                            };
                            if need_switch {
                                close_active(&mut active, &output, &sink);
                                let idx = output.content.len();
                                if is_thinking {
                                    output.content.push(AssistantBlock::Thinking(
                                        ThinkingContent {
                                            thinking: String::new(),
                                            thinking_signature: None,
                                            redacted: false,
                                        },
                                    ));
                                    active = Some(ActiveGoogleBlock::Thinking(idx));
                                    sink.send(AssistantMessageEvent::ThinkingStart {
                                        content_index: idx,
                                    });
                                } else {
                                    output
                                        .content
                                        .push(AssistantBlock::Text(TextContent::plain(
                                            String::new(),
                                        )));
                                    active = Some(ActiveGoogleBlock::Text(idx));
                                    sink.send(AssistantMessageEvent::TextStart {
                                        content_index: idx,
                                    });
                                }
                            }

                            match active {
                                Some(ActiveGoogleBlock::Thinking(idx)) => {
                                    if let Some(AssistantBlock::Thinking(b)) =
                                        output.content.get_mut(idx)
                                    {
                                        b.thinking.push_str(text);
                                        if let Some(sig) = thought_sig {
                                            b.thinking_signature = Some(sig.to_string());
                                        }
                                        sink.send(AssistantMessageEvent::ThinkingDelta {
                                            content_index: idx,
                                            delta: text.to_string(),
                                        });
                                    }
                                }
                                Some(ActiveGoogleBlock::Text(idx)) => {
                                    if let Some(AssistantBlock::Text(b)) =
                                        output.content.get_mut(idx)
                                    {
                                        b.text.push_str(text);
                                        if let Some(sig) = thought_sig {
                                            b.text_signature = Some(sig.to_string());
                                        }
                                        sink.send(AssistantMessageEvent::TextDelta {
                                            content_index: idx,
                                            delta: text.to_string(),
                                        });
                                    }
                                }
                                None => {}
                            }
                        }

                        if let Some(fc) = part.get("functionCall") {
                            close_active(&mut active, &output, &sink);
                            let name = fc
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let provided_id = fc
                                .get("id")
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty());
                            let is_dup = provided_id.is_some_and(|pid| {
                                output.content.iter().any(|b| {
                                    matches!(b, AssistantBlock::ToolCall(tc) if tc.id == pid)
                                })
                            });
                            let id = match provided_id {
                                Some(pid) if !is_dup => pid.to_string(),
                                _ => {
                                    tool_call_counter += 1;
                                    format!("{name}_{tool_call_counter}")
                                }
                            };

                            let (arguments, arguments_error) = match fc.get("args") {
                                Some(Value::Object(map)) => (map.clone(), None),
                                None | Some(Value::Null) => (Map::new(), None),
                                Some(other) => (
                                    Map::new(),
                                    Some(format!(
                                        "Tool arguments must be a JSON object\n\nReceived arguments:\n{other}"
                                    )),
                                ),
                            };

                            let tc = ToolCall {
                                id: id.clone(),
                                name: name.clone(),
                                arguments,
                                item_id: None,
                                thought_signature: thought_sig.map(ToString::to_string),
                                namespace: None,
                                arguments_error,
                            };

                            let content_index = output.content.len();
                            let args_delta =
                                serde_json::to_string(&Value::Object(tc.arguments.clone()))
                                    .unwrap_or_else(|_| "{}".into());
                            output.content.push(AssistantBlock::ToolCall(tc.clone()));

                            sink.send(AssistantMessageEvent::ToolCallStart {
                                content_index,
                                id,
                                name,
                            });
                            sink.send(AssistantMessageEvent::ToolCallDelta {
                                content_index,
                                delta: args_delta,
                            });
                            sink.send(AssistantMessageEvent::ToolCallEnd {
                                content_index,
                                tool_call: tc,
                            });
                        }
                    }
                }

                if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
                    output.stop_reason = match reason {
                        "STOP" => {
                            let has_tools = output
                                .content
                                .iter()
                                .any(|b| matches!(b, AssistantBlock::ToolCall(_)));
                            if has_tools {
                                StopReason::ToolUse
                            } else {
                                StopReason::Stop
                            }
                        }
                        "MAX_TOKENS" | "CONTINUATION" => StopReason::Length,
                        other => {
                            output.error_message =
                                Some(format!("Provider stopped with: {other}"));
                            StopReason::Error
                        }
                    };
                }
            }

            if let Some(u) = chunk.get("usageMetadata") {
                let prompt = u
                    .get("promptTokenCount")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let cached = u
                    .get("cachedContentTokenCount")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let candidates = u
                    .get("candidatesTokenCount")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let thoughts = u.get("thoughtsTokenCount").and_then(Value::as_u64);
                let total = u
                    .get("totalTokenCount")
                    .and_then(Value::as_u64)
                    .unwrap_or(prompt + candidates + thoughts.unwrap_or(0));

                output.usage = Usage {
                    input: prompt.saturating_sub(cached),
                    output: candidates + thoughts.unwrap_or(0),
                    cache_read: cached,
                    cache_write: 0,
                    reasoning: thoughts,
                    total_tokens: total,
                    cost_total: 0.0,
                    cost_from_provider: false,
                };
                calculate_cost(&model.cost, &mut output.usage);
            }
        }

        close_active(&mut active, &output, &sink);

        if cancel.is_cancelled() {
            emit_transport_error(
                &sink,
                output,
                crate::TransportError::Aborted,
                &cancel,
                started_at,
            );
            return;
        }

        if output.stop_reason == StopReason::Pending {
            emit_protocol_error(
                &sink,
                output,
                "incomplete_stream",
                "Google stream ended without a finishReason",
                &cancel,
                started_at,
            );
            return;
        }

        if output.stop_reason == StopReason::Error {
            let msg = output
                .error_message
                .clone()
                .unwrap_or_else(|| "Google request stopped with error".into());
            emit_protocol_error(&sink, output, "finish_error", msg, &cancel, started_at);
            return;
        }

        output.duration_ms = Some(now_ms().saturating_sub(started_at));
        sink.send(AssistantMessageEvent::Done {
            reason: output.stop_reason,
            message: output,
        });
    })
}
