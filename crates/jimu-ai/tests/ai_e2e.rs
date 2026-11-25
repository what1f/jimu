//! End-to-end tests for `jimu-ai` across all 4 wire protocols, Anthropic production edge cases,
//! cross-provider handoffs, multi-turn prompt cache prefix stability, and cooperative abort.

use bytes::Bytes;
use futures_executor::block_on;
use futures_util::StreamExt;
use jimu_ai::oauth::{
    CODEX_DEFAULT_BASE_URL, CodexCredentials, exchange_codex_code, fetch_codex_models,
    generate_pkce_from_bytes, parse_authorization_input, refresh_codex_credentials,
    start_codex_authorization,
};
use jimu_ai::providers::{
    anthropic_messages, google_generative_ai, openai_codex_responses, openai_completions,
    openai_responses,
};
use jimu_ai::{
    AnthropicThinkingMode, ApiKind, AssistantBlock, AssistantMessage, AssistantMessageEvent,
    CacheRetention, CancelToken, ContentPart, ImageContent, MaxTokensField, Message, Model,
    ModelCompat, ModelCost, Models, ProviderConfig, SimpleStreamOptions, StopReason,
    StreamProvider, SystemMessage, TextContent, ThinkingContent, ThinkingLevel, ToolCall,
    ToolDefinition, ToolResultMessage, TranscriptContext, UserMessage, transform_messages,
};
use jimu_schema::{int_schema, obj_schema, str_schema};
use jimu_testkit::{FauxTransport, ScriptedHttpResponse, assert_prompt_prefix_identical};
use serde_json::{Map, Value, json};
use std::str::FromStr;
use std::sync::Arc;

fn sample_tools() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "read_file".into(),
            description: "Read a file from disk".into(),
            parameters: obj_schema([
                ("path", str_schema(), true),
                ("limit", int_schema(), false),
                ("offset", int_schema(), false),
            ]),
        },
        ToolDefinition {
            name: "run_cmd".into(),
            description: "Run a shell command".into(),
            parameters: obj_schema([("cmd", str_schema(), true), ("cwd", str_schema(), false)]),
        },
    ]
}

fn openai_responses_model() -> Model {
    Model {
        id: "gpt-5.4".into(),
        name: "GPT-5.4".into(),
        provider: "openai".into(),
        api: ApiKind::OpenaiResponses,
        base_url: "https://api.openai.com/v1".into(),
        context_window: 200_000,
        max_output_tokens: 16_384,
        supports_images: true,
        supports_reasoning: true,
        cost: ModelCost {
            input: 2.5,
            output: 10.0,
            cache_read: 0.25,
            cache_write: 0.0,
        },
        headers: Vec::new(),
        compat: None,
    }
}

fn openai_completions_model() -> Model {
    Model {
        id: "deepseek-reasoner".into(),
        name: "DeepSeek Reasoner".into(),
        provider: "deepseek".into(),
        api: ApiKind::OpenaiCompletions,
        base_url: "https://api.deepseek.com/v1".into(),
        context_window: 128_000,
        max_output_tokens: 8_192,
        supports_images: false,
        supports_reasoning: true,
        cost: ModelCost {
            input: 1.0,
            output: 4.0,
            cache_read: 0.1,
            cache_write: 0.0,
        },
        headers: Vec::new(),
        compat: None,
    }
}

fn anthropic_model() -> Model {
    Model {
        id: "claude-sonnet-4-6".into(),
        name: "Claude Sonnet 4.6".into(),
        provider: "anthropic".into(),
        api: ApiKind::AnthropicMessages,
        base_url: "https://api.anthropic.com".into(),
        context_window: 200_000,
        max_output_tokens: 8_192,
        supports_images: true,
        supports_reasoning: true,
        cost: ModelCost {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 3.75,
        },
        headers: Vec::new(),
        compat: None,
    }
}

fn google_model() -> Model {
    Model {
        id: "gemini-2.5-pro".into(),
        name: "Gemini 2.5 Pro".into(),
        provider: "google".into(),
        api: ApiKind::GoogleGenerativeAi,
        base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
        context_window: 1_000_000,
        max_output_tokens: 8_192,
        supports_images: true,
        supports_reasoning: true,
        cost: ModelCost {
            input: 1.25,
            output: 10.0,
            cache_read: 0.3125,
            cache_write: 0.0,
        },
        headers: Vec::new(),
        compat: None,
    }
}

