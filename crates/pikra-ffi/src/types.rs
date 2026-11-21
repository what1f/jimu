use pikra_agent::{AgentToolResult, QueueMode, ToolExecutionMode};
use pikra_ai::{
    CacheRetention, ContentPart, Model, StopReason, ThinkingLevel, ToolDefinition, Usage,
};
use pikra_codemode::CodemodeSandboxOptions;
use pikra_js::{ExtensionSource, JsExtensionConfig};
use pikra_schema::Schema;
use pikra_session::CompactionConfig;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::PathBuf;

/// C-ABI status codes matching `enum PikraStatus` in `include/pikra.h`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PikraStatus {
    Ok = 0,
    ErrInvalidArgument = 1,
    ErrBusy = 2,
    ErrClosed = 3,
    ErrNotFound = 4,
    ErrIo = 5,
    ErrInternal = 6,
    ErrCancelled = 7,
    ErrBackpressure = 8,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FfiToolSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_object_schema")]
    pub parameters: Schema,
    #[serde(default)]
    pub deferred: bool,
    #[serde(default)]
    pub execution_mode: ToolExecutionMode,
}

fn default_object_schema() -> Schema {
    serde_json::json!({
        "type": "object",
        "properties": {}
    })
}

impl FfiToolSpec {
    pub fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct FfiCompactionConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub compaction_reserve_tokens: Option<u32>,
    #[serde(default = "default_keep_recent_tokens")]
    pub keep_recent_tokens: u64,
    #[serde(default = "default_max_summary_chars")]
    pub max_summary_chars: usize,
    #[serde(default)]
    pub custom_instructions: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_keep_recent_tokens() -> u64 {
    pikra_session::DEFAULT_KEEP_RECENT_TOKENS
}

fn default_max_summary_chars() -> usize {
    pikra_session::DEFAULT_MAX_SUMMARY_CHARS
}

impl From<FfiCompactionConfig> for CompactionConfig {
    fn from(c: FfiCompactionConfig) -> Self {
        Self {
            enabled: c.enabled,
            compaction_reserve_tokens: c.compaction_reserve_tokens,
            keep_recent_tokens: c.keep_recent_tokens,
            max_summary_chars: c.max_summary_chars,
            custom_instructions: c.custom_instructions,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct FfiCodemodeOptions {
    #[serde(default = "default_codemode_timeout")]
    pub timeout_ms: Option<u64>,
    #[serde(default = "default_codemode_mem")]
    pub memory_limit_bytes: usize,
    #[serde(default = "default_codemode_stack")]
    pub max_stack_size: usize,
}

fn default_codemode_timeout() -> Option<u64> {
    Some(pikra_codemode::DEFAULT_TIMEOUT_MS)
}

fn default_codemode_mem() -> usize {
    pikra_codemode::DEFAULT_MEMORY_LIMIT_BYTES
}

fn default_codemode_stack() -> usize {
    pikra_codemode::DEFAULT_MAX_STACK_SIZE
}

impl From<FfiCodemodeOptions> for CodemodeSandboxOptions {
    fn from(o: FfiCodemodeOptions) -> Self {
        Self {
            timeout_ms: o.timeout_ms,
            memory_limit_bytes: o.memory_limit_bytes,
            max_stack_size: o.max_stack_size,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FfiJsExtensionSource {
    Inline { name: String, code: String },
    File { path: String },
}

#[derive(Debug, Clone, Deserialize)]
pub struct FfiJsExtensionConfig {
    #[serde(default)]
    pub sources: Vec<FfiJsExtensionSource>,
    #[serde(default = "default_js_mem")]
    pub memory_limit_bytes: usize,
    #[serde(default = "default_js_stack")]
    pub max_stack_size_bytes: usize,
    #[serde(default = "default_js_timeout")]
    pub timeout_ms: u64,
}

fn default_js_mem() -> usize {
    pikra_js::DEFAULT_MEMORY_LIMIT_BYTES
}

fn default_js_stack() -> usize {
    pikra_js::DEFAULT_MAX_STACK_SIZE_BYTES
}

fn default_js_timeout() -> u64 {
    pikra_js::DEFAULT_TIMEOUT_MS
}

impl From<FfiJsExtensionConfig> for JsExtensionConfig {
    fn from(cfg: FfiJsExtensionConfig) -> Self {
        let sources = cfg
            .sources
            .into_iter()
            .map(|s| match s {
                FfiJsExtensionSource::Inline { name, code } => ExtensionSource::Inline { name, code },
                FfiJsExtensionSource::File { path } => ExtensionSource::File(PathBuf::from(path)),
            })
            .collect();
        Self {
            sources,
            memory_limit_bytes: cfg.memory_limit_bytes,
            max_stack_size_bytes: cfg.max_stack_size_bytes,
            timeout_ms: cfg.timeout_ms,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct FfiSessionConfig {
    pub model: Model,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default)]
    pub tools: Vec<FfiToolSpec>,
    #[serde(default = "default_thinking_off")]
    pub thinking_level: ThinkingLevel,
    #[serde(default)]
    pub fast: bool,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub cache_retention: CacheRetention,
    #[serde(default)]
    pub headers: Vec<(String, Option<String>)>,
    #[serde(default)]
    pub sampling_params: Map<String, Value>,
    #[serde(default)]
    pub skill_roots: Vec<String>,
    #[serde(default = "default_true")]
    pub enable_deferred_gateway: bool,
    #[serde(default)]
    pub enable_codemode: bool,
    #[serde(default)]
    pub codemode_options: Option<FfiCodemodeOptions>,
    #[serde(default)]
    pub persistence_path: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub compaction: Option<FfiCompactionConfig>,
    #[serde(default)]
    pub tool_execution_mode: ToolExecutionMode,
    #[serde(default)]
    pub steering_mode: QueueMode,
    #[serde(default)]
    pub follow_up_mode: QueueMode,
    #[serde(default)]
    pub rollback_first_turn_on_failure: bool,
    #[serde(default)]
    pub js_extensions: Option<FfiJsExtensionConfig>,
}

fn default_thinking_off() -> ThinkingLevel {
    ThinkingLevel::Off
}

#[derive(Debug, Clone, Deserialize)]
pub struct FfiPromptRequest {
    #[serde(default)]
    pub id: Option<String>,
    pub content: Vec<ContentPart>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FfiHttpRequestHead {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FfiSessionInfo {
    pub session_id: String,
    pub leaf_id: Option<String>,
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub fast: bool,
    pub is_running: bool,
    pub resident_tools: Vec<ToolDefinition>,
    pub deferred_tools: Vec<ToolDefinition>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FfiCompactionResultJson {
    pub entry_id: String,
    pub summary: String,
    pub first_kept_entry_id: String,
    pub tokens_before: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

/// Real-time structured events emitted over FFI to Swift.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FfiEvent {
    TurnStarted,
    TextDelta {
        content_index: usize,
        delta: String,
    },
    ThinkingDelta {
        content_index: usize,
        delta: String,
    },
    ToolStarted {
        tool_call_id: String,
        tool_name: String,
        args: Value,
    },
    ToolUpdated {
        tool_call_id: String,
        tool_name: String,
        args: Value,
        partial_result: AgentToolResult,
    },
    ToolFinished {
        tool_call_id: String,
        tool_name: String,
        result: AgentToolResult,
        is_error: bool,
    },
    RequestUsage {
        model: String,
        provider: String,
        usage: Usage,
    },
    Steered {
        id: String,
        text: String,
    },
    ContextAppended {
        name: String,
        text: String,
    },
    CompactionStarted {
        tokens_before: u64,
    },
    CompactionEnded {
        summary: String,
        tokens_before: u64,
        first_kept_entry_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    TurnEnded {
        stop_reason: StopReason,
        user_message_kept: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error_message: Option<String>,
    },
}
