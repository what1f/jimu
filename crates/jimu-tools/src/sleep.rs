use crate::events::{WakeGate, WakeOutcome, elapsed_ms};
use jimu_agent::{AgentTool, AgentToolResult, ToolExecutionMode, ToolProgressCallback};
use jimu_ai::{BoxFuture, CancelToken, ToolDefinition};
use jimu_schema::{int_schema, obj_schema, str_schema, with_bounds, with_description};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::time::Instant;

const MAX_SLEEP_MS: u64 = 43_200_000;

#[derive(Clone, Debug)]
pub struct SleepTool {
    wake_gate: WakeGate,
}

impl SleepTool {
    pub fn new(wake_gate: WakeGate) -> Self {
        Self { wake_gate }
    }
}

#[derive(Deserialize)]
struct SleepArgs {
    duration_ms: u64,
    reason: Option<String>,
}

impl AgentTool for SleepTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "sleep".into(),
            description: "Wait for a set duration or until new activity arrives.".into(),
            parameters: obj_schema([
                (
                    "duration_ms",
                    with_bounds(int_schema(), Some(1.0), Some(MAX_SLEEP_MS as f64)),
                    true,
                ),
                (
                    "reason",
                    with_description(str_schema(), "Why you are waiting."),
                    false,
                ),
            ]),
        }
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    fn execute(
        &self,
        _tool_call_id: String,
        args: Value,
        cancel: CancelToken,
        on_update: ToolProgressCallback,
    ) -> BoxFuture<'_, Result<AgentToolResult, String>> {
        Box::pin(async move {
            let args: SleepArgs =
                serde_json::from_value(args).map_err(|error| error.to_string())?;
            if !(1..=MAX_SLEEP_MS).contains(&args.duration_ms) {
                return Err(format!("duration_ms must be between 1 and {MAX_SLEEP_MS}"));
            }
            let started = Instant::now();
            let wait = self
                .wake_gate
                .wait_timeout(Duration::from_millis(args.duration_ms), &cancel);
            if let Some(reason) = args.reason
                && !cancel.is_cancelled()
            {
                on_update(AgentToolResult::text(reason));
            }
            let outcome = wait.await;
            let (text, details) = match outcome {
                WakeOutcome::Cancelled => return Err("Operation aborted".into()),
                WakeOutcome::Woken { reason, elapsed_ms } => (
                    "Wait ended early.",
                    json!({"woken_early": true, "wake_reason": reason, "elapsed_ms": elapsed_ms}),
                ),
                WakeOutcome::Completed => (
                    "Wait completed.",
                    json!({"woken_early": false, "elapsed_ms": elapsed_ms(started)}),
                ),
            };
            let mut result = AgentToolResult::text(text);
            result.details = details;
            Ok(result)
        })
    }
}