#[test]
fn all_four_protocols_stream_thinking_parallel_tools_malformed_json_and_cost() {
    block_on(async {
        let transport = Arc::new(FauxTransport::new());
        let mut models = Models::new(transport.clone());
        models
            .register_provider("openai", ProviderConfig::with_api_key("sk-openai"))
            .register_provider("deepseek", ProviderConfig::with_api_key("sk-deepseek"))
            .register_provider("anthropic", ProviderConfig::with_api_key("sk-anthropic"))
            .register_provider("google", ProviderConfig::with_api_key("sk-google"));

        let ctx = TranscriptContext::new(
            "You are Jimu.",
            sample_tools(),
            vec![Message::User(UserMessage::text("Inspect files", 1))],
        );

        // 1. OpenAI Responses: reasoning + commentary/final_answer phase + parallel tool calls + max_output_tokens Length.
        let long_session = "s".repeat(80);
        transport.enqueue_sse([
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\"}}\n\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"output_index\":0,\"delta\":\"Plan \"}\r\n\r\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"Plan done\"}],\"encrypted_content\":\"enc_blob_123\"}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"phase\":\"commentary\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":1,\"delta\":\"Checking...\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"phase\":\"commentary\",\"content\":[{\"type\":\"output_text\",\"text\":\"Checking...\"}]}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":2,\"item\":{\"type\":\"function_call\",\"id\":\"fc_a\",\"call_id\":\"call_1\",\"name\":\"read_file\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":2,\"delta\":\"{\\\"path\\\":\\\"a.rs\\\"\"}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":2,\"delta\":\"}\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":2,\"item\":{\"type\":\"function_call\",\"id\":\"fc_a\",\"call_id\":\"call_1\",\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\"}\"}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":3,\"item\":{\"type\":\"function_call\",\"id\":\"fc_b\",\"call_id\":\"call_2\",\"name\":\"run_cmd\"}}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":3,\"item\":{\"type\":\"function_call\",\"id\":\"fc_b\",\"call_id\":\"call_2\",\"name\":\"run_cmd\",\"arguments\":\"{\\\"cmd\\\":\\\"ls\\\"}\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":1000,\"output_tokens\":200,\"total_tokens\":1200,\"input_tokens_details\":{\"cached_tokens\":400},\"output_tokens_details\":{\"reasoning_tokens\":50}}}}\n\n",
        ]);

        let opts = SimpleStreamOptions {
            reasoning: Some(ThinkingLevel::High),
            fast: Some(true),
            session_id: Some(long_session),
            ..Default::default()
        };
        let resp_msg = models.complete(&openai_responses_model(), &ctx, opts).await;
        assert_eq!(resp_msg.stop_reason, StopReason::ToolUse);
        assert_eq!(resp_msg.response_id.as_deref(), Some("resp_1"));
        assert_eq!(resp_msg.tool_calls().len(), 2);
        assert_eq!(resp_msg.tool_calls()[0].id, "call_1");
        assert_eq!(resp_msg.tool_calls()[0].item_id.as_deref(), Some("fc_a"));
        assert!(resp_msg.tool_calls()[0].thought_signature.is_none());
        assert_eq!(resp_msg.usage.input, 600);
        assert_eq!(resp_msg.usage.cache_read, 400);
        assert_eq!(resp_msg.usage.reasoning, Some(50));
        // Cost = (600 * 2.5 + 200 * 10.0 + 400 * 0.25) / 1e6 = 0.0036
        assert!((resp_msg.usage.cost_total - 0.0036).abs() < 1e-9);
        match &resp_msg.content[0] {
            AssistantBlock::Thinking(t) => {
                assert!(
                    t.thinking_signature
                        .as_deref()
                        .unwrap()
                        .contains("enc_blob_123")
                );
            }
            other => panic!("expected thinking block, got {other:?}"),
        }
        match &resp_msg.content[1] {
            AssistantBlock::Text(t) => {
                assert_eq!(t.text, "Checking...");
                let sig: Value =
                    serde_json::from_str(t.text_signature.as_deref().unwrap()).unwrap();
                assert_eq!(sig["id"], "msg_1");
                assert_eq!(sig["phase"], "commentary");
            }
            other => panic!("expected text block, got {other:?}"),
        }

        // Verify OpenAI Responses wire payload & URL (no double /v1, clamped 64-char cache key, priority tier).
        let req0 = &transport.requests()[0];
        assert_eq!(req0.url, "https://api.openai.com/v1/responses");
        assert_eq!(req0.body_json["service_tier"], "priority");
        assert_eq!(req0.body_json["reasoning"]["effort"], "high");
        assert_eq!(
            req0.body_json["prompt_cache_key"].as_str().unwrap().len(),
            64
        );

        // Verify incomplete due to max_output_tokens maps strictly to StopReason::Length without injecting text.
        transport.enqueue_sse([
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_trunc\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"Partial\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_trunc\",\"content\":[{\"type\":\"output_text\",\"text\":\"Partial\"}]}}\n\n",
            "data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp_trunc\",\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n",
        ]);
        let trunc_msg = models
            .complete(
                &openai_responses_model(),
                &ctx,
                SimpleStreamOptions::default(),
            )
            .await;
        assert_eq!(trunc_msg.stop_reason, StopReason::Length);
        assert_eq!(trunc_msg.text_output(), "Partial");

        // 2. OpenAI Completions: reasoning_content + parallel tool calls + malformed JSON (`arguments_error`) + sampling_params.
        transport.enqueue_sse([
            "data: {\"id\":\"chatcmpl_1\",\"choices\":[{\"delta\":{\"reasoning_content\":\"Think step\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl_1\",\"choices\":[{\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl_1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"tc_good\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\"}},{\"index\":1,\"id\":\"tc_bad\",\"type\":\"function\",\"function\":{\"name\":\"run_cmd\",\"arguments\":\"{\\\"cmd\\\": broken\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl_1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"ok.rs\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: {\"id\":\"chatcmpl_1\",\"choices\":[],\"usage\":{\"prompt_tokens\":500,\"completion_tokens\":100,\"total_tokens\":600}}\n\n",
            "data: [DONE]\n\n",
        ]);
        let mut comp_opts = SimpleStreamOptions {
            reasoning: Some(ThinkingLevel::Medium),
            ..Default::default()
        };
        comp_opts
            .sampling_params
            .insert("top_p".into(), json!(0.85));
        let comp_msg = models
            .complete(&openai_completions_model(), &ctx, comp_opts)
            .await;
        assert_eq!(comp_msg.stop_reason, StopReason::ToolUse);
        assert_eq!(comp_msg.text_output(), "Hi");
        let comp_calls = comp_msg.tool_calls();
        assert_eq!(comp_calls.len(), 2);
        assert_eq!(comp_calls[0].arguments.get("path"), Some(&json!("ok.rs")));
        assert!(comp_calls[0].arguments_error.is_none());
        assert!(comp_calls[1].arguments.is_empty());
        assert!(comp_calls[1].arguments_error.is_some());
        let comp_req = transport.requests().pop().unwrap();
        assert_eq!(comp_req.url, "https://api.deepseek.com/v1/chat/completions");
        assert_eq!(comp_req.body_json["stream_options"]["include_usage"], true);
        assert_eq!(comp_req.body_json["reasoning_effort"], "medium");
        assert_eq!(comp_req.body_json["top_p"], 0.85);

        // 3. Anthropic Messages: thinking + signature_delta + redacted_thinking + text + tool_use + usage/cost.
        transport.enqueue_sse([
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_ant_1\",\"usage\":{\"input_tokens\":200,\"output_tokens\":1,\"cache_read_input_tokens\":1000,\"cache_creation_input_tokens\":500}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Deep thought\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig_part1\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"_part2\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"opaque_redacted_blob\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read_file\",\"input\":{}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"lib.rs\\\"}\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":80}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ]);
        let ant_msg = models
            .complete(&anthropic_model(), &ctx, SimpleStreamOptions::default())
            .await;
        assert_eq!(ant_msg.stop_reason, StopReason::ToolUse);
        assert_eq!(ant_msg.response_id.as_deref(), Some("msg_ant_1"));
        assert_eq!(ant_msg.content.len(), 3);
        match &ant_msg.content[0] {
            AssistantBlock::Thinking(t) => {
                assert_eq!(t.thinking, "Deep thought");
                assert_eq!(t.thinking_signature.as_deref(), Some("sig_part1_part2"));
                assert!(!t.redacted);
            }
            other => panic!("unexpected block: {other:?}"),
        }
        match &ant_msg.content[1] {
            AssistantBlock::Thinking(t) => {
                assert!(t.redacted);
                assert_eq!(
                    t.thinking_signature.as_deref(),
                    Some("opaque_redacted_blob")
                );
            }
            other => panic!("unexpected block: {other:?}"),
        }
        assert_eq!(ant_msg.tool_calls()[0].arguments["path"], "lib.rs");
        assert_eq!(ant_msg.usage.total_tokens, 1780);
        // Cost = (200*3 + 80*15 + 1000*0.3 + 500*3.75) / 1e6 = 0.003975
        assert!((ant_msg.usage.cost_total - 0.003975).abs() < 1e-9);

        // 4. Google Generative AI: thought part + functionCall with thoughtSignature + text + usage.
        transport.enqueue_sse([
            "data: {\"responseId\":\"gem_1\",\"candidates\":[{\"content\":{\"parts\":[{\"thought\":true,\"text\":\"Thinking in Gemini\"},{\"functionCall\":{\"id\":\"gem_fc_1\",\"name\":\"read_file\",\"args\":{\"path\":\"main.rs\"}},\"thoughtSignature\":\"Z2VtaW5pX3NpZw==\"}]}}]}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Done!\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":300,\"cachedContentTokenCount\":100,\"candidatesTokenCount\":40,\"thoughtsTokenCount\":20,\"totalTokenCount\":360}}\n\n",
        ]);
        let gem_opts = SimpleStreamOptions {
            reasoning: Some(ThinkingLevel::Low),
            ..Default::default()
        };
        let gem_msg = models.complete(&google_model(), &ctx, gem_opts).await;
        assert_eq!(gem_msg.stop_reason, StopReason::ToolUse);
        assert_eq!(gem_msg.response_id.as_deref(), Some("gem_1"));
        assert_eq!(
            gem_msg.tool_calls()[0].thought_signature.as_deref(),
            Some("Z2VtaW5pX3NpZw==")
        );
        assert_eq!(gem_msg.text_output(), "Done!");
        assert_eq!(gem_msg.usage.input, 200);
        assert_eq!(gem_msg.usage.output, 60);
        assert_eq!(gem_msg.usage.cache_read, 100);
        let gem_req = transport.requests().pop().unwrap();
        assert_eq!(
            gem_req.url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse"
        );
        assert_eq!(
            gem_req.body_json["generationConfig"]["thinkingConfig"]["includeThoughts"],
            true
        );
    });
}

