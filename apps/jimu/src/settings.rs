//! Persistent user defaults stored in `~/.jimu/settings.json`, shared across TUI, ACP, RPC, and
//! one-shot (`-p`) modes.

use crate::{mcp::McpServerConfig, providers::save};
use jimu_ai::ThinkingLevel;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::ErrorKind,
    path::{Path, PathBuf},
};

fn is_valid_permission_mode(raw: &str) -> bool {
    matches!(raw.trim(), "default" | "acceptEdits" | "bypassPermissions")
}

fn is_valid_theme(raw: &str) -> bool {
    !raw.trim().is_empty()
}

/// Configuration for an external shell command rendering the TUI bottom status line.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct StatusLineConfig {
    /// Shell command executed via `sh -c <command>` (receiving JSON state on stdin).
    pub command: String,
    /// Optional periodic refresh interval in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_ms: Option<u64>,
    /// Optional per-run execution timeout in milliseconds (defaults to 1500ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

fn acquire_settings_lock(jimu_home: &Path) -> std::io::Result<fs::File> {
    fs::create_dir_all(jimu_home)?;
    let lock_path = jimu_home.join("settings.json.lock");
    let mut opts = OpenOptions::new();
    opts.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(&lock_path)?;
    file.lock()?;
    Ok(file)
}

#[derive(Clone, Default, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct UserSettings {
    /// Default model key (`<provider>/<model_id>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Default thinking / reasoning effort level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<ThinkingLevel>,
    /// Default permission mode (`default`, `acceptEdits`, or `bypassPermissions`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_permission_mode: Option<String>,
    /// Default TUI color theme name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Default fast-mode toggle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast: Option<bool>,
    /// Optional external shell command rendering the TUI bottom status line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_line: Option<StatusLineConfig>,
    /// Show the built-in 1-row top header bar in fullscreen TUI mode (defaults to false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_header: Option<bool>,
    /// Additional QuickJS extension files or directories loaded across all modes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<PathBuf>,
    /// Global MCP servers attached across TUI, `-p`, RPC, and ACP modes.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
}

impl UserSettings {
    pub fn path(jimu_home: &Path) -> PathBuf {
        jimu_home.join("settings.json")
    }

    /// Parse typed settings from a raw JSON object with field- and server-level fault isolation.
    fn parse_from_object(obj: &Map<String, Value>) -> (Self, Vec<String>) {
        let mut settings = Self::default();
        let mut warnings = Vec::new();

        if let Some(v) = obj.get("default_model") {
            match serde_json::from_value(v.clone()) {
                Ok(parsed) => settings.default_model = parsed,
                Err(err) => warnings.push(format!("settings.json `default_model` ignored: {err}")),
            }
        }
        if let Some(v) = obj.get("default_effort") {
            match serde_json::from_value(v.clone()) {
                Ok(parsed) => settings.default_effort = parsed,
                Err(err) => warnings.push(format!("settings.json `default_effort` ignored: {err}")),
            }
        }
        if let Some(v) = obj.get("default_permission_mode") {
            match serde_json::from_value::<Option<String>>(v.clone()) {
                Ok(Some(mode)) if is_valid_permission_mode(&mode) => {
                    settings.default_permission_mode = Some(mode.trim().to_owned());
                }
                Ok(Some(mode)) => warnings.push(format!(
                    "settings.json `default_permission_mode` ignored: unsupported permission mode `{mode}`"
                )),
                Ok(None) => settings.default_permission_mode = None,
                Err(err) => warnings.push(format!(
                    "settings.json `default_permission_mode` ignored: {err}"
                )),
            }
        }
        if let Some(v) = obj.get("theme") {
            match serde_json::from_value::<Option<String>>(v.clone()) {
                Ok(Some(theme)) if is_valid_theme(&theme) => {
                    settings.theme = Some(theme);
                }
                Ok(Some(theme)) => warnings.push(format!(
                    "settings.json `theme` ignored: unknown theme `{theme}`"
                )),
                Ok(None) => settings.theme = None,
                Err(err) => warnings.push(format!("settings.json `theme` ignored: {err}")),
            }
        }
        if let Some(v) = obj.get("fast") {
            match serde_json::from_value(v.clone()) {
                Ok(parsed) => settings.fast = parsed,
                Err(err) => warnings.push(format!("settings.json `fast` ignored: {err}")),
            }
        }
        if let Some(v) = obj.get("status_line") {
            match serde_json::from_value::<Option<StatusLineConfig>>(v.clone()) {
                Ok(Some(cfg)) if !cfg.command.trim().is_empty() => {
                    settings.status_line = Some(cfg);
                }
                Ok(Some(_)) => warnings.push(
                    "settings.json `status_line` ignored: `command` must be a non-empty string"
                        .into(),
                ),
                Ok(None) => settings.status_line = None,
                Err(err) => warnings.push(format!("settings.json `status_line` ignored: {err}")),
            }
        }
        if let Some(v) = obj.get("show_header") {
            match serde_json::from_value(v.clone()) {
                Ok(parsed) => settings.show_header = parsed,
                Err(err) => warnings.push(format!("settings.json `show_header` ignored: {err}")),
            }
        }
        if let Some(v) = obj.get("extensions") {
            match serde_json::from_value::<Vec<PathBuf>>(v.clone()) {
                Ok(parsed) => {
                    settings.extensions = parsed
                        .into_iter()
                        .filter(|p| !p.as_os_str().is_empty())
                        .collect();
                }
                Err(err) => warnings.push(format!("settings.json `extensions` ignored: {err}")),
            }
        }
        if let Some(raw_servers) = obj.get("mcp_servers") {
            match raw_servers.as_object() {
                Some(map) => {
                    for (name, entry_val) in map {
                        match serde_json::from_value::<McpServerConfig>(entry_val.clone()) {
                            Ok(cfg) => {
                                settings.mcp_servers.insert(name.clone(), cfg);
                            }
                            Err(err) => warnings
                                .push(format!("settings.json MCP server `{name}` skipped: {err}")),
                        }
                    }
                }
                None => {
                    warnings.push("settings.json `mcp_servers` ignored: expected object".into())
                }
            }
        }

        (settings, warnings)
    }

