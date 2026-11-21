//! OpenAI Chat Completions (`POST {base_url}/chat/completions`) streaming adapter.
//! Supports OpenAI, DeepSeek, OpenRouter, and vLLM (`reasoning_content` deltas, `sampling_params`, malformed tool JSON).

use crate::providers::{
    drive_event_stream, emit_protocol_error, emit_transport_error, new_assistant_message, now_ms,
    send_cancellable, thinking_level_effort_str,
};
use crate::sse::SseStream;
use crate::transform::{normalize_tool_call_id_openai_completions, transform_messages};
use crate::usage::{
    calculate_cost, has_header, merge_headers, openai_completions_url, parse_tool_call_arguments,
};
use crate::{
    AssistantBlock, AssistantEventStream, AssistantMessageEvent, ContentPart, HttpRequest,
    HttpTransport, MaxTokensField, Message, Model, SimpleStreamOptions, StopReason, TextContent,
    ThinkingContent, ThinkingLevel, ToolCall, ToolDefinition, TranscriptContext, Usage,
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::Arc;

fn convert_tools(tools: &[ToolDefinition]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            let mut func = Map::new();
            func.insert("name".into(), Value::String(tool.name.clone()));
            func.insert(
                "description".into(),
                Value::String(tool.description.clone()),
            );
            func.insert("parameters".into(), tool.parameters.clone());
            let mut obj = Map::new();
            obj.insert("type".into(), Value::String("function".into()));
            obj.insert("function".into(), Value::Object(func));
            Value::Object(obj)
        })
        .collect()
}

