//! Optional local workspace tools (`read`, `write`, `edit`, `bash`, `find`) implementing [`pikra_agent::AgentTool`].

mod bash;
mod edit;
mod find;
mod read;
mod util;
mod write;

pub use bash::BashTool;
pub use edit::EditTool;
pub use find::FindTool;
pub use read::ReadTool;
pub use util::{TruncatedOutput, truncate_output};
pub use write::WriteTool;

use pikra_agent::AgentTool;
use pikra_ai::{BoxFuture, ToolDefinition};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

/// Default maximum output bytes per tool result (1 MiB).
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// Default directories skipped during recursive workspace `find`.
pub const DEFAULT_IGNORE_DIRS: &[&str] = &[".git", "node_modules", "target"];

/// Host-supplied async environment provider injected into `bash` subprocesses.
pub type EnvProvider = Arc<dyn Fn() -> BoxFuture<'static, Vec<(String, String)>> + Send + Sync>;

/// Canonical built-in local workspace tool kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BuiltinToolKind {
    Read,
    Write,
    Edit,
    Bash,
    Find,
}

impl BuiltinToolKind {
    /// Fixed canonical ordering so tool lists and prompt-cache prefixes never drift.
    pub const ALL: [BuiltinToolKind; 5] = [
        BuiltinToolKind::Read,
        BuiltinToolKind::Write,
        BuiltinToolKind::Edit,
        BuiltinToolKind::Bash,
        BuiltinToolKind::Find,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Edit => "edit",
            Self::Bash => "bash",
            Self::Find => "find",
        }
    }
}

/// Deterministic selectable subset of built-in workspace tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinToolSet {
    mask: u8,
}

impl BuiltinToolSet {
    const fn bit(kind: BuiltinToolKind) -> u8 {
        match kind {
            BuiltinToolKind::Read => 1 << 0,
            BuiltinToolKind::Write => 1 << 1,
            BuiltinToolKind::Edit => 1 << 2,
            BuiltinToolKind::Bash => 1 << 3,
            BuiltinToolKind::Find => 1 << 4,
        }
    }

    pub const fn empty() -> Self {
        Self { mask: 0 }
    }

    pub const fn all() -> Self {
        Self { mask: 0b1_1111 }
    }

    pub const fn read_only() -> Self {
        Self {
            mask: Self::bit(BuiltinToolKind::Read) | Self::bit(BuiltinToolKind::Find),
        }
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
        Self::all()
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
    /// Optional dynamic environment provider for `bash` (e.g. login-shell PATH injection).
    pub env_provider: Option<EnvProvider>,
    /// Maximum UTF-8 output bytes returned by `read`, `find`, and `bash` (default 1 MiB).
    pub max_output_bytes: usize,
    /// Optional explicit shell executable for `bash` (defaults to `/bin/sh` on Unix, `cmd` on Windows).
    pub shell: Option<String>,
    /// Optional default timeout in milliseconds for `bash` when the tool call omits `timeout_ms`.
    pub default_bash_timeout_ms: Option<u64>,
    /// Directory names skipped during recursive `find` traversal.
    pub ignore_dirs: Vec<String>,
}

impl std::fmt::Debug for WorkspaceToolsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceToolsConfig")
            .field("cwd", &self.cwd)
            .field("enabled_tools", &self.enabled_tools)
            .field("has_env_provider", &self.env_provider.is_some())
            .field("max_output_bytes", &self.max_output_bytes)
            .field("shell", &self.shell)
            .field("default_bash_timeout_ms", &self.default_bash_timeout_ms)
            .field("ignore_dirs", &self.ignore_dirs)
            .finish()
    }
}

impl WorkspaceToolsConfig {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            enabled_tools: BuiltinToolSet::all(),
            env_provider: None,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            shell: None,
            default_bash_timeout_ms: None,
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

    pub fn with_env_provider(mut self, provider: EnvProvider) -> Self {
        self.env_provider = Some(provider);
        self
    }

    pub fn with_max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = max_output_bytes;
        self
    }

    pub fn with_shell(mut self, shell: impl Into<String>) -> Self {
        self.shell = Some(shell.into());
        self
    }

    pub fn with_default_bash_timeout_ms(mut self, timeout_ms: Option<u64>) -> Self {
        self.default_bash_timeout_ms = timeout_ms;
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

    pub fn config(&self) -> &WorkspaceToolsConfig {
        &self.config
    }

    /// Construct all enabled tools in canonical order (`read`, `write`, `edit`, `bash`, `find`).
    pub fn tools(&self) -> Vec<Arc<dyn AgentTool>> {
        self.config
            .enabled_tools
            .kinds()
            .into_iter()
            .map(|kind| self.tool(kind))
            .collect()
    }

    /// Construct a single built-in tool sharing this configuration.
    pub fn tool(&self, kind: BuiltinToolKind) -> Arc<dyn AgentTool> {
        let cfg = Arc::clone(&self.config);
        match kind {
            BuiltinToolKind::Read => Arc::new(ReadTool::new(cfg)),
            BuiltinToolKind::Write => Arc::new(WriteTool::new(cfg)),
            BuiltinToolKind::Edit => Arc::new(EditTool::new(cfg)),
            BuiltinToolKind::Bash => Arc::new(BashTool::new(cfg)),
            BuiltinToolKind::Find => Arc::new(FindTool::new(cfg)),
        }
    }

    /// Deterministic tool definitions in canonical order.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools().into_iter().map(|t| t.definition()).collect()
    }
}
