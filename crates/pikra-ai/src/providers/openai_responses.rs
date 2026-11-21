//! OpenAI Responses API (`POST {base_url}/responses`) streaming adapter.

use crate::providers::{
    drive_event_stream, emit_protocol_error, emit_transport_error, new_assistant_message, now_ms,
    send_cancellable, thinking_level_effort_str,
};
use crate::sse::SseStream;
use crate::transform::{normalize_tool_call_id_openai_responses, short_hash, transform_messages};
use crate::usage::{
    calculate_cost, clamp_prompt_cache_key, has_header, merge_headers, openai_responses_url,
    parse_tool_call_arguments,
};
use crate::{
    AssistantBlock, AssistantDiagnostic, AssistantEventStream, AssistantMessageEvent,
    CacheRetention, ContentPart, HttpRequest, HttpTransport, Message, Model, SimpleStreamOptions,
    StopReason, TextContent, ThinkingContent, ThinkingLevel, ToolCall, ToolDefinition,
    TranscriptContext, Usage,
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::Arc;

const MIN_OUTPUT_TOKENS: u32 = 16;

fn encode_text_signature(id: &str, phase: Option<&str>) -> String {
    let mut obj = Map::new();
    obj.insert("v".into(), json!(1));
    obj.insert("id".into(), Value::String(id.to_string()));
    if let Some(p) = phase {
        obj.insert("phase".into(), Value::String(p.to_string()));
    }
    Value::Object(obj).to_string()
}

fn parse_text_signature(sig: Option<&str>) -> Option<(String, Option<String>)> {
    let sig = sig?;
    if sig.starts_with('{')
        && let Ok(Value::Object(map)) = serde_json::from_str::<Value>(sig)
        && map.get("v").and_then(Value::as_u64) == Some(1)
        && let Some(id) = map.get("id").and_then(Value::as_str)
    {
        let phase = map
            .get("phase")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        return Some((id.to_string(), phase));
    }
    Some((sig.to_string(), None))
}

pub(crate) fn convert_tools(tools: &[ToolDefinition]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            let mut obj = Map::new();
            obj.insert("type".into(), Value::String("function".into()));
            obj.insert("name".into(), Value::String(tool.name.clone()));
            obj.insert("description".into(), Value::String(tool.description.clone()));
            obj.insert("parameters".into(), tool.parameters.clone());
            Value::Object(obj)
        })
        .collect()
}

fn convert_tool_output(model: &Model, content: &[ContentPart]) -> Value {
    let mut text_parts = Vec::new();
    let mut images = Vec::new();
    for part in content {
        match part {
            ContentPart::Text(t) => text_parts.push(t.text.as_str()),
            ContentPart::Image(img) => images.push(img),
        }
    }
    let joined_text = text_parts.join("\n");
    let has_text = !joined_text.trim().is_empty();

    if images.is_empty() || !model.supports_images {
        let s = if has_text {
            joined_text
        } else if !images.is_empty() {
            "(see attached image)".to_string()
        } else {
            "(no tool output)".to_string()
        };
        return Value::String(s);
    }

    let mut out = Vec::new();
    if has_text {
        out.push(json!({ "type": "input_text", "text": joined_text }));
    }
    for img in images {
        out.push(json!({
            "type": "input_image",
            "detail": "auto",
            "image_url": format!("data:{};base64,{}", img.mime_type, img.data),
        }));
    }
    Value::Array(out)
}

