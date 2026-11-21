use crate::skills::{SkillCatalog, escape_xml};
use pikra_ai::ContentPart;
use pikra_session::{SessionEntry, SessionEntryPayload, SessionError, SessionManager};
use std::collections::BTreeMap;

pub const SKILLS_CONTEXT_NAME: &str = "skills";
const SKILL_STATE_ENTRY: &str = "kit.known_skills";

/// Records which skills (`name -> location_path`) the active branch has already presented to the model.
#[derive(Default)]
pub struct SkillContextState {
    entries: Option<BTreeMap<String, String>>,
    persisted_entries: Option<BTreeMap<String, String>>,
    pending_mention: Option<String>,
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
                    serde_json::from_value::<BTreeMap<String, String>>(data.clone())
                        .ok()
                        .map(Some)
                }
                SessionEntryPayload::Compaction { .. } => Some(None),
                _ => None,
            })
            .flatten();
        Self {
            persisted_entries: entries.clone(),
            entries,
            pending_mention: None,
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
        let current = current_known_skills(skills);
        if self.entries.as_ref() != Some(&current) {
            queue_skill_context(session, skills, self, &current);
            self.entries = Some(current.clone());
        }
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
                let mention_xml = mentions
                    .iter()
                    .map(|skill| {
                        format!(
                            "<skill_mention name=\"{}\" location=\"{}\" />",
                            escape_xml(&skill.name),
                            escape_xml(&skill.file_path.to_string_lossy()),
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if self.pending_mention.as_deref() != Some(mention_xml.as_str()) {
                    session.append_context(&mention_xml);
                    self.pending_mention = Some(mention_xml);
                }
            }
        }
        let flushed = session.flush_pending_context()?;
        self.pending_mention = None;
        if self.persisted_entries.as_ref() != Some(&current) {
            let data = serde_json::to_value(&current)?;
            session.append_custom(SKILL_STATE_ENTRY, data)?;
            self.persisted_entries = Some(current);
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
        self.persisted_entries = None;
        self.pending_mention = None;
    }
}

fn current_known_skills(catalog: &SkillCatalog) -> BTreeMap<String, String> {
    catalog
        .visible()
        .map(|skill| {
            (
                skill.name.clone(),
                skill.file_path.to_string_lossy().into_owned(),
            )
        })
        .collect()
}

fn queue_skill_context(
    session: &mut SessionManager,
    catalog: &SkillCatalog,
    known: &mut SkillContextState,
    current: &BTreeMap<String, String>,
) {
    if let Some(previous) = &known.entries {
        let removed: Vec<_> = previous
            .keys()
            .filter(|name| !current.contains_key(*name))
            .collect();
        let is_added_or_moved = |skill: &crate::Skill| {
            previous
                .get(&skill.name)
                .is_none_or(|prev_path| prev_path.as_str() != skill.file_path.to_string_lossy())
        };
        let has_additions = catalog.visible().any(is_added_or_moved);
        if has_additions || !removed.is_empty() {
            let mut diff = String::from("# Skills changed");
            if has_additions {
                diff.push('\n');
                let added = catalog.format_visible(is_added_or_moved);
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
        session.set_context(SKILLS_CONTEXT_NAME, xml);
        known.has_named_context = true;
    } else if known.has_named_context {
        session.set_context(SKILLS_CONTEXT_NAME, catalog.empty_context());
    }
}
