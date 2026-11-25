use crate::skills::escape_xml;
use jimu_session::{ReconstructedContext, SessionManager};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt::Write;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

pub const INSTRUCTIONS_CONTEXT_NAME: &str = "instructions";
pub const DEFAULT_INSTRUCTION_MAX_BYTES: usize = 32 * 1024;
const UNBOUNDED_FILE_READ_CAP: usize = 16 * 1024 * 1024;
const BOUNDED_FILE_READ_CAP: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WalkBoundary {
    /// Stop at the nearest ancestor with a root marker. With no marker, inspect only the start directory.
    #[default]
    ProjectRoot,
    /// Inspect all ancestors through the filesystem root.
    FilesystemRoot,
    /// Inspect only the start directory.
    CurrentDirOnly,
}

pub(crate) fn canonical_or_self(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub(crate) fn ancestor_chain_root_to_leaf(
    dir: &Path,
    boundary: WalkBoundary,
    markers: &[String],
) -> Vec<PathBuf> {
    let dir = canonical_or_self(dir);
    let count = match boundary {
        WalkBoundary::CurrentDirOnly => return vec![dir],
        WalkBoundary::FilesystemRoot => usize::MAX,
        WalkBoundary::ProjectRoot => dir
            .ancestors()
            .position(|ancestor| markers.iter().any(|marker| ancestor.join(marker).exists()))
            .map_or(1, |index| index + 1),
    };
    let mut chain: Vec<_> = dir.ancestors().take(count).map(Path::to_path_buf).collect();
    chain.reverse();
    chain
}

pub(crate) fn discovery_home(override_path: Option<&Path>) -> Option<PathBuf> {
    override_path.map(canonical_or_self).or_else(|| {
        ["HOME", "USERPROFILE"]
            .into_iter()
            .filter_map(std::env::var_os)
            .find(|home| !home.is_empty())
            .map(|home| canonical_or_self(Path::new(&home)))
    })
}

pub(crate) fn expand_home(path: &Path, home: Option<&Path>) -> Option<PathBuf> {
    match path.strip_prefix("~") {
        Ok(relative) => home.map(|home| home.join(relative)),
        Err(_) => Some(path.to_path_buf()),
    }
}

/// Search policy applied to working-directory and user instructions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct InstructionDiscoveryConfig {
    pub cwd: Option<PathBuf>,
    pub additional_directories: Vec<PathBuf>,
    pub walk_boundary: WalkBoundary,
    pub project_root_markers: Vec<String>,
    /// The first existing regular file in each directory wins.
    pub filenames: Vec<String>,
    pub include_user_instructions: bool,
    pub include_agents_dir: bool,
    pub user_dirs: Vec<PathBuf>,
    pub home_dir_override: Option<PathBuf>,
    pub extra_files: Vec<PathBuf>,
    /// Shared byte budget for normalized content. `None` is unlimited and zero disables loading.
    pub max_bytes: Option<usize>,
    pub deduplicate_identical_content: bool,
}

