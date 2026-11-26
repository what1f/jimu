//! Optional capabilities that a [`super::TuiHarness`] may expose to unlock richer UI panels
//! and built-in slash commands. Capabilities left as `None` stay completely hidden.

use super::BoxFuture;
use serde::{Deserialize, Serialize};

/// Snapshot of context-window utilization for the status bar and `/context` block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextMeterSnapshot {
    /// Authoritative token count (provider usage + trailing estimate), or `None` when unknown
    /// (for example immediately after compaction before the next assistant reply).
    pub tokens: Option<u64>,
    /// Local heuristic token estimate over the current context.
    pub estimated_tokens: u64,
    /// Model context window in tokens.
    pub context_window: u32,
    /// Utilization percentage (`0.0..=100.0+`) when `tokens` is known.
    pub percent: Option<f64>,
    /// Auto-compaction threshold in tokens, when enabled.
    pub compaction_threshold: Option<u64>,
}

/// Summary of one pending steering or follow-up message in the harness queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedItem {
    pub id: String,
    pub is_steering: bool,
    pub text: String,
}

/// Selectable model entry for the `/model` picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOption {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub supports_thinking: bool,
}

/// Selectable permission mode entry for `/permissions` and `Shift+Tab` cycling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionModeOption {
    pub id: String,
    pub description: String,
}

/// Selectable historical session entry for the `/resume` picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOption {
    pub id: String,
    pub title: String,
    pub updated_at: Option<String>,
    pub message_count: Option<usize>,
}

/// Running background shell or monitor process surfaced in the bottom status strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundTask {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub command: String,
}

/// Discovered skill summary for `/skills`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
}

/// Connected MCP server summary for `/mcp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerSummary {
    pub name: String,
    pub status: String,
    pub tool_count: usize,
}

/// Tree-aware checkpoint entry for `/rewind` (`/tree`) and double-`Esc`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindPoint {
    pub id: String,
    pub parent_id: Option<String>,
    /// Entry kind/role, e.g. `"user"`, `"assistant"`, `"compaction"`.
    pub role: String,
    pub label: String,
    pub timestamp: Option<String>,
    /// Tree depth (`0` for root-level visible turns).
    pub depth: usize,
    /// Pre-formatted tree prefix when the session contains branches (empty for linear sessions).
    pub tree_prefix: String,
    /// True if this node lies on the currently active root-to-leaf path.
    pub is_active_branch: bool,
    /// True if this node has no visible children.
    pub is_leaf: bool,
    /// True if this node is the current active leaf.
    pub is_current_leaf: bool,
    /// Optional user prompt text to refill into the composer when rewinding before this user turn.
    pub prompt_text: Option<String>,
}

/// Context-window utilization readout capability.
pub trait ContextMeterCap: Send + Sync {
    fn context_usage(&self) -> Option<ContextMeterSnapshot>;
}

/// Pending input queue inspection and removal capability.
pub trait QueueCap: Send + Sync {
    fn pending_inputs(&self) -> Vec<QueuedItem>;
    fn remove_queued(&self, id: &str) -> bool;

    /// Remove and return the full queued input (preserving attachments when supported).
    fn take_queued(&self, id: &str) -> Option<super::UserInput> {
        let text = self
            .pending_inputs()
            .into_iter()
            .find(|item| item.id == id)?
            .text;
        self.remove_queued(id).then(|| super::UserInput::text(text))
    }
}

/// Conversation compaction capability (`/compact`).
pub trait CompactionCap: Send + Sync {
    /// Returns `Ok(true)` if compacted, `Ok(false)` if already short.
    fn compact(&self) -> BoxFuture<'_, Result<bool, String>>;
}

/// Model & thinking-level switching capability (`/model`, `/effort`).
pub trait ModelPickerCap: Send + Sync {
    fn current_model(&self) -> ModelOption;
    fn available_models(&self) -> Vec<ModelOption>;
    fn set_model(&self, model_id: &str) -> BoxFuture<'_, Result<(), String>>;
    fn thinking_levels(&self) -> Vec<String> {
        Vec::new()
    }
    /// Return supported thinking levels for a specific model (`provider/id` or `id`).
    fn thinking_levels_for(&self, _model_id: &str) -> Vec<String> {
        self.thinking_levels()
    }
    fn current_thinking_level(&self) -> Option<String> {
        None
    }
    fn set_thinking_level(&self, _level: &str) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    /// Whether fast/low-latency mode is currently enabled for this model/session.
    fn fast_mode(&self) -> bool {
        false
    }
}

