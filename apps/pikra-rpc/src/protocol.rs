use pikra_agent::QueueMode;
use pikra_ai::{ApiKind, ImageContent, Model, ThinkingLevel, Usage};
use pikra_session::CompactionResult;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum StreamingBehavior {
    #[serde(rename = "steer")]
    Steer,
    #[serde(rename = "followUp", alias = "follow_up")]
    FollowUp,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RpcImageInput {
    pub data: String,
    #[serde(alias = "mimeType")]
    pub mime_type: String,
}

impl From<RpcImageInput> for ImageContent {
    fn from(value: RpcImageInput) -> Self {
        Self {
            data: value.data,
            mime_type: value.mime_type,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RpcCommand {
    Prompt {
        message: String,
        #[serde(default)]
        images: Option<Vec<RpcImageInput>>,
        #[serde(default, alias = "streamingBehavior")]
        streaming_behavior: Option<StreamingBehavior>,
    },
    Steer {
        message: String,
        #[serde(default)]
        images: Option<Vec<RpcImageInput>>,
    },
    FollowUp {
        message: String,
        #[serde(default)]
        images: Option<Vec<RpcImageInput>>,
    },
    Abort,
    ClearQueue,
    GetState,
    SetModel {
        provider: String,
        #[serde(alias = "modelId")]
        model_id: String,
        #[serde(default)]
        api: Option<ApiKind>,
        #[serde(default, alias = "baseUrl")]
        base_url: Option<String>,
        #[serde(default, alias = "contextWindow")]
        context_window: Option<u32>,
        #[serde(default, alias = "maxOutputTokens")]
        max_output_tokens: Option<u32>,
        #[serde(default, alias = "supportsImages")]
        supports_images: Option<bool>,
        #[serde(default, alias = "supportsReasoning")]
        supports_reasoning: Option<bool>,
    },
    GetAvailableModels,
    SetThinkingLevel {
        level: ThinkingLevel,
    },
    SetFast {
        #[serde(alias = "enabled")]
        fast: bool,
    },
    SetContext {
        name: String,
        text: String,
    },
    Compact {
        #[serde(default, alias = "customInstructions")]
        custom_instructions: Option<String>,
    },
    SetAutoCompaction {
        enabled: bool,
    },
    SetSteeringMode {
        mode: QueueMode,
    },
    SetFollowUpMode {
        mode: QueueMode,
    },
    NewSession {
        #[serde(default, alias = "sessionPath")]
        session_path: Option<String>,
        #[serde(default, alias = "parentSession")]
        parent_session: Option<String>,
    },
    SwitchSession {
        #[serde(alias = "sessionPath")]
        session_path: String,
    },
    Fork {
        #[serde(alias = "entryId")]
        entry_id: String,
    },
    GetEntries {
        #[serde(default)]
        since: Option<String>,
    },
    GetTree,
    GetMessages,
    GetLastAssistantText,
    ReloadExtensions,
}

impl RpcCommand {
    pub const fn command_name(&self) -> &'static str {
        match self {
            Self::Prompt { .. } => "prompt",
            Self::Steer { .. } => "steer",
            Self::FollowUp { .. } => "follow_up",
            Self::Abort => "abort",
            Self::ClearQueue => "clear_queue",
            Self::GetState => "get_state",
            Self::SetModel { .. } => "set_model",
            Self::GetAvailableModels => "get_available_models",
            Self::SetThinkingLevel { .. } => "set_thinking_level",
            Self::SetFast { .. } => "set_fast",
            Self::SetContext { .. } => "set_context",
            Self::Compact { .. } => "compact",
            Self::SetAutoCompaction { .. } => "set_auto_compaction",
            Self::SetSteeringMode { .. } => "set_steering_mode",
            Self::SetFollowUpMode { .. } => "set_follow_up_mode",
            Self::NewSession { .. } => "new_session",
            Self::SwitchSession { .. } => "switch_session",
            Self::Fork { .. } => "fork",
            Self::GetEntries { .. } => "get_entries",
            Self::GetTree => "get_tree",
            Self::GetMessages => "get_messages",
            Self::GetLastAssistantText => "get_last_assistant_text",
            Self::ReloadExtensions => "reload_extensions",
        }
    }
}

pub struct ParsedCommand {
    pub id: Option<String>,
    pub command: RpcCommand,
}

pub fn parse_command_line(line: &str) -> Result<ParsedCommand, Box<RpcResponse>> {
    let raw_val: Value = serde_json::from_str(line).map_err(|err| {
        Box::new(RpcResponse::error(
            None,
            "parse",
            format!("Failed to parse command: {err}"),
        ))
    })?;

    let Some(obj) = raw_val.as_object() else {
        return Err(Box::new(RpcResponse::error(
            None,
            "parse",
            "Command record must be a JSON object",
        )));
    };

    let id = match obj.get("id") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => {
            return Err(Box::new(RpcResponse::error(
                None,
                "parse",
                format!("Command field `id` must be a string, got {other}"),
            )));
        }
    };

    let cmd_name = obj
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("parse")
        .to_string();

    let command: RpcCommand = serde_json::from_value(raw_val).map_err(|err| {
        Box::new(RpcResponse::error(
            id.clone(),
            cmd_name,
            format!("Invalid command payload: {err}"),
        ))
    })?;

    Ok(ParsedCommand { id, command })
}

#[derive(Debug, Clone, Serialize)]
pub struct RpcResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub record_type: &'static str,
    pub command: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl RpcResponse {
    pub fn ok(id: Option<String>, command: impl Into<String>, data: Option<Value>) -> Self {
        Self {
            id,
            record_type: "response",
            command: command.into(),
            success: true,
            data,
            error: None,
        }
    }

    pub fn error(id: Option<String>, command: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            id,
            record_type: "response",
            command: command.into(),
            success: false,
            data: None,
            error: Some(error.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RpcStateData {
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub fast: bool,
    pub is_streaming: bool,
    pub is_compacting: bool,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
    pub session_id: String,
    pub leaf_id: Option<String>,
    pub cwd: String,
    pub auto_compaction_enabled: bool,
    pub message_count: usize,
    pub pending_message_count: usize,
    pub tools: Vec<String>,
    pub extensions: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompactionResponseData {
    pub compacted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_kept_entry_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_before: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

impl From<Option<CompactionResult>> for CompactionResponseData {
    fn from(res: Option<CompactionResult>) -> Self {
        match res {
            Some(r) => Self {
                compacted: true,
                entry_id: Some(r.entry_id),
                summary: Some(r.summary),
                first_kept_entry_id: Some(r.first_kept_entry_id),
                tokens_before: Some(r.tokens_before),
                usage: r.usage,
            },
            None => Self {
                compacted: false,
                entry_id: None,
                summary: None,
                first_kept_entry_id: None,
                tokens_before: None,
                usage: None,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentSettledEvent {
    #[serde(rename = "type")]
    pub record_type: &'static str,
    pub aborted: bool,
}

impl AgentSettledEvent {
    pub fn new(aborted: bool) -> Self {
        Self {
            record_type: "agent_settled",
            aborted,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ExtensionErrorEvent {
    #[serde(rename = "type")]
    pub record_type: &'static str,
    pub error: String,
}

impl ExtensionErrorEvent {
    pub fn new(error: impl Into<String>) -> Self {
        Self {
            record_type: "extension_error",
            error: error.into(),
        }
    }
}
