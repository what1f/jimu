use super::{
    Focus, Outcome, TuiApp,
    dialogs::{Modal, PickerTarget},
};
use crate::{
    engine::theme::ThemeKind,
    harness::{CombinedAutocompleteProvider, NoticeLevel, QueuedItem, RewindPoint, SlashContext},
    ui::views::queue_pane::QueueEntryWire,
};
use std::{collections::BTreeMap, path::PathBuf};

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

    pub(super) fn refresh_commands(&mut self) {
        self.completion = CombinedAutocompleteProvider {
            commands: self.commands().into_iter().collect(),
            directory: self.config.directory.clone(),
            providers: self.config.autocomplete.iter().cloned().collect(),
        };
        self.refresh_completion();
    }

    pub(super) fn theme_command(&mut self, name: &str) {
        if name.is_empty() {
            let options = std::iter::once(&ThemeKind::Auto)
                .chain(ThemeKind::selectable())
                .map(|k| (k.display_name().into(), k.display_name().into()))
                .collect();
            self.open_picker("Color theme", PickerTarget::Theme, options);
        } else if let Some(kind) = ThemeKind::from_name(name.trim()) {
            self.apply_theme(kind);
        } else {
            self.history.notice(
                NoticeLevel::Warning,
                format!("{}  {name}", self.config.labels.get("Unknown theme")),
            );
        }
    }

    fn open_picker(&mut self, title: &str, target: PickerTarget, opts: Vec<(String, String)>) {
        self.modals.push_back(Modal::picker(
            self.config.labels.get(title).into(),
            target,
            opts,
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
        let rows = points
            .into_iter()
            .map(|p| (p.id.clone(), format_rewind_row(&p)))
            .collect();
        self.open_picker(title, PickerTarget::Rewind, rows);
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
            return;
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
                    let options = c
                        .available_models()
                        .into_iter()
                        .map(|m| (m.id, format!("{}  {}", m.name, m.provider)))
                        .collect();
                    self.open_picker("Choose model", PickerTarget::Model, options);
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
                                let label = if current.as_deref() == Some(lvl.as_str()) {
                                    format!("{lvl}  (active)")
                                } else {
                                    lvl.clone()
                                };
                                (lvl, label)
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
                            let active = if m.id == current { " (active)" } else { "" };
                            let label = format!("{}{}  {}", m.id, active, m.description);
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
            _ => self.history.notice(
                NoticeLevel::Warning,
                format!("{}  /{name}", ui.unknown_command),
            ),
        }
    }

    pub(super) fn pick(&mut self, target: PickerTarget, id: String) {
        let h = self.harness.clone();
        match target {
            PickerTarget::Theme => self.theme_command(&id),
            PickerTarget::Model => self.jobs.push(Box::pin(async move {
                let Some(c) = h.capabilities().model_picker else {
                    return Outcome::Done;
                };
                Outcome::Result(c.set_model(&id).await)
            })),
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

fn format_rewind_row(point: &RewindPoint) -> String {
    let role = match point.role.as_str() {
        "user" => "you",
        "assistant" => "ai",
        "compaction" => "summary",
        other => other,
    };
    let marker = if point.is_current_leaf { "● " } else { "  " };
    match &point.timestamp {
        Some(ts) => format!(
            "{}{marker}[{role}] {}  {ts}",
            point.tree_prefix, point.label
        ),
        None => format!("{}{marker}[{role}] {}", point.tree_prefix, point.label),
    }
}
