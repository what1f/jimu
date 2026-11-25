//! Public configuration, errors, source discovery, and JS<->Rust wire conversion types.

use jimu_agent::{
    AfterToolCallCtx, AfterToolCallResult, AgentContextSnapshot, AgentMessage, AgentToolResult,
    BeforeToolCallCtx, BeforeToolCallResult, FinishTurnCtx, RequestOverride, ToolExecutionMode,
    TurnDecision,
};
use jimu_ai::{
    AssistantMessage, ContentPart, ImageContent, Model, TextContent, ThinkingLevel, ToolCall,
    ToolDefinition, ToolResultMessage,
};
use jimu_schema::{CompiledSchema, Schema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Default QuickJS heap memory limit (32 MiB).
pub const DEFAULT_MEMORY_LIMIT_BYTES: usize = 32 * 1024 * 1024;

/// Default QuickJS JS call stack size limit (1 MiB).
pub const DEFAULT_MAX_STACK_SIZE_BYTES: usize = 1024 * 1024;

/// Default per-hook / per-tool / extension-load timeout (10 seconds).
pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;

/// Source of a JavaScript extension to load into [`crate::JsExtensionRuntime`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionSource {
    /// Inline JavaScript source code identified by a virtual filename.
    Inline { name: String, code: String },
    /// Path to a `.js`/`.mjs` file or a directory containing extensions (`*.js`, `*/index.js`).
    File(PathBuf),
}

impl ExtensionSource {
    pub fn inline(name: impl Into<String>, code: impl Into<String>) -> Self {
        Self::Inline {
            name: name.into(),
            code: code.into(),
        }
    }

    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self::File(path.into())
    }
}

/// Configuration for constructing a [`crate::JsExtensionRuntime`].
#[derive(Debug, Clone)]
pub struct JsExtensionConfig {
    pub sources: Vec<ExtensionSource>,
    pub memory_limit_bytes: usize,
    pub max_stack_size_bytes: usize,
    pub timeout_ms: u64,
}

impl Default for JsExtensionConfig {
    fn default() -> Self {
        Self {
            sources: Vec::new(),
            memory_limit_bytes: DEFAULT_MEMORY_LIMIT_BYTES,
            max_stack_size_bytes: DEFAULT_MAX_STACK_SIZE_BYTES,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }
}

impl JsExtensionConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_source(mut self, source: ExtensionSource) -> Self {
        self.sources.push(source);
        self
    }

    pub fn with_sources(mut self, sources: impl IntoIterator<Item = ExtensionSource>) -> Self {
        self.sources.extend(sources);
        self
    }

    pub fn with_inline(mut self, name: impl Into<String>, code: impl Into<String>) -> Self {
        self.sources.push(ExtensionSource::inline(name, code));
        self
    }

    pub fn with_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.sources.push(ExtensionSource::file(path));
        self
    }

    pub fn with_memory_limit_bytes(mut self, bytes: usize) -> Self {
        self.memory_limit_bytes = bytes;
        self
    }

    pub fn with_max_stack_size_bytes(mut self, bytes: usize) -> Self {
        self.max_stack_size_bytes = bytes;
        self
    }

    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }
}

/// Control-plane lifecycle hook kinds supported by JS extensions.
///
/// Note: Per-token streaming delta events (`MessageUpdate`) are intentionally NOT exposed here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookKind {
    PrepareRequest,
    TransformContext,
    BeforeToolCall,
    AfterToolCall,
    FinishTurn,
}

impl HookKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PrepareRequest => "prepare_request",
            Self::TransformContext => "transform_context",
            Self::BeforeToolCall => "before_tool_call",
            Self::AfterToolCall => "after_tool_call",
            Self::FinishTurn => "finish_turn",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "prepare_request" => Some(Self::PrepareRequest),
            "transform_context" => Some(Self::TransformContext),
            "before_tool_call" => Some(Self::BeforeToolCall),
            "after_tool_call" => Some(Self::AfterToolCall),
            "finish_turn" => Some(Self::FinishTurn),
            _ => None,
        }
    }

    pub const fn bit(self) -> u8 {
        match self {
            Self::PrepareRequest => 1 << 0,
            Self::TransformContext => 1 << 1,
            Self::BeforeToolCall => 1 << 2,
            Self::AfterToolCall => 1 << 3,
            Self::FinishTurn => 1 << 4,
        }
    }
}

