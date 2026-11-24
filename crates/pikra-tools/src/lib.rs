//! Optional local workspace tools implementing [`pikra_agent::AgentTool`].
//!
//! Hosts opt into background tools by registering the command and monitor tools
//! with their managers. Share a `WakeGate` between the managers and `SleepTool`
//! to wake an ongoing wait when a background event arrives.

#[cfg(feature = "patch")]
mod apply_patch;
#[cfg(feature = "patch")]
mod edit;
#[cfg(feature = "sleep")]
mod events;
#[cfg(all(feature = "shell", not(target_os = "ios")))]
mod exec_session;
#[cfg(feature = "search")]
mod find;
#[cfg(feature = "search")]
mod grep;
#[cfg(all(feature = "monitor", not(target_os = "ios")))]
mod monitor;
mod override_spec;
#[cfg(feature = "fs")]
mod read;
#[cfg(feature = "patch")]
mod seek_sequence;
#[cfg(feature = "sleep")]
mod sleep;
mod util;
#[cfg(feature = "fs")]
mod write;

#[cfg(feature = "web-fetch")]
pub mod web;

#[cfg(feature = "patch")]
pub use apply_patch::ApplyPatchTool;
#[cfg(feature = "patch")]
pub use edit::EditTool;
#[cfg(feature = "sleep")]
pub use events::{
    BackgroundEventCallback, BackgroundToolEvent, MonitorStopReason, ShellStopReason, WakeGate,
    WakeOutcome, format_background_events_xml,
};
#[cfg(all(feature = "shell", not(target_os = "ios")))]
pub use exec_session::{
    ExecCommandTool, ProcessSessionManager, RunningProcessInfo, WriteStdinTool,
};
#[cfg(feature = "search")]
pub use find::FindTool;
#[cfg(feature = "search")]
pub use grep::GrepTool;
#[cfg(all(feature = "monitor", not(target_os = "ios")))]
pub use monitor::{MonitorConfig, MonitorManager, MonitorTool, RunningMonitorInfo};
pub use override_spec::{
    DescriptionOverride, OverriddenTool, ToolDefinitionOverride, wrap_tool_with_override,
};
#[cfg(feature = "fs")]
pub use read::ReadTool;
#[cfg(feature = "sleep")]
pub use sleep::SleepTool;
pub use util::{TruncatedOutput, truncate_head, truncate_output, truncate_tail_spillover};
#[cfg(feature = "web-fetch")]
pub use web::{HostResolver, WebFetchConfig, WebFetchTool};
#[cfg(feature = "fs")]
pub use write::WriteTool;

use pikra_agent::AgentTool;
use pikra_ai::{BoxFuture, ToolDefinition};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Default maximum model-facing output bytes per tool result (50 KiB, matching Pi).
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 50 * 1024;

/// Default maximum model-facing output lines per tool result (2,000, matching Pi & Claude Code).
pub const DEFAULT_MAX_OUTPUT_LINES: usize = 2_000;

/// Maximum structured output bytes retained for programmatic callers such as codemode (1 MiB).
pub const STRUCTURED_OUTPUT_MAX_BYTES: usize = 1024 * 1024;

/// Default timeout (120 seconds) for one-shot `ExecCommandTool::one_shot` executions.
pub const DEFAULT_EXEC_TIMEOUT_MS: u64 = 120_000;

/// Default directories skipped during recursive workspace `find`.
pub const DEFAULT_IGNORE_DIRS: &[&str] = &[".git", "node_modules", "target"];

/// Host-supplied async environment provider injected into shell subprocesses.
pub type EnvProvider = Arc<dyn Fn() -> BoxFuture<'static, Vec<(String, String)>> + Send + Sync>;

/// Canonical built-in local workspace tool kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinToolKind {
    Read,
    Write,
    Edit,
    ExecCommand,
    Find,
    Grep,
    ApplyPatch,
}

impl BuiltinToolKind {
    /// Fixed canonical ordering so tool lists and prompt-cache prefixes never drift.
    pub const ALL: [BuiltinToolKind; 7] = [
        BuiltinToolKind::Read,
        BuiltinToolKind::Write,
        BuiltinToolKind::Edit,
        BuiltinToolKind::ExecCommand,
        BuiltinToolKind::Find,
        BuiltinToolKind::Grep,
        BuiltinToolKind::ApplyPatch,
    ];

    /// Whether this build includes the tool.
    pub const fn is_available(self) -> bool {
        match self {
            Self::Read | Self::Write => cfg!(feature = "fs"),
            Self::Edit | Self::ApplyPatch => cfg!(feature = "patch"),
            Self::Find | Self::Grep => cfg!(feature = "search"),
            Self::ExecCommand => cfg!(all(feature = "shell", not(target_os = "ios"))),
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Edit => "edit",
            Self::ExecCommand => "exec_command",
            Self::Find => "find",
            Self::Grep => "grep",
            Self::ApplyPatch => "apply_patch",
        }
    }
}