pub(crate) fn convert_messages(model: &Model, messages: &[Message]) -> Vec<Value> {
    let transformed = transform_messages(messages, model, |id, target, src| {
        normalize_tool_call_id_openai_responses(
            id,
            src.provider == target.provider && src.api == target.api,
        )
    });

    let mut input = Vec::new();
    for (msg_idx, msg) in transformed.iter().enumerate() {
        match msg {
            Message::System(_) => {}
            Message::User(user) => {
                let mut parts = Vec::new();
                for part in &user.content {
                    match part {
                        ContentPart::Text(t) => {
                            parts.push(json!({ "type": "input_text", "text": t.text }));
                        }
                        ContentPart::Image(img) => {
                            parts.push(json!({
                                "type": "input_image",
                                "detail": "auto",
                                "image_url": format!("data:{};base64,{}", img.mime_type, img.data),
                            }));
                        }
                    }
                }
                if !parts.is_empty() {
                    input.push(json!({
                        "role": "user",
                        "content": parts,
                    }));
                }
            }
            Message::Assistant(assistant) => {
                let is_same_model = assistant.provider == model.provider
                    && assistant.api == model.api
                    && assistant.model == model.id;
                let mut text_idx = 0usize;

                for block in &assistant.content {
                    match block {
                        AssistantBlock::Thinking(thinking) => {
                            if let Some(sig) = thinking.thinking_signature.as_deref()
                                && let Ok(parsed) = serde_json::from_str::<Value>(sig)
                                && parsed.is_object()
                            {
                                input.push(parsed);
                            }
                        }
                        AssistantBlock::Text(text) => {
                            let parsed_sig = parse_text_signature(text.text_signature.as_deref());
                            let fallback_id = if text_idx == 0 {
                                format!("msg_pikra_{msg_idx}")
                            } else {
                                format!("msg_pikra_{msg_idx}_{text_idx}")
                            };
                            text_idx += 1;

                            let (mut msg_id, phase) = match parsed_sig {
                                Some((id, phase)) => (id, phase),
                                None => (fallback_id, None),
                            };
                            if msg_id.len() > 64 {
                                msg_id = format!("msg_{}", short_hash(&msg_id));
                            }

                            let mut item = Map::new();
                            item.insert("type".into(), json!("message"));
                            item.insert("role".into(), json!("assistant"));
                            item.insert("id".into(), Value::String(msg_id));
                            item.insert("status".into(), json!("completed"));
                            if let Some(p) = phase {
                                item.insert("phase".into(), Value::String(p));
                            }
                            item.insert(
                                "content".into(),
                                json!([{
                                    "type": "output_text",
                                    "text": text.text,
                                    "annotations": [],
                                }]),
                            );
                            input.push(Value::Object(item));
                        }
                        AssistantBlock::ToolCall(tc) => {
                            let mut item = Map::new();
                            item.insert("type".into(), json!("function_call"));
                            if is_same_model
                                && let Some(item_id) = tc.item_id.as_deref()
                                && item_id.starts_with("fc_")
                            {
                                item.insert("id".into(), Value::String(item_id.to_string()));
                            }
                            item.insert("call_id".into(), Value::String(tc.id.clone()));
                            item.insert("name".into(), Value::String(tc.name.clone()));
                            let args_str = serde_json::to_string(&Value::Object(tc.arguments.clone()))
                                .unwrap_or_else(|_| "{}".into());
                            item.insert("arguments".into(), Value::String(args_str));
                            if is_same_model && let Some(ns) = &tc.namespace {
                                item.insert("namespace".into(), Value::String(ns.clone()));
                            }
                            input.push(Value::Object(item));
                        }
                    }
                }
            }
            Message::ToolResult(tr) => {
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": &tr.tool_call_id,
                    "output": convert_tool_output(model, &tr.content),
                }));
            }
        }
    }

    input
}

