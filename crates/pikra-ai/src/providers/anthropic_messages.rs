//! Anthropic Messages API (`POST {base_url}/v1/messages`) streaming adapter.
//! Enforces consecutive `tool_result` merging, `is_error` image splitting, ephemeral prompt cache markers,
//! and thinking/redacted_thinking/signature preservation.

use crate::providers::{
    drive_event_stream, emit_protocol_error, emit_transport_error, new_assistant_message, now_ms,
    send_cancellable,
};
use crate::sse::SseStream;
use crate::transform::{
    normalize_tool_call_id_anthropic, split_error_tool_result_images, transform_messages,
};
use crate::usage::{
    anthropic_messages_url, calculate_cost, has_header, merge_headers, parse_tool_call_arguments,
};
use crate::{
    AssistantBlock, AssistantEventStream, AssistantMessageEvent, CacheRetention, ContentPart,
    HttpRequest, HttpTransport, ImageContent, Message, Model, SimpleStreamOptions, StopReason,
    TextContent, ThinkingContent, ThinkingLevel, ToolCall, ToolDefinition, TranscriptContext,
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::Arc;

const ANTHROPIC_VERSION: &str = "2023-06-01";

fn cache_control_value(retention: CacheRetention) -> Option<Value> {
    match retention {
        CacheRetention::None => None,
        CacheRetention::Short => Some(json!({ "type": "ephemeral" })),
        CacheRetention::Long => Some(json!({ "type": "ephemeral", "ttl": "1h" })),
    }
}

fn convert_image_block(img: &ImageContent) -> Value {
    json!({
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": img.mime_type,
            "data": img.data,
        }
    })
}

fn convert_tool_result_content(content: &[ContentPart]) -> Value {
    let has_images = content.iter().any(|p| matches!(p, ContentPart::Image(_)));
    if !has_images {
        let text = content
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text(t) => Some(t.text.as_str()),
                ContentPart::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Value::String(text);
    }

    let mut blocks = Vec::with_capacity(content.len() + 1);
    let mut has_text = false;
    for part in content {
        match part {
            ContentPart::Text(t) => {
                has_text = true;
                blocks.push(json!({ "type": "text", "text": t.text }));
            }
            ContentPart::Image(img) => blocks.push(convert_image_block(img)),
        }
    }
    if !has_text {
        blocks.insert(0, json!({ "type": "text", "text": "(see attached image)" }));
    }
    Value::Array(blocks)
}