/// Permission-mode inspection and switching capability (`/permissions`, `Shift+Tab`).
pub trait PermissionModeCap: Send + Sync {
    fn current_mode(&self) -> String;
    fn available_modes(&self) -> Vec<PermissionModeOption>;
    fn set_mode(&self, mode: &str) -> Result<(), String>;
    fn cycle_mode(&self) -> Result<String, String> {
        let modes = self.available_modes();
        if modes.is_empty() {
            return Err("No permission modes configured".into());
        }
        let current = self.current_mode();
        let next_idx = modes
            .iter()
            .position(|m| m.id == current)
            .map_or(0, |idx| (idx + 1) % modes.len());
        let next = modes[next_idx].id.clone();
        self.set_mode(&next)?;
        Ok(next)
    }
}

/// Multi-session discovery, switching (`/resume`), and creation (`/new`).
pub trait SessionPickerCap: Send + Sync {
    fn current_session_id(&self) -> Option<String>;
    fn list_sessions(&self) -> BoxFuture<'_, Result<Vec<SessionOption>, String>>;
    fn switch_session(&self, session_id: &str) -> BoxFuture<'_, Result<(), String>>;
    fn new_session(&self) -> BoxFuture<'_, Result<(), String>>;
}

/// Background process inspection and termination capability (surfaced in bottom status strip).
pub trait TasksCap: Send + Sync {
    fn list_tasks(&self) -> Vec<BackgroundTask>;
    fn kill_task(&self, id: &str) -> bool;
}

/// Skills listing and invocation capability (`/skills`, `/<skill-name>`).
pub trait SkillsCap: Send + Sync {
    fn list_skills(&self) -> Vec<SkillSummary>;
    fn reload_skills(&self) -> bool {
        false
    }
    /// Expand an explicit `/<skill-name> [args]` invocation into prompt text.
    /// Returns `None` if no skill with `name` is registered.
    fn invoke_skill(&self, _name: &str, _args: &str) -> Option<Result<String, String>> {
        None
    }
}

/// MCP server status capability (`/mcp`).
pub trait McpCap: Send + Sync {
    fn list_mcp_servers(&self) -> Vec<McpServerSummary>;
}

/// Result of [`RewindCap::rewind_to`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RewindOutcome {
    /// Target was already the active visible leaf; no session state changed.
    Unchanged,
    /// Active branch changed; optionally refill the composer with rewound user text.
    Rewound { draft: Option<String> },
}

/// Conversation checkpoint / tree-branch rewind capability (`/rewind`, `/tree`, double-`Esc`).
pub trait RewindCap: Send + Sync {
    fn list_rewind_points(&self) -> Vec<RewindPoint>;
    /// Rewind or switch branch to `id`, distinguishing a true no-op from an actual branch switch.
    fn rewind_to(&self, id: &str) -> BoxFuture<'_, Result<RewindOutcome, String>>;
}

/// Result of a user-initiated `!` local shell command executed via [`ShellCap`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellCommandResult {
    pub command: String,
    pub output: String,
    pub exit_code: Option<i32>,
    pub interrupted: bool,
}

/// Optional local shell execution capability backing `!` quick bash mode.
///
/// Harnesses implementing this execute the user command in the session workspace and record the
/// command + output into conversation context so subsequent agent turns can see it.
pub trait ShellCap: Send + Sync {
    fn run_shell(
        &self,
        command: String,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> BoxFuture<'_, Result<ShellCommandResult, String>>;
}

/// Collection of optional capabilities advertised by a [`super::TuiHarness`].
#[derive(Default)]
pub struct HarnessCapabilities<'a> {
    pub context_meter: Option<&'a dyn ContextMeterCap>,
    pub queue: Option<&'a dyn QueueCap>,
    pub compaction: Option<&'a dyn CompactionCap>,
    pub model_picker: Option<&'a dyn ModelPickerCap>,
    pub permission_mode: Option<&'a dyn PermissionModeCap>,
    pub session_picker: Option<&'a dyn SessionPickerCap>,
    pub tasks: Option<&'a dyn TasksCap>,
    pub skills: Option<&'a dyn SkillsCap>,
    pub mcp: Option<&'a dyn McpCap>,
    pub rewind: Option<&'a dyn RewindCap>,
    pub shell: Option<&'a dyn ShellCap>,
}