    /// Load `~/.jimu/settings.json` with field- and server-level fault isolation so a single
    /// malformed MCP entry never invalidates valid settings or sibling MCP servers.
    pub fn load_with_warnings(jimu_home: &Path) -> (Self, Vec<String>) {
        let path = Self::path(jimu_home);
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == ErrorKind::NotFound => return (Self::default(), Vec::new()),
            Err(err) => {
                return (
                    Self::default(),
                    vec![format!("failed to read {}: {err}", path.display())],
                );
            }
        };
        let value: Value = match serde_json::from_str(&raw) {
            Ok(value) => value,
            Err(err) => {
                return (
                    Self::default(),
                    vec![format!("failed to parse {}: {err}", path.display())],
                );
            }
        };
        let Some(obj) = value.as_object() else {
            return (
                Self::default(),
                vec![format!("{}: expected a JSON object", path.display())],
            );
        };
        Self::parse_from_object(obj)
    }

    pub fn load(jimu_home: &Path) -> Self {
        let (settings, warnings) = Self::load_with_warnings(jimu_home);
        for warning in warnings {
            eprintln!("warning: {warning}");
        }
        settings
    }

    /// Patch-update `~/.jimu/settings.json` under an exclusive cross-process lock while preserving
    /// untouched fields (including `mcp_servers` and unknown keys). Refuses to overwrite if the
    /// existing file cannot be read or parsed as a JSON object.
    pub fn update(jimu_home: &Path, apply: impl FnOnce(&mut Self)) -> std::io::Result<()> {
        let _lock = acquire_settings_lock(jimu_home)?;
        let path = Self::path(jimu_home);
        let mut root_obj: Map<String, Value> = match fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str::<Value>(&raw) {
                Ok(Value::Object(obj)) => obj,
                Ok(_) => {
                    return Err(std::io::Error::new(
                        ErrorKind::InvalidData,
                        "settings.json root must be a JSON object",
                    ));
                }
                Err(err) => {
                    return Err(std::io::Error::new(ErrorKind::InvalidData, err.to_string()));
                }
            },
            Err(err) if err.kind() == ErrorKind::NotFound => Map::new(),
            Err(err) => return Err(err),
        };

        let (before, _) = Self::parse_from_object(&root_obj);
        let mut after = before.clone();
        apply(&mut after);

        patch_optional_field(
            &mut root_obj,
            "default_model",
            &before.default_model,
            &after.default_model,
        )?;
        patch_optional_field(
            &mut root_obj,
            "default_effort",
            &before.default_effort,
            &after.default_effort,
        )?;
        patch_optional_field(
            &mut root_obj,
            "default_permission_mode",
            &before.default_permission_mode,
            &after.default_permission_mode,
        )?;
        patch_optional_field(&mut root_obj, "theme", &before.theme, &after.theme)?;
        patch_optional_field(&mut root_obj, "fast", &before.fast, &after.fast)?;
        patch_optional_field(
            &mut root_obj,
            "status_line",
            &before.status_line,
            &after.status_line,
        )?;
        patch_optional_field(
            &mut root_obj,
            "show_header",
            &before.show_header,
            &after.show_header,
        )?;
        if before.extensions != after.extensions {
            if after.extensions.is_empty() {
                root_obj.remove("extensions");
            } else {
                let encoded = serde_json::to_value(&after.extensions)
                    .map_err(|err| std::io::Error::new(ErrorKind::InvalidData, err.to_string()))?;
                root_obj.insert("extensions".into(), encoded);
            }
        }
        if before.mcp_servers != after.mcp_servers {
            if after.mcp_servers.is_empty() {
                root_obj.remove("mcp_servers");
            } else {
                let encoded = serde_json::to_value(&after.mcp_servers)
                    .map_err(|err| std::io::Error::new(ErrorKind::InvalidData, err.to_string()))?;
                root_obj.insert("mcp_servers".into(), encoded);
            }
        }

        save(&path, &Value::Object(root_obj))
    }
}

