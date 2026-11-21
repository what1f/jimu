//! OpenAI Codex (ChatGPT Subscription) Responses API (`POST {base_url}/codex/responses`) streaming adapter.
//! Reuses `openai_responses` message/tool conversion and SSE stream processing.

use crate::oauth::{CODEX_DEFAULT_BASE_URL, extract_account_id};
use crate::providers::openai_responses::{
    consume_responses_sse_stream, convert_messages, convert_tools,
};
use crate::providers::{
    drive_event_stream, emit_protocol_error, emit_transport_error, new_assistant_message, now_ms,
    send_cancellable, thinking_level_effort_str,
};
use crate::usage::{clamp_prompt_cache_key, has_header, merge_headers};
use crate::{
    AssistantEventStream, CacheRetention, HttpRequest, HttpTransport, Model, SimpleStreamOptions,
    ThinkingLevel, TranscriptContext,
};
use bytes::Bytes;
use serde_json::{Map, Value, json};
use std::sync::Arc;

const DEFAULT_CODEX_INSTRUCTIONS: &str = "You are a helpful assistant.";

/// Resolve `{base_url}/codex/responses` without duplicating `/codex` or `/responses`.
pub fn resolve_codex_responses_url(base_url: &str) -> String {
    let raw = if base_url.trim().is_empty() {
        CODEX_DEFAULT_BASE_URL
    } else {
        base_url.trim()
    };
    let trimmed = raw.trim_end_matches('/');
    if trimmed.ends_with("/codex/responses") {
        trimmed.to_string()
    } else if trimmed.ends_with("/codex") {
        format!("{trimmed}/responses")
    } else {
        format!("{trimmed}/codex/responses")
    }
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

    let instructions = if resolved.system_prompt.trim().is_empty() {
        DEFAULT_CODEX_INSTRUCTIONS.to_string()
    } else {
        resolved.system_prompt.clone()
    };
    body.insert("instructions".into(), Value::String(instructions));
    body.insert("input".into(), Value::Array(input));

    if !resolved.tools.is_empty() {
        body.insert("tools".into(), Value::Array(convert_tools(&resolved.tools)));
    }

    if options.cache_retention != CacheRetention::None
        && let Some(session_id) = options.session_id.as_deref()
        && !session_id.is_empty()
    {
        body.insert(
            "prompt_cache_key".into(),
            Value::String(clamp_prompt_cache_key(session_id)),
        );
    }

    if options.fast == Some(true) {
        body.insert("service_tier".into(), Value::String("priority".into()));
    }

    // Reasoning models on Codex reject `temperature`; only include when non-reasoning (or via sampling_params).
    if !model.supports_reasoning
        && let Some(temperature) = options.temperature
    {
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
                    format!("failed to serialize OpenAI Codex Responses request: {err}"),
                    &cancel,
                    started_at,
                );
                return;
            }
        };

        let mut base_headers = vec![
            ("content-type".into(), "application/json".into()),
            ("accept".into(), "text/event-stream".into()),
            ("OpenAI-Beta".into(), "responses=experimental".into()),
            ("originator".into(), "pikra".into()),
        ];

        if let Some(session_id) = options.session_id.as_deref()
            && !session_id.is_empty()
        {
            base_headers.push(("session_id".into(), clamp_prompt_cache_key(session_id)));
        }

        if let Some(api_key) = options.api_key.as_deref()
            && !api_key.is_empty()
        {
            if !has_header(&model.headers, "authorization") {
                base_headers.push(("authorization".into(), format!("Bearer {api_key}")));
            }
            if !has_header(&model.headers, "chatgpt-account-id")
                && let Some(account_id) = extract_account_id(api_key)
            {
                base_headers.push(("chatgpt-account-id".into(), account_id));
            }
        }

        let headers = merge_headers(base_headers, &model.headers, &options.headers);

        let request = HttpRequest {
            method: "POST".into(),
            url: resolve_codex_responses_url(&model.base_url),
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