pub fn build_request_body(
    model: &Model,
    context: &TranscriptContext,
    options: &SimpleStreamOptions,
) -> Value {
    let resolved = context.resolve();
    let input = convert_messages(model, &resolved.messages);

    let mut body = Map::new();
    body.insert("model".into(), Value::String(model.id.clone()));
    body.insert("stream".into(), Value::Bool(true));
    body.insert("store".into(), Value::Bool(false));

    if !resolved.system_prompt.is_empty() {
        body.insert(
            "instructions".into(),
            Value::String(resolved.system_prompt.clone()),
        );
    }

    body.insert("input".into(), Value::Array(input));

    if !resolved.tools.is_empty() {
        body.insert("tools".into(), Value::Array(convert_tools(&resolved.tools)));
    }

    if options.cache_retention != CacheRetention::None {
        if let Some(session_id) = options.session_id.as_deref()
            && !session_id.is_empty()
        {
            body.insert(
                "prompt_cache_key".into(),
                Value::String(clamp_prompt_cache_key(session_id)),
            );
        }
        if options.cache_retention == CacheRetention::Long {
            body.insert("prompt_cache_retention".into(), Value::String("24h".into()));
        }
    }

    if options.fast == Some(true) {
        body.insert("service_tier".into(), Value::String("priority".into()));
    }

    if let Some(max_tokens) = options.max_tokens {
        body.insert(
            "max_output_tokens".into(),
            json!(max_tokens.max(MIN_OUTPUT_TOKENS)),
        );
    }

    if let Some(temperature) = options.temperature {
        body.insert("temperature".into(), json!(temperature));
    }

    if model.supports_reasoning
        && let Some(level) = options.reasoning
        && let Some(effort) = thinking_level_effort_str(level)
    {
        if level == ThinkingLevel::None {
            body.insert("reasoning".into(), json!({ "effort": effort }));
        } else {
            body.insert(
                "reasoning".into(),
                json!({
                    "effort": effort,
                    "summary": "auto",
                }),
            );
            body.insert("include".into(), json!(["reasoning.encrypted_content"]));
        }
    }

    for (k, v) in &options.sampling_params {
        body.insert(k.clone(), v.clone());
    }

    Value::Object(body)
}

enum OutputSlot {
    Thinking { content_index: usize },
    Text { content_index: usize },
    ToolCall { content_index: usize, raw_args: String },
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
        let output = new_assistant_message(&model, options.reasoning);

