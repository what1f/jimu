//! Host entry points for the scrollback model.

use crate::harness::ToolRendererRegistry;

use super::blocks::{ContextInfo, SessionEvent};
use super::{EntryId, RenderBlock, ScrollbackEntry, ScrollbackState, ToolBlockData};

impl ScrollbackState {
    pub fn add_user(&mut self, text: impl Into<String>) -> EntryId {
        self.push_block(RenderBlock::user_prompt(text))
    }

    pub fn add_agent(&mut self, text: impl Into<String>) -> EntryId {
        self.push_block(RenderBlock::agent_message(text))
    }

    pub fn start_thinking(&mut self) -> EntryId {
        self.push(ScrollbackEntry::running(RenderBlock::thinking_streaming()))
    }

    pub fn add_system(&mut self, text: impl Into<String>) -> EntryId {
        self.push_block(RenderBlock::system(text))
    }

    pub fn add_notice(&mut self, event: SessionEvent) -> EntryId {
        self.push_block(RenderBlock::session_event(event))
    }

    pub fn add_context_info(&mut self, snapshot: ContextInfo, model: impl Into<String>) -> EntryId {
        self.push_block(RenderBlock::context_info(snapshot, model))
    }

    pub fn add_background_task(&mut self, task: super::blocks::BgTaskBlock) -> EntryId {
        let running = task.is_running();
        let block = RenderBlock::BgTask(task);
        if running {
            self.push(ScrollbackEntry::running(block))
        } else {
            self.push_block(block)
        }
    }

    pub fn finish_background_task(
        &mut self,
        id: EntryId,
        elapsed: std::time::Duration,
        failure: Option<(Option<i32>, Option<String>)>,
    ) -> bool {
        let Some(entry) = self.get_by_id_mut(id) else {
            return false;
        };
        let RenderBlock::BgTask(task) = &mut entry.block else {
            return false;
        };
        if !task.is_running() {
            return false;
        }
        if let Some((exit_code, signal)) = failure {
            task.mark_failed(elapsed, exit_code, signal);
        } else {
            task.mark_completed(elapsed);
        }
        self.finish_running(id);
        true
    }

    pub fn start_tool(&mut self, tool: ToolBlockData, renderers: &ToolRendererRegistry) -> EntryId {
        let mut block = tool.into_block(renderers);
        block.start_timing();
        self.push(ScrollbackEntry::running(RenderBlock::ToolCall(block)))
    }

    /// Replace the tool's current presentation snapshot while preserving entry identity and folds.
    pub fn update_tool(
        &mut self,
        id: EntryId,
        tool: ToolBlockData,
        renderers: &ToolRendererRegistry,
        complete: bool,
    ) -> bool {
        let Some(entry) = self.get_by_id(id) else {
            return false;
        };
        let RenderBlock::ToolCall(previous) = &entry.block else {
            return false;
        };
        if !entry.is_running {
            return false;
        }
        let mut next = tool.into_block(renderers);
        next.transfer_timing_from(previous);
        if let (super::ToolCallBlock::Execute(old), super::ToolCallBlock::Execute(new)) =
            (previous, &mut next)
            && new.output.is_none()
        {
            new.output.clone_from(&old.output);
        }
        self.replace_tool_block(id, RenderBlock::ToolCall(next), None);
        if complete {
            self.finish_running(id);
        } else {
            self.mark_height_dirty(id);
        }
        true
    }

    pub fn export_markdown(&self) -> String {
        super::export::render_blocks_to_markdown(self.iter_entries().map(|(_, e)| &e.block))
    }
}