fn patch_optional_field<T: PartialEq + Serialize>(
    obj: &mut Map<String, Value>,
    key: &str,
    before: &Option<T>,
    after: &Option<T>,
) -> std::io::Result<()> {
    if before == after {
        return Ok(());
    }
    match after {
        Some(value) => {
            let encoded = serde_json::to_value(value)
                .map_err(|err| std::io::Error::new(ErrorKind::InvalidData, err.to_string()))?;
            obj.insert(key.to_owned(), encoded);
        }
        None => {
            obj.remove(key);
        }
    }
    Ok(())
}

/// Discover custom JSON themes from `~/.jimu/themes/*.json` (user-level) and
/// `<cwd>/.jimu/themes/*.json` (project-level). Project themes override user themes of the same
/// name. Malformed JSON files produce warnings and are skipped without blocking startup.
#[cfg(feature = "tui")]
pub fn discover_custom_themes(
    jimu_home: &Path,
    cwd: &Path,
) -> (
    BTreeMap<String, jimu_tui::engine::theme::ThemeOverrides>,
    Vec<String>,
) {
    let mut themes: BTreeMap<String, jimu_tui::engine::theme::ThemeOverrides> = BTreeMap::new();
    let mut warnings: Vec<String> = Vec::new();

    let user_dir = jimu_home.join("themes");
    let project_dir = cwd.join(".jimu").join("themes");

    let mut dirs = vec![user_dir.clone()];
    if !crate::host::paths_same_dir(&user_dir, &project_dir) {
        dirs.push(project_dir);
    }

    for dir in dirs {
        let Ok(read_dir) = fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = read_dir
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file()
                    && path
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
            })
            .collect();
        files.sort();

        for path in files {
            let Some(stem) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let raw = match fs::read_to_string(&path) {
                Ok(raw) => raw,
                Err(err) => {
                    warnings.push(format!("failed to read theme {}: {err}", path.display()));
                    continue;
                }
            };
            match serde_json::from_str::<jimu_tui::engine::theme::ThemeOverrides>(&raw) {
                Ok(overrides) => {
                    // Replace any existing case-insensitive match so project overrides user cleanly.
                    let lower = stem.to_ascii_lowercase();
                    let prev_key = themes
                        .keys()
                        .find(|k| k.to_ascii_lowercase() == lower)
                        .cloned();
                    if let Some(prev) = prev_key
                        && prev != stem
                    {
                        themes.remove(&prev);
                    }
                    themes.insert(stem.to_owned(), overrides);
                }
                Err(err) => {
                    warnings.push(format!(
                        "theme {} skipped (invalid JSON): {err}",
                        path.display()
                    ));
                }
            }
        }
    }

    (themes, warnings)
}
