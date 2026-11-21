use crate::bm25::{Document, score_documents};
use crate::instructions::{WalkBoundary, ancestor_chain_root_to_leaf, discovery_home, expand_home};
use pikra_agent::{AgentTool, AgentToolResult, ToolProgressCallback};
use pikra_ai::{BoxFuture, CancelToken, ToolDefinition};
use pikra_schema::{int_schema, obj_schema, str_schema, with_bounds};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Discovered skill descriptor loaded from a SKILL.md or standalone Markdown file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: PathBuf,
    pub base_dir: PathBuf,
    pub disable_model_invocation: bool,
    pub revision: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDiagnostic {
    pub path: PathBuf,
    pub message: String,
}

/// Select a skill by declared name or source file path.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSelector {
    ByName(String),
    ByPath(PathBuf),
}

/// Later matching rules take precedence over earlier ones.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SkillEnableRule {
    pub selector: SkillSelector,
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SkillDiscoveryConfig {
    pub cwd: Option<PathBuf>,
    pub project_root_markers: Vec<String>,
    pub project_skill_dirs: Vec<String>,
    pub walk_boundary: WalkBoundary,
    pub include_agents_skills: bool,
    pub include_user_skills: bool,
    pub user_skill_dirs: Vec<PathBuf>,
    pub home_dir_override: Option<PathBuf>,
    pub extra_roots: Vec<PathBuf>,
    pub bundled_roots: Vec<PathBuf>,
    pub rules: Vec<SkillEnableRule>,
    pub max_catalog_bytes: usize,
    pub shorten_paths: bool,
}

impl Default for SkillDiscoveryConfig {
    fn default() -> Self {
        Self {
            cwd: None,
            project_root_markers: vec![".git".into()],
            project_skill_dirs: Vec::new(),
            walk_boundary: WalkBoundary::ProjectRoot,
            include_agents_skills: true,
            include_user_skills: true,
            user_skill_dirs: Vec::new(),
            home_dir_override: None,
            extra_roots: Vec::new(),
            bundled_roots: Vec::new(),
            rules: Vec::new(),
            max_catalog_bytes: 8192,
            shorten_paths: false,
        }
    }
}

impl SkillDiscoveryConfig {
    fn roots(&self) -> Vec<PathBuf> {
        let mut roots = self.extra_roots.clone();
        if let Some(cwd) = &self.cwd {
            for dir in
                ancestor_chain_root_to_leaf(cwd, self.walk_boundary, &self.project_root_markers)
                    .into_iter()
                    .rev()
            {
                roots.extend(
                    self.project_skill_dirs
                        .iter()
                        .map(|relative| dir.join(relative)),
                );
                if self.include_agents_skills {
                    roots.push(dir.join(".agents/skills"));
                }
            }
        }
        if self.include_user_skills {
            let home = discovery_home(self.home_dir_override.as_deref());
            roots.extend(
                self.user_skill_dirs
                    .iter()
                    .filter_map(|path| expand_home(path, home.as_deref())),
            );
            if let Some(home) = home.filter(|_| self.include_agents_skills) {
                roots.push(home.join(".agents/skills"));
            }
        }
        roots.extend(self.bundled_roots.iter().cloned());
        roots
    }
}

/// Pure-Rust skill discovery catalog.
#[derive(Debug, Clone)]
pub struct SkillCatalog {
    skills: Vec<Skill>,
    skill_roots: Vec<PathBuf>,
    diagnostics: Vec<SkillDiagnostic>,
    max_catalog_bytes: usize,
    shorten_paths: bool,
}

impl Default for SkillCatalog {
    fn default() -> Self {
        Self::discover(&[])
    }
}

impl SkillCatalog {
    /// Discover only the supplied roots, in order.
    pub fn discover(skill_roots: &[PathBuf]) -> Self {
        Self::discover_with_config(&SkillDiscoveryConfig {
            extra_roots: skill_roots.to_vec(),
            include_user_skills: false,
            ..SkillDiscoveryConfig::default()
        })
    }

