use crate::{
    engine::diff::{DiffHunk, diff_hunks_from_strings, extract_edit_hunks},
    harness::{
        HistoryMessage, NoticeLevel, SubagentStatus, ToolOutput, ToolRendererRegistry, TuiEvent,
    },
    ui::scrollback::{
        EntryId, RenderBlock, ScrollbackEntry, ScrollbackState, ToolBlockData, ToolCallBlock,
        blocks::{SessionEvent, SubagentBlock, SubagentBlockKind, tool::*},
    },
};
use serde_json::Value;
use std::{collections::HashMap, time::Duration};

#[derive(Default)]
pub(super) struct Transcript {
    pub state: ScrollbackState,
    messages: HashMap<(bool, usize), EntryId>,
    tools: HashMap<String, (EntryId, ToolBlockData)>,
    agents: HashMap<String, EntryId>,
    pending: Option<(EntryId, String, String)>,
}

impl Transcript {
    pub fn restore(&mut self, history: Vec<HistoryMessage>, renderers: &ToolRendererRegistry) {
        self.state.begin_batch();
        for message in history {
            match message {
                HistoryMessage::User { text, steered } => self.user(text, steered),
                HistoryMessage::Assistant {
                    thinking,
                    text,
                    error,
                } => {
                    if let Some(thinking) = thinking {
                        self.delta(true, 0, &thinking);
                    }
                    if !text.is_empty() {
                        self.delta(false, 1, &text);
                    }
                    self.finish_message(error);
                }
                HistoryMessage::Tool {
                    call_id,
                    name,
                    effective_name,
                    args,
                    output,
                } => {
                    self.event(
                        TuiEvent::ToolStart {
                            call_id: call_id.clone(),
                            name: name.clone(),
                            effective_name: effective_name.clone(),
                            args,
                        },
                        renderers,
                    );
                    self.event(
                        TuiEvent::ToolEnd {
                            call_id,
                            name,
                            effective_name,
                            output,
                        },
                        renderers,
                    );
                }
                HistoryMessage::Notice { level, message } => self.notice(level, message),
            }
        }
        self.state.end_batch();
    }

    pub fn pending_user(&mut self, text: String) {
        self.pending_user_with_submitted(text.clone(), text);
    }

    pub fn pending_user_with_submitted(&mut self, display: String, submitted: String) {
        let id = self.state.add_user(display.clone());
        self.state.set_pending_user_input(id, true);
        self.pending = Some((id, display, submitted));
    }

    fn user(&mut self, text: String, steered: bool) {
        if !steered
            && self.pending.as_ref().is_some_and(|(_, display, draft)| {
                let draft_trimmed = draft.trim();
                let leading_stripped = super::image_paste::strip_leading_image_chips(&text);
                draft == &text
                    || display == &text
                    || (!draft_trimmed.is_empty()
                        && (leading_stripped.trim() == draft_trimmed
                            || super::image_paste::strip_image_chips_text(&text) == draft_trimmed))
                    || (draft_trimmed.is_empty()
                        && leading_stripped.trim().is_empty()
                        && display.contains("[Image #"))
            })
        {
            self.finish_pending();
            return;
        }
        let block = if steered {
            RenderBlock::interjection_prompt(text)
        } else {
            RenderBlock::user_prompt(text)
        };
        let id = self.state.push_block(block);
        self.state
            .follow_new_turn(self.state.index_of_id(id), false);
    }

    pub fn finish_pending(&mut self) {
        if let Some((id, _, _)) = self.pending.take() {
            self.state.set_pending_user_input(id, false);
        }
    }

    pub fn active_tool(&self) -> Option<&str> {
        self.tools
            .values()
            .max_by_key(|(id, _)| id.value())
            .map(|(_, data)| data.name.as_str())
    }

    pub fn last_assistant_text(&self) -> Option<String> {
        let len = self.state.len();
        for idx in (0..len).rev() {
            if let Some(entry) = self.state.get(idx)
                && let RenderBlock::AgentMessage(msg) = &entry.block
            {
                let text = msg.copy_text(false);
                if !text.trim().is_empty() {
                    return Some(text);
                }
            }
        }
        None
    }

    pub fn export_markdown(&self) -> String {
        let len = self.state.len();
        let blocks: Vec<&RenderBlock> = (0..len)
            .filter_map(|idx| self.state.get(idx).map(|entry| &entry.block))
            .collect();
        crate::ui::scrollback::export::render_blocks_to_markdown(blocks)
    }

    pub fn notice(&mut self, level: NoticeLevel, text: String) {
        self.state.add_notice(match level {
            NoticeLevel::Info | NoticeLevel::Success => SessionEvent::Notice(text),
            NoticeLevel::Warning => SessionEvent::Warning(text),
            NoticeLevel::Error => SessionEvent::TurnFailed(text),
        });
    }

    fn delta(&mut self, thinking: bool, index: usize, text: &str) {
        let id = *self.messages.entry((thinking, index)).or_insert_with(|| {
            if thinking {
                self.state.start_thinking()
            } else {
                self.state.start_streaming_agent()
            }
        });
        if thinking {
            self.state.push_chunk_to_thinking(id, text);
        } else {
            self.state.push_chunk_to_agent(id, text);
        }
    }