#[test]
fn anthropic_tool_result_merging_error_image_splitting_cache_control_and_cross_provider_handoff() {
    block_on(async {
        let transport = Arc::new(FauxTransport::new());
        let mut models = Models::new(transport.clone());
        models
            .register_provider("anthropic", ProviderConfig::with_api_key("sk-ant"))
            .register_provider("google", ProviderConfig::with_api_key("sk-goog"))
            .register_provider("deepseek", ProviderConfig::with_api_key("sk-ds"));

        // Build an OpenAI Responses assistant turn with:
        // - foreign ThinkingContent + encrypted signature (MUST be dropped on Anthropic/Gemini)
        // - foreign text_signature (MUST be stripped)
        // - two 450-char pipe-delimited tool call IDs sharing a 100-char prefix (MUST normalize uniquely to ^[a-zA-Z0-9_-]{1,64}$)
        let long_prefix = "x".repeat(100);
        let oa_id_1 = format!("call_{long_prefix}|fc_{}AAA", "y".repeat(360));
        let oa_id_2 = format!("call_{long_prefix}|fc_{}BBB", "y".repeat(360));

        let mut args1 = Map::new();
        args1.insert("path".into(), json!("a.png"));
        let mut args2 = Map::new();
        args2.insert("path".into(), json!("b.png"));

        let foreign_assistant = AssistantMessage {
            content: vec![
                AssistantBlock::Thinking(ThinkingContent {
                    thinking: "SECRET FOREIGN CHAIN OF THOUGHT".into(),
                    thinking_signature: Some(
                        "{\"type\":\"reasoning\",\"encrypted_content\":\"enc\"}".into(),
                    ),
                    redacted: false,
                }),
                AssistantBlock::Text(TextContent {
                    text: "Calling both tools".into(),
                    text_signature: Some(
                        "{\"v\":1,\"id\":\"msg_oa\",\"phase\":\"commentary\"}".into(),
                    ),
                }),
                AssistantBlock::ToolCall(ToolCall {
                    id: oa_id_1.clone(),
                    name: "read_file".into(),
                    arguments: args1,
                    item_id: Some("fc_orig_1".into()),
                    thought_signature: Some("foreign_thought_sig".into()),
                    namespace: Some("ns".into()),
                    arguments_error: None,
                }),
                AssistantBlock::ToolCall(ToolCall {
                    id: oa_id_2.clone(),
                    name: "read_file".into(),
                    arguments: args2,
                    item_id: Some("fc_orig_2".into()),
                    thought_signature: None,
                    namespace: None,
                    arguments_error: None,
                }),
            ],
            api: ApiKind::OpenaiResponses,
            provider: "openai".into(),
            model: "gpt-5.4".into(),
            response_id: Some("resp_oa".into()),
            thinking_level: Some(ThinkingLevel::High),
            usage: Default::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            diagnostics: Vec::new(),
            timestamp: 1,
            duration_ms: None,
        };

        let tr1 = ToolResultMessage {
            tool_call_id: oa_id_1.clone(),
            tool_name: "read_file".into(),
            content: vec![ContentPart::Text(TextContent::plain("ok_text"))],
            is_error: false,
            timestamp: 2,
        };
        let tr2_error_with_image = ToolResultMessage {
            tool_call_id: oa_id_2.clone(),
            tool_name: "read_file".into(),
            content: vec![
                ContentPart::Text(TextContent::plain("boom")),
                ContentPart::Image(ImageContent {
                    data: "iVBORw0KGgo=".into(),
                    mime_type: "image/png".into(),
                }),
            ],
            is_error: true,
            timestamp: 3,
        };

        let ctx = TranscriptContext::new(
            "System instructions",
            sample_tools(),
            vec![
                Message::User(UserMessage {
                    is_meta: false,
                    id: None,
                    content: vec![
                        ContentPart::Text(TextContent::plain("Check this")),
                        ContentPart::Image(ImageContent {
                            data: "AAAA".into(),
                            mime_type: "image/png".into(),
                        }),
                    ],
                    timestamp: 0,
                }),
                Message::Assistant(foreign_assistant),
                Message::ToolResult(tr1),
                Message::ToolResult(tr2_error_with_image),
            ],
        );

        // Send to Anthropic with CacheRetention::Long -> inspect exact request wire JSON!
        transport.enqueue_sse([
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m_1\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Recovered\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ]);
        let ant_opts = SimpleStreamOptions {
            cache_retention: CacheRetention::Long,
            ..Default::default()
        };
        let ant_reply = models.complete(&anthropic_model(), &ctx, ant_opts).await;
        assert_eq!(ant_reply.stop_reason, StopReason::Stop);

        let ant_req = &transport.requests()[0];
        assert_eq!(ant_req.url, "https://api.anthropic.com/v1/messages");
        // Cache control Long on system, last tool, and tail user message block.
        assert_eq!(
            ant_req.body_json["system"][0]["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"})
        );
        let tools = ant_req.body_json["tools"].as_array().unwrap();
        assert!(tools[0].get("cache_control").is_none());
        assert_eq!(
            tools[1]["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"})
        );

        // Foreign thinking must NOT leak into Anthropic assistant message!
        let serialized_req = String::from_utf8(ant_req.body_bytes.to_vec()).unwrap();
        assert!(!serialized_req.contains("SECRET FOREIGN CHAIN OF THOUGHT"));

        let msgs = ant_req.body_json["messages"].as_array().unwrap();
        // [0] user, [1] assistant, [2] SINGLE merged user message for both tool_results + split error image!
        assert_eq!(msgs.len(), 3);
        let assistant_blocks = msgs[1]["content"].as_array().unwrap();
        let norm_id_1 = assistant_blocks[1]["id"].as_str().unwrap();
        let norm_id_2 = assistant_blocks[2]["id"].as_str().unwrap();
        assert_ne!(norm_id_1, norm_id_2);
        for id in [norm_id_1, norm_id_2] {
            assert!(!id.is_empty() && id.len() <= 64);
            assert!(
                id.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            );
        }

        let merged_user = msgs[2]["content"].as_array().unwrap();
        assert_eq!(msgs[2]["role"], "user");
        // Expect: tool_result #1, tool_result #2 (text-only with placeholder), caption text, image (with cache_control)!
        assert_eq!(merged_user.len(), 4);
        assert_eq!(merged_user[0]["type"], "tool_result");
        assert_eq!(merged_user[0]["tool_use_id"], norm_id_1);
        assert_eq!(merged_user[1]["type"], "tool_result");
        assert_eq!(merged_user[1]["tool_use_id"], norm_id_2);
        assert_eq!(merged_user[1]["is_error"], true);
        assert!(
            merged_user[1]["content"]
                .as_str()
                .unwrap()
                .contains("(image attached after the tool results)")
        );
        assert_eq!(merged_user[2]["type"], "text");
        assert!(merged_user[2]["text"].as_str().unwrap().contains(norm_id_2));
        assert_eq!(merged_user[3]["type"], "image");
        assert_eq!(
            merged_user[3]["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"})
        );

        // Send same context to non-vision model (`supports_images: false`) -> verify image placeholders!
        transport.enqueue_sse([
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"No vision ok\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ]);
        let ds_reply = models
            .complete(
                &openai_completions_model(),
                &ctx,
                SimpleStreamOptions::default(),
            )
            .await;
        assert_eq!(ds_reply.stop_reason, StopReason::Stop);
        let ds_req_str = String::from_utf8(transport.requests()[1].body_bytes.to_vec()).unwrap();
        assert!(ds_req_str.contains("(image omitted: model does not support images)"));
        assert!(ds_req_str.contains("(tool image omitted: model does not support images)"));
        assert!(!ds_req_str.contains("SECRET FOREIGN CHAIN OF THOUGHT"));

        // Send same context to Gemini -> verify foreign thoughtSignature stripped & IDs normalized.
        transport.enqueue_sse([
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Gemini ok\"}]},\"finishReason\":\"STOP\"}]}\n\n",
        ]);
        let gem_reply = models
            .complete(&google_model(), &ctx, SimpleStreamOptions::default())
            .await;
        assert_eq!(gem_reply.stop_reason, StopReason::Stop);
        let gem_req_str = String::from_utf8(transport.requests()[2].body_bytes.to_vec()).unwrap();
        assert!(!gem_req_str.contains("foreign_thought_sig"));
        assert!(!gem_req_str.contains("SECRET FOREIGN CHAIN OF THOUGHT"));
    });
}

