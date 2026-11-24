//! Pure terminal rendering engine adapted from Grok Build.
//!
//! This module contains zero agent, session, or harness concepts and can be used standalone
//! by any terminal application.

pub mod clipboard;
pub mod diff;
pub mod host;
pub mod inline;
pub mod input;
pub mod location_path;
pub mod markdown;
pub mod modal_window_state;
pub mod render;
pub mod search;
pub mod terminal;
pub mod textarea;
pub mod theme;
pub mod tty;
pub mod util;
pub mod x10_filter;

#[cfg(feature = "mermaid")]
pub mod mermaid;