fn convert_text_slice_content(content: &[TextContent]) -> Value {
    let text = content
        .iter()
        .map(|t| t.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    Value::String(text)
}

fn convert_tools(tools: &[ToolDefinition], cache_control: Option<&Value>) -> Vec<Value> {
    let len = tools.len();
    tools
        .iter()
        .enumerate()
        .map(|(idx, tool)| {
            let mut input_schema = Map::new();
            input_schema.insert("type".into(), Value::String("object".into()));
            if let Value::Object(src) = &tool.parameters {
                input_schema.insert(
                    "properties".into(),
                    src.get("properties")
                        .cloned()
                        .unwrap_or_else(|| Value::Object(Map::new())),
                );
                input_schema.insert(
                    "required".into(),
                    src.get("required")
                        .cloned()
                        .unwrap_or_else(|| Value::Array(Vec::new())),
                );
                for key in ["additionalProperties", "$defs", "definitions"] {
                    if let Some(val) = src.get(key) {
                        input_schema.insert(key.into(), val.clone());
                    }
                }
            } else {
                input_schema.insert("properties".into(), Value::Object(Map::new()));
                input_schema.insert("required".into(), Value::Array(Vec::new()));
            }

            let mut obj = Map::new();
            obj.insert("name".into(), Value::String(tool.name.clone()));
            obj.insert("description".into(), Value::String(tool.description.clone()));
            obj.insert("input_schema".into(), Value::Object(input_schema));
            if idx + 1 == len
                && let Some(cc) = cache_control
            {
                obj.insert("cache_control".into(), cc.clone());
            }
            Value::Object(obj)
        })
        .collect()
}

fn convert_messages(
    model: &Model,
    messages: &[Message],
    cache_control: Option<&Value>,
) -> Vec<Value> {
    let transformed = transform_messages(messages, model, |id, _, _| {
        normalize_tool_call_id_anthropic(id)
    });

    let mut out: Vec<Value> = Vec::new();
    let mut i = 0;

    while i < transformed.len() {
        match &transformed[i] {
            Message::System(_) => {
                i += 1;
            }
            Message::User(user) => {
                let mut blocks = Vec::new();
                for part in &user.content {
                    match part {
                        ContentPart::Text(t) if !t.text.trim().is_empty() => {
                            blocks.push(json!({ "type": "text", "text": t.text }));
                        }
                        ContentPart::Image(img) => blocks.push(convert_image_block(img)),
                        _ => {}
                    }
                }
                if !blocks.is_empty() {
                    out.push(json!({
                        "role": "user",
                        "content": blocks,
                    }));
                }
                i += 1;
            }
            Message::Assistant(assistant) => {
                let mut blocks = Vec::new();
                for block in &assistant.content {
                    match block {
                        AssistantBlock::Text(t) => {
                            if !t.text.trim().is_empty() {
                                blocks.push(json!({ "type": "text", "text": t.text }));
                            }
                        }
                        AssistantBlock::Thinking(th) => {
                            if th.redacted {
                                blocks.push(json!({
                                    "type": "redacted_thinking",
                                    "data": th.thinking_signature.as_deref().unwrap_or(""),
                                }));
                            } else if let Some(sig) = th
                                .thinking_signature
                                .as_deref()
                                .filter(|s| !s.trim().is_empty())
                            {
                                blocks.push(json!({
                                    "type": "thinking",
                                    "thinking": th.thinking,
                                    "signature": sig,
                                }));
                            } else if !th.thinking.trim().is_empty() {
                                // Unsigned same-model thinking (e.g., interrupted turn) cannot be replayed
                                // as a signed thinking block on Anthropic; preserve text safely.
                                blocks.push(json!({ "type": "text", "text": th.thinking }));
                            }
                        }
                        AssistantBlock::ToolCall(tc) => {
                            blocks.push(json!({
                                "type": "tool_use",
                                "id": tc.id,
                                "name": tc.name,
                                "input": Value::Object(tc.arguments.clone()),
                            }));
                        }
                    }
                }
                if !blocks.is_empty() {
                    out.push(json!({
                        "role": "assistant",
                        "content": blocks,
                    }));
                }
                i += 1;
            }
            Message::ToolResult(_) => {
                // Merge ALL consecutive ToolResult messages into a single `role: "user"` message.
                // Split images out of `is_error: true` tool results and append them after all tool_result blocks.
                let mut tool_result_blocks = Vec::new();
                let mut trailing_error_image_blocks = Vec::new();

                while i < transformed.len() {
                    let Message::ToolResult(tr) = &transformed[i] else {
                        break;
                    };

                    if let Some(split) = split_error_tool_result_images(tr) {
                        tool_result_blocks.push(json!({
                            "type": "tool_result",
                            "tool_use_id": tr.tool_call_id,
                            "content": convert_text_slice_content(&split.content),
                            "is_error": true,
                        }));
                        trailing_error_image_blocks
                            .push(json!({ "type": "text", "text": split.caption }));
                        for img in &split.images {
                            trailing_error_image_blocks.push(convert_image_block(img));
                        }
                    } else {
                        tool_result_blocks.push(json!({
                            "type": "tool_result",
                            "tool_use_id": tr.tool_call_id,
                            "content": convert_tool_result_content(&tr.content),
                            "is_error": tr.is_error,
                        }));
                    }

                    i += 1;
                }

                tool_result_blocks.extend(trailing_error_image_blocks);
                out.push(json!({
                    "role": "user",
                    "content": tool_result_blocks,
                }));
            }
        }
    }

    // Attach cache_control to the last cacheable block of the tail user message.
    if let Some(cc) = cache_control
        && let Some(Value::Object(last_msg)) = out.last_mut()
        && last_msg.get("role").and_then(Value::as_str) == Some("user")
        && let Some(Value::Array(blocks)) = last_msg.get_mut("content")
        && let Some(Value::Object(last_block)) = blocks.last_mut()
    {
        let block_type = last_block.get("type").and_then(Value::as_str).unwrap_or("");
        if matches!(block_type, "text" | "image" | "tool_result") {
            last_block.insert("cache_control".into(), cc.clone());
        }
    }

    out
}

fn thinking_budget_for_level(level: ThinkingLevel, max_tokens: u32) -> Option<u32> {
    let raw = match level {
        ThinkingLevel::Off => return None,
        ThinkingLevel::Minimal => 1_024,
        ThinkingLevel::Low => 2_048,
        ThinkingLevel::Medium => 8_192,
        ThinkingLevel::High => 16_384,
        ThinkingLevel::Xhigh | ThinkingLevel::Max => 32_768,
    };
    let cap = max_tokens.saturating_sub(1_024);
    if cap >= 1_024 {
        Some(raw.min(cap))
    } else {
        Some(1_024.min(max_tokens.saturating_sub(1).max(1)))
    }
}

pub fn build_request_body(
    model: &Model,
    context: &TranscriptContext,
    options: &SimpleStreamOptions,
) -> Value {
    let resolved = context.resolve();
    let cache_control = cache_control_value(options.cache_retention);
    let max_tokens = options.max_tokens.unwrap_or(model.max_output_tokens);
    let messages = convert_messages(model, &resolved.messages, cache_control.as_ref());

    let mut body = Map::new();
    body.insert("model".into(), Value::String(model.id.clone()));
    body.insert("max_tokens".into(), json!(max_tokens));
    body.insert("stream".into(), Value::Bool(true));

    if !resolved.system_prompt.is_empty() {
        let mut sys_block = Map::new();
        sys_block.insert("type".into(), Value::String("text".into()));
        sys_block.insert("text".into(), Value::String(resolved.system_prompt.clone()));
        if let Some(cc) = &cache_control {
            sys_block.insert("cache_control".into(), cc.clone());
        }
        body.insert("system".into(), Value::Array(vec![Value::Object(sys_block)]));
    }

    if !resolved.tools.is_empty() {
        body.insert(
            "tools".into(),
            Value::Array(convert_tools(&resolved.tools, cache_control.as_ref())),
        );
    }

    body.insert("messages".into(), Value::Array(messages));

    let mut thinking_enabled = false;
    if model.supports_reasoning
        && let Some(level) = options.reasoning
        && let Some(budget) = thinking_budget_for_level(level, max_tokens)
    {
        thinking_enabled = true;
        body.insert(
            "thinking".into(),
            json!({
                "type": "enabled",
                "budget_tokens": budget,
            }),
        );
    }

    if let Some(temperature) = options.temperature
        && !thinking_enabled
    {
        body.insert("temperature".into(), json!(temperature));
    }

    for (k, v) in &options.sampling_params {
        body.insert(k.clone(), v.clone());
    }

    Value::Object(body)
}

enum OpenBlock {
    Text { content_index: usize },
    Thinking { content_index: usize },
    ToolUse { content_index: usize, raw_args: String },
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
                    format!("failed to serialize Anthropic Messages request: {err}"),
                    &cancel,
                    started_at,
                );
                return;
            }
        };

        let mut base_headers = vec![
            ("content-type".into(), "application/json".into()),
            ("accept".into(), "text/event-stream".into()),
            ("anthropic-version".into(), ANTHROPIC_VERSION.into()),
        ];
        if let Some(api_key) = options.api_key.as_deref()
            && !api_key.is_empty()
            && !has_header(&model.headers, "x-api-key")
            && !has_header(&model.headers, "authorization")
        {
            base_headers.push(("x-api-key".into(), api_key.to_string()));
        }
        let headers = merge_headers(base_headers, &model.headers, &options.headers);

        let request = HttpRequest {
            method: "POST".into(),
            url: anthropic_messages_url(&model.base_url),
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
        let mut open_blocks: HashMap<usize, OpenBlock> = HashMap::new();
        let mut saw_message_start = false;
        let mut saw_message_stop = false;

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
            if ev.event.as_deref() == Some("error") {
                emit_protocol_error(
                    &sink,
                    output,
                    "anthropic_stream_error",
                    ev.data,
                    &cancel,
                    started_at,
                );
                return;
            }
            if ev.event.as_deref() == Some("ping") {
                continue;
            }

            let parsed: Value = match serde_json::from_str(&ev.data) {
                Ok(v) => v,
                Err(err) => {
                    emit_protocol_error(
                        &sink,
                        output,
                        "sse_parse_error",
                        format!("invalid Anthropic SSE JSON: {err}"),
                        &cancel,
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
                "message_start" => {
                    saw_message_start = true;
                    if let Some(msg) = parsed.get("message") {
                        if let Some(id) = msg.get("id").and_then(Value::as_str) {
                            output.response_id = Some(id.to_string());
                        }
                        if let Some(u) = msg.get("usage") {
                            output.usage.input =
                                u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
                            output.usage.output =
                                u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
                            output.usage.cache_read = u
                                .get("cache_read_input_tokens")
                                .and_then(Value::as_u64)
                                .unwrap_or(0);
                            output.usage.cache_write = u
                                .get("cache_creation_input_tokens")
                                .and_then(Value::as_u64)
                                .unwrap_or(0);
                            output.usage.total_tokens = output.usage.input
                                + output.usage.output
                                + output.usage.cache_read
                                + output.usage.cache_write;
                            calculate_cost(&model.cost, &mut output.usage);
                        }
                    }
                }
                "content_block_start" => {
                    let index = parsed.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    let Some(cb) = parsed.get("content_block") else {
                        continue;
                    };
                    let cb_type = cb.get("type").and_then(Value::as_str).unwrap_or("");
                    match cb_type {
                        "text" => {
                            let content_index = output.content.len();
                            let initial_text =
                                cb.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                            output
                                .content
                                .push(AssistantBlock::Text(TextContent::plain(initial_text)));
                            open_blocks.insert(index, OpenBlock::Text { content_index });
                            sink.send(AssistantMessageEvent::TextStart { content_index });
                        }
                        "thinking" => {
                            let content_index = output.content.len();
                            let initial_thinking = cb
                                .get("thinking")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let initial_sig = cb
                                .get("signature")
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                                .map(ToString::to_string);
                            output.content.push(AssistantBlock::Thinking(ThinkingContent {
                                thinking: initial_thinking,
                                thinking_signature: initial_sig,
                                redacted: false,
                            }));
                            open_blocks.insert(index, OpenBlock::Thinking { content_index });
                            sink.send(AssistantMessageEvent::ThinkingStart { content_index });
                        }
                        "redacted_thinking" => {
                            let content_index = output.content.len();
                            let data = cb
                                .get("data")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            output.content.push(AssistantBlock::Thinking(ThinkingContent {
                                thinking: "[Reasoning redacted]".into(),
                                thinking_signature: Some(data),
                                redacted: true,
                            }));
                            open_blocks.insert(index, OpenBlock::Thinking { content_index });
                            sink.send(AssistantMessageEvent::ThinkingStart { content_index });
                        }
                        "tool_use" => {
                            let content_index = output.content.len();
                            let id = cb
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let name = cb
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let initial_input = cb
                                .get("input")
                                .and_then(Value::as_object)
                                .cloned()
                                .unwrap_or_default();
                            output.content.push(AssistantBlock::ToolCall(ToolCall {
                                id: id.clone(),
                                name: name.clone(),
                                arguments: initial_input,
                                thought_signature: None,
                                namespace: None,
                                arguments_error: None,
                            }));
                            open_blocks.insert(
                                index,
                                OpenBlock::ToolUse {
                                    content_index,
                                    raw_args: String::new(),
                                },
                            );
                            sink.send(AssistantMessageEvent::ToolCallStart {
                                content_index,
                                id,
                                name,
                            });
                        }
                        _ => {}
                    }
                }
                "content_block_delta" => {
                    let index = parsed.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    let Some(delta) = parsed.get("delta") else {
                        continue;
                    };
                    let delta_type = delta.get("type").and_then(Value::as_str).unwrap_or("");
                    match delta_type {
                        "text_delta" => {
                            let text = delta.get("text").and_then(Value::as_str).unwrap_or("");
                            if let Some(OpenBlock::Text { content_index }) = open_blocks.get(&index)
                                && let Some(AssistantBlock::Text(block)) =
                                    output.content.get_mut(*content_index)
                            {
                                block.text.push_str(text);
                                sink.send(AssistantMessageEvent::TextDelta {
                                    content_index: *content_index,
                                    delta: text.to_string(),
                                });
                            }
                        }
                        "thinking_delta" => {
                            let thinking =
                                delta.get("thinking").and_then(Value::as_str).unwrap_or("");
                            if let Some(OpenBlock::Thinking { content_index }) =
                                open_blocks.get(&index)
                                && let Some(AssistantBlock::Thinking(block)) =
                                    output.content.get_mut(*content_index)
                            {
                                block.thinking.push_str(thinking);
                                sink.send(AssistantMessageEvent::ThinkingDelta {
                                    content_index: *content_index,
                                    delta: thinking.to_string(),
                                });
                            }
                        }
                        "signature_delta" => {
                            let sig = delta.get("signature").and_then(Value::as_str).unwrap_or("");
                            if let Some(OpenBlock::Thinking { content_index }) =
                                open_blocks.get(&index)
                                && let Some(AssistantBlock::Thinking(block)) =
                                    output.content.get_mut(*content_index)
                            {
                                block
                                    .thinking_signature
                                    .get_or_insert_with(String::new)
                                    .push_str(sig);
                            }
                        }
                        "input_json_delta" => {
                            let partial = delta
                                .get("partial_json")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            if let Some(OpenBlock::ToolUse {
                                content_index,
                                raw_args,
                            }) = open_blocks.get_mut(&index)
                            {
                                raw_args.push_str(partial);
                                sink.send(AssistantMessageEvent::ToolCallDelta {
                                    content_index: *content_index,
                                    delta: partial.to_string(),
                                });
                            }
                        }
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    let index = parsed.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    if let Some(open) = open_blocks.remove(&index) {
                        match open {
                            OpenBlock::Text { content_index } => {
                                if let Some(AssistantBlock::Text(block)) =
                                    output.content.get(content_index)
                                {
                                    sink.send(AssistantMessageEvent::TextEnd {
                                        content_index,
                                        text: block.text.clone(),
                                    });
                                }
                            }
                            OpenBlock::Thinking { content_index } => {
                                if let Some(AssistantBlock::Thinking(block)) =
                                    output.content.get(content_index)
                                {
                                    sink.send(AssistantMessageEvent::ThinkingEnd {
                                        content_index,
                                        thinking: block.thinking.clone(),
                                    });
                                }
                            }
                            OpenBlock::ToolUse {
                                content_index,
                                raw_args,
                            } => {
                                if let Some(AssistantBlock::ToolCall(tc)) =
                                    output.content.get_mut(content_index)
                                {
                                    if !raw_args.is_empty() {
                                        let (args_map, args_err) =
                                            parse_tool_call_arguments(&raw_args);
                                        tc.arguments = args_map;
                                        tc.arguments_error = args_err;
                                    }
                                    sink.send(AssistantMessageEvent::ToolCallEnd {
                                        content_index,
                                        tool_call: tc.clone(),
                                    });
                                }
                            }
                        }
                    }
                }
                "message_delta" => {
                    if let Some(delta) = parsed.get("delta")
                        && let Some(reason) = delta.get("stop_reason").and_then(Value::as_str)
                    {
                        output.stop_reason = match reason {
                            "end_turn" | "stop_sequence" | "pause_turn" => StopReason::Stop,
                            "max_tokens" => StopReason::Length,
                            "tool_use" => StopReason::ToolUse,
                            "refusal" | "sensitive" => {
                                output.error_message =
                                    Some(format!("Provider stopped with: {reason}"));
                                StopReason::Error
                            }
                            other => {
                                output.error_message =
                                    Some(format!("Unhandled stop reason: {other}"));
                                StopReason::Error
                            }
                        };
                    }
                    if let Some(u) = parsed.get("usage") {
                        if let Some(v) = u.get("input_tokens").and_then(Value::as_u64) {
                            output.usage.input = v;
                        }
                        if let Some(v) = u.get("output_tokens").and_then(Value::as_u64) {
                            output.usage.output = v;
                        }
                        if let Some(v) = u.get("cache_read_input_tokens").and_then(Value::as_u64) {
                            output.usage.cache_read = v;
                        }
                        if let Some(v) =
                            u.get("cache_creation_input_tokens").and_then(Value::as_u64)
                        {
                            output.usage.cache_write = v;
                        }
                        if let Some(v) = u
                            .get("output_tokens_details")
                            .and_then(|d| d.get("thinking_tokens"))
                            .and_then(Value::as_u64)
                        {
                            output.usage.reasoning = Some(v);
                        }
                        output.usage.total_tokens = output.usage.input
                            + output.usage.output
                            + output.usage.cache_read
                            + output.usage.cache_write;
                        if let Some(billed) = u.get("cost").and_then(Value::as_f64) {
                            output.usage.cost_total = billed;
                            output.usage.cost_from_provider = true;
                        } else {
                            calculate_cost(&model.cost, &mut output.usage);
                        }
                    }
                }
                "message_stop" => {
                    saw_message_stop = true;
                }
                "error" => {
                    let msg = parsed
                        .get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or("Anthropic stream error");
                    emit_protocol_error(
                        &sink,
                        output,
                        "anthropic_stream_error",
                        msg,
                        &cancel,
                        started_at,
                    );
                    return;
                }
                _ => {}
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

        if (saw_message_start && !saw_message_stop) || output.stop_reason == StopReason::Pending {
            emit_protocol_error(
                &sink,
                output,
                "incomplete_stream",
                "Anthropic stream ended before message_stop or stop_reason",
                &cancel,
                started_at,
            );
            return;
        }

        if output.stop_reason == StopReason::Error {
            let msg = output
                .error_message
                .clone()
                .unwrap_or_else(|| "Anthropic request stopped with error".into());
            emit_protocol_error(&sink, output, "stop_error", msg, &cancel, started_at);
            return;
        }

        output.duration_ms = Some(now_ms().saturating_sub(started_at));
        sink.send(AssistantMessageEvent::Done {
            reason: output.stop_reason,
            message: output,
        });
    })
}
