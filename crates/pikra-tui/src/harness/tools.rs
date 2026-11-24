//! Presentation-only tool renderer registry, decoupled from any tool execution crate.

use super::InputImage;
use crate::engine::theme::Theme;
use ratatui::text::Line;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// Framework-agnostic tool output payload passed to tool renderers.
#[derive(Debug, Clone, Default)]
pub struct ToolOutput {
    pub text: String,
    pub images: Vec<InputImage>,
    pub structured: Option<Value>,
    pub details: Value,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Default::default()
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
            ..Default::default()
        }
    }
}

/// Borrowed view passed to a [`ToolRenderer`].
#[derive(Clone, Copy)]
pub struct ToolView<'a> {
    /// Effective tool name (e.g. delegated inner tool name for `call_tool`).
    pub name: &'a str,
    /// Effective JSON arguments for the tool call.
    pub args: &'a Value,
    /// Active quantized theme.
    pub theme: &'a Theme,
    /// Available content width in terminal cells.
    pub width: u16,
}

/// Customizes how a specific tool is rendered in the scrollback without linking its execution code.
pub trait ToolRenderer: Send + Sync {
    /// Render the call header and any pre-execution preview (such as a diff).
    fn render_call(&self, call: ToolView<'_>) -> Vec<Line<'static>>;

    /// Render incremental streaming output while the tool is running.
    fn render_update(&self, call: ToolView<'_>, partial: &ToolOutput) -> Vec<Line<'static>> {
        self.render_result(call, partial)
    }

    /// Render the final tool result body.
    fn render_result(&self, call: ToolView<'_>, result: &ToolOutput) -> Vec<Line<'static>>;
}

/// Registry mapping tool names to custom [`ToolRenderer`] overrides.
///
/// Built-in rich blocks (read, write, edit, apply_patch, exec_command, grep, find, web_fetch,
/// monitor, generic fallback) are used whenever no custom override matches.
#[derive(Default, Clone)]
pub struct ToolRendererRegistry {
    overrides: HashMap<String, Arc<dyn ToolRenderer>>,
}

impl ToolRendererRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install or replace a custom renderer for an effective tool name.
    pub fn register(&mut self, name: impl Into<String>, renderer: Arc<dyn ToolRenderer>) {
        self.overrides.insert(name.into(), renderer);
    }

    /// Look up a custom renderer override by effective tool name.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn ToolRenderer>> {
        self.overrides.get(name)
    }
}
