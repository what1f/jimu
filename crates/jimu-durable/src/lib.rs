//! Embedded single-node transactional durable execution engine for Jimu.
//!
//! Provides SQLite WAL atomic storage, crash-safe turn recovery, idempotent submissions,
//! side-effect tool ledgers, external signals, durable timers, prefix-preserving compaction,
//! and optimistic CAS state documents.

mod clock;
mod engine;
mod error;
mod session_bridge;
mod store;
mod tool;
mod types;

pub use clock::{Clock, ManualClock, RetryPolicy, SystemClock};
pub use engine::{DEFAULT_LEASE_TTL_MS, DurableEngine, DurableEngineConfig, DurableSubscriptionId};
pub use error::DurableError;
pub use store::{DurableStore, MemoryDurableStore, SqliteDurableStore};
pub use tool::{
    AgentToolAdapter, DurableTool, DurableToolContext, DurableToolOutcome, SignalResolution,
};
pub use types::{
    CheckpointMutation, CommitBatch, CommitReceipt, CompletedToolRecord, ConversationRecord,
    ConversationStatus, DocumentRecord, DocumentWrite, DurableEvent, LeaseClaimResult, ParkState,
    ParkedToolCall, RunOutcome, SignalDeliveryResult, SignalRecord, SubmissionRecord,
    SubmissionState, TaskCheckpoint,
};
