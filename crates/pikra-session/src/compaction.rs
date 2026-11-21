use crate::error::SessionError;
use crate::types::{SessionEntry, SessionEntryPayload, now_ms};
use pikra_agent::AgentMessage;
use pikra_ai::{
    AssistantBlock, BoxFuture, ContentPart, Message, Model, SimpleStreamOptions, StopReason,
    StreamProvider, TranscriptContext, Usage, UserMessage, complete,
};

/// Fixed token estimate per image content part (excludes raw base64 bytes).
pub const IMAGE_TOKEN_ESTIMATE: u64 = 1_200;

/// Default maximum character length for a persisted compaction summary.
pub const DEFAULT_MAX_SUMMARY_CHARS: usize = 24_000;

/// Default minimum output reserve when deriving from `model.max_output_tokens`.
pub const MIN_AUTO_RESERVE_TOKENS: u32 = 16_384;

/// Default maximum output reserve when deriving from `model.max_output_tokens`.
pub const MAX_AUTO_RESERVE_TOKENS: u32 = 20_000;

/// Default recent token budget kept verbatim across compaction boundaries.
pub const DEFAULT_KEEP_RECENT_TOKENS: u64 = 20_000;

const INITIAL_SUMMARIZATION_PROMPT: &str = "\
STOP and do NOT call any tools. Respond with plain text only.

Create a concise, structured checkpoint summary of the conversation above so another assistant turn can seamlessly continue the work.

Use this exact structure:
## Goal
[What the user is trying to accomplish]

## Constraints & Preferences
- [Requirements or preferences mentioned, or \"(none)\"]

## Progress
### Done
- [Completed tasks/changes]
### In Progress
- [Ongoing work]

## Key Decisions
- [Important decisions and rationale]

## Next Steps
1. [Immediate next actions]

## Critical Context
- [Exact file paths, identifiers, error messages, or values needed to continue]

Keep each section concise and factual.";

const UPDATE_SUMMARIZATION_PROMPT: &str = "\
STOP and do NOT call any tools. Respond with plain text only.

Update and fold the existing <compaction-summary> from earlier in the conversation together with all new messages since then into ONE single, self-contained checkpoint summary. Do not repeat or stack multiple summaries.

Use this exact structure:
## Goal
[Preserve existing goals and include any new ones]

## Constraints & Preferences
- [Preserve relevant constraints and add any newly discovered ones]

## Progress
### Done
- [Previously completed and newly completed items]
### In Progress
- [Current work]

## Key Decisions
- [Consolidated decisions and rationale]

## Next Steps
1. [Updated next actions]

## Critical Context
- [Exact file paths, identifiers, error messages, or values needed to continue]

Keep each section concise and factual.";

/// Compaction policy knobs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionConfig {
    /// Whether auto-compaction is enabled.
    pub enabled: bool,
    /// Optional explicit output reserve tokens (e.g. `Some(33_000)`).
    /// When `None`, derived as `model.max_output_tokens.min(20_000).max(16_384)`.
    pub compaction_reserve_tokens: Option<u32>,
    /// Target token budget of recent turns to retain verbatim after the compaction checkpoint.
    pub keep_recent_tokens: u64,
    /// Upper bound on stored summary string length (further bounded by `model.context_window`).
    pub max_summary_chars: usize,
    /// Optional extra instructions appended to the summarization prompt.
    pub custom_instructions: Option<String>,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            compaction_reserve_tokens: None,
            keep_recent_tokens: DEFAULT_KEEP_RECENT_TOKENS,
            max_summary_chars: DEFAULT_MAX_SUMMARY_CHARS,
            custom_instructions: None,
        }
    }
}

/// Breakdown of estimated context token usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ContextTokenEstimate {
    /// Total estimated tokens for the next turn.
    pub total_tokens: u64,
    /// Prompt tokens reported by the latest valid assistant turn (`input + cache_read + cache_write`).
    pub reported_tokens: u64,
    /// Locally estimated tokens for messages appended after the latest valid assistant turn.
    pub trailing_tokens: u64,
    /// Index of the assistant message whose reported usage anchored this estimate, if any.
    pub last_usage_index: Option<usize>,
}

fn chars_to_tokens(chars: usize) -> u64 {
    (chars as u64).div_ceil(4)
}