/// Deterministic selectable subset of built-in workspace tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinToolSet {
    mask: u16,
}

impl BuiltinToolSet {
    const fn bit(kind: BuiltinToolKind) -> u16 {
        match kind {
            BuiltinToolKind::Read => 1 << 0,
            BuiltinToolKind::Write => 1 << 1,
            BuiltinToolKind::Edit => 1 << 2,
            BuiltinToolKind::ExecCommand => 1 << 3,
            BuiltinToolKind::Find => 1 << 4,
            BuiltinToolKind::Grep => 1 << 5,
            BuiltinToolKind::ApplyPatch => 1 << 6,
        }
    }

    pub const fn empty() -> Self {
        Self { mask: 0 }
    }

    pub const fn all() -> Self {
        Self::coding().with(BuiltinToolKind::ApplyPatch)
    }

    /// Default workspace tool selection. Hosts can opt in to `apply_patch`.
    pub const fn coding() -> Self {
        Self::read_only()
            .with(BuiltinToolKind::Write)
            .with(BuiltinToolKind::Edit)
            .with(BuiltinToolKind::ExecCommand)
    }

    pub const fn read_only() -> Self {
        Self::empty()
            .with(BuiltinToolKind::Read)
            .with(BuiltinToolKind::Find)
            .with(BuiltinToolKind::Grep)
    }

    pub fn from_kinds(kinds: &[BuiltinToolKind]) -> Self {
        let mut set = Self::empty();
        for &kind in kinds {
            set.mask |= Self::bit(kind);
        }
        set
    }

    pub const fn with(mut self, kind: BuiltinToolKind) -> Self {
        self.mask |= Self::bit(kind);
        self
    }

    pub const fn without(mut self, kind: BuiltinToolKind) -> Self {
        self.mask &= !Self::bit(kind);
        self
    }

    pub const fn contains(&self, kind: BuiltinToolKind) -> bool {
        (self.mask & Self::bit(kind)) != 0
    }

    pub fn kinds(&self) -> Vec<BuiltinToolKind> {
        BuiltinToolKind::ALL
            .into_iter()
            .filter(|k| self.contains(*k))
            .collect()
    }
}

impl Default for BuiltinToolSet {
    fn default() -> Self {
        Self::coding()
    }
}

impl From<&[BuiltinToolKind]> for BuiltinToolSet {
    fn from(kinds: &[BuiltinToolKind]) -> Self {
        Self::from_kinds(kinds)
    }
}

impl From<Vec<BuiltinToolKind>> for BuiltinToolSet {
    fn from(kinds: Vec<BuiltinToolKind>) -> Self {
        Self::from_kinds(&kinds)
    }
}

impl<const N: usize> From<[BuiltinToolKind; N]> for BuiltinToolSet {
    fn from(kinds: [BuiltinToolKind; N]) -> Self {
        Self::from_kinds(&kinds)
    }
}

impl FromIterator<BuiltinToolKind> for BuiltinToolSet {
    fn from_iter<T: IntoIterator<Item = BuiltinToolKind>>(iter: T) -> Self {
        let mut set = Self::empty();
        for kind in iter {
            set.mask |= Self::bit(kind);
        }
        set
    }
}

/// Host controls for [`WorkspaceTools`].
#[derive(Clone)]
pub struct WorkspaceToolsConfig {
    /// Base workspace directory; relative paths in tool calls resolve against `cwd`.
    pub cwd: PathBuf,
    /// Enabled subset of built-in tools.
    pub enabled_tools: BuiltinToolSet,
    /// Definition and argument overrides for individual built-in tools.
    pub tool_overrides: HashMap<BuiltinToolKind, ToolDefinitionOverride>,
    /// Optional dynamic environment provider for shell subprocesses (e.g. login-shell PATH injection).
    pub env_provider: Option<EnvProvider>,
    /// Maximum model-facing UTF-8 output bytes for tools with variable-length output (default 50 KiB).
    pub max_output_bytes: usize,
    /// Maximum model-facing output lines for tools with variable-length output (default 2,000).
    pub max_output_lines: usize,
    /// Optional explicit shell executable (defaults to `/bin/sh` on Unix, `cmd` on Windows).
    pub shell: Option<String>,
    /// Optional default timeout in milliseconds for one-shot `ExecCommandTool::one_shot` (default 120s).
    pub default_exec_timeout_ms: Option<u64>,
    /// Directory names skipped during recursive searches.
    pub ignore_dirs: Vec<String>,
}

impl std::fmt::Debug for WorkspaceToolsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceToolsConfig")
            .field("cwd", &self.cwd)
            .field("enabled_tools", &self.enabled_tools)
            .field("has_env_provider", &self.env_provider.is_some())
            .field("max_output_bytes", &self.max_output_bytes)
            .field("max_output_lines", &self.max_output_lines)
            .field("shell", &self.shell)
            .field("default_exec_timeout_ms", &self.default_exec_timeout_ms)
            .field("ignore_dirs", &self.ignore_dirs)
            .finish()
    }
}