    fn finish_message(&mut self, error: Option<String>) {
        for (_, id) in self.messages.drain() {
            self.state.finish_running(id);
        }
        if let Some(error) = error {
            self.notice(NoticeLevel::Error, error);
        }
    }

    pub fn event(&mut self, event: TuiEvent, renderers: &ToolRendererRegistry) {
        match event {
            TuiEvent::UserMessage { text, steered, .. } => self.user(text, steered),
            TuiEvent::TextDelta { block_index, delta } => self.delta(false, block_index, &delta),
            TuiEvent::ThinkingDelta { block_index, delta } => self.delta(true, block_index, &delta),
            TuiEvent::ThinkingRedacted { block_index } => {
                if let Some(id) = self.messages.remove(&(true, block_index)) {
                    self.state.remove_entry(id);
                }
            }
            TuiEvent::AssistantMessageEnd { error } => self.finish_message(error),
            TuiEvent::ToolStart {
                call_id,
                name,
                effective_name,
                args,
            } => {
                let mut args = args;
                let effective = effective_name.or_else(|| {
                    (name == "call_tool")
                        .then(|| args["name"].as_str().map(str::to_owned))
                        .flatten()
                });
                if name == "call_tool"
                    && let Some(inner) = args.get_mut("arguments")
                {
                    args = inner.take();
                }
                let data = tool_data(effective.unwrap_or(name), args, None);
                let id = self.state.start_tool(data.clone(), renderers);
                self.tools.insert(call_id, (id, data));
            }
            TuiEvent::ToolUpdate {
                call_id,
                mut output,
            } => {
                if let Some((id, data)) = self.tools.get_mut(&call_id) {
                    if matches!(data.block, ToolCallBlock::Execute(_)) {
                        accumulate_output(data, &mut output);
                    }
                    *data = tool_data(data.name.clone(), data.args.clone(), Some(output));
                    self.state.update_tool(*id, data.clone(), renderers, false);
                }
            }
            TuiEvent::ToolEnd {
                call_id,
                name,
                effective_name,
                mut output,
            } => {
                let (id, previous) = self.tools.remove(&call_id).unwrap_or_else(|| {
                    let data = tool_data(effective_name.clone().unwrap_or(name), Value::Null, None);
                    (self.state.start_tool(data.clone(), renderers), data)
                });
                if matches!(previous.block, ToolCallBlock::Execute(_)) {
                    if !output.is_error
                        && let Some(text) = output
                            .structured
                            .as_ref()
                            .and_then(|v| v["output"].as_str())
                    {
                        output.text = text.to_owned();
                    }
                    if output.text.is_empty() {
                        accumulate_output(&previous, &mut output);
                    }
                }
                let is_local_bash = effective_name.as_deref() == Some("!");
                let data = tool_data(
                    effective_name.unwrap_or(previous.name),
                    previous.args,
                    Some(output),
                );
                self.state.update_tool(id, data, renderers, true);
                if is_local_bash && let Some(entry) = self.state.get_by_id_mut(id) {
                    entry.set_display_mode(crate::ui::scrollback::DisplayMode::Expanded);
                    self.state.mark_height_dirty(id);
                }
            }
            TuiEvent::SubagentEvent(update) => {
                let mut block = SubagentBlock::started(
                    update.task,
                    &update.id,
                    update.label,
                    None,
                    None,
                    None,
                    false,
                );
                block.activity_label = update.detail;
                let running = update.status == SubagentStatus::Running;
                block.kind = match update.status {
                    SubagentStatus::Running => SubagentBlockKind::Started,
                    SubagentStatus::Cancelled => SubagentBlockKind::Cancelled {
                        elapsed: Duration::ZERO,
                    },
                    SubagentStatus::Completed { .. } => SubagentBlockKind::Completed {
                        elapsed: Duration::ZERO,
                    },
                    SubagentStatus::Failed { error } => SubagentBlockKind::Failed {
                        elapsed: Duration::ZERO,
                        error: Some(error),
                    },
                };
                let block = RenderBlock::Subagent(block);
                if let Some(&id) = self.agents.get(&update.id) {
                    if let Some(entry) = self.state.get_by_id_mut(id) {
                        entry.block = block;
                        entry.invalidate_cache();
                    }
                    self.state.set_entry_running(id, running);
                    self.state.mark_height_dirty(id);
                } else {
                    let entry = if running {
                        ScrollbackEntry::running(block)
                    } else {
                        ScrollbackEntry::new(block)
                    };
                    self.agents.insert(update.id, self.state.push(entry));
                }
            }
            TuiEvent::TurnEnded => {
                self.finish_pending();
                self.finish_message(None);
                for (_, (id, _)) in self.tools.drain() {
                    self.state.finish_running(id);
                }
            }
            TuiEvent::SessionNotice { level, message } => self.notice(level, message),
            TuiEvent::Error(message) => self.notice(NoticeLevel::Error, message),
            _ => {}
        }
    }
}

