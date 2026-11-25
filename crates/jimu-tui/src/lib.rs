//! Harness-agnostic inline and fullscreen terminal UI.
//!
//! Architecture layers (strict one-way dependency):
//! 1. [`engine`] — Pure terminal rendering primitives, themes, streaming Markdown, multiline
//!    textarea, inline scrollback, diff hunks, VTE output, and optional media helpers. Zero
//!    agent or harness concepts.
//! 2. [`harness`] — Framework-agnostic [`harness::TuiHarness`] contract, neutral events,
//!    optional capabilities, tool-card registry, slash commands, and chrome slots. Zero
//!    `jimu-*` dependencies.
//! 3. [`app`] — Conversation scrollback engine, blocks, modals/views, dual-screen host, and
//!    event loop driving any [`harness::TuiHarness`].
//! 4. `jimu` (`#[cfg(feature = "jimu")]`) — Native in-process adapter connecting
//!    `jimu-kit` (`KitSession`, `AgentMiddleware`, and optional background task managers).
#![cfg(not(target_os = "ios"))]

pub mod app;
pub mod engine;
pub mod harness;
#[cfg(feature = "jimu")]
pub mod jimu;
pub mod ui;
