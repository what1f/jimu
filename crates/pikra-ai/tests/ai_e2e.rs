//! End-to-end tests for `pikra-ai` across all 4 wire protocols, Anthropic production edge cases,
//! cross-provider handoffs, multi-turn prompt cache prefix stability, and cooperative abort.

use futures_executor::block_on;
use futures_util::StreamExt;
use pikra_ai::{
    ApiKind, AssistantBlock, AssistantMessage, AssistantMessageEvent, CacheRetention, CancelToken,
    ContentPart, ImageContent, Message, Model, ModelCost, Models, ProviderConfig,
    SimpleStreamOptions, StopReason, StreamProvider, SystemMessage, TextContent, ThinkingContent,
    ThinkingLevel, ToolCall, ToolDefinition, ToolResultMessage, TranscriptContext, UserMessage,
};
use pikra_schema::{int_schema, obj_schema, str_schema};
use pikra_testkit::{FauxTransport, assert_prompt_prefix_identical};
use serde_json::{Map, Value, json};
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
            parameters: obj_schema([
                ("cmd", str_schema(), true),
                ("cwd", str_schema(), false),
            ]),
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
            "You are Pikra.",
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
        assert_eq!(resp_msg.tool_calls()[0].id, "call_1|fc_a");
        assert_eq!(resp_msg.usage.input, 600);
        assert_eq!(resp_msg.usage.cache_read, 400);
        assert_eq!(resp_msg.usage.reasoning, Some(50));
        // Cost = (600 * 2.5 + 200 * 10.0 + 400 * 0.25) / 1e6 = 0.0036
        assert!((resp_msg.usage.cost_total - 0.0036).abs() < 1e-9);
        match &resp_msg.content[0] {
            AssistantBlock::Thinking(t) => {
                assert!(t.thinking_signature.as_deref().unwrap().contains("enc_blob_123"));
            }
            other => panic!("expected thinking block, got {other:?}"),
        }
        match &resp_msg.content[1] {
            AssistantBlock::Text(t) => {
                assert_eq!(t.text, "Checking...");
                let sig: Value = serde_json::from_str(t.text_signature.as_deref().unwrap()).unwrap();
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
        assert_eq!(req0.body_json["prompt_cache_key"].as_str().unwrap().len(), 64);

        // Verify incomplete due to max_output_tokens maps strictly to StopReason::Length without injecting text.
        transport.enqueue_sse([
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_trunc\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"Partial\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_trunc\",\"content\":[{\"type\":\"output_text\",\"text\":\"Partial\"}]}}\n\n",
            "data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp_trunc\",\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n",
        ]);
        let trunc_msg = models
            .complete(&openai_responses_model(), &ctx, SimpleStreamOptions::default())
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
        comp_opts.sampling_params.insert("top_p".into(), json!(0.85));
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
                assert_eq!(t.thinking_signature.as_deref(), Some("opaque_redacted_blob"));
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
                    thinking_signature: Some("{\"type\":\"reasoning\",\"encrypted_content\":\"enc\"}".into()),
                    redacted: false,
                }),
                AssistantBlock::Text(TextContent {
                    text: "Calling both tools".into(),
                    text_signature: Some("{\"v\":1,\"id\":\"msg_oa\",\"phase\":\"commentary\"}".into()),
                }),
                AssistantBlock::ToolCall(ToolCall {
                    id: oa_id_1.clone(),
                    name: "read_file".into(),
                    arguments: args1,
                    thought_signature: Some("foreign_thought_sig".into()),
                    namespace: Some("ns".into()),
                    arguments_error: None,
                }),
                AssistantBlock::ToolCall(ToolCall {
                    id: oa_id_2.clone(),
                    name: "read_file".into(),
                    arguments: args2,
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
            assert!(id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
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
            .complete(&openai_completions_model(), &ctx, SimpleStreamOptions::default())
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
