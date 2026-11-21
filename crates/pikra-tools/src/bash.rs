use crate::WorkspaceToolsConfig;
use crate::util::{BoundedOutputBuffer, display_path, resolve_path};
use pikra_agent::{AgentTool, AgentToolResult, ToolExecutionMode, ToolProgressCallback};
use pikra_ai::{BoxFuture, CancelToken, ToolDefinition};
use pikra_schema::{int_schema, num_schema, obj_schema, str_schema, with_bounds, with_description};
use serde_json::{Value, json};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

const CHUNK_READ_BYTES: usize = 8192;

/// Built-in `bash` tool executing shell commands in an isolated Unix process group
/// with dynamic host `env_provider`, bounded UTF-8-safe output capture, and process-group SIGKILL on cancel/timeout.
#[derive(Clone, Debug)]
pub struct BashTool {
    config: Arc<WorkspaceToolsConfig>,
}

impl BashTool {
    pub fn new(config: Arc<WorkspaceToolsConfig>) -> Self {
        Self { config }
    }
}

#[cfg(unix)]
fn kill_process_group(pid: u32) {
    if let Ok(pgid) = i32::try_from(pid)
        && pgid > 0
    {
        // SAFETY: Sending SIGKILL to the negative process group id we created via
        // `Command::process_group(0)` terminates the child and every descendant in that group.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {}

pub(crate) struct ProcessGroupGuard {
    pid: Option<u32>,
}

impl ProcessGroupGuard {
    pub(crate) fn new(pid: Option<u32>) -> Self {
        Self { pid }
    }

    pub(crate) fn kill_now(&mut self) {
        if let Some(pid) = self.pid.take() {
            kill_process_group(pid);
        }
    }

    fn disarm(&mut self) {
        self.pid = None;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.kill_now();
    }
}

enum StopReasonKind {
    Completed,
    Aborted,
    TimedOut(u64),
}

impl AgentTool for BashTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bash".into(),
            description:
                "Run a shell command in the workspace directory with an optional working directory and timeout."
                    .into(),
            parameters: obj_schema([
                (
                    "command",
                    with_description(str_schema(), "Shell command to execute."),
                    true,
                ),
                (
                    "cwd",
                    with_description(
                        str_schema(),
                        "Working directory for the command (relative to workspace cwd or absolute).",
                    ),
                    false,
                ),
                (
                    "timeout",
                    with_description(num_schema(), "Optional timeout in seconds (greater than 0 and at most 86400)."),
                    false,
                ),
                (
                    "timeout_ms",
                    with_description(
                        with_bounds(int_schema(), Some(1.0), None),
                        "Optional timeout in milliseconds before terminating the process group.",
                    ),
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
        let config = Arc::clone(&self.config);
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err("Operation aborted".into());
            }

            let command = args
                .get("command")
                .and_then(Value::as_str)
                .ok_or_else(|| "missing required string argument: command".to_string())?;
            if command.is_empty() || command.len() > 65_536 || command.contains('\0') {
                return Err("Command must contain 1 to 65536 bytes without NUL".into());
            }

            let effective_cwd = match args.get("cwd").and_then(Value::as_str) {
                Some(raw) => resolve_path(&config.cwd, raw)?,
                None => config.cwd.clone(),
            };
            let shown_cwd = display_path(&config.cwd, &effective_cwd);

            let meta = tokio::fs::metadata(&effective_cwd)
                .await
                .map_err(|e| format!("invalid bash cwd '{shown_cwd}': {e}"))?;
            if !meta.is_dir() {
                return Err(format!("bash cwd '{shown_cwd}' is not a directory"));
            }

            let timeout_ms = match args.get("timeout_ms") {
                None | Some(Value::Null) => match args.get("timeout") {
                    None | Some(Value::Null) => config.default_bash_timeout_ms,
                    Some(value) => {
                        let seconds = value
                            .as_f64()
                            .filter(|seconds| *seconds > 0.0 && *seconds <= 86_400.0)
                            .ok_or("timeout must be greater than 0 and at most 86400 seconds")?;
                        Some((seconds * 1000.0).ceil() as u64)
                    }
                },
                Some(v) => Some(
                    v.as_u64()
                        .filter(|&n| n >= 1)
                        .ok_or_else(|| "timeout_ms must be an integer >= 1".to_string())?,
                ),
            };

            let extra_env = if let Some(provider) = &config.env_provider {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err("Operation aborted".into()),
                    vars = (provider)() => vars,
                }
            } else {
                Vec::new()
            };

            if cancel.is_cancelled() {
                return Err("Operation aborted".into());
            }

            let mut cmd = build_shell_command(config.shell.as_deref(), command);
            cmd.current_dir(&effective_cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);

            for (key, val) in extra_env {
                cmd.env(key, val);
            }
            cmd.env("AI_AGENT", "pikra");

            #[cfg(unix)]
            {
                cmd.process_group(0);
            }

            let mut child = cmd
                .spawn()
                .map_err(|e| format!("failed to spawn shell command: {e}"))?;
            let mut pg_guard = ProcessGroupGuard::new(child.id());

            let mut stdout = child.stdout.take();
            let mut stderr = child.stderr.take();
            let mut stdout_done = stdout.is_none();
            let mut stderr_done = stderr.is_none();
            let mut out_chunk = [0u8; CHUNK_READ_BYTES];
            let mut err_chunk = [0u8; CHUNK_READ_BYTES];

            let mut buffer = BoundedOutputBuffer::new(config.max_output_bytes);
            let deadline =
                timeout_ms.map(|ms| tokio::time::Instant::now() + Duration::from_millis(ms));
            let mut stop_kind = StopReasonKind::Completed;

            // Phase 1: stream stdout & stderr concurrently, racing cancellation and timeout.
            while !stdout_done || !stderr_done {
                let timer = async {
                    match deadline {
                        Some(dl) => tokio::time::sleep_until(dl).await,
                        None => std::future::pending::<()>().await,
                    }
                };

                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        stop_kind = StopReasonKind::Aborted;
                        break;
                    }
                    _ = timer => {
                        stop_kind = StopReasonKind::TimedOut(timeout_ms.unwrap_or(0));
                        break;
                    }
                    res = async {
                        match stdout.as_mut() {
                            Some(pipe) => pipe.read(&mut out_chunk).await,
                            None => std::future::pending().await,
                        }
                    }, if !stdout_done => {
                        match res {
                            Ok(0) | Err(_) => stdout_done = true,
                            Ok(n) => {
                                let slice = &out_chunk[..n];
                                buffer.push(slice);
                                on_update(AgentToolResult::text(String::from_utf8_lossy(slice)));
                            }
                        }
                    }
                    res = async {
                        match stderr.as_mut() {
                            Some(pipe) => pipe.read(&mut err_chunk).await,
                            None => std::future::pending().await,
                        }
                    }, if !stderr_done => {
                        match res {
                            Ok(0) | Err(_) => stderr_done = true,
                            Ok(n) => {
                                let slice = &err_chunk[..n];
                                buffer.push(slice);
                                on_update(AgentToolResult::text(String::from_utf8_lossy(slice)));
                            }
                        }
                    }
                }
            }

            // Drop pipe handles before reaping on abort/timeout so closed FDs never linger.
            drop(stdout);
            drop(stderr);

            // Phase 2: wait for process exit or kill entire process group immediately on abort/timeout.
            let exit_status = match stop_kind {
                StopReasonKind::Aborted | StopReasonKind::TimedOut(_) => {
                    pg_guard.kill_now();
                    let _ = child.start_kill();
                    let status = child.wait().await.ok();
                    pg_guard.disarm();
                    status
                }
                StopReasonKind::Completed => {
                    let timer = async {
                        match deadline {
                            Some(dl) => tokio::time::sleep_until(dl).await,
                            None => std::future::pending::<()>().await,
                        }
                    };

                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            stop_kind = StopReasonKind::Aborted;
                            pg_guard.kill_now();
                            let _ = child.start_kill();
                            let status = child.wait().await.ok();
                            pg_guard.disarm();
                            status
                        }
                        _ = timer => {
                            stop_kind = StopReasonKind::TimedOut(timeout_ms.unwrap_or(0));
                            pg_guard.kill_now();
                            let _ = child.start_kill();
                            let status = child.wait().await.ok();
                            pg_guard.disarm();
                            status
                        }
                        res = child.wait() => {
                            pg_guard.disarm();
                            Some(res.map_err(|e| format!("failed to wait for shell command: {e}"))?)
                        }
                    }
                }
            };

            let (output, output_truncated) = buffer.prefix();
            let structured = |exit_code| {
                json!({
                    "exit_code": exit_code,
                    "output": output,
                    "output_truncated": output_truncated,
                })
            };

            match stop_kind {
                StopReasonKind::Aborted => {
                    let notice = "Command aborted.";
                    let (text, truncated, omitted_bytes) =
                        render_with_notice(&buffer, config.max_output_bytes, notice);
                    let mut res = AgentToolResult::error(text);
                    res.details = json!({
                        "cwd": shown_cwd,
                        "aborted": true,
                        "truncated": truncated,
                        "omitted_bytes": omitted_bytes,
                    });
                    Ok(res)
                }
                StopReasonKind::TimedOut(ms) => {
                    let notice = format!("Command timed out after {ms}ms.");
                    let (text, truncated, omitted_bytes) =
                        render_with_notice(&buffer, config.max_output_bytes, &notice);
                    let mut res = AgentToolResult::error(text);
                    res.details = json!({
                        "cwd": shown_cwd,
                        "timed_out": true,
                        "timeout_ms": ms,
                        "truncated": truncated,
                        "omitted_bytes": omitted_bytes,
                    });
                    res.structured_content = Some(structured(124));
                    Ok(res)
                }
                StopReasonKind::Completed => {
                    let status = exit_status.expect("completed command must have exit status");
                    let code = status.code();
                    if status.success() {
                        let rendered = buffer.render(config.max_output_bytes);
                        let mut res = AgentToolResult::text(rendered.text);
                        res.details = json!({
                            "cwd": shown_cwd,
                            "exit_code": code.unwrap_or(0),
                            "truncated": rendered.truncated,
                            "omitted_bytes": rendered.omitted_bytes,
                        });
                        res.structured_content = Some(structured(0));
                        Ok(res)
                    } else {
                        let notice = match code {
                            Some(c) => format!("Command exited with code {c}."),
                            None => format_signal_notice(&status),
                        };
                        let (text, truncated, omitted_bytes) =
                            render_with_notice(&buffer, config.max_output_bytes, &notice);
                        let mut res = AgentToolResult::error(text);
                        res.details = json!({
                            "cwd": shown_cwd,
                            "exit_code": code,
                            "truncated": truncated,
                            "omitted_bytes": omitted_bytes,
                        });
                        res.structured_content = Some(structured(shell_exit_code(&status)));
                        Ok(res)
                    }
                }
            }
        })
    }
}