/// Estimate tokens for a slice of [`ContentPart`]s, counting each image as a fixed
/// [`IMAGE_TOKEN_ESTIMATE`] (`1_200`) and excluding raw base64 `ImageContent.data`.
pub fn estimate_content_parts_tokens(parts: &[ContentPart]) -> u64 {
    let mut text_chars: usize = 0;
    let mut image_tokens: u64 = 0;
    for part in parts {
        match part {
            ContentPart::Text(t) => text_chars = text_chars.saturating_add(t.text.len()),
            ContentPart::Image(_) => {
                image_tokens = image_tokens.saturating_add(IMAGE_TOKEN_ESTIMATE)
            }
        }
    }
    chars_to_tokens(text_chars).saturating_add(image_tokens)
}

/// Estimate tokens for a single [`Message`] (`chars / 4`, excluding base64 image data).
pub fn estimate_message_tokens(msg: &Message) -> u64 {
    match msg {
        Message::System(sys) => {
            let mut chars = sys.content.len();
            for (_, sec) in &sys.sections {
                if let Some(text) = sec {
                    chars = chars.saturating_add(text.len());
                }
            }
            if !sys.tools_added.is_empty()
                && let Ok(serialized) = serde_json::to_string(&sys.tools_added)
            {
                chars = chars.saturating_add(serialized.len());
            }
            chars_to_tokens(chars)
        }
        Message::User(user) => estimate_content_parts_tokens(&user.content),
        Message::Assistant(assistant) => {
            let mut chars: usize = 0;
            for block in &assistant.content {
                match block {
                    AssistantBlock::Text(t) => chars = chars.saturating_add(t.text.len()),
                    AssistantBlock::Thinking(th) => chars = chars.saturating_add(th.thinking.len()),
                    AssistantBlock::ToolCall(tc) => {
                        chars = chars.saturating_add(tc.name.len());
                        if let Ok(args_json) = serde_json::to_string(&tc.arguments) {
                            chars = chars.saturating_add(args_json.len());
                        }
                    }
                }
            }
            chars_to_tokens(chars)
        }
        Message::ToolResult(tr) => estimate_content_parts_tokens(&tr.content),
    }
}

/// Estimate tokens for an [`AgentMessage`].
pub fn estimate_agent_message_tokens(msg: &AgentMessage) -> u64 {
    match msg {
        AgentMessage::Llm(m) => estimate_message_tokens(m),
        AgentMessage::Custom {
            content,
            include_in_llm: true,
            ..
        } => estimate_content_parts_tokens(content),
        AgentMessage::Custom {
            include_in_llm: false,
            ..
        } => 0,
    }
}

/// Estimate context tokens for a reconstructed message list.
///
/// Prefers the last valid assistant turn's reported `usage.input + usage.cache_read + usage.cache_write`
/// plus that assistant message's output tokens and locally estimated tokens of messages appended
/// after that assistant turn.
pub fn estimate_context_tokens(messages: &[AgentMessage]) -> ContextTokenEstimate {
    for (idx, msg) in messages.iter().enumerate().rev() {
        if let AgentMessage::Llm(Message::Assistant(assistant)) = msg
            && assistant.stop_reason != StopReason::Aborted
            && assistant.stop_reason != StopReason::Error
        {
            let prompt_sum = assistant
                .usage
                .input
                .saturating_add(assistant.usage.cache_read)
                .saturating_add(assistant.usage.cache_write);
            let (reported, assistant_output) = if prompt_sum > 0 {
                let out = if assistant.usage.output > 0 {
                    assistant.usage.output
                } else {
                    estimate_agent_message_tokens(msg)
                };
                (prompt_sum, out)
            } else if assistant.usage.total_tokens > 0 {
                // `total_tokens` already encompasses both prompt and output tokens when prompt
                // fields were not broken out separately.
                (assistant.usage.total_tokens, 0)
            } else {
                (0, 0)
            };
            if reported > 0 {
                let trailing: u64 = messages[idx + 1..]
                    .iter()
                    .map(estimate_agent_message_tokens)
                    .fold(assistant_output, u64::saturating_add);
                return ContextTokenEstimate {
                    total_tokens: reported.saturating_add(trailing),
                    reported_tokens: reported,
                    trailing_tokens: trailing,
                    last_usage_index: Some(idx),
                };
            }
        }
    }

    let estimated: u64 = messages
        .iter()
        .map(estimate_agent_message_tokens)
        .fold(0u64, u64::saturating_add);
    ContextTokenEstimate {
        total_tokens: estimated,
        reported_tokens: 0,
        trailing_tokens: estimated,
        last_usage_index: None,
    }
}