#[test]
fn multi_turn_prompt_cache_prefix_byte_stability_and_mid_stream_cancel() {
    block_on(async {
        let transport = Arc::new(FauxTransport::new());
        let mut models = Models::new(transport.clone());
        models.register_provider("anthropic", ProviderConfig::with_api_key("sk-ant"));
        let model = anthropic_model();

        // Turn 1: system message with base prompt + section + deterministic tools.
        let mut ctx = TranscriptContext {
            messages: vec![
                Message::System(SystemMessage {
                    content: "Base system prompt".into(),
                    sections: vec![("env".into(), Some("OS=macOS".into()))],
                    tools_added: sample_tools(),
                    tools_removed: Vec::new(),
                    timestamp: 0,
                }),
                Message::User(UserMessage::text("Turn 1 question", 1)),
            ],
        };

        transport.enqueue_sse([
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"usage\":{\"input_tokens\":50,\"output_tokens\":1}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Turn 1 answer\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":10}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ]);
        let turn1 = models
            .complete(&model, &ctx, SimpleStreamOptions::default())
            .await;
        assert_eq!(turn1.stop_reason, StopReason::Stop);

        // Turn 2: append assistant reply + next user turn without mutating system/tools.
        ctx.messages.push(Message::Assistant(turn1));
        ctx.messages
            .push(Message::User(UserMessage::text("Turn 2 follow-up", 2)));

        transport.enqueue_sse([
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m2\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1,\"cache_read_input_tokens\":50}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Turn 2 answer\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":10}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ]);
        let turn2 = models
            .complete(&model, &ctx, SimpleStreamOptions::default())
            .await;
        assert_eq!(turn2.stop_reason, StopReason::Stop);

        let reqs = transport.requests();
        assert_prompt_prefix_identical(&reqs[0], &reqs[1]);

        // Mid-stream cooperative CancelToken abort over channel-gated SSE (zero sleep!).
        let sender = transport.enqueue_gated_sse();
        let cancel = CancelToken::new();
        let opts = SimpleStreamOptions {
            cancel: cancel.clone(),
            ..Default::default()
        };

        let mut stream = models.stream(&model, &ctx, opts);

        // Push initial frames; channel stays open waiting for more chunks.
        sender.push_str(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m3\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
        );
        sender.push_str(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        );
        // Split a multi-byte UTF-8 emoji across two byte chunks to exercise UTF-8 boundary reassembly too!
        let split_frame = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello 🚀\"}}\n\n";
        let bytes = split_frame.as_bytes();
        let emoji_pos = split_frame.find('🚀').unwrap();
        sender.push_bytes(bytes[..emoji_pos + 2].to_vec());
        sender.push_bytes(bytes[emoji_pos + 2..].to_vec());

        let mut saw_delta = String::new();
        let mut final_error: Option<AssistantMessage> = None;
        while let Some(ev) = stream.next().await {
            match ev {
                AssistantMessageEvent::TextDelta { delta, .. } => {
                    saw_delta.push_str(&delta);
                    // Cancel immediately after receiving the first delta while SSE stream is still open!
                    cancel.cancel();
                }
                AssistantMessageEvent::Error { reason, error } => {
                    assert_eq!(reason, StopReason::Aborted);
                    final_error = Some(error);
                    break;
                }
                _ => {}
            }
        }

        assert_eq!(saw_delta, "Hello 🚀");
        let aborted = final_error.expect("expected Error event with StopReason::Aborted");
        assert_eq!(aborted.stop_reason, StopReason::Aborted);
    });
}

fn json_http_ok(body: Value) -> ScriptedHttpResponse {
    let bytes = Bytes::from(serde_json::to_vec(&body).unwrap());
    ScriptedHttpResponse::stream(
        200,
        vec![("content-type".into(), "application/json".into())],
        Box::pin(futures_util::stream::iter(vec![Ok(bytes)])),
    )
}