/// Errors returned by [`crate::JsExtensionRuntime`].
#[derive(Debug, Clone, Error)]
pub enum JsExtensionError {
    #[error("failed to read extension source '{path}': {message}")]
    Io { path: String, message: String },

    #[error("JavaScript error: {0}")]
    Script(String),

    #[error("JavaScript execution timed out after {timeout_ms}ms")]
    Timeout { timeout_ms: u64 },

    #[error("JavaScript execution cancelled")]
    Cancelled,

    #[error("JavaScript out of memory: {0}")]
    OutOfMemory(String),

    #[error("invalid tool registration '{name}': {reason}")]
    InvalidTool { name: String, reason: String },

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("JS extension runtime worker unavailable: {0}")]
    RuntimeUnavailable(String),
}

/// Resolved concrete JavaScript source ready to evaluate in QuickJS.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedSource {
    pub name: String,
    pub code: String,
}

/// Expand all configured [`ExtensionSource`]s by reading file/directory entries from disk.
pub(crate) fn resolve_sources(
    sources: &[ExtensionSource],
) -> Result<Vec<ResolvedSource>, JsExtensionError> {
    let mut resolved = Vec::new();
    for source in sources {
        match source {
            ExtensionSource::Inline { name, code } => {
                resolved.push(ResolvedSource {
                    name: name.clone(),
                    code: code.clone(),
                });
            }
            ExtensionSource::File(path) => {
                let meta = fs::metadata(path).map_err(|err| JsExtensionError::Io {
                    path: path.display().to_string(),
                    message: err.to_string(),
                })?;
                if meta.is_file() {
                    let code = fs::read_to_string(path).map_err(|err| JsExtensionError::Io {
                        path: path.display().to_string(),
                        message: err.to_string(),
                    })?;
                    resolved.push(ResolvedSource {
                        name: path.display().to_string(),
                        code,
                    });
                } else if meta.is_dir() {
                    let files = discover_dir_extensions(path)?;
                    for file_path in files {
                        let code =
                            fs::read_to_string(&file_path).map_err(|err| JsExtensionError::Io {
                                path: file_path.display().to_string(),
                                message: err.to_string(),
                            })?;
                        resolved.push(ResolvedSource {
                            name: file_path.display().to_string(),
                            code,
                        });
                    }
                } else {
                    return Err(JsExtensionError::Io {
                        path: path.display().to_string(),
                        message: "path is neither a regular file nor a directory".to_string(),
                    });
                }
            }
        }
    }
    Ok(resolved)
}

fn is_js_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("js" | "mjs")
    )
}

fn discover_dir_extensions(dir: &Path) -> Result<Vec<PathBuf>, JsExtensionError> {
    let entries = fs::read_dir(dir).map_err(|err| JsExtensionError::Io {
        path: dir.display().to_string(),
        message: err.to_string(),
    })?;

    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| JsExtensionError::Io {
            path: dir.display().to_string(),
            message: err.to_string(),
        })?;
        let path = entry.path();
        if path.is_file() && is_js_file(&path) {
            paths.push(path);
        } else if path.is_dir() {
            let index_js = path.join("index.js");
            let index_mjs = path.join("index.mjs");
            if index_js.is_file() {
                paths.push(index_js);
            } else if index_mjs.is_file() {
                paths.push(index_mjs);
            }
        }
    }
    paths.sort();
    Ok(paths)
}

/// Metadata for a registered JS tool captured after VM load.
#[derive(Debug, Clone)]
pub(crate) struct RegisteredToolMeta {
    pub definition: ToolDefinition,
    pub execution_mode: ToolExecutionMode,
}

