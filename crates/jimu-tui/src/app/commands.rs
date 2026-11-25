use super::{
    Focus, Outcome, TuiApp,
    dialogs::{Modal, ModelEffortEntry, PickerTarget},
};
use crate::{
    engine::theme::ThemeKind,
    harness::{
        CombinedAutocompleteProvider, CompletionItem, NoticeLevel, QueuedItem, RewindPoint,
        SlashContext, chrome::fuzzy_subseq,
    },
    ui::views::queue_pane::QueueEntryWire,
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub(super) enum SlashDispatch {
    Handled,
    PassThrough,
    SendSkill { display: String, expanded: String },
}

impl TuiApp {
    fn commands(&self) -> BTreeMap<String, String> {
        let caps = self.harness.capabilities();
        let mut commands = BTreeMap::new();
        for (name, description, enabled) in [
            ("new", "Start a new session", true),
            ("resume", "Resume a session", caps.session_picker.is_some()),
            ("rewind", "Rewind or switch branch", caps.rewind.is_some()),
            ("export", "Export conversation to Markdown", true),
            ("exit", "Exit", true),
            ("model", "Choose model", caps.model_picker.is_some()),
            (
                "effort",
                "Thinking level",
                caps.model_picker
                    .is_some_and(|c| !c.thinking_levels().is_empty()),
            ),
            (
                "compact",
                "Summarize conversation",
                caps.compaction.is_some(),
            ),
            ("context", "Context usage", caps.context_meter.is_some()),
            (
                "permissions",
                "Permission mode",
                caps.permission_mode.is_some(),
            ),
            ("skills", "Available skills", caps.skills.is_some()),
            ("mcp", "Connected servers", caps.mcp.is_some()),
            ("copy", "Copy last response", true),
            ("theme", "Color theme", true),
            ("help", "Keyboard shortcuts", true),
        ] {
            if enabled {
                commands.insert(
                    name.to_owned(),
                    self.config.labels.get(description).to_owned(),
                );
            }
        }
        for cmd in &self.config.slash_commands {
            commands.insert(cmd.name().into(), cmd.description().into());
        }
        commands
    }

    pub(super) fn sync_skills(&mut self) {
        self.completion.skills = self
            .harness
            .capabilities()
            .skills
            .map(|cap| {
                cap.list_skills()
                    .into_iter()
                    .map(|s| (s.name, s.description))
                    .collect()
            })
            .unwrap_or_default();
    }

    pub(super) fn is_known_slash_name(&self, raw_name: &str) -> bool {
        if raw_name.is_empty() {
            return false;
        }
        if matches!(
            raw_name,
            "help"
                | "hotkeys"
                | "theme"
                | "new"
                | "clear"
                | "exit"
                | "quit"
                | "copy"
                | "export"
                | "model"
                | "effort"
                | "thinking"
                | "permissions"
                | "resume"
                | "rewind"
                | "tree"
                | "skills"
                | "mcp"
                | "compact"
                | "context"
        ) || self
            .completion
            .commands
            .iter()
            .any(|(cmd, _)| cmd == raw_name)
        {
            return true;
        }
        let skill_name = raw_name.strip_prefix("skill:").unwrap_or(raw_name);
        self.completion
            .skills
            .iter()
            .any(|(name, _)| name == skill_name)
    }

    pub(super) fn is_known_skill_name(&self, raw_name: &str) -> bool {
        let skill_name = raw_name.strip_prefix("skill:").unwrap_or(raw_name);
        !skill_name.is_empty()
            && self
                .completion
                .skills
                .iter()
                .any(|(name, _)| name == skill_name)
    }

    pub(super) fn refresh_commands(&mut self) {
        let skills = self
            .harness
            .capabilities()
            .skills
            .map(|cap| {
                cap.list_skills()
                    .into_iter()
                    .map(|s| (s.name, s.description))
                    .collect()
            })
            .unwrap_or_default();
        self.completion = CombinedAutocompleteProvider {
            commands: self.commands().into_iter().collect(),
            skills,
            directory: self.config.directory.clone(),
            providers: self.config.autocomplete.iter().cloned().collect(),
        };
        self.refresh_completion();
    }

    pub(super) fn theme_command(&mut self, name: &str) {
        if name.is_empty() {
            let current = self.config.theme_kind;
            let options = std::iter::once(&ThemeKind::Auto)
                .chain(ThemeKind::selectable())
                .map(|k| {
                    let marker = if *k == current { "● " } else { "  " };
                    (
                        k.display_name().into(),
                        format!("{marker}{}", k.display_name()),
                    )
                })
                .collect();
            self.open_picker("Color theme", PickerTarget::Theme, options);
        } else if let Some(kind) = ThemeKind::from_name(name.trim()) {
            self.apply_theme(kind);
            if let Some(cb) = &self.config.on_theme_change {
                cb(kind);
            }
        } else {
            self.history.notice(
                NoticeLevel::Warning,
                format!("{}  {name}", self.config.labels.get("Unknown theme")),
            );
        }
    }

    pub(super) fn command_arg_suggestions(
        &self,
        cmd_name: &str,
        query: &str,
    ) -> Vec<CompletionItem> {
        let caps = self.harness.capabilities();
        let mut out = Vec::new();
        match cmd_name {
            "model" => {
                if let Some(c) = caps.model_picker {
                    let current = c.current_model();
                    for m in c.available_models() {
                        let key = format!("{}/{}", m.provider, m.id);
                        let hay = format!("{key} {}", m.name);
                        if fuzzy_subseq(&hay, query) {
                            let active = m.provider == current.provider && m.id == current.id;
                            let desc = if m.name != m.id {
                                if active {
                                    format!("● {}", m.name)
                                } else {
                                    m.name.clone()
                                }
                            } else if active {
                                format!("● {}", m.provider)
                            } else {
                                m.provider.clone()
                            };
                            out.push(CompletionItem {
                                token: key,
                                description: Some(desc),
                            });
                        }
                    }
                }
            }
            "effort" | "thinking" => {
                if let Some(c) = caps.model_picker {
                    let current = c.current_thinking_level();
                    for lvl in c.thinking_levels() {
                        if fuzzy_subseq(&lvl, query) {
                            let desc =
                                (current.as_deref() == Some(lvl.as_str())).then(|| "●".to_owned());
                            out.push(CompletionItem {
                                token: lvl,
                                description: desc,
                            });
                        }
                    }
                }
            }
            "permissions" => {
                if let Some(c) = caps.permission_mode {
                    let current = c.current_mode();
                    for m in c.available_modes() {
                        if fuzzy_subseq(&m.id, query) || fuzzy_subseq(&m.description, query) {
                            let desc = if m.id == current {
                                format!("● {}", m.description)
                            } else {
                                m.description
                            };
                            out.push(CompletionItem {
                                token: m.id,
                                description: Some(desc),
                            });
                        }
                    }
                }
            }
            "theme" => {
                let current = self.config.theme_kind;
                for kind in std::iter::once(&ThemeKind::Auto).chain(ThemeKind::selectable()) {
                    let name = kind.display_name();
                    if fuzzy_subseq(name, query) {
                        out.push(CompletionItem {
                            token: name.to_owned(),
                            description: (*kind == current).then(|| "●".to_owned()),
                        });
                    }
                }
            }
            other => {
                if let Some(cmd) = self
                    .config
                    .slash_commands
                    .iter()
                    .rev()
                    .find(|c| c.name() == other)
                {
                    out.extend(cmd.completions(query));
                }
            }
        }
        out
    }

    fn open_picker(&mut self, title: &str, target: PickerTarget, opts: Vec<(String, String)>) {
        self.modals.push_back(Modal::picker(
            self.config.labels.get(title).into(),
            target,
            opts,
        ));
    }

    fn open_picker_with_selected(
        &mut self,
        title: &str,
        target: PickerTarget,
        opts: Vec<(String, String)>,
        initial_selected: usize,
    ) {
        self.modals.push_back(Modal::picker_with_selected(
            self.config.labels.get(title).into(),
            target,
            opts,
            initial_selected,
        ));
    }

    fn show_lines(&mut self, title: &str, rows: Vec<String>) {
        self.open_picker(
            title,
            PickerTarget::ReadOnly,
            rows.into_iter().map(|row| (String::new(), row)).collect(),
        );
    }

    pub(super) fn shortcuts(&mut self) {
        self.modals.push_back(Modal::shortcuts(&self.config.labels));
    }

    pub(super) fn welcome_action(&mut self, command: &str) {
        if command == "theme" {
            self.cycle_theme();
        } else {
            self.command(command);
        }
    }

    pub(super) fn open_rewind(&mut self) {
        let Some(cap) = self.harness.capabilities().rewind else {
            return;
        };
        let points = cap.list_rewind_points();
        if points.is_empty() {
            self.show_toast("No checkpoints in this session");
            return;
        }
        let branched = points.iter().any(|p| !p.tree_prefix.is_empty());
        let title = if branched {
            "Session tree"
        } else {
            "Rewind conversation"
        };
        let initial_selected = points.iter().position(|p| p.is_current_leaf).unwrap_or(0);
        let rows = points
            .into_iter()
            .map(|p| (p.id.clone(), format_rewind_row(&p, &self.config.labels)))
            .collect();
        self.open_picker_with_selected(title, PickerTarget::Rewind, rows, initial_selected);
    }

    pub(super) fn open_background_tasks(&mut self) {
        let Some(cap) = self.harness.capabilities().tasks else {
            return;
        };
        let tasks = cap.list_tasks();
        if tasks.is_empty() {
            self.show_toast("No running background processes");
            return;
        }
        let rows = tasks
            .into_iter()
            .map(|t| (t.id, format!("{}  {}  {}", t.kind, t.title, t.command)))
            .collect();
        self.open_picker("Stop background process", PickerTarget::Tasks, rows);
    }

    pub(super) fn cycle_permission_mode(&mut self) {
        let Some(cap) = self.harness.capabilities().permission_mode else {
            return;
        };
        match cap.cycle_mode() {
            Ok(mode) => self.show_toast(&format!("Permissions: {mode}")),
            Err(err) => self.history.notice(NoticeLevel::Warning, err),
        }
    }

    pub(super) fn cycle_thinking_level(&mut self) {
        let Some(cap) = self.harness.capabilities().model_picker else {
            return;
        };
        let levels = cap.thinking_levels();
        if levels.is_empty() {
            return;
        }
        let current = cap.current_thinking_level();
        let next_idx = current
            .as_deref()
            .and_then(|cur| levels.iter().position(|lvl| lvl == cur))
            .map_or(0, |idx| (idx + 1) % levels.len());
        let next = levels[next_idx].clone();
        self.pick(PickerTarget::Effort, next);
    }

    pub(super) fn new_session_command(&mut self) {
        if self.harness.capabilities().session_picker.is_some() {
            let h = self.harness.clone();
            self.jobs.push(Box::pin(async move {
                let Some(c) = h.capabilities().session_picker else {
                    return Outcome::Done;
                };
                Outcome::Session(c.new_session().await)
            }));
        } else {
            self.reset_transcript();
        }
    }

    fn copy_last_response(&mut self) {
        if let Some(text) = self.history.last_assistant_text() {
            self.copy(&text);
        } else {
            self.show_toast("No assistant response to copy");
        }
    }

    fn export_markdown(&mut self, raw_path: &str) {
        let markdown = self.history.export_markdown();
        if markdown.trim().is_empty() {
            self.show_toast("Nothing to export");
            return;
        }
        let path = if raw_path.is_empty() {
            let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
            let dir = self
                .config
                .directory
                .clone()
                .unwrap_or_else(|| PathBuf::from("."));
            dir.join(format!("conversation-{stamp}.md"))
        } else {
            let candidate = PathBuf::from(raw_path);
            if candidate.is_absolute() {
                candidate
            } else {
                self.config
                    .directory
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(candidate)
            }
        };
        match std::fs::write(&path, markdown) {
            Ok(()) => self.show_toast(&format!("Exported {}", path.display())),
            Err(err) => self.history.notice(
                NoticeLevel::Error,
                format!("Failed to export {}: {err}", path.display()),
            ),
        }
    }

    pub(super) fn set_queue(&mut self, queue: Vec<QueuedItem>) {
        if self.queue == queue {
            return;
        }
        self.queue = queue;
        let wire = self
            .queue
            .iter()
            .map(|item| QueueEntryWire {
                id: item.id.clone(),
                text: item.text.clone(),
                kind: if item.is_steering {
                    "steering"
                } else {
                    "follow_up"
                }
                .into(),
                version: 0,
            })
            .collect::<Vec<_>>();
        self.queue_pane.sync_from_merged(
            &Default::default(),
            &wire,
            None,
            None,
            &Default::default(),
        );
        if self.queue.is_empty() && self.focus == Focus::Queue {
            self.focus_prompt();
        }
    }

    pub(super) fn remove_queued_row(&mut self, id: u64, edit: bool) {
        let Some(key) = self.queue_pane.row_ref(id).and_then(|row| row.server_id) else {
            return;
        };
        if edit && !self.composer.is_effectively_empty() {
            self.show_toast("Finish editing your draft first");
            return;
        }
        if let Some(cap) = self.harness.capabilities().queue {
            let draft = if edit {
                self.queue
                    .iter()
                    .find(|item| item.id == key)
                    .map(|item| item.text.clone())
            } else {
                None
            };
            let removed = cap.remove_queued(&key);
            let queue = cap.pending_inputs();
            if removed {
                self.queue_pane.select_after_delete(id);
                if let Some(text) = draft {
                    self.focus_prompt();
                    self.composer.set_text(&text);
                    self.refresh_completion();
                }
            }
            self.set_queue(queue);
        }
    }

    pub(super) fn command(&mut self, cmd: &str) {
        let _ = self.dispatch_slash(cmd);
    }

    pub(super) fn dispatch_slash(&mut self, cmd: &str) -> SlashDispatch {
        let (name, args) = cmd.split_once(char::is_whitespace).unwrap_or((cmd, ""));
        if let Some(command) = self
            .config
            .slash_commands
            .iter()
            .rev()
            .find(|c| c.name() == name)
            .cloned()
        {
            let ctx = SlashContext {
                args: args.trim().into(),
                harness: self.harness.clone(),
                controller: self.controller.clone(),
            };
            self.jobs.push(Box::pin(async move {
                command.run(ctx).await;
                Outcome::Done
            }));
            return SlashDispatch::Handled;
        }
        let harness = self.harness.clone();
        let caps = harness.capabilities();
        let ui = &self.config.text;
        match name {
            "help" | "hotkeys" => self.shortcuts(),
            "theme" => self.theme_command(args.trim()),
            "new" | "clear" => self.new_session_command(),
            "exit" | "quit" => self.quit = true,
            "copy" => self.copy_last_response(),
            "export" => self.export_markdown(args.trim()),
            "model" if let Some(c) = caps.model_picker => {
                let target = args.trim();
                if target.is_empty() {
                    let available = c.available_models();
                    if available.is_empty() {
                        self.history.notice(
                            NoticeLevel::Info,
                            "No models configured in ~/.jimu/providers.json — use /model <provider/id>"
                                .into(),
                        );
                    } else {
                        let current = c.current_model();
                        let current_effort = c.current_thinking_level();
                        let entries = available
                            .into_iter()
                            .map(|m| {
                                let key = format!("{}/{}", m.provider, m.id);
                                let is_current =
                                    m.provider == current.provider && m.id == current.id;
                                let thinking_levels = c.thinking_levels_for(&key);
                                ModelEffortEntry {
                                    id: key,
                                    name: m.name,
                                    provider: m.provider,
                                    is_current,
                                    thinking_levels,
                                }
                            })
                            .collect();
                        self.modals.push_back(Modal::model_effort(
                            self.config.labels.get("Model").into(),
                            entries,
                            current_effort,
                        ));
                    }
                } else {
                    self.pick(PickerTarget::Model, target.to_owned());
                }
            }
            "effort" | "thinking" if let Some(c) = caps.model_picker => {
                let levels = c.thinking_levels();
                if levels.is_empty() {
                    self.history.notice(
                        NoticeLevel::Info,
                        self.config
                            .labels
                            .get("Thinking level is not configurable for this model")
                            .into(),
                    );
                } else {
                    let target = args.trim();
                    if target.is_empty() {
                        let current = c.current_thinking_level();
                        let options = levels
                            .into_iter()
                            .map(|lvl| {
                                let marker = if current.as_deref() == Some(lvl.as_str()) {
                                    "● "
                                } else {
                                    "  "
                                };
                                (lvl.clone(), format!("{marker}{lvl}"))
                            })
                            .collect();
                        self.open_picker("Thinking level", PickerTarget::Effort, options);
                    } else {
                        self.pick(PickerTarget::Effort, target.to_owned());
                    }
                }
            }
            "permissions" if let Some(c) = caps.permission_mode => {
                let target = args.trim();
                if target.is_empty() {
                    let current = c.current_mode();
                    let options = c
                        .available_modes()
                        .into_iter()
                        .map(|m| {
                            let marker = if m.id == current { "● " } else { "  " };
                            let label = format!("{marker}{}  {}", m.id, m.description);
                            (m.id, label)
                        })
                        .collect();
                    self.open_picker("Permission mode", PickerTarget::Permissions, options);
                } else {
                    self.pick(PickerTarget::Permissions, target.to_owned());
                }
            }
            "resume" if caps.session_picker.is_some() => {
                let h = self.harness.clone();
                let controller = self.controller.clone();
                let title = self.config.labels.get("Resume session").to_owned();
                self.jobs.push(Box::pin(async move {
                    let Some(c) = h.capabilities().session_picker else {
                        return Outcome::Done;
                    };
                    match c.list_sessions().await {
                        Ok(items) => {
                            if items.is_empty() {
                                controller
                                    .push_notice(NoticeLevel::Info, "No saved sessions found");
                                return Outcome::Done;
                            }
                            let current = c.current_session_id();
                            let labels = items
                                .iter()
                                .map(|s| {
                                    let marker = if current.as_deref() == Some(s.id.as_str()) {
                                        "● "
                                    } else {
                                        "  "
                                    };
                                    match &s.updated_at {
                                        Some(ts) => format!("{marker}{}  {ts}", s.title),
                                        None => format!("{marker}{}", s.title),
                                    }
                                })
                                .collect();
                            if let Some(i) = controller.show_select(title, labels).await
                                && let Some(s) = items.get(i)
                            {
                                Outcome::Session(c.switch_session(&s.id).await)
                            } else {
                                Outcome::Done
                            }
                        }
                        Err(e) => Outcome::Result(Err(e)),
                    }
                }));
            }
            "rewind" | "tree" if caps.rewind.is_some() => self.open_rewind(),
            "skills" if let Some(c) = caps.skills => {
                let items = c.list_skills();
                if items.is_empty() {
                    self.history.notice(NoticeLevel::Info, ui.no_skills.clone());
                } else {
                    self.show_lines(
                        "Available skills",
                        items
                            .into_iter()
                            .map(|s| format!("{}  {}", s.name, s.description))
                            .collect(),
                    );
                }
            }
            "mcp" if let Some(c) = caps.mcp => {
                let items = c.list_mcp_servers();
                if items.is_empty() {
                    self.history.notice(NoticeLevel::Info, ui.no_mcp.clone());
                } else {
                    self.show_lines(
                        "Connected servers",
                        items
                            .into_iter()
                            .map(|s| format!("{}  {}  {}", s.name, s.status, s.tool_count))
                            .collect(),
                    );
                }
            }
            "compact" if caps.compaction.is_some() => {
                self.history
                    .notice(NoticeLevel::Info, ui.compacting.clone());
                let controller = self.controller.clone();
                let h = self.harness.clone();
                self.jobs.push(Box::pin(async move {
                    let Some(c) = h.capabilities().compaction else {
                        return Outcome::Done;
                    };
                    match c.compact().await {
                        Ok(changed) => {
                            controller.push_notice(
                                NoticeLevel::Info,
                                if changed {
                                    &controller.text().compacted
                                } else {
                                    &controller.text().compact_unneeded
                                },
                            );
                            Outcome::Done
                        }
                        Err(e) => Outcome::Result(Err(e)),
                    }
                }));
            }
            "context" if let Some(c) = caps.context_meter => {
                if let Some(s) = c.context_usage() {
                    self.context = Some(s);
                }
                let label = self.config.labels.get("tokens");
                let body = self.context.as_ref().map_or_else(
                    || format!("{} {label}", self.usage.total_tokens),
                    |s| {
                        format!(
                            "{} / {} {label}",
                            s.tokens.unwrap_or(s.estimated_tokens),
                            s.context_window
                        )
                    },
                );
                self.history.notice(NoticeLevel::Info, body);
            }
            _ => {
                // 1. Dynamic skill invocation (`/<skill-name> [args]` or `/skill:<name> [args]`).
                if let Some(skills_cap) = caps.skills {
                    let skill_name = name.strip_prefix("skill:").unwrap_or(name);
                    if let Some(result) = skills_cap.invoke_skill(skill_name, args.trim()) {
                        self.sync_skills();
                        return match result {
                            Ok(expanded) => SlashDispatch::SendSkill {
                                display: format!("/{cmd}"),
                                expanded,
                            },
                            Err(err) => {
                                self.history.notice(NoticeLevel::Error, err);
                                SlashDispatch::Handled
                            }
                        };
                    }
                }

                // 2. Claude Code slash-vs-path disambiguation:
                //    - Multi-segment or dotted paths (`/Users/foo/a.txt`, `/foo.rs`) fail
                //      `looks_like_command` -> pass straight through as ordinary user text.
                //    - Existing single-segment filesystem paths (`/tmp`, `/Users`) -> pass through.
                //    - Genuine typos -> block and show a warning with edit-distance <= 2 suggestion.
                if !looks_like_command(name) {
                    return SlashDispatch::PassThrough;
                }
                if Path::new(&format!("/{name}")).exists() {
                    return SlashDispatch::PassThrough;
                }

                let unknown_label = ui.unknown_command.clone();
                self.sync_skills();
                let mut msg = format!("{unknown_label}  /{name}");
                if let Some(suggestion) = self.suggest_similar_command(name) {
                    msg.push_str(&format!("  — Did you mean /{suggestion}?"));
                }
                self.history.notice(NoticeLevel::Warning, msg);
                return SlashDispatch::Handled;
            }
        }
        SlashDispatch::Handled
    }

    fn suggest_similar_command(&self, query: &str) -> Option<String> {
        let mut best: Option<(usize, String)> = None;
        for candidate in self
            .completion
            .commands
            .iter()
            .map(|(n, _)| n.as_str())
            .chain(self.completion.skills.iter().map(|(n, _)| n.as_str()))
        {
            let dist = levenshtein(query, candidate);
            if dist <= 2 && best.as_ref().is_none_or(|(d, _)| dist < *d) {
                best = Some((dist, candidate.to_owned()));
            }
        }
        best.map(|(_, name)| name)
    }

    pub(super) fn pick_model_and_effort(&mut self, model: String, effort: Option<String>) {
        let h = self.harness.clone();
        let ctrl = self.controller.clone();
        self.jobs.push(Box::pin(async move {
            let Some(c) = h.capabilities().model_picker else {
                return Outcome::Done;
            };
            if let Err(err) = c.set_model(&model).await {
                return Outcome::Result(Err(err));
            }
            if let Some(level) = effort {
                if let Err(err) = c.set_thinking_level(&level).await {
                    return Outcome::Result(Err(err));
                }
                ctrl.push_notice(
                    NoticeLevel::Info,
                    format!("Model: {model}  Effort: {level}"),
                );
            } else {
                ctrl.push_notice(NoticeLevel::Info, format!("Model: {model}"));
            }
            Outcome::Done
        }));
    }

    pub(super) fn pick(&mut self, target: PickerTarget, id: String) {
        let h = self.harness.clone();
        match target {
            PickerTarget::Theme => self.theme_command(&id),
            PickerTarget::Model => {
                let ctrl = self.controller.clone();
                self.jobs.push(Box::pin(async move {
                    let Some(c) = h.capabilities().model_picker else {
                        return Outcome::Done;
                    };
                    let res = c.set_model(&id).await;
                    if res.is_ok() {
                        ctrl.push_notice(NoticeLevel::Info, format!("Model: {id}"));
                    }
                    Outcome::Result(res)
                }));
            }
            PickerTarget::Effort => {
                let ctrl = self.controller.clone();
                self.jobs.push(Box::pin(async move {
                    let Some(c) = h.capabilities().model_picker else {
                        return Outcome::Done;
                    };
                    let res = c.set_thinking_level(&id).await;
                    if res.is_ok() {
                        ctrl.push_notice(NoticeLevel::Info, format!("Effort: {id}"));
                    }
                    Outcome::Result(res)
                }));
            }
            PickerTarget::Permissions => {
                if let Some(c) = h.capabilities().permission_mode {
                    match c.set_mode(&id) {
                        Ok(()) => self.show_toast(&format!("Permissions: {id}")),
                        Err(err) => self.history.notice(NoticeLevel::Warning, err),
                    }
                }
            }
            PickerTarget::Rewind => self.jobs.push(Box::pin(async move {
                let Some(c) = h.capabilities().rewind else {
                    return Outcome::Done;
                };
                Outcome::Rewind(c.rewind_to(&id).await)
            })),
            PickerTarget::Tasks => {
                if let Some(c) = h.capabilities().tasks
                    && !c.kill_task(&id)
                {
                    self.history.notice(
                        NoticeLevel::Warning,
                        self.config.labels.get("Process already ended").into(),
                    );
                }
            }
            PickerTarget::ReadOnly => {}
        }
    }
}

fn looks_like_command(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphanumeric() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-'))
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.to_lowercase().chars().collect();
    let b: Vec<char> = b.to_lowercase().chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

fn format_rewind_row(point: &RewindPoint, labels: &super::Labels) -> String {
    let role = match point.role.as_str() {
        "user" => labels.get("you"),
        "assistant" => labels.get("ai"),
        "compaction" => labels.get("summary"),
        other => other,
    };
    let marker = if point.is_current_leaf {
        "● "
    } else if point.is_active_branch {
        "• "
    } else {
        "  "
    };
    match &point.timestamp {
        Some(ts) => format!(
            "{}{marker}[{role}] {}  {ts}",
            point.tree_prefix, point.label
        ),
        None => format!("{}{marker}[{role}] {}", point.tree_prefix, point.label),
    }
}
