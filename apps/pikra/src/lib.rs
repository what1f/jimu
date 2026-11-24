//! Unified Pikra CLI: JSONL RPC server, ACP v1 server, one-shot `ask`, and optional interactive TUI.

pub mod acp;
pub mod prompt;
pub mod tools;
#[cfg(feature = "tui")]
pub mod tui;