fn accumulate_output(previous: &ToolBlockData, next: &mut ToolOutput) {
    if let Some(previous) = &previous.output {
        let mut text = previous.text.clone();
        text.push_str(&next.text);
        next.text = text;
    }
}

fn edit_hunks(args: &Value, details: Option<&Value>) -> Vec<DiffHunk> {
    let (hunks, _) = details.map(extract_edit_hunks).unwrap_or_default();
    if !hunks.is_empty() {
        return hunks;
    }
    args["edits"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(std::slice::from_ref(args))
        .iter()
        .filter_map(|edit| {
            let old = edit["oldText"]
                .as_str()
                .or_else(|| edit["old_string"].as_str())?;
            let new = edit["newText"]
                .as_str()
                .or_else(|| edit["new_string"].as_str())?;
            Some(diff_hunks_from_strings(
                old,
                new,
                edit["start_line"].as_u64().unwrap_or(1) as usize,
            ))
        })
        .flatten()
        .collect()
}

pub(super) fn tool_data(name: String, args: Value, output: Option<ToolOutput>) -> ToolBlockData {
    let field = |key: &str| args[key].as_str().unwrap_or_default();
    let path = field("path");
    let block = match name.as_str() {
        "exec_command" | "execute" => {
            let command = args["cmd"].as_str().unwrap_or_else(|| field("command"));
            ToolCallBlock::Execute(ExecuteToolCallBlock::new(command))
        }
        "write_stdin" => ToolCallBlock::Execute(ExecuteToolCallBlock::new(field("chars"))),
        "read" => {
            let mut read = ReadToolCallBlock::new(path);
            if let Some(output) = output.as_ref() {
                let offset = output.details["offset"]
                    .as_u64()
                    .or_else(|| args["offset"].as_u64())
                    .unwrap_or(1) as usize;
                if let Some(count) = output.details["returned_lines"].as_u64().filter(|n| *n > 0) {
                    read = read.with_line_range(LineRange::new(
                        offset,
                        offset.saturating_add(count as usize - 1),
                    ));
                }
                read.total_lines = output.details["total_lines"].as_u64().map(|n| n as usize);
            }
            ToolCallBlock::Read(read)
        }
        "edit" => ToolCallBlock::Edit(EditToolCallBlock::new(
            path,
            edit_hunks(&args, output.as_ref().map(|o| &o.details)),
        )),
        "write" if args["content"].is_string() => ToolCallBlock::Edit(EditToolCallBlock::new(
            path,
            diff_hunks_from_strings("", field("content"), 1),
        )),
        "list_dir" => ToolCallBlock::ListDir(ListDirToolCallBlock::new(path)),
        "web_fetch" => ToolCallBlock::WebFetch(WebFetchToolCallBlock::new(field("url"))),
        "apply_patch" => {
            let (mut hunks, _) = output
                .as_ref()
                .map(|o| extract_edit_hunks(&o.details))
                .unwrap_or_default();
            let patch = field("patch");
            let path = patch
                .lines()
                .find_map(|line| {
                    line.strip_prefix("*** Update File: ")
                        .or_else(|| line.strip_prefix("*** Add File: "))
                })
                .unwrap_or(path);
            if hunks.is_empty() {
                hunks = patch_hunks(patch);
            }
            ToolCallBlock::Edit(EditToolCallBlock::new(path, hunks))
        }
        _ => {
            let summary = ["path", "pattern", "query", "command", "url", "reason"]
                .into_iter()
                .find_map(|key| args[key].as_str())
                .unwrap_or_default();
            ToolCallBlock::Other(OtherToolCallBlock::new(&name, summary))
        }
    };
    ToolBlockData {
        name,
        args,
        block,
        output,
    }
}

fn patch_hunks(patch: &str) -> Vec<DiffHunk> {
    use crate::engine::diff::DiffLine;
    use similar::ChangeTag;

    let (mut old, mut new) = (1, 1);
    let mut hunks = Vec::new();
    let mut hunk = Vec::new();
    for line in patch.lines() {
        if line.starts_with("*** ") || line.starts_with("@@") {
            if !hunk.is_empty() {
                hunks.push(std::mem::take(&mut hunk));
            }
            if line.starts_with("*** ") {
                (old, new) = (1, 1);
            } else if let Some((a, b)) = line.strip_prefix("@@ -").and_then(|s| s.split_once(" +"))
            {
                let number = |s: &str| s.split([',', ' ']).next()?.parse::<usize>().ok();
                if let (Some(a), Some(b)) = (number(a), number(b)) {
                    (old, new) = (a, b);
                }
            }
            continue;
        }
        let (tag, text) = match line.as_bytes().first() {
            Some(b'+') => (ChangeTag::Insert, &line[1..]),
            Some(b'-') => (ChangeTag::Delete, &line[1..]),
            Some(b' ') => (ChangeTag::Equal, &line[1..]),
            _ => continue,
        };
        hunk.push(DiffLine {
            text: text.to_owned(),
            lo: old,
            ln: new,
            tag,
        });
        old += usize::from(tag != ChangeTag::Insert);
        new += usize::from(tag != ChangeTag::Delete);
    }
    if !hunk.is_empty() {
        hunks.push(hunk);
    }
    hunks
}