/// Compute the effective output reserve tokens for a model and compaction config.
pub fn effective_reserve_tokens(model: &Model, config: &CompactionConfig) -> u32 {
    config.compaction_reserve_tokens.unwrap_or_else(|| {
        model
            .max_output_tokens
            .clamp(MIN_AUTO_RESERVE_TOKENS, MAX_AUTO_RESERVE_TOKENS)
    })
}

/// Compute the auto-compaction trigger threshold:
/// `max(context_window.saturating_sub(reserve), context_window / 2)`.
pub fn compaction_threshold(context_window: u32, reserve_tokens: u32) -> u64 {
    let cw = u64::from(context_window);
    let res = u64::from(reserve_tokens);
    cw.saturating_sub(res).max(cw / 2)
}

/// Return `true` when estimated context tokens meet or exceed the compaction threshold.
pub fn should_compact(estimated_tokens: u64, model: &Model, config: &CompactionConfig) -> bool {
    if !config.enabled || model.context_window == 0 {
        return false;
    }
    let reserve = effective_reserve_tokens(model, config);
    let threshold = compaction_threshold(model.context_window, reserve);
    estimated_tokens >= threshold
}

/// Cap a summary string to `min(max_summary_chars, context_window as usize)` on a valid UTF-8 boundary.
pub fn cap_summary_length(summary: &str, max_summary_chars: usize, context_window: u32) -> String {
    let limit = max_summary_chars.min(context_window as usize);
    if summary.len() <= limit {
        return summary.to_string();
    }
    if limit == 0 {
        return String::new();
    }
    let suffix = "\n...[summary truncated]";
    if limit <= suffix.len() {
        let mut end = limit;
        while end > 0 && !summary.is_char_boundary(end) {
            end -= 1;
        }
        return summary[..end].to_string();
    }
    let mut end = limit - suffix.len();
    while end > 0 && !summary.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(limit);
    out.push_str(&summary[..end]);
    out.push_str(suffix);
    out
}

/// Select `first_kept_entry_id` from active branch entries after the latest compaction.
///
/// Only cuts at turn-start boundaries (user messages) so tool calls and their tool results are
/// never orphaned. Returns an empty string when no pre-compaction entries should be kept.
pub(crate) fn select_first_kept_entry_id(
    branch_entries: &[SessionEntry],
    keep_recent_tokens: u64,
) -> String {
    let start_idx = branch_entries
        .iter()
        .rposition(|e| matches!(e.payload, SessionEntryPayload::Compaction { .. }))
        .map(|i| i + 1)
        .unwrap_or(0);

    let slice = &branch_entries[start_idx..];
    if slice.is_empty() || keep_recent_tokens == 0 {
        return String::new();
    }

    // Collect indices (relative to `slice`) that are clean turn starts (User message)
    // and that actually drop at least one earlier message entry.
    let mut seen_message = false;
    let mut cut_candidates: Vec<usize> = Vec::new();
    for (rel_idx, entry) in slice.iter().enumerate() {
        let is_user_msg = matches!(
            &entry.payload,
            SessionEntryPayload::Message {
                message: AgentMessage::Llm(Message::User(_))
            }
        );
        if is_user_msg && seen_message {
            cut_candidates.push(rel_idx);
        }
        if matches!(
            &entry.payload,
            SessionEntryPayload::Message { .. } | SessionEntryPayload::ContextBlock { .. }
        ) {
            seen_message = true;
        }
    }

    if cut_candidates.is_empty() {
        return String::new();
    }

    // Walk backwards accumulating tokens until `keep_recent_tokens` is reached.
    let mut accumulated: u64 = 0;
    let mut chosen_rel: Option<usize> = None;
    let mut exceeded = false;
    for rel_idx in (0..slice.len()).rev() {
        let entry_tokens = match &slice[rel_idx].payload {
            SessionEntryPayload::Message { message } => estimate_agent_message_tokens(message),
            SessionEntryPayload::ContextBlock { name, text, .. } => {
                chars_to_tokens(name.len() + text.len() + 40)
            }
            _ => 0,
        };
        accumulated = accumulated.saturating_add(entry_tokens);
        if accumulated > keep_recent_tokens {
            exceeded = true;
            // Including `rel_idx` exceeds `keep_recent_tokens`, so pick the earliest valid
            // turn-start cut candidate strictly after `rel_idx`.
            chosen_rel = cut_candidates.iter().copied().find(|&cand| cand > rel_idx);
            break;
        }
        if accumulated == keep_recent_tokens {
            exceeded = true;
            // Including `rel_idx` fits exactly within `keep_recent_tokens`.
            chosen_rel = cut_candidates.iter().copied().find(|&cand| cand >= rel_idx);
            break;
        }
    }

    if exceeded {
        return match chosen_rel {
            Some(rel) => slice[rel].id.clone(),
            None => String::new(),
        };
    }

    // If the whole slice fit inside `keep_recent_tokens`, still compact at least up to the last
    // turn boundary if available, otherwise keep nothing so compaction always makes progress.
    let rel = *cut_candidates.last().unwrap();
    slice[rel].id.clone()
}