fn make_jwt(claims: Value) -> String {
    use base64::Engine;
    let enc = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let header = enc.encode(r#"{"alg":"RS256","typ":"JWT"}"#);
    let payload = enc.encode(serde_json::to_vec(&claims).unwrap());
    format!("{header}.{payload}.sig")
}

#[test]
fn thinking_level_none_vs_off_compat_and_item_id_handoff() {
    assert_eq!(
        ThinkingLevel::from_str("none").unwrap(),
        ThinkingLevel::None
    );
    assert_eq!(
        ThinkingLevel::from_str("disable").unwrap(),
        ThinkingLevel::None
    );
    assert_eq!(ThinkingLevel::from_str("off").unwrap(), ThinkingLevel::Off);
    assert_eq!(
        serde_json::to_string(&ThinkingLevel::None).unwrap(),
        "\"none\""
    );
    assert_eq!(
        serde_json::to_string(&ApiKind::OpenaiCodexResponses).unwrap(),
        "\"openai-codex-responses\""
    );

    let ctx = TranscriptContext::new(
        "Be concise.",
        vec![],
        vec![Message::User(UserMessage::text("Ping", 1))],
    );

    // 1. OpenAI Completions: ModelCompat & ThinkingLevel::None vs None (unset).
    let ds_model = openai_completions_model();
    let opts_unset = SimpleStreamOptions {
        max_tokens: Some(1024),
        reasoning: None,
        ..Default::default()
    };
    let ds_body_unset = openai_completions::build_request_body(&ds_model, &ctx, &opts_unset);
    assert_eq!(ds_body_unset["max_tokens"], 1024);
    assert!(ds_body_unset.get("max_completion_tokens").is_none());
    assert!(ds_body_unset.get("reasoning_effort").is_none());

    let opts_none = SimpleStreamOptions {
        max_tokens: Some(1024),
        reasoning: Some(ThinkingLevel::None),
        ..Default::default()
    };
    let ds_body_none = openai_completions::build_request_body(&ds_model, &ctx, &opts_none);
    assert_eq!(ds_body_none["thinking"], json!({ "type": "disabled" }));
    assert!(ds_body_none.get("reasoning_effort").is_none());

    let mut oa_comp = ds_model.clone();
    oa_comp.provider = "openai".into();
    oa_comp.base_url = "https://api.openai.com/v1".into();
    let oa_comp_none = openai_completions::build_request_body(&oa_comp, &ctx, &opts_none);
    assert_eq!(oa_comp_none["reasoning_effort"], "none");
    let oa_comp_body = openai_completions::build_request_body(&oa_comp, &ctx, &opts_unset);
    assert_eq!(oa_comp_body["max_completion_tokens"], 1024);
    assert!(oa_comp_body.get("max_tokens").is_none());

    // Explicit ModelCompat override beats provider heuristic.
    oa_comp.compat = Some(ModelCompat {
        max_tokens_field: Some(MaxTokensField::MaxTokens),
        ..ModelCompat::default()
    });
    let oa_override_body = openai_completions::build_request_body(&oa_comp, &ctx, &opts_unset);
    assert_eq!(oa_override_body["max_tokens"], 1024);
    assert!(oa_override_body.get("max_completion_tokens").is_none());

    // 2. OpenAI Responses & Codex Responses: ThinkingLevel::None sends effort "none" WITHOUT include.
    let resp_body_none =
        openai_responses::build_request_body(&openai_responses_model(), &ctx, &opts_none);
    assert_eq!(resp_body_none["reasoning"], json!({ "effort": "none" }));
    assert!(resp_body_none.get("include").is_none());

    let mut codex_model = Model::new(
        "gpt-5.4-codex",
        "openai-codex",
        ApiKind::OpenaiCodexResponses,
        CODEX_DEFAULT_BASE_URL,
    );
    codex_model.supports_reasoning = true;
    let codex_body_none =
        openai_codex_responses::build_request_body(&codex_model, &ctx, &opts_none);
    assert_eq!(codex_body_none["reasoning"], json!({ "effort": "none" }));
    assert!(codex_body_none.get("include").is_none());
    assert!(codex_body_none.get("max_output_tokens").is_none());

    // 3. Anthropic Messages: ThinkingLevel::None sends thinking.type == "disabled".
    let ant_body_none =
        anthropic_messages::build_request_body(&anthropic_model(), &ctx, &opts_none);
    assert_eq!(ant_body_none["thinking"], json!({ "type": "disabled" }));

    // 4. Google Generative AI: non-Pro 2.5 -> thinkingBudget: 0; Gemini 3 -> thinkingLevel: MINIMAL; Off -> omitted.
    let mut gem_flash = google_model();
    gem_flash.id = "gemini-2.5-flash".into();
    let gem_flash_none = google_generative_ai::build_request_body(&gem_flash, &ctx, &opts_none);
    assert_eq!(
        gem_flash_none["generationConfig"]["thinkingConfig"],
        json!({ "thinkingBudget": 0 })
    );
    let gem_flash_off = google_generative_ai::build_request_body(
        &gem_flash,
        &ctx,
        &SimpleStreamOptions {
            reasoning: Some(ThinkingLevel::Off),
            ..Default::default()
        },
    );
    assert!(gem_flash_off.get("generationConfig").is_none());

    let mut gem3 = google_model();
    gem3.id = "gemini-3-flash".into();
    let gem3_none = google_generative_ai::build_request_body(&gem3, &ctx, &opts_none);
    assert_eq!(
        gem3_none["generationConfig"]["thinkingConfig"],
        json!({ "thinkingLevel": "MINIMAL" })
    );
    let mut gem3_pro = google_model();
    gem3_pro.id = "gemini-3.1-pro".into();
    let gem3_pro_none = google_generative_ai::build_request_body(&gem3_pro, &ctx, &opts_none);
    assert_eq!(
        gem3_pro_none["generationConfig"]["thinkingConfig"],
        json!({ "thinkingLevel": "LOW" })
    );

    // 5. ToolCall.item_id handoff: same-model replays `fc_`, same-provider different-model clears, foreign normalizes to bounded `fc_{hash}`.
    let assistant_msg = Message::Assistant(AssistantMessage {
        content: vec![AssistantBlock::ToolCall(ToolCall {
            id: "call_1".into(),
            name: "read_file".into(),
            arguments: Map::new(),
            item_id: Some("fc_original_item".into()),
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        })],
        api: ApiKind::OpenaiResponses,
        provider: "openai".into(),
        model: "gpt-5.4".into(),
        response_id: None,
        thinking_level: None,
        usage: Default::default(),
        stop_reason: StopReason::ToolUse,
        error_message: None,
        diagnostics: Vec::new(),
        timestamp: 1,
        duration_ms: None,
    });
    let tr_msg = Message::ToolResult(ToolResultMessage {
        tool_call_id: "call_1".into(),
        tool_name: "read_file".into(),
        content: vec![ContentPart::Text(TextContent::plain("ok"))],
        is_error: false,
        timestamp: 2,
    });

    // Same-provider different-model clears item_id.
    let mut same_prov_diff_model = openai_responses_model();
    same_prov_diff_model.id = "gpt-5.4-mini".into();
    let transformed_diff = transform_messages(
        &[assistant_msg.clone(), tr_msg.clone()],
        &same_prov_diff_model,
        |id, _, _| id.to_string(),
    );
    if let Message::Assistant(a) = &transformed_diff[0] {
        assert_eq!(a.tool_calls()[0].item_id, None);
    } else {
        panic!("expected assistant");
    }

    // Foreign provider/API targeting OpenAI Codex Responses normalizes item_id to bounded fc_{hash}.
    let transformed_foreign =
        transform_messages(&[assistant_msg, tr_msg], &codex_model, |id, _, _| {
            id.to_string()
        });
    if let Message::Assistant(a) = &transformed_foreign[0] {
        let item_id = a.tool_calls()[0].item_id.as_deref().unwrap();
        assert!(item_id.starts_with("fc_"));
        assert!(item_id.len() <= 64);
        assert_ne!(item_id, "fc_original_item");
    } else {
        panic!("expected assistant");
    }
}

#[test]
fn codex_oauth_pkce_refresh_models_and_codex_responses_stream() {
    block_on(async {
        let pkce = generate_pkce_from_bytes(&[7u8; 32]);
        assert_eq!(pkce.verifier.len(), 43);
        assert_eq!(pkce.challenge.len(), 43);

        let auth = start_codex_authorization(None).unwrap();
        assert!(auth.url.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
        assert!(auth.url.contains("code_challenge_method=S256"));
        assert!(auth.url.contains("originator=jimu"));
        assert!(auth.url.contains(&format!("state={}", auth.state)));

        let parsed_url = parse_authorization_input(&format!(
            "http://localhost:1455/auth/callback?code=auth_code_1&state={}",
            auth.state
        ));
        assert_eq!(parsed_url.code.as_deref(), Some("auth_code_1"));
        assert_eq!(parsed_url.state.as_deref(), Some(auth.state.as_str()));
        let parsed_hash = parse_authorization_input("code_abc#state_xyz");
        assert_eq!(parsed_hash.code.as_deref(), Some("code_abc"));
        assert_eq!(parsed_hash.state.as_deref(), Some("state_xyz"));

        let access_jwt = make_jwt(json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "acct_codex_42" },
            "https://api.openai.com/profile": { "email": "dev@jimu.ai" },
            "exp": 2_000_000_000u64,
        }));
        let refreshed_jwt = make_jwt(json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "acct_codex_42" },
            "email": "dev@jimu.ai",
            "exp": 2_000_003_600u64,
        }));

        let transport = Arc::new(FauxTransport::new());
        transport.enqueue(json_http_ok(json!({
            "access_token": access_jwt,
            "refresh_token": "rt_initial",
            "expires_in": 3600
        })));
        transport.enqueue(json_http_ok(json!({
            "access_token": refreshed_jwt,
            "expires_in": 3600
        })));
        transport.enqueue(json_http_ok(json!({
            "models": [{
                "slug": "gpt-5.4-codex",
                "display_name": "GPT-5.4 Codex",
                "supported_reasoning_levels": [{"effort": "low"}, {"effort": "high"}],
                "default_reasoning_level": "medium",
                "input_modalities": ["text", "image"],
                "service_tiers": ["default", "priority"]
            }]
        })));
        transport.enqueue_sse([
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_cdx\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"Codex ready\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_cdx\",\"content\":[{\"type\":\"output_text\",\"text\":\"Codex ready\"}]}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_cdx\",\"status\":\"completed\",\"usage\":{\"input_tokens\":50,\"output_tokens\":10,\"total_tokens\":60}}}\n\n",
        ]);

        let creds = exchange_codex_code(
            transport.as_ref(),
            &auth,
            &format!(
                "http://localhost:1455/auth/callback?code=auth_code_1&state={}",
                auth.state
            ),
            CancelToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(creds.account_id.as_deref(), Some("acct_codex_42"));
        assert_eq!(creds.extract_email().as_deref(), Some("dev@jimu.ai"));
        assert!(creds.is_expiring_within(creds.expires_at_ms.unwrap() - 1000, 5000));
        assert!(!creds.is_expiring_within(1000, 5000));

        // Refresh preserves refresh_token when omitted by server.
        let refreshed = refresh_codex_credentials(transport.as_ref(), &creds, CancelToken::new())
            .await
            .unwrap();
        assert_eq!(refreshed.refresh_token.as_deref(), Some("rt_initial"));
        assert_eq!(refreshed.account_id.as_deref(), Some("acct_codex_42"));
        assert_eq!(
            CodexCredentials::extract_account_id(&refreshed.access_token).as_deref(),
            Some("acct_codex_42")
        );

        let catalog = fetch_codex_models(
            transport.as_ref(),
            None,
            &refreshed,
            "0.1.0",
            CancelToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].id, "gpt-5.4-codex");
        assert_eq!(catalog[0].reasoning_levels, vec!["low", "high"]);
        assert!(catalog[0].supports_images);
        assert!(catalog[0].supports_fast);

        // Stream via Models with ApiKind::OpenaiCodexResponses!
        let mut models = Models::new(transport.clone());
        models.register_provider(
            "openai-codex",
            ProviderConfig::with_api_key(refreshed.access_token.clone()),
        );
        let mut codex_model = Model::new(
            "gpt-5.4-codex",
            "openai-codex",
            ApiKind::OpenaiCodexResponses,
            "https://chatgpt.com/backend-api/",
        );
        codex_model.supports_reasoning = true;
        let ctx = TranscriptContext::new(
            "",
            vec![],
            vec![Message::User(UserMessage::text("Hello Codex", 1))],
        );
        let reply = models
            .complete(
                &codex_model,
                &ctx,
                SimpleStreamOptions {
                    session_id: Some("sess_codex_1".into()),
                    reasoning: Some(ThinkingLevel::High),
                    max_tokens: Some(4096),
                    ..Default::default()
                },
            )
            .await;
        assert_eq!(reply.stop_reason, StopReason::Stop);
        assert_eq!(reply.text_output(), "Codex ready");

        let reqs = transport.requests();
        let codex_req = reqs.last().unwrap();
        assert_eq!(
            codex_req.url,
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            codex_req.header("openai-beta"),
            Some("responses=experimental")
        );
        assert_eq!(codex_req.header("originator"), Some("jimu"));
        assert_eq!(codex_req.header("session_id"), Some("sess_codex_1"));
        assert_eq!(
            codex_req.header("chatgpt-account-id"),
            Some("acct_codex_42")
        );
        assert_eq!(codex_req.body_json["store"], false);
        assert_eq!(codex_req.body_json["stream"], true);
        assert_eq!(
            codex_req.body_json["instructions"],
            "You are a helpful assistant."
        );
        assert!(codex_req.body_json.get("max_output_tokens").is_none());
        assert_eq!(codex_req.body_json["reasoning"]["effort"], "high");
        assert_eq!(
            codex_req.body_json["include"],
            json!(["reasoning.encrypted_content"])
        );
    });
}