fn convert_messages(model: &Model, system_prompt: &str, messages: &[Message]) -> Vec<Value> {
    let transformed = transform_messages(messages, model, |id, _, _| {
        normalize_tool_call_id_openai_completions(id)
    });

    let mut out = Vec::new();
    if !system_prompt.is_empty() {
        out.push(json!({
            "role": "system",
            "content": system_prompt,
        }));
    }

    let mut i = 0;
    while i < transformed.len() {
        match &transformed[i] {
            Message::System(_) => {
                i += 1;
            }
            Message::User(user) => {
                let has_images = user
                    .content
                    .iter()
                    .any(|p| matches!(p, ContentPart::Image(_)));
                if !has_images {
                    let joined: String = user
                        .content
                        .iter()
                        .filter_map(|p| match p {
                            ContentPart::Text(t) => Some(t.text.as_str()),
                            ContentPart::Image(_) => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !joined.is_empty() {
                        out.push(json!({
                            "role": "user",
                            "content": joined,
                        }));
                    }
                } else {
                    let mut parts = Vec::new();
                    for part in &user.content {
                        match part {
                            ContentPart::Text(t) if !t.text.is_empty() => {
                                parts.push(json!({ "type": "text", "text": t.text }));
                            }
                            ContentPart::Image(img) => {
                                parts.push(json!({
                                    "type": "image_url",
                                    "image_url": {
                                        "url": format!("data:{};base64,{}", img.mime_type, img.data),
                                    }
                                }));
                            }
                            _ => {}
                        }
                    }
                    if !parts.is_empty() {
                        out.push(json!({
                            "role": "user",
                            "content": parts,
                        }));
                    }
                }
                i += 1;
            }
            Message::Assistant(assistant) => {
                let mut text_parts = Vec::new();
                let mut thinking_parts = Vec::new();
                let mut tool_calls = Vec::new();

                for block in &assistant.content {
                    match block {
                        AssistantBlock::Text(t) if !t.text.is_empty() => {
                            text_parts.push(t.text.as_str());
                        }
                        AssistantBlock::Thinking(th) if !th.thinking.is_empty() => {
                            thinking_parts.push(th.thinking.as_str());
                        }
                        AssistantBlock::ToolCall(tc) => {
                            let args_str =
                                serde_json::to_string(&Value::Object(tc.arguments.clone()))
                                    .unwrap_or_else(|_| "{}".into());
                            tool_calls.push(json!({
                                "id": tc.id,
                                "type": "function",
                                "function": {
                                    "name": tc.name,
                                    "arguments": args_str,
                                }
                            }));
                        }
                        _ => {}
                    }
                }

                if text_parts.is_empty() && thinking_parts.is_empty() && tool_calls.is_empty() {
                    i += 1;
                    continue;
                }

                let mut obj = Map::new();
                obj.insert("role".into(), Value::String("assistant".into()));
                if text_parts.is_empty() {
                    obj.insert("content".into(), Value::Null);
                } else {
                    obj.insert("content".into(), Value::String(text_parts.join("")));
                }
                if !thinking_parts.is_empty() {
                    obj.insert(
                        "reasoning_content".into(),
                        Value::String(thinking_parts.join("\n")),
                    );
                }
                if !tool_calls.is_empty() {
                    obj.insert("tool_calls".into(), Value::Array(tool_calls));
                }
                out.push(Value::Object(obj));
                i += 1;
            }
            Message::ToolResult(_) => {
                let mut batch_images: Vec<Value> = Vec::new();
                while i < transformed.len() {
                    let Message::ToolResult(tr) = &transformed[i] else {
                        break;
                    };
                    let mut text_parts = Vec::new();
                    let mut has_images = false;
                    for part in &tr.content {
                        match part {
                            ContentPart::Text(t) => text_parts.push(t.text.as_str()),
                            ContentPart::Image(img) => {
                                has_images = true;
                                if model.supports_images {
                                    batch_images.push(json!({
                                        "type": "image_url",
                                        "image_url": {
                                            "url": format!("data:{};base64,{}", img.mime_type, img.data),
                                        }
                                    }));
                                }
                            }
                        }
                    }
                    let text = text_parts.join("\n");
                    let content = if text.is_empty() && has_images {
                        "[image attached]".to_string()
                    } else {
                        text
                    };
                    out.push(json!({
                        "role": "tool",
                        "tool_call_id": tr.tool_call_id,
                        "content": content,
                    }));
                    i += 1;
                }

                if model.supports_images && !batch_images.is_empty() {
                    let mut user_blocks = Vec::with_capacity(batch_images.len() + 1);
                    user_blocks.push(json!({
                        "type": "text",
                        "text": "Attached image(s) from tool result:",
                    }));
                    user_blocks.extend(batch_images);
                    out.push(json!({
                        "role": "user",
                        "content": user_blocks,
                    }));
                }
            }
        }
    }

    out
}

pub fn build_request_body(
    model: &Model,
    context: &TranscriptContext,
    options: &SimpleStreamOptions,
) -> Value {
    let resolved = context.resolve();
    let messages = convert_messages(model, &resolved.system_prompt, &resolved.messages);

    let mut body = Map::new();
    body.insert("model".into(), Value::String(model.id.clone()));
    body.insert("messages".into(), Value::Array(messages));
    body.insert("stream".into(), Value::Bool(true));
    body.insert("stream_options".into(), json!({ "include_usage": true }));

    if !resolved.tools.is_empty() {
        body.insert("tools".into(), Value::Array(convert_tools(&resolved.tools)));
    }

    if let Some(raw_max_tokens) = options.max_tokens {
        let max_tokens = crate::clamp_max_tokens_to_context(model, &resolved, raw_max_tokens);
        let field = match model.compat.as_ref().and_then(|c| c.max_tokens_field) {
            Some(MaxTokensField::MaxCompletionTokens) => "max_completion_tokens",
            Some(MaxTokensField::MaxTokens) => "max_tokens",
            None => {
                if model.provider == "openai" || model.base_url.contains("api.openai.com") {
                    "max_completion_tokens"
                } else {
                    "max_tokens"
                }
            }
        };
        body.insert(field.into(), json!(max_tokens));
    }

    if let Some(temperature) = options.temperature {
        body.insert("temperature".into(), json!(temperature));
    }

    if model.supports_reasoning
        && let Some(level) = options.reasoning
        && let Some(effort) = thinking_level_effort_str(level)
    {
        let is_deepseek = model.provider.eq_ignore_ascii_case("deepseek")
            || model.base_url.to_ascii_lowercase().contains("deepseek");
        if is_deepseek && level == ThinkingLevel::None {
            body.insert("thinking".into(), json!({ "type": "disabled" }));
        } else {
            body.insert("reasoning_effort".into(), Value::String(effort.into()));
        }
    }

    for (k, v) in &options.sampling_params {
        body.insert(k.clone(), v.clone());
    }

    Value::Object(body)
}

fn parse_usage(usage_val: &Value, model: &Model) -> Usage {
    let prompt_tokens = usage_val
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let completion_tokens = usage_val
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached_tokens = usage_val
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning_tokens = usage_val
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_u64);
    let total_tokens = usage_val
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(prompt_tokens + completion_tokens);

    let mut usage = Usage {
        input: prompt_tokens.saturating_sub(cached_tokens),
        output: completion_tokens,
        cache_read: cached_tokens,
        cache_write: 0,
        reasoning: reasoning_tokens,
        total_tokens,
        cost_total: 0.0,
        cost_from_provider: false,
    };

    if let Some(billed) = usage_val.get("cost").and_then(Value::as_f64) {
        usage.cost_total = billed;
        usage.cost_from_provider = true;
    } else {
        calculate_cost(&model.cost, &mut usage);
    }

    usage
}

struct PendingToolCall {
    content_index: usize,
    raw_args: String,
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
                    format!("failed to serialize OpenAI Completions request: {err}"),
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
            && !has_header(&model.headers, "authorization")
        {
            base_headers.push(("authorization".into(), format!("Bearer {api_key}")));
        }
        let headers = merge_headers(base_headers, &model.headers, &options.headers);

        let request = HttpRequest {
            method: "POST".into(),
            url: openai_completions_url(&model.base_url),
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
        let mut thinking_index: Option<usize> = None;
        let mut text_index: Option<usize> = None;
        let mut tool_calls_by_idx: HashMap<usize, PendingToolCall> = HashMap::new();
        let mut saw_finish_reason = false;
        let mut saw_done = false;

        while let Some(frame) = sse.next().await {
            let ev = match frame {
                Ok(ev) => ev,
                Err(err) => {
                    emit_transport_error(&sink, output, err, &cancel, started_at);
                    return;
                }
            };
            if ev.data.trim().is_empty() {
                continue;
            }
            if ev.is_done() {
                saw_done = true;
                break;
            }

            let chunk: Value = match serde_json::from_str(&ev.data) {
                Ok(v) => v,
                Err(err) => {
                    emit_protocol_error(
                        &sink,
                        output,
                        "sse_parse_error",
                        format!("invalid OpenAI Completions SSE JSON: {err}"),
                        &cancel,
                        started_at,
                    );
                    return;
                }
            };

            if let Some(err_val) = chunk.get("error").filter(|v| !v.is_null()) {
                let err_msg = err_val
                    .get("message")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
                    .unwrap_or_else(|| err_val.to_string());
                emit_protocol_error(&sink, output, "stream_error", err_msg, &cancel, started_at);
                return;
            }

            if output.response_id.is_none()
                && let Some(id) = chunk.get("id").and_then(Value::as_str)
            {
                output.response_id = Some(id.to_string());
            }

            if let Some(usage_val) = chunk.get("usage").filter(|v| v.is_object()) {
                output.usage = parse_usage(usage_val, &model);
            }

            let Some(choice) = chunk
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|arr| arr.first())
            else {
                continue;
            };

            if chunk.get("usage").is_none()
                && let Some(usage_val) = choice.get("usage").filter(|v| v.is_object())
            {
                output.usage = parse_usage(usage_val, &model);
            }

            if let Some(delta) = choice.get("delta") {
                // Reasoning fields (DeepSeek / OpenRouter / vLLM / llama.cpp).
                let reasoning_delta = ["reasoning_content", "reasoning", "reasoning_text"]
                    .iter()
                    .find_map(|field| {
                        delta
                            .get(*field)
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                    });

                if let Some(r_delta) = reasoning_delta {
                    let idx = match thinking_index {
                        Some(idx) => idx,
                        None => {
                            let idx = output.content.len();
                            output
                                .content
                                .push(AssistantBlock::Thinking(ThinkingContent {
                                    thinking: String::new(),
                                    thinking_signature: None,
                                    redacted: false,
                                }));
                            thinking_index = Some(idx);
                            sink.send(AssistantMessageEvent::ThinkingStart { content_index: idx });
                            idx
                        }
                    };
                    if let Some(AssistantBlock::Thinking(block)) = output.content.get_mut(idx) {
                        block.thinking.push_str(r_delta);
                        sink.send(AssistantMessageEvent::ThinkingDelta {
                            content_index: idx,
                            delta: r_delta.to_string(),
                        });
                    }
                }

                // Visible text delta.
                if let Some(text_delta) = delta
                    .get("content")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    let idx = match text_index {
                        Some(idx) => idx,
                        None => {
                            let idx = output.content.len();
                            output
                                .content
                                .push(AssistantBlock::Text(TextContent::plain(String::new())));
                            text_index = Some(idx);
                            sink.send(AssistantMessageEvent::TextStart { content_index: idx });
                            idx
                        }
                    };
                    if let Some(AssistantBlock::Text(block)) = output.content.get_mut(idx) {
                        block.text.push_str(text_delta);
                        sink.send(AssistantMessageEvent::TextDelta {
                            content_index: idx,
                            delta: text_delta.to_string(),
                        });
                    }
                }

                // Tool call deltas accumulated by stream index.
                if let Some(tc_deltas) = delta.get("tool_calls").and_then(Value::as_array) {
                    for tc_delta in tc_deltas {
                        let stream_idx =
                            tc_delta.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                        let id_opt = tc_delta.get("id").and_then(Value::as_str);
                        let name_opt = tc_delta
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(Value::as_str);
                        let args_delta = tc_delta
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(Value::as_str)
                            .unwrap_or("");

                        let entry = tool_calls_by_idx.entry(stream_idx).or_insert_with(|| {
                            let content_index = output.content.len();
                            let id = id_opt.unwrap_or("").to_string();
                            let name = name_opt.unwrap_or("").to_string();
                            output.content.push(AssistantBlock::ToolCall(ToolCall {
                                id: id.clone(),
                                name: name.clone(),
                                arguments: Map::new(),
                                item_id: None,
                                thought_signature: None,
                                namespace: None,
                                arguments_error: None,
                            }));
                            sink.send(AssistantMessageEvent::ToolCallStart {
                                content_index,
                                id,
                                name,
                            });
                            PendingToolCall {
                                content_index,
                                raw_args: String::new(),
                            }
                        });

                        if let Some(AssistantBlock::ToolCall(tc)) =
                            output.content.get_mut(entry.content_index)
                        {
                            if tc.id.is_empty()
                                && let Some(id) = id_opt
                            {
                                tc.id = id.to_string();
                            }
                            if tc.name.is_empty()
                                && let Some(name) = name_opt
                            {
                                tc.name = name.to_string();
                            }
                        }

                        if !args_delta.is_empty() {
                            entry.raw_args.push_str(args_delta);
                            sink.send(AssistantMessageEvent::ToolCallDelta {
                                content_index: entry.content_index,
                                delta: args_delta.to_string(),
                            });
                        }
                    }
                }
            }

            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                saw_finish_reason = true;
                output.stop_reason = match reason {
                    "stop" => StopReason::Stop,
                    "length" => StopReason::Length,
                    "tool_calls" | "function_call" => StopReason::ToolUse,
                    "content_filter" => {
                        output.error_message = Some("Response stopped by content_filter".into());
                        StopReason::Error
                    }
                    other => {
                        output.error_message = Some(format!("Provider stopped with: {other}"));
                        StopReason::Error
                    }
                };
            }
        }

        // Finalize all blocks in content order, parsing tool call arguments without dropping malformed calls.
        let mut raw_args_by_content_index: HashMap<usize, String> = tool_calls_by_idx
            .into_values()
            .map(|p| (p.content_index, p.raw_args))
            .collect();

        for content_index in 0..output.content.len() {
            match &mut output.content[content_index] {
                AssistantBlock::Thinking(block) => {
                    sink.send(AssistantMessageEvent::ThinkingEnd {
                        content_index,
                        thinking: block.thinking.clone(),
                    });
                }
                AssistantBlock::Text(block) => {
                    sink.send(AssistantMessageEvent::TextEnd {
                        content_index,
                        text: block.text.clone(),
                    });
                }
                AssistantBlock::ToolCall(tc) => {
                    let raw = raw_args_by_content_index
                        .remove(&content_index)
                        .unwrap_or_default();
                    let (args_map, args_err) = parse_tool_call_arguments(&raw);
                    tc.arguments = args_map;
                    tc.arguments_error = args_err;
                    sink.send(AssistantMessageEvent::ToolCallEnd {
                        content_index,
                        tool_call: tc.clone(),
                    });
                }
            }
        }

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

        if !saw_finish_reason {
            if saw_done {
                let has_tools = output
                    .content
                    .iter()
                    .any(|b| matches!(b, AssistantBlock::ToolCall(_)));
                output.stop_reason = if has_tools {
                    StopReason::ToolUse
                } else {
                    StopReason::Stop
                };
            } else {
                emit_protocol_error(
                    &sink,
                    output,
                    "incomplete_stream",
                    "OpenAI Completions stream ended without finish_reason",
                    &cancel,
                    started_at,
                );
                return;
            }
        }

        if output.stop_reason == StopReason::Error {
            let msg = output
                .error_message
                .clone()
                .unwrap_or_else(|| "OpenAI Completions error".into());
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
