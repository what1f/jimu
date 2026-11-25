use crate::instructions::{WalkBoundary, ancestor_chain_root_to_leaf, discovery_home, expand_home};
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Discovered skill descriptor loaded from a SKILL.md or standalone Markdown file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: PathBuf,
    pub canonical_path: PathBuf,
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
        }
    }
}

impl SkillDiscoveryConfig {
    /// Resolve all configured roots in scope order: explicit, project, user, then bundled.
    pub fn roots(&self) -> Vec<PathBuf> {
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
    diagnostics: Vec<SkillDiagnostic>,
}

impl Default for SkillCatalog {
    fn default() -> Self {
        Self::discover(&[])
    }
}

impl Skill {
    /// Load this skill's Markdown body from disk, strip frontmatter, apply argument substitution
    /// (`$ARGUMENTS`, `$ARGUMENTS[i]`, `$1`..`$N`, or trailing `ARGUMENTS:` when unused), and wrap
    /// it with base-directory metadata for direct prompt invocation.
    pub fn render_invocation(&self, args: &str) -> std::io::Result<String> {
        let raw = fs::read_to_string(&self.file_path)?;
        let parsed = parse_frontmatter(&raw);
        let body = substitute_skill_arguments(parsed.body.trim(), args.trim());
        Ok(format!(
            "<skill name=\"{}\" location=\"{}\">\nBase directory for this skill: {}\n\n{}\n</skill>",
            escape_xml(&self.name),
            escape_xml(&self.file_path.to_string_lossy()),
            self.base_dir.display(),
            body
        ))
    }
}

/// Find enabled skills in mention order, including manual-only skills.
/// Recognizes both `$skill-name` and standalone `/skill-name` tokens while ignoring paths like
/// `/Users/foo/bar` or `a/b`.
fn is_skill_name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '-' | '_' | ':')
}

fn is_mention_boundary_before(prefix: &str, trigger: char) -> bool {
    let Some(prev) = prefix.chars().next_back() else {
        return true;
    };
    if trigger == '$' {
        !(prev.is_ascii_alphanumeric() || matches!(prev, '_' | '$' | '\\'))
    } else {
        // Standalone `/skill`: must follow whitespace or opening/CJK punctuation, never path/word chars.
        prev.is_whitespace()
            || matches!(
                prev,
                '(' | '['
                    | '{'
                    | '"'
                    | '\''
                    | '。'
                    | '、'
                    | '？'
                    | '！'
                    | '，'
                    | '；'
                    | '：'
                    | '（'
                    | '【'
            )
    }
}