#[test]
fn anthropic_thinking_modes_budget_floor_root_schema_and_completions_tool_images() {
    let ctx = TranscriptContext::new(
        "Be helpful.",
        vec![],
        vec![Message::User(UserMessage::text("Check", 1))],
    );
    let opts_high = SimpleStreamOptions {
        reasoning: Some(ThinkingLevel::High),
        max_tokens: Some(8_192),
        ..Default::default()
    };

    // 1. (#10) Claude 4.7+, 4.8+, 5.x+ use adaptive thinking; legacy 3.7 and 4.0-4.6 use manual budget.
    for adaptive_id in [
        "claude-opus-4-7",
        "claude-opus-4-8",
        "claude-sonnet-5-5",
        "claude-opus-5-5",
        "claude-fable-5-2",
        "claude-haiku-5-5",
    ] {
        let mut m = anthropic_model();
        m.id = adaptive_id.into();
        let body = anthropic_messages::build_request_body(&m, &ctx, &opts_high);
        assert_eq!(
            body["thinking"],
            json!({ "type": "adaptive" }),
            "model {adaptive_id} must use adaptive thinking"
        );
        assert_eq!(body["output_config"], json!({ "effort": "high" }));
    }

    for legacy_id in [
        "claude-3-7-sonnet-20250219",
        "claude-sonnet-4-20250514",
        "claude-opus-4-20250514",
        "claude-opus-4-1-20250805",
        "claude-sonnet-4-5-20250929",
        "claude-sonnet-4-6",
        "claude-opus-4-6",
    ] {
        let mut m = anthropic_model();
        m.id = legacy_id.into();
        let body = anthropic_messages::build_request_body(&m, &ctx, &opts_high);
        assert_eq!(
            body["thinking"]["type"], "enabled",
            "legacy model {legacy_id} must use manual budget"
        );
        let budget = body["thinking"]["budget_tokens"].as_u64().unwrap();
        assert!((1024..8192).contains(&budget));
        assert!(body.get("output_config").is_none());
    }

    // Explicit ModelCompat.anthropic_thinking overrides version heuristic in both directions.
    let mut legacy_forced_adaptive = anthropic_model();
    legacy_forced_adaptive.id = "claude-sonnet-4-6".into();
    legacy_forced_adaptive.compat = Some(ModelCompat {
        anthropic_thinking: Some(AnthropicThinkingMode::Adaptive),
        ..ModelCompat::default()
    });
    let forced_adaptive_body =
        anthropic_messages::build_request_body(&legacy_forced_adaptive, &ctx, &opts_high);
    assert_eq!(
        forced_adaptive_body["thinking"],
        json!({ "type": "adaptive" })
    );

    let mut modern_forced_manual = anthropic_model();
    modern_forced_manual.id = "claude-opus-4-8".into();
    modern_forced_manual.compat = Some(ModelCompat {
        anthropic_thinking: Some(AnthropicThinkingMode::ManualBudget),
        ..ModelCompat::default()
    });
    let forced_manual_body =
        anthropic_messages::build_request_body(&modern_forced_manual, &ctx, &opts_high);
    assert_eq!(forced_manual_body["thinking"]["type"], "enabled");

    // 2. (#26) Legacy manual budget omits `thinking` when max_tokens <= 1024, and clamps to 1024..=(max_tokens-1).
    let legacy = anthropic_model(); // claude-sonnet-4-6
    for tiny_max in [512u32, 1024u32] {
        let opts_tiny = SimpleStreamOptions {
            reasoning: Some(ThinkingLevel::Medium),
            max_tokens: Some(tiny_max),
            temperature: Some(0.5),
            ..Default::default()
        };
        let body = anthropic_messages::build_request_body(&legacy, &ctx, &opts_tiny);
        assert!(
            body.get("thinking").is_none(),
            "max_tokens={tiny_max} must omit manual thinking instead of emitting illegal budget_tokens"
        );
        assert_eq!(body["temperature"], 0.5);
    }
    let opts_1025 = SimpleStreamOptions {
        reasoning: Some(ThinkingLevel::High),
        max_tokens: Some(1025),
        ..Default::default()
    };
    let body_1025 = anthropic_messages::build_request_body(&legacy, &ctx, &opts_1025);
    assert_eq!(body_1025["thinking"]["budget_tokens"], 1024);

    // 3. (#25) Anthropic convert_tools preserves root-level constraints and expands root `$ref`.
    let ref_tool = ToolDefinition {
        name: "search".into(),
        description: "Search with root $ref".into(),
        parameters: json!({
            "$defs": {
                "Query": {
                    "type": "object",
                    "properties": {
                        "q": { "type": "string", "minLength": 1 },
                        "limit": { "type": "integer" }
                    },
                    "required": ["q"],
                    "additionalProperties": false
                }
            },
            "$ref": "#/$defs/Query",
            "minProperties": 1
        }),
    };
    let oneof_tool = ToolDefinition {
        name: "dispatch".into(),
        description: "Root oneOf preserved".into(),
        parameters: json!({
            "type": "object",
            "properties": { "a": { "type": "string" }, "b": { "type": "string" } },
            "oneOf": [{ "required": ["a"] }, { "required": ["b"] }]
        }),
    };
    let tool_ctx = TranscriptContext::new(
        "Use tools.",
        vec![ref_tool, oneof_tool],
        vec![Message::User(UserMessage::text("Go", 1))],
    );
    let ant_tool_body =
        anthropic_messages::build_request_body(&legacy, &tool_ctx, &SimpleStreamOptions::default());
    let tools = ant_tool_body["tools"].as_array().unwrap();
    let search_schema = &tools[0]["input_schema"];
    assert_eq!(search_schema["type"], "object");
    assert_eq!(search_schema["properties"]["q"]["minLength"], 1);
    assert_eq!(search_schema["required"], json!(["q"]));
    assert_eq!(search_schema["additionalProperties"], false);
    assert_eq!(search_schema["minProperties"], 1);
    assert!(search_schema.get("$defs").is_some());
    assert!(search_schema.get("$ref").is_none());

    let dispatch_schema = &tools[1]["input_schema"];
    assert_eq!(
        dispatch_schema["oneOf"],
        json!([{ "required": ["a"] }, { "required": ["b"] }])
    );

    // 4. (#24) OpenAI Chat Completions preserves ToolResult images after contiguous `role: "tool"` block.
    let mut vision_comp = openai_completions_model();
    vision_comp.supports_images = true;
    let assistant_calls = AssistantMessage {
        content: vec![
            AssistantBlock::ToolCall(ToolCall {
                id: "call_img".into(),
                name: "screenshot".into(),
                arguments: Map::new(),
                item_id: None,
                thought_signature: None,
                namespace: None,
                arguments_error: None,
            }),
            AssistantBlock::ToolCall(ToolCall {
                id: "call_txt".into(),
                name: "read".into(),
                arguments: Map::new(),
                item_id: None,
                thought_signature: None,
                namespace: None,
                arguments_error: None,
            }),
        ],
        api: ApiKind::OpenaiCompletions,
        provider: vision_comp.provider.clone(),
        model: vision_comp.id.clone(),
        response_id: None,
        thinking_level: None,
        usage: Default::default(),
        stop_reason: StopReason::ToolUse,
        error_message: None,
        diagnostics: Vec::new(),
        timestamp: 1,
        duration_ms: None,
    };
    let img_tr = ToolResultMessage {
        tool_call_id: "call_img".into(),
        tool_name: "screenshot".into(),
        content: vec![ContentPart::Image(ImageContent {
            data: "iVBORw0KGgo=".into(),
            mime_type: "image/png".into(),
        })],
        is_error: false,
        timestamp: 2,
    };
    let txt_tr = ToolResultMessage {
        tool_call_id: "call_txt".into(),
        tool_name: "read".into(),
        content: vec![ContentPart::Text(TextContent::plain("done"))],
        is_error: false,
        timestamp: 3,
    };
    let img_ctx = TranscriptContext::new(
        "Sys",
        vec![],
        vec![
            Message::User(UserMessage::text("Take screenshot", 0)),
            Message::Assistant(assistant_calls),
            Message::ToolResult(img_tr),
            Message::ToolResult(txt_tr),
        ],
    );
    let comp_body = openai_completions::build_request_body(
        &vision_comp,
        &img_ctx,
        &SimpleStreamOptions::default(),
    );
    let msgs = comp_body["messages"].as_array().unwrap();
    // [0] system, [1] user, [2] assistant, [3] tool(call_img), [4] tool(call_txt), [5] user(image_url)
    assert_eq!(msgs.len(), 6);
    assert_eq!(msgs[3]["role"], "tool");
    assert_eq!(msgs[3]["tool_call_id"], "call_img");
    assert_eq!(msgs[3]["content"], "[image attached]");
    assert_eq!(msgs[4]["role"], "tool");
    assert_eq!(msgs[4]["tool_call_id"], "call_txt");
    assert_eq!(msgs[4]["content"], "done");
    assert_eq!(msgs[5]["role"], "user");
    let trailing_parts = msgs[5]["content"].as_array().unwrap();
    assert!(trailing_parts.iter().any(|p| {
        p["type"] == "image_url" && p["image_url"]["url"] == "data:image/png;base64,iVBORw0KGgo="
    }));
}