    /// Discover in scope order: explicit, project, user, then bundled.
    pub fn discover_with_config(config: &SkillDiscoveryConfig) -> Self {
        let mut scanner = SkillScanner {
            catalog: Self {
                skills: Vec::new(),
                skill_roots: Vec::new(),
                diagnostics: Vec::new(),
                max_catalog_bytes: config.max_catalog_bytes,
                shorten_paths: config.shorten_paths,
            },
            rules: config
                .rules
                .iter()
                .map(|rule| match &rule.selector {
                    SkillSelector::ByPath(path) => SkillEnableRule {
                        selector: SkillSelector::ByPath(
                            fs::canonicalize(path).unwrap_or_else(|_| path.clone()),
                        ),
                        enabled: rule.enabled,
                    },
                    SkillSelector::ByName(_) => rule.clone(),
                })
                .collect(),
            seen_names: HashSet::new(),
            seen_paths: HashSet::new(),
            ancestry: HashSet::new(),
        };
        for root in config.roots() {
            if root.is_dir() {
                scanner.scan_dir(&root, &root);
            } else if is_markdown_file(&root) {
                let parent = root.parent().filter(|path| !path.as_os_str().is_empty());
                scanner.collect_file(&root, parent.unwrap_or(Path::new(".")));
            }
        }
        scanner.catalog
    }

    pub fn skills(&self) -> &[Skill] {
        &self.skills
    }

    pub fn diagnostics(&self) -> &[SkillDiagnostic] {
        &self.diagnostics
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|skill| skill.name == name)
    }

    /// Search all enabled skills, including those that require an explicit invocation.
    pub fn search(&self, query: &str, limit: usize) -> Vec<Skill> {
        search_matching(query, limit, self.skills.iter())
    }

    /// Find enabled skills in mention order, including manual-only skills.
    pub fn resolve_explicit_mentions(&self, text: &str) -> Vec<Skill> {
        let name_char = |c: char| c.is_alphanumeric() || matches!(c, '-' | '_' | ':');
        let mut seen = HashSet::new();
        text.match_indices('$')
            .filter(|(index, _)| {
                !text[..*index]
                    .ends_with(|c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '\\'))
            })
            .filter_map(|(index, _)| {
                let tail = &text[index + 1..];
                let name = tail[..tail.find(|c| !name_char(c)).unwrap_or(tail.len())]
                    .trim_end_matches(':');
                if seen.insert(name) {
                    self.get(name).cloned()
                } else {
                    None
                }
            })
            .collect()
    }

    /// Compare the names available for model invocation.
    pub fn has_same_visible_names(&self, other: &Self) -> bool {
        let names = |catalog: &Self| {
            catalog
                .visible()
                .map(|skill| skill.name.clone())
                .collect::<HashSet<_>>()
        };
        names(self) == names(other)
    }

    pub(crate) fn visible(&self) -> impl Iterator<Item = &Skill> {
        self.skills
            .iter()
            .filter(|skill| !skill.disable_model_invocation)
    }

    /// Render a bounded index. The search tool can still find omitted skills.
    pub fn format_for_context(&self) -> Option<String> {
        self.format_visible(|_| true)
    }

    pub(crate) fn empty_context(&self) -> &'static str {
        const EMPTY: &str = "<available_skills>\n</available_skills>";
        if EMPTY.len() <= self.max_catalog_bytes {
            EMPTY
        } else {
            ""
        }
    }

    pub(crate) fn format_visible(&self, include: impl Fn(&Skill) -> bool) -> Option<String> {
        let entries: Vec<_> = self
            .skills
            .iter()
            .zip(&self.skill_roots)
            .filter(|(skill, _)| !skill.disable_model_invocation && include(skill))
            .collect();
        if entries.is_empty() {
            return None;
        }

        let budget = self.max_catalog_bytes;
        let mut catalog = CatalogRows::new(&entries, usize::MAX, false);
        if self.shorten_paths || catalog.size() >= budget - budget / 4 {
            let aliased = CatalogRows::new(&entries, usize::MAX, true);
            if self.shorten_paths || aliased.size() < catalog.size() {
                catalog = aliased;
            }
        }
        let use_aliases = !catalog.roots.is_empty();
        for max_chars in [256, 128, 64, 24] {
            if catalog.size() <= budget {
                break;
            }
            catalog = CatalogRows::new(&entries, max_chars, use_aliases);
        }
        catalog.render(budget)
    }
}

fn search_matching<'a>(
    query: &str,
    limit: usize,
    skills: impl Iterator<Item = &'a Skill>,
) -> Vec<Skill> {
    if limit == 0 {
        return Vec::new();
    }
    let skills: Vec<_> = skills.collect();
    let documents: Vec<_> = skills
        .iter()
        .map(|skill| Document {
            name: &skill.name,
            description: &skill.description,
            properties: "",
        })
        .collect();
    score_documents(query, &documents)
        .into_iter()
        .take(limit)
        .map(|i| skills[i].clone())
        .collect()
}