        let body_val = build_request_body(&model, &context, &options);
        let body_bytes = match serde_json::to_vec(&body_val) {
            Ok(v) => Bytes::from(v),
            Err(err) => {
                emit_protocol_error(
                    &sink,
                    output,
                    "serialization_error",
                    format!("failed to serialize OpenAI Responses request: {err}"),
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
            url: openai_responses_url(&model.base_url),
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

        consume_responses_sse_stream(response.body, &model, output, &sink, &cancel, started_at)
            .await;
    })
}

pub(crate) async fn consume_responses_sse_stream(
    response_body: crate::ByteStream,
    model: &Model,
    mut output: crate::AssistantMessage,
    sink: &crate::providers::EventSink,
    cancel: &crate::CancelToken,
    started_at: u64,
) {
        sink.send(AssistantMessageEvent::Start {
            partial: output.clone(),
        });

        let mut sse = SseStream::new(response_body, cancel.clone());
        let mut slots: HashMap<usize, OutputSlot> = HashMap::new();
        let mut reasoning_by_id: HashMap<String, usize> = HashMap::new();
        let mut saw_terminal = false;

        while let Some(frame) = sse.next().await {
            let ev = match frame {
                Ok(ev) => ev,
                Err(err) => {
                    emit_transport_error(sink, output, err, cancel, started_at);
                    return;
                }
            };
            if ev.data.trim().is_empty() || ev.is_done() {
                continue;
            }

            let parsed: Value = match serde_json::from_str(&ev.data) {
                Ok(v) => v,
                Err(err) => {
                    emit_protocol_error(
                        sink,
                        output,
                        "sse_parse_error",
                        format!("invalid OpenAI Responses SSE JSON: {err}"),
                        cancel,
                        started_at,
                    );
                    return;
                }
            };

            let ev_type = parsed
                .get("type")
                .and_then(Value::as_str)
                .or(ev.event.as_deref())
                .unwrap_or("");

            match ev_type {
                "response.created" => {
                    if let Some(id) = parsed
                        .get("response")
                        .and_then(|r| r.get("id"))
                        .and_then(Value::as_str)
                    {
                        output.response_id = Some(id.to_string());
                    }
                }
                "response.output_item.added" => {
                    let output_index = parsed
                        .get("output_index")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as usize;
                    if let Some(item) = parsed.get("item") {
                        create_slot_from_item(
                            output_index,
                            item,
                            &mut output,
                            &mut slots,
                            sink,
                        );
                    }
                }
                "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                    let output_index = parsed
                        .get("output_index")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as usize;
                    let delta = parsed.get("delta").and_then(Value::as_str).unwrap_or("");
                    if let Some(OutputSlot::Thinking { content_index }) = slots.get(&output_index)
                        && let Some(AssistantBlock::Thinking(block)) =
                            output.content.get_mut(*content_index)
                    {
                        block.thinking.push_str(delta);
                        sink.send(AssistantMessageEvent::ThinkingDelta {
                            content_index: *content_index,
                            delta: delta.to_string(),
                        });
                    }
                }
                "response.reasoning_summary_part.done" => {
                    let output_index = parsed
                        .get("output_index")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as usize;
                    if let Some(OutputSlot::Thinking { content_index }) = slots.get(&output_index)
                        && let Some(AssistantBlock::Thinking(block)) =
                            output.content.get_mut(*content_index)
                    {
                        block.thinking.push_str("\n\n");
                        sink.send(AssistantMessageEvent::ThinkingDelta {
                            content_index: *content_index,
                            delta: "\n\n".to_string(),
                        });
                    }
                }
                "response.output_text.delta" | "response.refusal.delta" => {
                    let output_index = parsed
                        .get("output_index")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as usize;
                    let delta = parsed.get("delta").and_then(Value::as_str).unwrap_or("");
                    if let Some(OutputSlot::Text { content_index }) = slots.get(&output_index)
                        && let Some(AssistantBlock::Text(block)) =
                            output.content.get_mut(*content_index)
                    {
                        block.text.push_str(delta);
                        sink.send(AssistantMessageEvent::TextDelta {
                            content_index: *content_index,
                            delta: delta.to_string(),
                        });
                    }
                }
                "response.function_call_arguments.delta" => {
                    let output_index = parsed
                        .get("output_index")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as usize;
                    let delta = parsed.get("delta").and_then(Value::as_str).unwrap_or("");
                    if let Some(OutputSlot::ToolCall {
                        content_index,
                        raw_args,
                    }) = slots.get_mut(&output_index)
                    {
                        raw_args.push_str(delta);
                        sink.send(AssistantMessageEvent::ToolCallDelta {
                            content_index: *content_index,
                            delta: delta.to_string(),
                        });
                    }
                }
                "response.function_call_arguments.done" => {
                    let output_index = parsed
                        .get("output_index")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as usize;
                    if let Some(full) = parsed.get("arguments").and_then(Value::as_str)
                        && let Some(OutputSlot::ToolCall {
                            content_index,
                            raw_args,
                        }) = slots.get_mut(&output_index)
                    {
                        if let Some(extra) = full.strip_prefix(raw_args.as_str()) {
                            if !extra.is_empty() {
                                raw_args.push_str(extra);
                                sink.send(AssistantMessageEvent::ToolCallDelta {
                                    content_index: *content_index,
                                    delta: extra.to_string(),
                                });
                            }
                        } else {
                            *raw_args = full.to_string();
                        }
                    }
                }
                "response.output_item.done" => {
                    let output_index = parsed
                        .get("output_index")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as usize;
                    let Some(item) = parsed.get("item") else {
                        continue;
                    };
                    if !slots.contains_key(&output_index) {
                        create_slot_from_item(output_index, item, &mut output, &mut slots, sink);
                    }
                    let Some(slot) = slots.remove(&output_index) else {
                        continue;
                    };

                    match slot {
                        OutputSlot::Thinking { content_index } => {
                            if let Some(AssistantBlock::Thinking(block)) =
                                output.content.get_mut(content_index)
                            {
                                let summary = item
                                    .get("summary")
                                    .and_then(Value::as_array)
                                    .map(|arr| {
                                        arr.iter()
                                            .filter_map(|s| s.get("text").and_then(Value::as_str))
                                            .collect::<Vec<_>>()
                                            .join("\n\n")
                                    })
                                    .unwrap_or_default();
                                let content_text = item
                                    .get("content")
                                    .and_then(Value::as_array)
                                    .map(|arr| {
                                        arr.iter()
                                            .filter_map(|c| c.get("text").and_then(Value::as_str))
                                            .collect::<Vec<_>>()
                                            .join("\n\n")
                                    })
                                    .unwrap_or_default();
                                if !summary.is_empty() {
                                    block.thinking = summary;
                                } else if !content_text.is_empty() {
                                    block.thinking = content_text;
                                }
                                block.thinking_signature = Some(item.to_string());
                                if let Some(item_id) = item.get("id").and_then(Value::as_str) {
                                    reasoning_by_id.insert(item_id.to_string(), content_index);
                                }
                                let final_thinking = block.thinking.clone();
                                sink.send(AssistantMessageEvent::ThinkingEnd {
                                    content_index,
                                    thinking: final_thinking,
                                });
                            }
                        }
                        OutputSlot::Text { content_index } => {
                            if let Some(AssistantBlock::Text(block)) =
                                output.content.get_mut(content_index)
                            {
                                if let Some(parts) = item.get("content").and_then(Value::as_array) {
                                    let assembled: String = parts
                                        .iter()
                                        .filter_map(|p| {
                                            p.get("text")
                                                .and_then(Value::as_str)
                                                .or_else(|| p.get("refusal").and_then(Value::as_str))
                                        })
                                        .collect();
                                    if !assembled.is_empty() || block.text.is_empty() {
                                        block.text = assembled;
                                    }
                                }
                                let item_id =
                                    item.get("id").and_then(Value::as_str).unwrap_or("");
                                let phase = item.get("phase").and_then(Value::as_str);
                                if !item_id.is_empty() {
                                    block.text_signature =
                                        Some(encode_text_signature(item_id, phase));
                                }
                                if phase == Some("final_answer") {
                                    output.stop_reason = StopReason::Stop;
                                }
                                let final_text = block.text.clone();
                                sink.send(AssistantMessageEvent::TextEnd {
                                    content_index,
                                    text: final_text,
                                });
                            }
                        }
                        OutputSlot::ToolCall {
                            content_index,
                            mut raw_args,
                        } => {
                            if let Some(item_args) = item.get("arguments").and_then(Value::as_str)
                                && !item_args.is_empty()
                                && raw_args.is_empty()
                            {
                                raw_args = item_args.to_string();
                            }
                            let (args_map, args_err) = parse_tool_call_arguments(&raw_args);
                            if let Some(AssistantBlock::ToolCall(tc)) =
                                output.content.get_mut(content_index)
                            {
                                tc.arguments = args_map;
                                tc.arguments_error = args_err;
                                if let Some(ns) = item.get("namespace").and_then(Value::as_str) {
                                    tc.namespace = Some(ns.to_string());
                                }
                                let finalized = tc.clone();
                                sink.send(AssistantMessageEvent::ToolCallEnd {
                                    content_index,
                                    tool_call: finalized,
                                });
                            }
                        }
                    }
                }
                "response.completed" | "response.incomplete" => {
                    saw_terminal = true;
                    if let Some(resp) = parsed.get("response") {
                        if let Some(id) = resp.get("id").and_then(Value::as_str) {
                            output.response_id = Some(id.to_string());
                        }
                        // Backfill encrypted_content if omitted from output_item.done.
                        if let Some(resp_items) = resp.get("output").and_then(Value::as_array) {
                            for item in resp_items {
                                if item.get("type").and_then(Value::as_str) == Some("reasoning")
                                    && let Some(enc) =
                                        item.get("encrypted_content").and_then(Value::as_str)
                                    && let Some(id) = item.get("id").and_then(Value::as_str)
                                    && let Some(&idx) = reasoning_by_id.get(id)
                                    && let Some(AssistantBlock::Thinking(block)) =
                                        output.content.get_mut(idx)
                                    && let Some(sig) = block.thinking_signature.as_deref()
                                    && let Ok(Value::Object(mut stored)) =
                                        serde_json::from_str::<Value>(sig)
                                    && !stored.contains_key("encrypted_content")
                                {
                                    stored.insert(
                                        "encrypted_content".into(),
                                        Value::String(enc.to_string()),
                                    );
                                    block.thinking_signature =
                                        Some(Value::Object(stored).to_string());
                                }
                            }
                        }

                        if let Some(usage) = resp.get("usage") {
                            let input_tokens =
                                usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
                            let output_tokens = usage
                                .get("output_tokens")
                                .and_then(Value::as_u64)
                                .unwrap_or(0);
                            let cached_tokens = usage
                                .get("input_tokens_details")
                                .and_then(|d| d.get("cached_tokens"))
                                .and_then(Value::as_u64)
                                .unwrap_or(0);
                            let cache_write_tokens = usage
                                .get("input_tokens_details")
                                .and_then(|d| d.get("cache_write_tokens"))
                                .and_then(Value::as_u64)
                                .unwrap_or(0);
                            let reasoning_tokens = usage
                                .get("output_tokens_details")
                                .and_then(|d| d.get("reasoning_tokens"))
                                .and_then(Value::as_u64);
                            let total_tokens = usage
                                .get("total_tokens")
                                .and_then(Value::as_u64)
                                .unwrap_or(input_tokens + output_tokens);

                            output.usage = Usage {
                                input: input_tokens
                                    .saturating_sub(cached_tokens + cache_write_tokens),
                                output: output_tokens,
                                cache_read: cached_tokens,
                                cache_write: cache_write_tokens,
                                reasoning: reasoning_tokens,
                                total_tokens,
                                cost_total: 0.0,
                                cost_from_provider: false,
                            };
                            calculate_cost(&model.cost, &mut output.usage);
                        }

                        let status = resp
                            .get("status")
                            .and_then(Value::as_str)
                            .unwrap_or("completed");
                        let incomplete_reason = resp
                            .get("incomplete_details")
                            .and_then(|d| d.get("reason"))
                            .and_then(Value::as_str);

                        match status {
                            "completed" => {
                                let has_tool_calls = output
                                    .content
                                    .iter()
                                    .any(|b| matches!(b, AssistantBlock::ToolCall(_)));
                                output.stop_reason = if has_tool_calls {
                                    StopReason::ToolUse
                                } else {
                                    StopReason::Stop
                                };
                            }
                            "incomplete" => {
                                if incomplete_reason == Some("max_output_tokens") {
                                    output.stop_reason = StopReason::Length;
                                    output.diagnostics.push(AssistantDiagnostic {
                                        code: "max_output_tokens".into(),
                                        message: "response reached max_output_tokens".into(),
                                        retryable: false,
                                    });
                                } else {
                                    let reason_msg = incomplete_reason.map_or_else(
                                        || "Response incomplete without a provider reason".into(),
                                        |r| format!("Response incomplete: {r}"),
                                    );
                                    emit_protocol_error(
                                        sink,
                                        output,
                                        "response_incomplete",
                                        reason_msg,
                                        cancel,
                                        started_at,
                                    );
                                    return;
                                }
                            }
                            "failed" | "cancelled" => {
                                let err_msg = resp
                                    .get("error")
                                    .and_then(|e| e.get("message"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("OpenAI Responses request failed");
                                emit_protocol_error(
                                    sink,
                                    output,
                                    "response_failed",
                                    err_msg,
                                    cancel,
                                    started_at,
                                );
                                return;
                            }
                            _ => {
                                output.stop_reason = StopReason::Stop;
                            }
                        }
                    }
                    break;
                }
                "response.failed" => {
                    let err_msg = parsed
                        .get("response")
                        .and_then(|r| r.get("error"))
                        .map(|e| {
                            let code = e.get("code").and_then(Value::as_str).unwrap_or("unknown");
                            let msg = e
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("no message");
                            format!("{code}: {msg}")
                        })
                        .unwrap_or_else(|| "OpenAI Responses failed".into());
                    emit_protocol_error(
                        sink,
                        output,
                        "response_failed",
                        err_msg,
                        cancel,
                        started_at,
                    );
                    return;
                }
                "error" => {
                    let code = parsed
                        .get("code")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let msg = parsed
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("Unknown stream error");
                    emit_protocol_error(
                        sink,
                        output,
                        code,
                        format!("{code}: {msg}"),
                        cancel,
                        started_at,
                    );
                    return;
                }
                _ => {
                    // Non-terminal/informational events are ignored gracefully without mutating assistant text.
                }
            }
        }

        if cancel.is_cancelled() {
            emit_transport_error(sink, output, crate::TransportError::Aborted, cancel, started_at);
            return;
        }

        if !saw_terminal || output.stop_reason == StopReason::Pending {
            emit_protocol_error(
                sink,
                output,
                "incomplete_stream",
                "OpenAI Responses stream ended before a terminal response event",
                cancel,
                started_at,
            );
            return;
        }

        output.duration_ms = Some(now_ms().saturating_sub(started_at));
        sink.send(AssistantMessageEvent::Done {
            reason: output.stop_reason,
            message: output,
        });
}

fn create_slot_from_item(
    output_index: usize,
    item: &Value,
    output: &mut crate::AssistantMessage,
    slots: &mut HashMap<usize, OutputSlot>,
    sink: &crate::providers::EventSink,
) {
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
    match item_type {
        "reasoning" => {
            let content_index = output.content.len();
            output.content.push(AssistantBlock::Thinking(ThinkingContent {
                thinking: String::new(),
                thinking_signature: None,
                redacted: false,
            }));
            slots.insert(output_index, OutputSlot::Thinking { content_index });
            sink.send(AssistantMessageEvent::ThinkingStart { content_index });
        }
        "message" => {
            if item.get("phase").and_then(Value::as_str) == Some("final_answer") {
                output.stop_reason = StopReason::Stop;
            }
            let content_index = output.content.len();
            output
                .content
                .push(AssistantBlock::Text(TextContent::plain(String::new())));
            slots.insert(output_index, OutputSlot::Text { content_index });
            sink.send(AssistantMessageEvent::TextStart { content_index });
        }
        "function_call" => {
            let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("");
            let item_id = item.get("id").and_then(Value::as_str).unwrap_or("");
            let id = if !call_id.is_empty() {
                call_id.to_string()
            } else {
                item_id.to_string()
            };
            let stored_item_id = (!item_id.is_empty()).then(|| item_id.to_string());
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let initial_args = item
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let namespace = item
                .get("namespace")
                .and_then(Value::as_str)
                .map(ToString::to_string);

            let content_index = output.content.len();
            output.content.push(AssistantBlock::ToolCall(ToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: Map::new(),
                item_id: stored_item_id,
                thought_signature: None,
                namespace,
                arguments_error: None,
            }));
            slots.insert(
                output_index,
                OutputSlot::ToolCall {
                    content_index,
                    raw_args: initial_args.clone(),
                },
            );
            sink.send(AssistantMessageEvent::ToolCallStart {
                content_index,
                id,
                name,
            });
            if !initial_args.is_empty() {
                sink.send(AssistantMessageEvent::ToolCallDelta {
                    content_index,
                    delta: initial_args,
                });
            }
        }
        _ => {}
    }
}