#[test]
fn hidden_user_flags_preserve_provider_request_bytes() {
    let ordinary = UserMessage::text("Context for the model", 1);
    let hidden: UserMessage = serde_json::from_value(json!({
        "content": ordinary.content, "isMeta": true, "timestamp": 1,
    }))
    .unwrap();
    assert!(hidden.is_meta);
    assert_eq!(serde_json::to_value(&hidden).unwrap()["is_meta"], true);
    assert!(
        serde_json::to_value(&ordinary)
            .unwrap()
            .get("is_meta")
            .is_none()
    );

    let transcript =
        |user| TranscriptContext::new("Base", sample_tools(), vec![Message::User(user)]);
    let (ordinary, hidden) = (transcript(ordinary), transcript(hidden));
    let options = SimpleStreamOptions::default();
    type BodyBuilder = fn(&Model, &TranscriptContext, &SimpleStreamOptions) -> Value;
    for (model, build) in [
        (
            openai_responses_model(),
            openai_responses::build_request_body as BodyBuilder,
        ),
        (
            openai_completions_model(),
            openai_completions::build_request_body,
        ),
        (anthropic_model(), anthropic_messages::build_request_body),
        (google_model(), google_generative_ai::build_request_body),
    ] {
        let body = serde_json::to_string(&build(&model, &hidden, &options)).unwrap();
        assert_eq!(
            body,
            serde_json::to_string(&build(&model, &ordinary, &options)).unwrap()
        );
        assert!(body.contains("Context for the model"));
        assert!(!body.contains("is_meta") && !body.contains("isMeta"));
    }
}

