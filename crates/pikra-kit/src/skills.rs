use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Discovered skill descriptor loaded from `<dir>/*/SKILL.md` or `<dir>/*.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: PathBuf,
    pub base_dir: PathBuf,
    pub disable_model_invocation: bool,
    pub revision: String,
}

/// Non-fatal diagnostic encountered while scanning skill roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDiagnostic {
    pub path: PathBuf,
    pub message: String,
}

/// Pure-Rust skill discovery catalog (no external YAML parser dependency).
#[derive(Debug, Clone, Default)]
pub struct SkillCatalog {
    skills: Vec<Skill>,
    diagnostics: Vec<SkillDiagnostic>,
}

impl SkillCatalog {
    /// Discover skills across all configured `skill_roots` in deterministic sorted order.
    ///
    /// Rules per root:
    /// - If a directory contains `SKILL.md`, treat it as a skill directory and do not recurse further.
    /// - Otherwise, load direct `*.md` files at the root level and recurse into subdirectories
    ///   (skipping hidden `.`-prefixed dirs and `node_modules`) looking for `SKILL.md`.
    /// - Deduplicates by skill name (first discovered root wins).
    pub fn discover(skill_roots: &[PathBuf]) -> Self {
        let mut skills = Vec::new();
        let mut diagnostics = Vec::new();
        let mut seen_names: HashSet<String> = HashSet::new();
        let mut seen_paths: HashSet<PathBuf> = HashSet::new();

        for root in skill_roots {
            if !root.exists() {
                continue;
            }
            if root.is_file() {
                if root.extension().and_then(|e| e.to_str()) == Some("md") {
                    collect_file(
                        root,
                        &mut skills,
                        &mut diagnostics,
                        &mut seen_names,
                        &mut seen_paths,
                    );
                }
                continue;
            }
            if root.is_dir() {
                scan_dir(
                    root,
                    true,
                    &mut skills,
                    &mut diagnostics,
                    &mut seen_names,
                    &mut seen_paths,
                );
            }
        }

        Self {
            skills,
            diagnostics,
        }
    }

    pub fn skills(&self) -> &[Skill] {
        &self.skills
    }

    pub fn diagnostics(&self) -> &[SkillDiagnostic] {
        &self.diagnostics
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// Format visible skills into a compact `<available_skills>` XML index for
    /// `session.set_context("skills", ...)`. Returns `None` when no visible skills exist.
    pub fn format_for_context(&self) -> Option<String> {
        let visible: Vec<&Skill> = self
            .skills
            .iter()
            .filter(|s| !s.disable_model_invocation)
            .collect();
        if visible.is_empty() {
            return None;
        }

        let mut lines = Vec::with_capacity(visible.len() * 5 + 2);
        lines.push("<available_skills>".to_string());
        for skill in visible {
            lines.push("  <skill>".to_string());
            lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
            lines.push(format!(
                "    <description>{}</description>",
                escape_xml(&skill.description)
            ));
            lines.push(format!(
                "    <location>{}</location>",
                escape_xml(&skill.file_path.to_string_lossy())
            ));
            lines.push("  </skill>".to_string());
        }
        lines.push("</available_skills>".to_string());
        Some(lines.join("\n"))
    }
}

fn scan_dir(
    dir: &Path,
    include_root_md: bool,
    skills: &mut Vec<Skill>,
    diagnostics: &mut Vec<SkillDiagnostic>,
    seen_names: &mut HashSet<String>,
    seen_paths: &mut HashSet<PathBuf>,
) {
    let read_dir = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(err) => {
            diagnostics.push(SkillDiagnostic {
                path: dir.to_path_buf(),
                message: format!("failed to read directory: {err}"),
            });
            return;
        }
    };

    let mut entries: Vec<PathBuf> = read_dir
        .filter_map(|res| res.ok().map(|e| e.path()))
        .collect();
    entries.sort();

    // If this directory contains SKILL.md, load it and stop recursing.
    if let Some(skill_md) = entries
        .iter()
        .find(|p| p.file_name().and_then(|n| n.to_str()) == Some("SKILL.md") && p.is_file())
    {
        collect_file(skill_md, skills, diagnostics, seen_names, seen_paths);
        return;
    }

    for path in entries {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }
        if path.is_dir() {
            scan_dir(&path, false, skills, diagnostics, seen_names, seen_paths);
        } else if include_root_md
            && path.is_file()
            && path.extension().and_then(|e| e.to_str()) == Some("md")
        {
            collect_file(&path, skills, diagnostics, seen_names, seen_paths);
        }
    }
}

fn collect_file(
    path: &Path,
    skills: &mut Vec<Skill>,
    diagnostics: &mut Vec<SkillDiagnostic>,
    seen_names: &mut HashSet<String>,
    seen_paths: &mut HashSet<PathBuf>,
) {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if !seen_paths.insert(canonical) {
        return;
    }

    let raw = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(err) => {
            diagnostics.push(SkillDiagnostic {
                path: path.to_path_buf(),
                message: format!("failed to read skill file: {err}"),
            });
            return;
        }
    };

    let parsed = parse_frontmatter(&raw);
    let is_skill_md = path.file_name().and_then(|n| n.to_str()) == Some("SKILL.md");
    let base_dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    let fallback_name = if is_skill_md {
        base_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("skill")
            .to_string()
    } else {
        path.file_stem()
            .and_then(|n| n.to_str())
            .unwrap_or("skill")
            .to_string()
    };

    let name = parsed
        .name
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(fallback_name);

    let description = parsed
        .description
        .filter(|s| !s.trim().is_empty())
        .or_else(|| extract_fallback_description(parsed.body))
        .unwrap_or_else(|| name.clone());

    if !seen_names.insert(name.clone()) {
        diagnostics.push(SkillDiagnostic {
            path: path.to_path_buf(),
            message: format!("duplicate skill name \"{name}\", keeping earlier definition"),
        });
        return;
    }

    let revision = pikra_ai::transform::short_hash(&raw);
    skills.push(Skill {
        name,
        description,
        file_path: path.to_path_buf(),
        base_dir,
        disable_model_invocation: parsed.disable_model_invocation,
        revision,
    });
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
                disable_model_invocation =
                    matches!(value.to_ascii_lowercase().as_str(), "true" | "yes" | "1");
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

fn escape_xml(input: &str) -> String {
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