impl WorkspaceToolsConfig {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            enabled_tools: BuiltinToolSet::default(),
            tool_overrides: HashMap::new(),
            env_provider: None,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_output_lines: DEFAULT_MAX_OUTPUT_LINES,
            shell: None,
            default_exec_timeout_ms: Some(DEFAULT_EXEC_TIMEOUT_MS),
            ignore_dirs: DEFAULT_IGNORE_DIRS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
        }
    }

    pub fn with_enabled_tools(mut self, enabled_tools: impl Into<BuiltinToolSet>) -> Self {
        self.enabled_tools = enabled_tools.into();
        self
    }

    pub fn with_tool_override(
        mut self,
        kind: BuiltinToolKind,
        spec: ToolDefinitionOverride,
    ) -> Self {
        self.tool_overrides.insert(kind, spec);
        self
    }

    pub fn with_env_provider(mut self, provider: EnvProvider) -> Self {
        self.env_provider = Some(provider);
        self
    }

    pub fn with_max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = max_output_bytes;
        self
    }

    pub fn with_max_output_lines(mut self, max_output_lines: usize) -> Self {
        self.max_output_lines = max_output_lines;
        self
    }

    pub fn with_shell(mut self, shell: impl Into<String>) -> Self {
        self.shell = Some(shell.into());
        self
    }

    pub fn with_default_exec_timeout_ms(mut self, timeout_ms: Option<u64>) -> Self {
        self.default_exec_timeout_ms = timeout_ms;
        self
    }

    pub fn with_ignore_dirs(mut self, dirs: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.ignore_dirs = dirs.into_iter().map(Into::into).collect();
        self
    }
}

/// Factory and container for built-in local workspace tools.
#[derive(Clone, Debug)]
pub struct WorkspaceTools {
    config: Arc<WorkspaceToolsConfig>,
}

impl WorkspaceTools {
    pub fn new(config: WorkspaceToolsConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }

    pub fn build(config: WorkspaceToolsConfig) -> Vec<Arc<dyn AgentTool>> {
        Self::new(config).tools()
    }

    pub fn try_build(config: WorkspaceToolsConfig) -> Result<Vec<Arc<dyn AgentTool>>, String> {
        Self::new(config).try_tools()
    }

    pub fn config(&self) -> &WorkspaceToolsConfig {
        &self.config
    }

    /// Construct available, enabled tools in canonical order.
    ///
    /// Panics for invalid host overrides. Use [`Self::try_tools`] for fallible configuration.
    pub fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        self.try_tools().expect("invalid workspace tool overrides")
    }

    pub fn try_tools(&self) -> Result<Vec<Arc<dyn AgentTool>>, String> {
        self.config
            .enabled_tools
            .kinds()
            .into_iter()
            .filter(|kind| kind.is_available())
            .map(|kind| self.try_tool(kind))
            .collect()
    }

    /// Construct a single built-in tool sharing this configuration.
    ///
    /// Panics if the tool is unavailable or its override is invalid.
    pub fn tool(&self, kind: BuiltinToolKind) -> Arc<dyn AgentTool> {
        self.try_tool(kind)
            .expect("could not construct workspace tool")
    }

    pub fn try_tool(&self, kind: BuiltinToolKind) -> Result<Arc<dyn AgentTool>, String> {
        #[cfg(any(
            feature = "fs",
            feature = "search",
            all(feature = "shell", not(target_os = "ios"))
        ))]
        let cfg = Arc::clone(&self.config);
        let tool: Option<Arc<dyn AgentTool>> = match kind {
            #[cfg(feature = "fs")]
            BuiltinToolKind::Read => Some(Arc::new(ReadTool::new(cfg))),
            #[cfg(feature = "fs")]
            BuiltinToolKind::Write => Some(Arc::new(WriteTool::new(cfg))),
            #[cfg(feature = "patch")]
            BuiltinToolKind::Edit => Some(Arc::new(EditTool::new(cfg))),
            #[cfg(feature = "patch")]
            BuiltinToolKind::ApplyPatch => Some(Arc::new(ApplyPatchTool::new(cfg))),
            #[cfg(all(feature = "shell", not(target_os = "ios")))]
            BuiltinToolKind::ExecCommand => Some(Arc::new(ExecCommandTool::one_shot(cfg))),
            #[cfg(feature = "search")]
            BuiltinToolKind::Find => Some(Arc::new(FindTool::new(cfg))),
            #[cfg(feature = "search")]
            BuiltinToolKind::Grep => Some(Arc::new(GrepTool::new(cfg))),
            #[allow(unreachable_patterns)]
            _ => None,
        };
        let tool = tool.ok_or_else(|| format!("Tool '{}' is unavailable", kind.name()))?;
        match self.config.tool_overrides.get(&kind) {
            Some(spec) => wrap_tool_with_override(tool, spec.clone()),
            None => Ok(tool),
        }
    }

    /// Deterministic tool definitions in canonical order.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools().into_iter().map(|t| t.definition()).collect()
    }
}