pub(crate) fn build_shell_command(shell: Option<&str>, command: &str) -> Command {
    #[cfg(unix)]
    {
        let bin = shell.unwrap_or("/bin/sh");
        let mut cmd = Command::new(bin);
        cmd.arg("-c").arg(command);
        cmd
    }

    #[cfg(not(unix))]
    {
        let bin = shell.unwrap_or("cmd");
        let mut cmd = Command::new(bin);
        cmd.arg("/C").arg(command);
        cmd
    }
}

#[cfg(unix)]
fn format_signal_notice(status: &std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match status.signal() {
        Some(sig) => format!("Command terminated by signal {sig}."),
        None => "Command failed.".to_string(),
    }
}

#[cfg(not(unix))]
fn format_signal_notice(_status: &std::process::ExitStatus) -> String {
    "Command failed.".to_string()
}

fn render_with_notice(
    buffer: &BoundedOutputBuffer,
    max_bytes: usize,
    notice: &str,
) -> (String, bool, usize) {
    let sep_and_notice_len = notice.len() + 1;
    let body_budget = max_bytes.saturating_sub(sep_and_notice_len);
    let rendered = buffer.render(body_budget);
    let text = if rendered.text.is_empty() {
        notice.to_string()
    } else if rendered.text.ends_with('\n') {
        format!("{}{notice}", rendered.text)
    } else {
        format!("{}\n{notice}", rendered.text)
    };
    (text, rendered.truncated, rendered.omitted_bytes)
}

fn shell_exit_code(status: &std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    status.code().unwrap_or(1)
}
