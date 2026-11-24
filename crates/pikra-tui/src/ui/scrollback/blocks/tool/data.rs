use std::fmt;
use std::sync::Arc;

use serde_json::Value;

use crate::harness::{ToolOutput, ToolRenderer, ToolRendererRegistry, ToolView};

use super::{OtherToolCallBlock, ToolCallBlock};
use crate::ui::scrollback::types::{BlockContext, BlockLine, BlockOutput, DisplayMode};

/// Tool presentation supplied by a host. The same typed renderer is used for live updates and final output.
#[derive(Debug, Clone)]
pub struct ToolBlockData {
    pub name: String,
    pub args: Value,
    pub block: ToolCallBlock,
    pub output: Option<ToolOutput>,
}

impl ToolBlockData {
    pub fn new(name: impl Into<String>, args: Value, block: ToolCallBlock) -> Self {
        Self {
            name: name.into(),
            args,
            block,
            output: None,
        }
    }

    pub fn with_output(mut self, output: ToolOutput) -> Self {
        self.output = Some(output);
        self
    }

    pub fn into_block(self, renderers: &ToolRendererRegistry) -> ToolCallBlock {
        let Self {
            name,
            args,
            mut block,
            output,
        } = self;
        if let Some(renderer) = renderers.get(&name) {
            let mut other = OtherToolCallBlock::new(name, "");
            if let Some(result) = &output {
                other.set_output_text(result.text.clone());
                if result.is_error {
                    other.set_error(Some(result.text.clone()));
                }
            }
            other.custom = Some(CustomToolContent {
                renderer: Arc::clone(renderer),
                args,
                output,
            });
            return ToolCallBlock::Other(other);
        }

        if let Some(output) = output {
            let error = output.is_error.then(|| output.text.clone());
            match &mut block {
                ToolCallBlock::Execute(tool) => {
                    if !output.text.is_empty() {
                        tool.output = Some(output.text);
                    }
                    tool.error = error;
                }
                ToolCallBlock::Read(tool) => {
                    if tool.content.is_none() && !output.text.is_empty() && error.is_none() {
                        tool.content = Some(output.text);
                    }
                    tool.error = error;
                }
                ToolCallBlock::Edit(tool) => tool.error = error,
                ToolCallBlock::ListDir(tool) => {
                    tool.set_output(output.text);
                    tool.error = error;
                }
                ToolCallBlock::Search(tool) => tool.error = error,
                ToolCallBlock::WebFetch(tool) => {
                    if !output.text.is_empty() {
                        tool.output = Some(output.text);
                    }
                    tool.error = error;
                }
                ToolCallBlock::WebSearch(tool) => tool.error = error,
                ToolCallBlock::IntegrationSearch(tool) => tool.error = error,
                ToolCallBlock::UseTool(tool) => {
                    tool.output = Some(output.text);
                    tool.error = error;
                }
                ToolCallBlock::MemorySearch(tool) => tool.error = error,
                ToolCallBlock::SentMessage(tool) => {
                    if let Some(reason) = error {
                        tool.presentation = super::SentMessagePresentation::Rejected { reason };
                    }
                }
                ToolCallBlock::Skill(tool) | ToolCallBlock::Other(tool) => {
                    tool.set_output_text(output.text);
                    tool.error = error;
                }
            }
        }
        block
    }
}

#[derive(Clone)]
pub(super) struct CustomToolContent {
    renderer: Arc<dyn ToolRenderer>,
    args: Value,
    output: Option<ToolOutput>,
}

impl fmt::Debug for CustomToolContent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CustomToolContent").finish_non_exhaustive()
    }
}

impl CustomToolContent {
    pub(super) fn render(&self, name: &str, ctx: &BlockContext) -> BlockOutput {
        let theme = crate::engine::theme::Theme::current();
        let view = ToolView {
            name,
            args: &self.args,
            theme: &theme,
            width: ctx.width,
        };
        let mut rows = self.renderer.render_call(view);
        if ctx.mode != DisplayMode::Collapsed
            && let Some(output) = &self.output
        {
            rows.extend(if ctx.is_running {
                self.renderer.render_update(view, output)
            } else {
                self.renderer.render_result(view, output)
            });
        }

        let (lines, joiners) = crate::engine::render::wrapping::word_wrap_lines_with_joiners(
            rows,
            usize::from(ctx.width),
        );
        BlockOutput {
            lines: lines
                .into_iter()
                .zip(joiners)
                .map(|(line, joiner)| {
                    BlockLine::styled(line)
                        .with_joiner(joiner)
                        .with_selection_range(Some(0))
                })
                .collect(),
        }
    }
}