struct CatalogRows {
    rows: Vec<(String, Option<usize>)>,
    roots: Vec<String>,
}

impl CatalogRows {
    const OPEN: &str = "<available_skills>\n";
    const CLOSE: &str = "</available_skills>";

    fn new(entries: &[(&Skill, &PathBuf)], max_chars: usize, shorten_paths: bool) -> Self {
        let mut counts = HashMap::new();
        for (_, root) in entries {
            *counts.entry(*root).or_insert(0usize) += 1;
        }
        let mut roots = Vec::new();
        let mut aliases = HashMap::new();
        let rows = entries.iter().map(|(skill, root)| {
            let description = match skill.description.char_indices().nth(max_chars) {
                Some((index, _)) => skill.description[..index].to_owned(),
                None => skill.description.clone(),
            };
            let (location, alias) = match skill.file_path.strip_prefix(root) {
                Ok(relative) if shorten_paths && counts[root] > 1 => {
                    let alias = *aliases.entry(*root).or_insert_with(|| {
                        let index = roots.len();
                        roots.push(format!("<skill_root alias=\"r{index}\">{}</skill_root>\n", escape_xml(&root.to_string_lossy())));
                        index
                    });
                    (format!("r{alias}/{}", relative.to_string_lossy()), Some(alias))
                }
                _ => (skill.file_path.to_string_lossy().into_owned(), None),
            };
            let row = format!("  <skill><name>{}</name><description>{}</description><location>{}</location></skill>\n",
                escape_xml(&skill.name), escape_xml(&description), escape_xml(&location));
            (row, alias)
        }).collect();
        Self { rows, roots }
    }

    fn size(&self) -> usize {
        Self::OPEN.len()
            + Self::CLOSE.len()
            + self.rows.iter().map(|(row, _)| row.len()).sum::<usize>()
            + self.roots.iter().map(String::len).sum::<usize>()
    }

    fn render(&self, budget: usize) -> Option<String> {
        let notice = |omitted| {
            format!(
                "<truncated count=\"{omitted}\">Use skill_search to find additional skills.</truncated>\n"
            )
        };
        let needs_truncation = self.size() > budget;
        let mut size = Self::OPEN.len() + Self::CLOSE.len();
        let mut included = 0;
        let mut used_roots = 0;
        for (index, (row, alias)) in self.rows.iter().enumerate() {
            let root_size = alias
                .filter(|&i| i == used_roots)
                .map_or(0, |i| self.roots[i].len());
            let omitted = self.rows.len() - index - 1;
            let notice_size = if omitted > 0 {
                notice(omitted).len()
            } else {
                0
            };
            if needs_truncation && size + row.len() + root_size + notice_size > budget {
                break;
            }
            size += row.len() + root_size;
            used_roots += usize::from(root_size != 0);
            included += 1;
        }
        let mut xml = Self::OPEN.to_owned();
        for root in &self.roots[..used_roots] {
            xml.push_str(root);
        }
        for (row, _) in &self.rows[..included] {
            xml.push_str(row);
        }
        if included < self.rows.len() {
            xml.push_str(&notice(self.rows.len() - included));
        }
        xml.push_str(Self::CLOSE);
        (xml.len() <= budget).then_some(xml)
    }
}

struct SkillScanner {
    catalog: SkillCatalog,
    rules: Vec<SkillEnableRule>,
    seen_names: HashSet<String>,
    seen_paths: HashSet<PathBuf>,
    ancestry: HashSet<PathBuf>,
}

fn is_markdown_file(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "md") && path.is_file()
}

impl SkillScanner {
    fn scan_dir(&mut self, dir: &Path, root: &Path) {
        match fs::canonicalize(dir) {
            Ok(canonical) if self.ancestry.insert(canonical.clone()) => {
                self.scan_entries(dir, root);
                self.ancestry.remove(&canonical);
            }
            Ok(_) => {}
            Err(err) => self.catalog.diagnostics.push(SkillDiagnostic {
                path: dir.to_path_buf(),
                message: format!("failed to resolve directory: {err}"),
            }),
        }
    }