impl Default for InstructionDiscoveryConfig {
    fn default() -> Self {
        Self {
            cwd: None,
            additional_directories: Vec::new(),
            walk_boundary: WalkBoundary::ProjectRoot,
            project_root_markers: vec![".git".into()],
            filenames: vec!["AGENTS.override.md".into(), "AGENTS.md".into()],
            include_user_instructions: true,
            include_agents_dir: true,
            user_dirs: Vec::new(),
            home_dir_override: None,
            extra_files: Vec::new(),
            max_bytes: Some(DEFAULT_INSTRUCTION_MAX_BYTES),
            deduplicate_identical_content: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionEntry {
    pub path: PathBuf,
    pub content: String,
    pub truncated: bool,
}

/// Instructions ordered from broadest to most specific.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstructionCatalog {
    entries: Vec<InstructionEntry>,
    total_bytes: usize,
    truncated: bool,
}

impl InstructionCatalog {
    pub fn discover(config: &InstructionDiscoveryConfig) -> Self {
        let mut catalog = Self::default();
        let mut remaining = config.max_bytes;
        if remaining == Some(0) {
            return catalog;
        }

        let filenames: Vec<_> = config
            .filenames
            .iter()
            .filter(|name| {
                !name.contains(['/', '\\', '\0'])
                    && !name.contains("..")
                    && matches!(
                        Path::new(name).components().next(),
                        Some(Component::Normal(_))
                    )
            })
            .collect();
        let mut dirs = Vec::new();
        if config.include_user_instructions {
            let home = discovery_home(config.home_dir_override.as_deref());
            if config.include_agents_dir
                && let Some(home) = &home
            {
                dirs.push(home.join(".agents"));
            }
            dirs.extend(
                config
                    .user_dirs
                    .iter()
                    .filter_map(|path| expand_home(path, home.as_deref())),
            );
        }
        for dir in config.cwd.iter().chain(&config.additional_directories) {
            dirs.extend(ancestor_chain_root_to_leaf(
                dir,
                config.walk_boundary,
                &config.project_root_markers,
            ));
        }
        let files = dirs
            .iter()
            .filter_map(|dir| {
                filenames
                    .iter()
                    .map(|name| dir.join(name))
                    .find(|file| file.is_file())
            })
            .chain(
                config
                    .extra_files
                    .iter()
                    .filter(|file| file.is_file())
                    .cloned(),
            );

        let mut seen_paths = HashSet::new();
        let mut seen_content = HashSet::new();
        for path in files {
            if !seen_paths.insert(canonical_or_self(&path)) {
                continue;
            }
            let read_cap = match remaining {
                Some(limit) => limit
                    .saturating_mul(2)
                    .saturating_add(4096)
                    .min(BOUNDED_FILE_READ_CAP),
                None => UNBOUNDED_FILE_READ_CAP,
            };
            let Ok(file) = fs::File::open(&path) else {
                continue;
            };
            let mut bytes = Vec::new();
            if file
                .take(read_cap.saturating_add(1) as u64)
                .read_to_end(&mut bytes)
                .is_err()
            {
                continue;
            }
            let read_truncated = bytes.len() > read_cap;
            if read_truncated {
                bytes.truncate(read_cap);
            }
            let text = String::from_utf8_lossy(&bytes);
            let mut content = text
                .strip_prefix('\u{feff}')
                .unwrap_or(&text)
                .replace("\r\n", "\n");
            let trimmed = content.trim();
            if trimmed.is_empty()
                || (!read_truncated
                    && config.deduplicate_identical_content
                    && !seen_content.insert(trimmed.to_owned()))
            {
                continue;
            }

            if remaining == Some(0) {
                catalog.truncated = true;
                break;
            }

            let truncated = read_truncated || remaining.is_some_and(|limit| content.len() > limit);
            if let Some(limit) = &mut remaining {
                if truncated {
                    let cut = content.floor_char_boundary(*limit);
                    content.truncate(cut);
                    *limit = 0;
                    if content.is_empty() {
                        catalog.truncated = true;
                        break;
                    }
                } else {
                    *limit -= content.len();
                }
            }
            catalog.total_bytes += content.len();
            catalog.entries.push(InstructionEntry {
                path,
                content,
                truncated,
            });
            catalog.truncated |= truncated;
        }
        catalog
    }

    pub fn entries(&self) -> &[InstructionEntry] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn format_for_context(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut text = String::from("<project_instructions>\n");
        for entry in &self.entries {
            let _ = writeln!(
                text,
                "<instructions source=\"{}\">\n{}\n</instructions>",
                escape_xml(&entry.path.to_string_lossy()),
                escape_xml(&entry.content),
            );
        }
        if self.truncated {
            text.push_str("<notice>Instruction files exceeded the byte limit.</notice>\n");
        }
        text.push_str("</project_instructions>");
        Some(text)
    }
}

/// Keeps the configured discovery policy and its named session context in sync.
pub struct InstructionContextState {
    config: Option<InstructionDiscoveryConfig>,
    catalog: InstructionCatalog,
    had_instructions_context: bool,
}

impl InstructionContextState {
    pub fn new(
        mut config: Option<InstructionDiscoveryConfig>,
        cwd: Option<&Path>,
        session: &mut SessionManager,
        branch: &ReconstructedContext,
    ) -> Self {
        if let Some(config) = &mut config
            && config.cwd.is_none()
        {
            config.cwd = cwd.map(Path::to_path_buf);
        }
        let catalog = config
            .as_ref()
            .map(InstructionCatalog::discover)
            .unwrap_or_default();
        let mut state = Self {
            config,
            catalog,
            had_instructions_context: false,
        };
        state.sync_branch(session, branch);
        state
    }

    pub fn catalog(&self) -> &InstructionCatalog {
        &self.catalog
    }

    pub fn reload(&mut self, session: &mut SessionManager) -> bool {
        let Some(config) = &self.config else {
            return false;
        };
        let catalog = InstructionCatalog::discover(config);
        if catalog == self.catalog {
            return false;
        }
        self.catalog = catalog;
        self.sync_context(session);
        true
    }

    pub fn sync_branch(&mut self, session: &mut SessionManager, branch: &ReconstructedContext) {
        if self.config.is_none() {
            return;
        }
        self.had_instructions_context = branch
            .all_branch_context_blocks
            .contains_key(INSTRUCTIONS_CONTEXT_NAME);
        self.sync_context(session);
    }

    fn sync_context(&mut self, session: &mut SessionManager) {
        if let Some(text) = self.catalog.format_for_context() {
            self.had_instructions_context = true;
            session.set_context(INSTRUCTIONS_CONTEXT_NAME, text);
        } else if self.had_instructions_context {
            session.set_context(
                INSTRUCTIONS_CONTEXT_NAME,
                "Previously supplied project instructions no longer apply.",
            );
        }
    }
}
