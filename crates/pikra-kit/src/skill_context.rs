use crate::skills::{SkillCatalog, escape_xml};
use pikra_ai::ContentPart;
use pikra_session::{SessionEntry, SessionEntryPayload, SessionError, SessionManager};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SKILLS_CONTEXT_NAME: &str = "skills";
const SKILL_STATE_ENTRY: &str = "kit.known_skills";

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct KnownSkillEntry {
    description: String,
    file_path: String,
    revision: String,
}

/// Records which skills the active branch has already presented to the model.
#[derive(Default)]
pub struct SkillContextState {
    entries: Option<BTreeMap<String, KnownSkillEntry>>,
    has_named_context: bool,
}

impl SkillContextState {
    pub fn from_branch(session: &SessionManager) -> Self {
        let branch = session.active_branch();
        let previous_catalog = branch.iter().rev().find_map(|entry| match &entry.payload {
            SessionEntryPayload::ContextBlock {
                name,
                text,
                ephemeral: false,
            } if name == SKILLS_CONTEXT_NAME => Some(text),
            _ => None,
        });
        let entries = branch
            .iter()
            .rev()
            .find_map(|entry| match &entry.payload {
                SessionEntryPayload::Custom { custom_type, data }
                    if custom_type == SKILL_STATE_ENTRY =>
                {
                    serde_json::from_value(data.clone()).ok().map(Some)
                }
                SessionEntryPayload::Compaction { .. } => Some(None),
                // Older sessions put the whole catalog in the named context block.
                SessionEntryPayload::ContextBlock {
                    name,
                    text,
                    ephemeral: false,
                } if name == SKILLS_CONTEXT_NAME => {
                    let skills = text
                        .split("<skill>")
                        .skip(1)
                        .filter_map(|row| {
                            let field = |tag: &str| {
                                let content = row.split_once(&format!("<{tag}>"))?.1;
                                let raw = content.split_once(&format!("</{tag}>"))?.0;
                                Some(
                                    raw.replace("&lt;", "<")
                                        .replace("&gt;", ">")
                                        .replace("&quot;", "\"")
                                        .replace("&apos;", "'")
                                        .replace("&amp;", "&"),
                                )
                            };
                            Some((
                                field("name")?,
                                KnownSkillEntry {
                                    description: field("description")?,
                                    file_path: field("location")?,
                                    revision: String::new(),
                                },
                            ))
                        })
                        .collect();
                    Some(Some(skills))
                }
                _ => None,
            })
            .flatten();
        Self {
            entries,
            has_named_context: previous_catalog.is_some(),
        }
    }
}

impl SkillContextState {
    /// Append the current catalog or its incremental changes and resolve explicit mentions.
    pub fn flush_for_prompt(
        &mut self,
        session: &mut SessionManager,
        skills: &SkillCatalog,
        content: Option<&[ContentPart]>,
    ) -> Result<Vec<SessionEntry>, SessionError> {
        let current = update_skill_context(session, skills, self);
        if let Some(content) = content {
            let text = content
                .iter()
                .filter_map(|part| match part {
                    ContentPart::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let mentions = skills.resolve_explicit_mentions(&text);
            if !mentions.is_empty() {
                session.append_context(
                    mentions
                        .iter()
                        .map(|skill| {
                            format!(
                                "<skill_mention name=\"{}\" location=\"{}\" />",
                                escape_xml(&skill.name),
                                escape_xml(&skill.file_path.to_string_lossy()),
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
            }
        }
        let flushed = session.flush_pending_context()?;
        if self.entries.as_ref() != Some(&current) {
            let data = serde_json::to_value(&current)?;
            session.append_custom(SKILL_STATE_ENTRY, data)?;
            self.entries = Some(current);
        }

        Ok(flushed)
    }

    pub fn restore_fork(&mut self, session: &mut SessionManager, catalog: Option<&str>) {
        let mut restored = Self::from_branch(session);
        if let Some(catalog) = catalog {
            session.set_context(SKILLS_CONTEXT_NAME, catalog);
        } else if self.has_named_context {
            session.set_context(SKILLS_CONTEXT_NAME, "");
            restored.has_named_context = true;
        }
        *self = restored;
    }

    pub fn reset_after_compaction(&mut self) {
        self.entries = None;
    }
}

fn update_skill_context(
    session: &mut SessionManager,
    catalog: &SkillCatalog,
    known: &mut SkillContextState,
) -> BTreeMap<String, KnownSkillEntry> {
    let current: BTreeMap<_, _> = catalog
        .visible()
        .map(|skill| {
            (
                skill.name.clone(),
                KnownSkillEntry {
                    description: skill.description.clone(),
                    file_path: skill.file_path.to_string_lossy().into_owned(),
                    revision: skill.revision.clone(),
                },
            )
        })
        .collect();
    if let Some(previous) = &known.entries {
        let removed: Vec<_> = previous
            .keys()
            .filter(|name| !current.contains_key(*name))
            .collect();
        let is_added = |skill: &crate::Skill| !previous.contains_key(&skill.name);
        let has_additions = catalog.visible().any(is_added);
        if has_additions || !removed.is_empty() {
            let mut diff = String::from("# Skills changed");
            if has_additions {
                diff.push('\n');
                let added = catalog.format_visible(is_added);
                diff.push_str(
                    added
                        .as_deref()
                        .unwrap_or("Use skill_search to find additional skills."),
                );
            }
            for name in removed {
                diff.push_str(&format!(
                    "\nThe {} skill has been removed and is unavailable.",
                    escape_xml(name)
                ));
            }
            session.append_context(diff);
        }
    } else if let Some(xml) = catalog.format_for_context() {
        if !session.set_context(SKILLS_CONTEXT_NAME, &xml) {
            // Compaction can retain the previous catalog as part of its kept tail.
            session.append_context(xml);
        }
        known.has_named_context = true;
    } else if known.has_named_context {
        session.set_context(SKILLS_CONTEXT_NAME, catalog.empty_context());
    }
    current
}
