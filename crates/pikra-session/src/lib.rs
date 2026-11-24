//! Append-only session entry tree, branching, deduplicated context blocks, and compaction.

mod compaction;
mod error;
mod manager;
mod store;
mod types;

pub use compaction::{
    CompactionConfig, CompactionInput, CompactionOutput, CompactionResult, CompactionStrategy,
    ContextTokenEstimate, ContextUsage, DEFAULT_KEEP_RECENT_TOKENS, DEFAULT_MAX_SUMMARY_CHARS,
    DefaultCompactionStrategy, IMAGE_TOKEN_ESTIMATE, MAX_AUTO_RESERVE_TOKENS,
    MIN_AUTO_RESERVE_TOKENS, cap_summary_length, compaction_threshold, effective_reserve_tokens,
    estimate_agent_message_tokens, estimate_content_parts_tokens, estimate_context_tokens,
    estimate_message_tokens, should_compact,
};
pub use error::SessionError;
pub use manager::{PreparedCompaction, SessionConfig, SessionManager, SessionSubscriptionId};
pub use store::{JsonlSessionStore, MemorySessionStore, SessionStore};
pub use types::{
    CURRENT_SESSION_VERSION, CompactionCheckpoint, ReconstructedContext, SessionEntry,
    SessionEntryPayload, SessionEvent, SessionHeader, SessionTreeNode, baseline_to_system_message,
    compaction_summary_to_agent_message, context_block_to_agent_message, format_context_block,
    prepare_flushed_prompt,
};