fn split_skill_args(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = args.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => {
                quote = None;
            }
            Some(_) if c == '\\' => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            Some(_) => current.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
            }
            None if c.is_whitespace() => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            None => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn substitute_skill_arguments(body: &str, args: &str) -> String {
    let positional = split_skill_args(args);
    let mut out = String::with_capacity(body.len() + args.len());
    let mut used_placeholder = false;
    let bytes = body.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'$' {
            out.push('$');
            i += 2;
            continue;
        }
        if bytes[i] == b'$' {
            let rest = &body[i + 1..];
            if let Some(after_args) = rest.strip_prefix("ARGUMENTS") {
                if let Some(after_bracket) = after_args.strip_prefix('[')
                    && let Some(close) = after_bracket.find(']')
                    && let Ok(idx) = after_bracket[..close].parse::<usize>()
                {
                    used_placeholder = true;
                    if let Some(val) = positional.get(idx) {
                        out.push_str(val);
                    }
                    i += 1 + "ARGUMENTS[".len() + close + 1;
                    continue;
                }
                let next_char = after_args.chars().next();
                if next_char.is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_')) {
                    used_placeholder = true;
                    out.push_str(args);
                    i += 1 + "ARGUMENTS".len();
                    continue;
                }
            }
            let digit_len = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
            if digit_len > 0 {
                let after_digits = &rest[digit_len..];
                let next_char = after_digits.chars().next();
                if next_char.is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'))
                    && let Ok(one_based) = rest[..digit_len].parse::<usize>()
                    && one_based >= 1
                {
                    used_placeholder = true;
                    if let Some(val) = positional.get(one_based - 1) {
                        out.push_str(val);
                    }
                    i += 1 + digit_len;
                    continue;
                }
            }
        }
        let ch = body[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }

    if !used_placeholder && !args.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("ARGUMENTS: ");
        out.push_str(args);
    }
    out
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
                diagnostics: Vec::new(),
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
                scanner.collect_file(&root);
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

    /// Find enabled skills in mention order, including manual-only skills.
    pub fn resolve_explicit_mentions(&self, text: &str) -> Vec<Skill> {
        let mut seen = HashSet::new();
        let mut found = Vec::new();
        for (index, ch) in text.char_indices() {
            if ch != '$' && ch != '/' {
                continue;
            }
            if !is_mention_boundary_before(&text[..index], ch) {
                continue;
            }
            let tail = &text[index + 1..];
            let end = tail.find(|c| !is_skill_name_char(c)).unwrap_or(tail.len());
            // Reject paths such as `/Users/foo` or `/foo.txt` when triggered by `/`.
            if ch == '/'
                && tail[end..]
                    .chars()
                    .next()
                    .is_some_and(|next| matches!(next, '/' | '.'))
            {
                continue;
            }
            let raw_name = tail[..end].trim_end_matches(':');
            let name = raw_name.strip_prefix("skill:").unwrap_or(raw_name);
            if name.is_empty() {
                continue;
            }
            if let Some(skill) = self.get(name)
                && seen.insert(skill.name.clone())
            {
                found.push(skill.clone());
            }
        }
        found
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

    /// Compare model-visible skills by `(name, file_path, canonical_path)`.
    ///
    /// Editing a skill file in place keeps both paths identical and returns `true`, while
    /// adding, removing, moving, or retargeting a symlinked skill returns `false`.
    pub fn has_same_visible_skills(&self, other: &Self) -> bool {
        fn entries(catalog: &SkillCatalog) -> HashSet<(&str, &Path, &Path)> {
            catalog
                .visible()
                .map(|skill| {
                    (
                        skill.name.as_str(),
                        skill.file_path.as_path(),
                        skill.canonical_path.as_path(),
                    )
                })
                .collect()
        }
        entries(self) == entries(other)
    }

    /// Compare all discovered skills (including manual-only skills and descriptions) for UI refresh.
    pub fn has_same_catalog_metadata(&self, other: &Self) -> bool {
        fn entries(catalog: &SkillCatalog) -> Vec<(&str, &str, &Path, &Path, bool)> {
            catalog
                .skills
                .iter()
                .map(|skill| {
                    (
                        skill.name.as_str(),
                        skill.description.as_str(),
                        skill.file_path.as_path(),
                        skill.canonical_path.as_path(),
                        skill.disable_model_invocation,
                    )
                })
                .collect()
        }
        entries(self) == entries(other)
    }

    /// Return whether `path` refers to a discovered skill file or a file inside a skill directory
    /// (including symlinked skill directories and their canonical targets).
    pub fn contains_path(&self, path: &Path) -> bool {
        let resolved = fs::canonicalize(path).ok();
        self.skills.iter().any(|skill| {
            if path == skill.file_path
                || resolved
                    .as_deref()
                    .is_some_and(|candidate| candidate == skill.canonical_path)
            {
                return true;
            }
            if !skill
                .file_path
                .file_name()
                .is_some_and(|name| name == "SKILL.md")
            {
                return false;
            }
            if path.starts_with(&skill.base_dir) {
                return true;
            }
            let Some(resolved) = resolved.as_deref() else {
                return false;
            };
            skill
                .canonical_path
                .parent()
                .is_some_and(|canonical_dir| resolved.starts_with(canonical_dir))
        })
    }

    pub(crate) fn visible(&self) -> impl Iterator<Item = &Skill> {
        self.skills
            .iter()
            .filter(|skill| !skill.disable_model_invocation)
    }

    /// Render all visible skills into an `<available_skills>` XML catalog.
    pub fn format_for_context(&self) -> Option<String> {
        self.format_visible(|_| true)
    }

    pub(crate) fn empty_context(&self) -> &'static str {
        "<available_skills>\n</available_skills>"
    }

    pub(crate) fn format_visible(&self, include: impl Fn(&Skill) -> bool) -> Option<String> {
        let visible: Vec<_> = self.visible().filter(|skill| include(skill)).collect();
        if visible.is_empty() {
            return None;
        }
        let mut xml = String::from(
            "When a task matches a skill below, read its SKILL.md file at <location> before proceeding.\n\n<available_skills>\n",
        );
        for skill in visible {
            xml.push_str("  <skill>\n");
            xml.push_str(&format!("    <name>{}</name>\n", escape_xml(&skill.name)));
            xml.push_str(&format!(
                "    <description>{}</description>\n",
                escape_xml(&skill.description)
            ));
            xml.push_str(&format!(
                "    <location>{}</location>\n",
                escape_xml(&skill.file_path.to_string_lossy())
            ));
            xml.push_str("  </skill>\n");
        }
        xml.push_str("</available_skills>");
        Some(xml)
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
            self.collect_file(&skill);
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
                self.collect_file(&path);
            }
        }
    }

    fn collect_file(&mut self, path: &Path) {
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
        self.catalog.skills.push(Skill {
            name,
            description,
            file_path: path.to_path_buf(),
            canonical_path: canonical,
            base_dir: base_dir.to_path_buf(),
            disable_model_invocation: parsed.disable_model_invocation
                || disables_implicit_invocation(base_dir),
            revision: jimu_ai::transform::short_hash(&raw),
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