/// Input passed to a [`CompactionStrategy`].
#[derive(Debug, Clone)]
pub struct CompactionInput {
    pub model: Model,
    /// Exact current `TranscriptContext` prefix (exact `SystemBaseline` + resident tools + messages).
    pub transcript: TranscriptContext,
    /// Active branch entries (root -> leaf) prior to compaction.
    pub branch_entries: Vec<SessionEntry>,
    /// Previous compaction summary on the active branch, if any (for folding).
    pub previous_summary: Option<String>,
    /// Suggested `first_kept_entry_id` computed from `keep_recent_tokens`.
    pub first_kept_entry_id: String,
    /// Estimated context tokens before compaction.
    pub tokens_before: u64,
    pub config: CompactionConfig,
    pub options: SimpleStreamOptions,
}

/// Output produced by a [`CompactionStrategy`].
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionOutput {
    pub summary: String,
    pub first_kept_entry_id: String,
    pub usage: Option<Usage>,
}

/// Persisted result returned after committing a compaction entry to [`crate::SessionManager`].
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionResult {
    pub entry_id: String,
    pub summary: String,
    pub first_kept_entry_id: String,
    pub tokens_before: u64,
    pub usage: Option<Usage>,
}

/// Pluggable strategy trait allowing hosts/extensions to override compaction behavior completely.
pub trait CompactionStrategy: Send + Sync {
    fn compact<'a>(
        &'a self,
        provider: &'a dyn StreamProvider,
        input: CompactionInput,
    ) -> BoxFuture<'a, Result<CompactionOutput, SessionError>>;
}

/// Default prefix-cache-preserving compaction strategy.
///
/// Reuses the exact current `TranscriptContext` prefix (system prompt + resident tools + prior
/// messages) and appends one trailing summarization `UserMessage`, enabling 100% prompt cache hits.
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultCompactionStrategy;

impl CompactionStrategy for DefaultCompactionStrategy {
    fn compact<'a>(
        &'a self,
        provider: &'a dyn StreamProvider,
        input: CompactionInput,
    ) -> BoxFuture<'a, Result<CompactionOutput, SessionError>> {
        Box::pin(async move {
            let mut prompt = if input.previous_summary.is_some() {
                UPDATE_SUMMARIZATION_PROMPT.to_string()
            } else {
                INITIAL_SUMMARIZATION_PROMPT.to_string()
            };
            if let Some(extra) = input
                .config
                .custom_instructions
                .as_deref()
                .filter(|s| !s.trim().is_empty())
            {
                prompt.push_str("\n\nAdditional instructions:\n");
                prompt.push_str(extra);
            }

            // Preserve exact leading SystemMessage, resident tools, and all prior messages byte-for-byte.
            let mut ctx = input.transcript;
            ctx.messages
                .push(Message::User(UserMessage::text(prompt, now_ms())));

            let response = complete(provider, &input.model, &ctx, input.options).await;

            match response.stop_reason {
                StopReason::Error => {
                    return Err(SessionError::CompactionFailed(
                        response
                            .error_message
                            .unwrap_or_else(|| "provider returned error".into()),
                    ));
                }
                StopReason::Aborted => {
                    return Err(SessionError::CompactionFailed("compaction aborted".into()));
                }
                StopReason::Length => {
                    return Err(SessionError::CompactionFailed(
                        "compaction summary truncated by token limit".into(),
                    ));
                }
                _ => {}
            }

            let raw_summary = response.text_output().trim().to_string();
            if raw_summary.is_empty() {
                return Err(SessionError::CompactionFailed(
                    "compaction produced empty summary".into(),
                ));
            }

            let capped = cap_summary_length(
                &raw_summary,
                input.config.max_summary_chars,
                input.model.context_window,
            );

            Ok(CompactionOutput {
                summary: capped,
                first_kept_entry_id: input.first_kept_entry_id,
                usage: Some(response.usage),
            })
        })
    }
}