/// Snapshot of registered tools, hooks, and loaded extension names for the active VM.
#[derive(Debug, Clone, Default)]
pub(crate) struct RegistrySnapshot {
    pub generation: u64,
    pub tools: Vec<RegisteredToolMeta>,
    pub hook_mask: u8,
    pub loaded_extensions: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawInspectPayload {
    #[serde(default)]
    tools: Vec<RawToolMeta>,
    #[serde(default)]
    hooks: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawToolMeta {
    name: String,
    #[serde(default)]
    description: String,
    parameters: Schema,
    #[serde(default, alias = "execution_mode")]
    execution_mode: Option<String>,
}

pub(crate) fn parse_registry_snapshot(
    inspect_json: &str,
    loaded_extensions: Vec<String>,
) -> Result<RegistrySnapshot, JsExtensionError> {
    let raw: RawInspectPayload = serde_json::from_str(inspect_json).map_err(|err| {
        JsExtensionError::Serialization(format!("invalid inspect payload: {err}"))
    })?;

    let mut tools = Vec::with_capacity(raw.tools.len());
    for item in raw.tools {
        if item.name.trim().is_empty() {
            return Err(JsExtensionError::InvalidTool {
                name: item.name,
                reason: "tool name must not be empty".to_string(),
            });
        }
        CompiledSchema::compile(item.parameters.clone()).map_err(|err| {
            JsExtensionError::InvalidTool {
                name: item.name.clone(),
                reason: err.to_string(),
            }
        })?;
        let execution_mode = match item.execution_mode.as_deref() {
            None | Some("parallel") => ToolExecutionMode::Parallel,
            Some("sequential") => ToolExecutionMode::Sequential,
            Some(other) => {
                return Err(JsExtensionError::InvalidTool {
                    name: item.name,
                    reason: format!(
                        "unsupported executionMode '{other}' (expected 'parallel' or 'sequential')"
                    ),
                });
            }
        };
        tools.push(RegisteredToolMeta {
            definition: ToolDefinition {
                name: item.name,
                description: item.description,
                parameters: item.parameters,
            },
            execution_mode,
        });
    }

    let mut hook_mask = 0u8;
    for hook in raw.hooks {
        if let Some(kind) = HookKind::from_name(&hook) {
            hook_mask |= kind.bit();
        }
    }

    Ok(RegistrySnapshot {
        generation: 0,
        tools,
        hook_mask,
        loaded_extensions,
    })
}

// ---- Wire DTOs for Tool Execution & Lifecycle Hooks ----

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JsPrepareRequestSnapshot<'a> {
    pub system_prompt: &'a str,
    pub tools: &'a [ToolDefinition],
    pub messages: &'a [AgentMessage],
    pub model: &'a Model,
    pub thinking_level: ThinkingLevel,
    pub fast: bool,
}

impl<'a> From<&'a AgentContextSnapshot> for JsPrepareRequestSnapshot<'a> {
    fn from(s: &'a AgentContextSnapshot) -> Self {
        Self {
            system_prompt: &s.system_prompt,
            tools: &s.tools,
            messages: &s.messages,
            model: &s.model,
            thinking_level: s.thinking_level,
            fast: s.fast,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JsRequestOverride {
    #[serde(default, alias = "system_prompt")]
    system_prompt: Option<String>,
    #[serde(default)]
    tools: Option<Vec<ToolDefinition>>,
    #[serde(default)]
    messages: Option<Vec<AgentMessage>>,
    #[serde(default)]
    model: Option<Model>,
    #[serde(default, alias = "thinking_level")]
    thinking_level: Option<ThinkingLevel>,
    #[serde(default)]
    fast: Option<bool>,
}

pub(crate) fn parse_request_override(
    json: &str,
) -> Result<Option<RequestOverride>, JsExtensionError> {
    let raw: JsRequestOverride = serde_json::from_str(json).map_err(|err| {
        JsExtensionError::Serialization(format!("invalid prepare_request return value: {err}"))
    })?;
    if raw.system_prompt.is_none()
        && raw.tools.is_none()
        && raw.messages.is_none()
        && raw.model.is_none()
        && raw.thinking_level.is_none()
        && raw.fast.is_none()
    {
        return Ok(None);
    }
    Ok(Some(RequestOverride {
        system_prompt: raw.system_prompt,
        tools: raw.tools,
        messages: raw.messages,
        model: raw.model,
        thinking_level: raw.thinking_level,
        fast: raw.fast,
    }))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JsBeforeToolCallPayload<'a> {
    pub tool_call: &'a ToolCall,
    pub args: &'a Value,
    pub assistant_message: &'a AssistantMessage,
}

impl<'a> From<&'a BeforeToolCallCtx<'a>> for JsBeforeToolCallPayload<'a> {
    fn from(ctx: &'a BeforeToolCallCtx<'a>) -> Self {
        Self {
            tool_call: ctx.tool_call,
            args: ctx.args,
            assistant_message: ctx.assistant_message,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JsBeforeToolCallResult {
    #[serde(default)]
    block: bool,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    terminate: bool,
    #[serde(default, alias = "args_override")]
    args_override: Option<Value>,
}

pub(crate) fn parse_before_tool_call_result(
    json: &str,
) -> Result<Option<BeforeToolCallResult>, JsExtensionError> {
    let raw: JsBeforeToolCallResult = serde_json::from_str(json).map_err(|err| {
        JsExtensionError::Serialization(format!("invalid before_tool_call return value: {err}"))
    })?;
    if !raw.block && raw.reason.is_none() && !raw.terminate && raw.args_override.is_none() {
        return Ok(None);
    }
    Ok(Some(BeforeToolCallResult {
        block: raw.block,
        reason: raw.reason,
        terminate: raw.terminate,
        args_override: raw.args_override,
    }))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JsAgentToolResultOut<'a> {
    pub content: Vec<JsContentPartOut<'a>>,
    pub details: &'a Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<&'a Value>,
    pub is_error: bool,
    pub terminate: bool,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(crate) enum JsContentPartOut<'a> {
    Text {
        text: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        text_signature: Option<&'a str>,
    },
    Image {
        data: &'a str,
        #[serde(rename = "mimeType")]
        mime_type: &'a str,
    },
}

fn to_js_content_parts(parts: &[ContentPart]) -> Vec<JsContentPartOut<'_>> {
    parts
        .iter()
        .map(|p| match p {
            ContentPart::Text(t) => JsContentPartOut::Text {
                text: &t.text,
                text_signature: t.text_signature.as_deref(),
            },
            ContentPart::Image(img) => JsContentPartOut::Image {
                data: &img.data,
                mime_type: &img.mime_type,
            },
        })
        .collect()
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JsAfterToolCallPayload<'a> {
    pub tool_call: &'a ToolCall,
    pub args: &'a Value,
    pub result: JsAgentToolResultOut<'a>,
    pub is_error: bool,
}

impl<'a> From<&'a AfterToolCallCtx<'a>> for JsAfterToolCallPayload<'a> {
    fn from(ctx: &'a AfterToolCallCtx<'a>) -> Self {
        Self {
            tool_call: ctx.tool_call,
            args: ctx.args,
            result: JsAgentToolResultOut {
                content: to_js_content_parts(&ctx.result.content),
                details: &ctx.result.details,
                structured_content: ctx.result.structured_content.as_ref(),
                is_error: ctx.result.is_error,
                terminate: ctx.result.terminate,
            },
            is_error: ctx.result.is_error,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum JsContentInput {
    Text(String),
    Parts(Vec<JsContentPartIn>),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum JsContentPartIn {
    PlainText(String),
    Part {
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        text: Option<String>,
        #[serde(default, alias = "textSignature")]
        text_signature: Option<String>,
        #[serde(default)]
        data: Option<String>,
        #[serde(default, alias = "mimeType")]
        mime_type: Option<String>,
    },
}

impl JsContentInput {
    fn into_content_parts(self) -> Result<Vec<ContentPart>, JsExtensionError> {
        match self {
            Self::Text(s) => Ok(vec![ContentPart::Text(TextContent::plain(s))]),
            Self::Parts(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    match item {
                        JsContentPartIn::PlainText(s) => {
                            out.push(ContentPart::Text(TextContent::plain(s)));
                        }
                        JsContentPartIn::Part {
                            kind,
                            text,
                            text_signature,
                            data,
                            mime_type,
                        } => match kind.as_str() {
                            "text" => out.push(ContentPart::Text(TextContent {
                                text: text.unwrap_or_default(),
                                text_signature,
                            })),
                            "image" => {
                                let data = data.ok_or_else(|| {
                                    JsExtensionError::Serialization(
                                        "image content part missing 'data'".to_string(),
                                    )
                                })?;
                                let mime_type = mime_type.ok_or_else(|| {
                                    JsExtensionError::Serialization(
                                        "image content part missing 'mimeType'".to_string(),
                                    )
                                })?;
                                out.push(ContentPart::Image(ImageContent { data, mime_type }));
                            }
                            other => {
                                return Err(JsExtensionError::Serialization(format!(
                                    "unsupported content part type '{other}'"
                                )));
                            }
                        },
                    }
                }
                Ok(out)
            }
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JsAfterToolCallResult {
    #[serde(default)]
    content: Option<JsContentInput>,
    #[serde(default)]
    details: Option<Value>,
    #[serde(default, alias = "structured_content")]
    structured_content: Option<Value>,
    #[serde(default, alias = "is_error")]
    is_error: Option<bool>,
    #[serde(default)]
    terminate: Option<bool>,
}

pub(crate) fn parse_after_tool_call_result(
    json: &str,
) -> Result<Option<AfterToolCallResult>, JsExtensionError> {
    let raw: JsAfterToolCallResult = serde_json::from_str(json).map_err(|err| {
        JsExtensionError::Serialization(format!("invalid after_tool_call return value: {err}"))
    })?;
    let content = match raw.content {
        Some(c) => Some(c.into_content_parts()?),
        None => None,
    };
    if content.is_none()
        && raw.details.is_none()
        && raw.structured_content.is_none()
        && raw.is_error.is_none()
        && raw.terminate.is_none()
    {
        return Ok(None);
    }
    Ok(Some(AfterToolCallResult {
        content,
        details: raw.details,
        structured_content: raw.structured_content,
        is_error: raw.is_error,
        terminate: raw.terminate,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JsToolExecuteResult {
    #[serde(default)]
    content: Option<JsContentInput>,
    #[serde(default)]
    details: Value,
    #[serde(default, alias = "structured_content")]
    structured_content: Option<Value>,
    #[serde(default, alias = "is_error")]
    is_error: bool,
    #[serde(default)]
    terminate: bool,
}

pub(crate) fn parse_tool_execute_result(json: &str) -> Result<AgentToolResult, JsExtensionError> {
    let raw: JsToolExecuteResult = serde_json::from_str(json).map_err(|err| {
        JsExtensionError::Serialization(format!("invalid tool execute return value: {err}"))
    })?;
    let content = match raw.content {
        Some(c) => c.into_content_parts()?,
        None => Vec::new(),
    };
    Ok(AgentToolResult {
        content,
        details: raw.details,
        structured_content: raw.structured_content,
        is_error: raw.is_error,
        terminate: raw.terminate,
    })
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JsToolResultMessageOut<'a> {
    pub tool_call_id: &'a str,
    pub tool_name: &'a str,
    pub content: Vec<JsContentPartOut<'a>>,
    pub is_error: bool,
    pub timestamp: u64,
}

impl<'a> From<&'a ToolResultMessage> for JsToolResultMessageOut<'a> {
    fn from(msg: &'a ToolResultMessage) -> Self {
        Self {
            tool_call_id: &msg.tool_call_id,
            tool_name: &msg.tool_name,
            content: to_js_content_parts(&msg.content),
            is_error: msg.is_error,
            timestamp: msg.timestamp,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JsFinishTurnPayload<'a> {
    pub message: &'a AssistantMessage,
    pub tool_results: Vec<JsToolResultMessageOut<'a>>,
}

impl<'a> From<&'a FinishTurnCtx<'a>> for JsFinishTurnPayload<'a> {
    fn from(ctx: &'a FinishTurnCtx<'a>) -> Self {
        Self {
            message: ctx.assistant_message,
            tool_results: ctx.tool_results.iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum JsFinishTurnResult {
    ActionObject { action: Option<String> },
    ActionString(String),
}

pub(crate) fn parse_finish_turn_result(
    json: &str,
) -> Result<Option<TurnDecision>, JsExtensionError> {
    let raw: JsFinishTurnResult = serde_json::from_str(json).map_err(|err| {
        JsExtensionError::Serialization(format!("invalid finish_turn return value: {err}"))
    })?;
    let action = match raw {
        JsFinishTurnResult::ActionObject { action } => action,
        JsFinishTurnResult::ActionString(s) => Some(s),
    };
    match action.as_deref() {
        None => Ok(None),
        Some("continue") => Ok(Some(TurnDecision::Continue)),
        Some("end") => Ok(Some(TurnDecision::End)),
        Some(other) => Err(JsExtensionError::Serialization(format!(
            "unsupported finish_turn action '{other}' (expected 'continue' or 'end')"
        ))),
    }
}