#[test]
fn model_defaults_and_context_window_max_tokens_clamping() {
    let m = Model::new(
        "claude-sonnet-4-6",
        "anthropic",
        ApiKind::AnthropicMessages,
        "https://api.anthropic.com",
    );
    assert_eq!(m.context_window, jimu_ai::DEFAULT_CONTEXT_WINDOW);
    assert_eq!(m.max_output_tokens, jimu_ai::DEFAULT_MAX_OUTPUT_TOKENS);

    // 200k window with 160k token input (~640KB) + 128k max_output_tokens must clamp max_tokens
    // below remaining context room instead of sending 128k and hitting upstream 400.
    let mut constrained = m;
    constrained.context_window = 200_000;
    constrained.max_output_tokens = 128_000;
    let heavy_input = "a".repeat(160_000 * 4);
    let ctx = TranscriptContext::new(
        "",
        Vec::new(),
        vec![Message::User(UserMessage::text(heavy_input, 1))],
    );
    let body =
        anthropic_messages::build_request_body(&constrained, &ctx, &SimpleStreamOptions::default());
    let sent_max = body["max_tokens"].as_u64().unwrap();
    assert!(
        (30_000..=36_000).contains(&sent_max),
        "unexpected clamped max_tokens={sent_max}"
    );
}

#[test]
fn consecutive_user_and_tool_result_messages_merge_across_strict_providers() {
    let assistant_call = Message::Assistant(AssistantMessage {
        content: vec![AssistantBlock::ToolCall(ToolCall {
            id: "call_1".into(),
            name: "read".into(),
            arguments: Map::new(),
            item_id: None,
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        })],
        api: ApiKind::AnthropicMessages,
        provider: "anthropic".into(),
        model: "claude-sonnet-4-6".into(),
        response_id: None,
        thinking_level: None,
        usage: Default::default(),
        stop_reason: StopReason::ToolUse,
        error_message: None,
        diagnostics: Vec::new(),
        timestamp: 2,
        duration_ms: None,
    });
    let tool_res = Message::ToolResult(ToolResultMessage {
        tool_call_id: "call_1".into(),
        tool_name: "read".into(),
        content: vec![ContentPart::Text(TextContent::plain("file contents"))],
        is_error: false,
        timestamp: 3,
    });
    let ctx = TranscriptContext::new(
        "sys",
        Vec::new(),
        vec![
            Message::User(UserMessage::meta_text(
                "<system-reminder>A</system-reminder>",
                0,
            )),
            Message::User(UserMessage::text("hello", 1)),
            assistant_call,
            tool_res,
            Message::User(UserMessage::meta_text(
                "<system-reminder>B</system-reminder>",
                4,
            )),
            Message::User(UserMessage::text("follow up", 5)),
        ],
    );
    let opts = SimpleStreamOptions::default();

    // Anthropic: user -> assistant -> user(tool_result + text + text)
    let anth = anthropic_messages::build_request_body(&anthropic_model(), &ctx, &opts);
    let anth_msgs = anth["messages"].as_array().unwrap();
    assert_eq!(anth_msgs.len(), 3);
    assert_eq!(anth_msgs[0]["role"], "user");
    assert_eq!(anth_msgs[0]["content"].as_array().unwrap().len(), 2);
    assert_eq!(anth_msgs[1]["role"], "assistant");
    assert_eq!(anth_msgs[2]["role"], "user");
    let anth_tail = anth_msgs[2]["content"].as_array().unwrap();
    assert_eq!(anth_tail.len(), 3);
    assert_eq!(anth_tail[0]["type"], "tool_result");
    assert_eq!(anth_tail[1]["text"], "<system-reminder>B</system-reminder>");
    assert_eq!(anth_tail[2]["text"], "follow up");

    // Google Gemini: user -> model -> user(functionResponse + text + text)
    let goog = google_generative_ai::build_request_body(&google_model(), &ctx, &opts);
    let goog_msgs = goog["contents"].as_array().unwrap();
    assert_eq!(goog_msgs.len(), 3);
    assert_eq!(goog_msgs[0]["role"], "user");
    assert_eq!(goog_msgs[0]["parts"].as_array().unwrap().len(), 2);
    assert_eq!(goog_msgs[1]["role"], "model");
    assert_eq!(goog_msgs[2]["role"], "user");
    let goog_tail = goog_msgs[2]["parts"].as_array().unwrap();
    assert_eq!(goog_tail.len(), 3);
    assert!(goog_tail[0].get("functionResponse").is_some());
    assert_eq!(goog_tail[1]["text"], "<system-reminder>B</system-reminder>");
    assert_eq!(goog_tail[2]["text"], "follow up");

    // OpenAI Completions: system -> user(merged) -> assistant -> tool -> user(merged)
    let oai = openai_completions::build_request_body(&openai_completions_model(), &ctx, &opts);
    let oai_msgs = oai["messages"].as_array().unwrap();
    assert_eq!(oai_msgs.len(), 5);
    assert_eq!(oai_msgs[1]["role"], "user");
    assert_eq!(
        oai_msgs[1]["content"],
        "<system-reminder>A</system-reminder>\n\nhello"
    );
    assert_eq!(oai_msgs[3]["role"], "tool");
    assert_eq!(oai_msgs[4]["role"], "user");
    assert_eq!(
        oai_msgs[4]["content"],
        "<system-reminder>B</system-reminder>\n\nfollow up"
    );
}