    fn scan_entries(&mut self, dir: &Path, root: &Path) {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(err) => {
                self.catalog.diagnostics.push(SkillDiagnostic {
                    path: dir.to_path_buf(),
                    message: format!("failed to read directory: {err}"),
                });
                return;
            }
        };
        let skill = dir.join("SKILL.md");
        if skill.is_file() {
            self.collect_file(&skill, root);
            return;
        }
        let mut paths: Vec<_> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .collect();
        paths.sort();
        for path in paths {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.starts_with('.') || name == "node_modules" {
                continue;
            }
            if path.is_dir() {
                self.scan_dir(&path, root);
            } else if dir == root && is_markdown_file(&path) {
                self.collect_file(&path, root);
            }
        }
    }

    fn collect_file(&mut self, path: &Path, root: &Path) {
        let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if !self.seen_paths.insert(canonical.clone()) {
            return;
        }
        let raw = match fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(err) => {
                self.catalog.diagnostics.push(SkillDiagnostic {
                    path: path.to_path_buf(),
                    message: format!("failed to read skill file: {err}"),
                });
                return;
            }
        };
        let parsed = parse_frontmatter(&raw);
        let base_dir = path.parent().unwrap_or(Path::new("."));
        let fallback_name = if path.file_name().is_some_and(|name| name == "SKILL.md") {
            base_dir.file_name()
        } else {
            path.file_stem()
        };
        let name = parsed
            .name
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| {
                fallback_name
                    .and_then(|name| name.to_str())
                    .unwrap_or("skill")
                    .to_owned()
            });
        let enabled = self
            .rules
            .iter()
            .rev()
            .find(|rule| match &rule.selector {
                SkillSelector::ByName(candidate) => candidate == &name,
                SkillSelector::ByPath(candidate) => candidate == &canonical || candidate == path,
            })
            .is_none_or(|rule| rule.enabled);
        if !enabled {
            return;
        }
        if !self.seen_names.insert(name.clone()) {
            self.catalog.diagnostics.push(SkillDiagnostic {
                path: path.to_path_buf(),
                message: format!("duplicate skill name \"{name}\", keeping earlier definition"),
            });
            return;
        }
        let description = parsed
            .description
            .filter(|text| !text.trim().is_empty())
            .or_else(|| extract_fallback_description(parsed.body))
            .unwrap_or_else(|| name.clone());
        self.catalog.skill_roots.push(root.to_path_buf());
        self.catalog.skills.push(Skill {
            name,
            description,
            file_path: path.to_path_buf(),
            base_dir: base_dir.to_path_buf(),
            disable_model_invocation: parsed.disable_model_invocation
                || disables_implicit_invocation(base_dir),
            revision: pikra_ai::transform::short_hash(&raw),
        });
    }
}

fn disables_implicit_invocation(dir: &Path) -> bool {
    let Ok(metadata) = fs::read_to_string(dir.join("agents/openai.yaml")) else {
        return false;
    };
    metadata.trim_start_matches('\u{feff}').lines().any(|line| {
        let Some((key, value)) = line.split_once(':') else {
            return false;
        };
        let value = value.split('#').next().unwrap_or_default().trim();
        key.trim() == "allow_implicit_invocation" && value.eq_ignore_ascii_case("false")
    })
}

struct ParsedFrontmatter<'a> {
    name: Option<String>,
    description: Option<String>,
    disable_model_invocation: bool,
    body: &'a str,
}

