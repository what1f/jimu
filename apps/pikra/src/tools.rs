use pikra_tools::{BuiltinToolKind, BuiltinToolSet};
use std::collections::BTreeSet;

/// Canonical names of built-in tools managed by the headless `pikra` CLI/host.
pub const ALL_BUILTIN_TOOL_NAMES: &[&str] = &[
    "read",
    "write",
    "edit",
    "apply_patch",
    "find",
    "grep",
    "exec_command",
    "write_stdin",
    "monitor",
    "sleep",
    "web_fetch",
];

/// Fine-grained selection of built-in tools enabled for a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSelection {
    enabled: BTreeSet<String>,
}

impl Default for ToolSelection {
    fn default() -> Self {
        Self::all()
    }
}

impl ToolSelection {
    pub fn all() -> Self {
        Self {
            enabled: ALL_BUILTIN_TOOL_NAMES
                .iter()
                .map(|name| (*name).to_string())
                .collect(),
        }
    }

    pub fn none() -> Self {
        Self {
            enabled: BTreeSet::new(),
        }
    }

    /// Build a selection from `--no-tools`, `--tools <csv>`, and `--disable-tools <csv>`.
    pub fn from_cli(no_tools: bool, allow: &[String], deny: &[String]) -> Result<Self, String> {
        let allow_names = parse_tool_name_list("--tools", allow)?;
        let deny_names = parse_tool_name_list("--disable-tools", deny)?;

        if no_tools && (!allow_names.is_empty() || !deny_names.is_empty()) {
            return Err(
                "cannot combine `--no-tools` with `--tools` or `--disable-tools`".to_string(),
            );
        }
        if no_tools {
            return Ok(Self::none());
        }

        let mut enabled: BTreeSet<String> = if allow_names.is_empty() {
            ALL_BUILTIN_TOOL_NAMES
                .iter()
                .map(|name| (*name).to_string())
                .collect()
        } else {
            allow_names.into_iter().collect()
        };

        for name in deny_names {
            enabled.remove(&name);
        }

        Ok(Self { enabled })
    }

    pub fn is_empty(&self) -> bool {
        self.enabled.is_empty()
    }

    pub fn is_enabled(&self, name: &str) -> bool {
        self.enabled.contains(name)
    }

    pub fn disable(&mut self, name: &str) {
        self.enabled.remove(name);
    }

    /// Workspace tools built via `WorkspaceTools::build`.
    /// On non-iOS platforms, `exec_command` is registered via `ProcessSessionManager` instead.
    pub fn workspace_tool_set(&self) -> BuiltinToolSet {
        let mut set = BuiltinToolSet::empty();
        for kind in BuiltinToolKind::ALL {
            if !cfg!(target_os = "ios") && kind == BuiltinToolKind::ExecCommand {
                continue;
            }
            if self.is_enabled(kind.name()) {
                set = set.with(kind);
            }
        }
        set
    }

    pub fn uses_process_manager(&self) -> bool {
        self.is_enabled("exec_command") || self.is_enabled("write_stdin")
    }

    pub fn uses_monitor_manager(&self) -> bool {
        self.is_enabled("monitor")
    }
}

fn parse_tool_name_list(flag: &str, raw_groups: &[String]) -> Result<Vec<String>, String> {
    let mut parsed = Vec::new();
    for group in raw_groups {
        let mut saw_any_in_group = false;
        for part in group.split(',') {
            let name = part.trim();
            if name.is_empty() {
                continue;
            }
            saw_any_in_group = true;
            if !ALL_BUILTIN_TOOL_NAMES.contains(&name) {
                return Err(format!(
                    "unknown tool `{name}` for `{flag}` (valid: {})",
                    ALL_BUILTIN_TOOL_NAMES.join(", ")
                ));
            }
            parsed.push(name.to_string());
        }
        if !saw_any_in_group {
            return Err(format!("`{flag}` requires at least one tool name"));
        }
    }
    Ok(parsed)
}