fn parse_frontmatter(raw: &str) -> ParsedFrontmatter<'_> {
    let trimmed_bom = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let mut lines = trimmed_bom.split_inclusive('\n');
    let Some(first_line) = lines.next() else {
        return ParsedFrontmatter {
            name: None,
            description: None,
            disable_model_invocation: false,
            body: trimmed_bom,
        };
    };

    if first_line.trim() != "---" {
        return ParsedFrontmatter {
            name: None,
            description: None,
            disable_model_invocation: false,
            body: trimmed_bom,
        };
    }

    let mut offset = first_line.len();
    let fm_start = offset;
    let mut fm_end = None;

    for line in lines {
        let line_trim = line.trim();
        if line_trim == "---" || line_trim == "..." {
            fm_end = Some((offset, offset + line.len()));
            break;
        }
        offset += line.len();
    }

    let Some((end_of_fm, body_start)) = fm_end else {
        return ParsedFrontmatter {
            name: None,
            description: None,
            disable_model_invocation: false,
            body: trimmed_bom,
        };
    };

    let fm_block = &trimmed_bom[fm_start..end_of_fm];
    let body = &trimmed_bom[body_start..];

    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut disable_model_invocation = false;

    let fm_lines: Vec<&str> = fm_block.lines().collect();
    let mut i = 0;
    while i < fm_lines.len() {
        let line = fm_lines[i];
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }
        let Some((raw_key, raw_val)) = line.split_once(':') else {
            i += 1;
            continue;
        };
        let key = raw_key.trim();
        let val_trim = raw_val.trim();

        let value = if val_trim == "|" || val_trim == ">" || val_trim == "|-" || val_trim == ">-" {
            let folded = val_trim.starts_with('>');
            let mut block_lines = Vec::new();
            i += 1;
            while i < fm_lines.len() {
                let next = fm_lines[i];
                if next.starts_with(' ') || next.starts_with('\t') || next.trim().is_empty() {
                    block_lines.push(next.trim());
                    i += 1;
                } else {
                    break;
                }
            }
            if folded {
                block_lines
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                block_lines.join("\n").trim().to_string()
            }
        } else {
            i += 1;
            unquote_scalar(val_trim)
        };

        match key {
            "name" => name = Some(value),
            "description" => description = Some(value),
            "disable-model-invocation" | "disable_model_invocation" => {
                disable_model_invocation = matches!(
                    value
                        .split('#')
                        .next()
                        .unwrap_or_default()
                        .trim()
                        .trim_matches(['\'', '"'])
                        .to_ascii_lowercase()
                        .as_str(),
                    "true" | "yes" | "1"
                );
            }
            _ => {}
        }
    }

    ParsedFrontmatter {
        name,
        description,
        disable_model_invocation,
        body,
    }
}

fn unquote_scalar(raw: &str) -> String {
    if raw.len() >= 2
        && ((raw.starts_with('"') && raw.ends_with('"'))
            || (raw.starts_with('\'') && raw.ends_with('\'')))
    {
        let inner = &raw[1..raw.len() - 1];
        if raw.starts_with('"') {
            inner
                .replace("\\\"", "\"")
                .replace("\\n", "\n")
                .replace("\\\\", "\\")
        } else {
            inner.replace("''", "'")
        }
    } else {
        raw.to_string()
    }
}

fn extract_fallback_description(body: &str) -> Option<String> {
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let cleaned = trimmed.trim_start_matches('#').trim();
        if !cleaned.is_empty() {
            return Some(cleaned.to_string());
        }
    }
    None
}

pub(crate) fn escape_xml(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

pub const SKILL_SEARCH_NAME: &str = "skill_search";

/// Searches implicitly invocable skills and returns their full file paths.
pub struct SkillSearchTool {
    catalog: Arc<Mutex<SkillCatalog>>,
}

impl SkillSearchTool {
    pub fn new(catalog: SkillCatalog) -> Self {
        Self::shared(Arc::new(Mutex::new(catalog)))
    }

    pub fn shared(catalog: Arc<Mutex<SkillCatalog>>) -> Self {
        Self { catalog }
    }
}

impl AgentTool for SkillSearchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: SKILL_SEARCH_NAME.into(),
            description:
                "Find available skills by name or description and return their file paths.".into(),
            parameters: obj_schema([
                ("query", str_schema(), true),
                (
                    "limit",
                    with_bounds(int_schema(), Some(1.0), Some(20.0)),
                    false,
                ),
            ]),
        }
    }

    fn execute(
        &self,
        _tool_call_id: String,
        args: Value,
        _cancel: CancelToken,
        _on_update: ToolProgressCallback,
    ) -> BoxFuture<'_, Result<AgentToolResult, String>> {
        Box::pin(async move {
            let query = args["query"]
                .as_str()
                .ok_or("A search query is required.")?;
            let limit = args["limit"].as_u64().unwrap_or(5).clamp(1, 20) as usize;
            let catalog = self.catalog.lock().unwrap_or_else(|e| e.into_inner());
            let matches: Vec<_> = search_matching(query, limit, catalog.visible())
                .into_iter()
                .map(|skill| {
                    json!({
                        "name": skill.name,
                        "description": skill.description,
                        "location": skill.file_path.to_string_lossy(),
                    })
                })
                .collect();
            let payload = json!({ "skills": matches });
            let mut result = AgentToolResult::text(payload.to_string());
            result.structured_content = Some(payload);
            Ok(result)
        })
    }
}
