use crate::{
    edits::{self, Change},
    rpc::{CLIENT_TIMEOUT, Peer},
    schema::v1 as acp,
    session::Mode,
    turn::content_block,
};
use pikra_agent::{
    AfterToolCallCtx, AfterToolCallResult, AgentMiddleware, AgentTool, AgentToolResult,
    BeforeToolCallCtx, BeforeToolCallResult, FinishTurnCtx, ToolExecutionMode,
    ToolProgressCallback, TurnDecision,
};
use pikra_ai::{BoxFuture, CancelToken, ToolDefinition};
use pikra_tools::WebFetchTool;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use url::Origin;

struct Call {
    fields: acp::ToolCallUpdateFields,
    changes: Vec<Change>,
    private_origin: Option<Origin>,
    ended: bool,
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum Permission {
    Files,
    Execute,
    Tool(String),
    WebFetch(Origin),
}

pub(crate) struct Bridge {
    pub(crate) id: acp::SessionId,
    pub(crate) cwd: PathBuf,
    pub(crate) capabilities: acp::ClientCapabilities,
    pub(crate) peer: Arc<Peer>,
    pub(crate) mode: Mutex<Mode>,
    web_fetch: Option<Arc<WebFetchTool>>,
    roots: Vec<PathBuf>,
    permissions: Mutex<HashMap<Permission, bool>>,
    calls: Mutex<HashMap<String, Call>>,
    builtins: Mutex<HashSet<String>>,
    turn: Mutex<Option<CancelToken>>,
    permission_gate: tokio::sync::Mutex<()>,
}

impl Bridge {
    pub(crate) fn new(
        id: acp::SessionId,
        cwd: PathBuf,
        additional: &[PathBuf],
        mode: Mode,
        capabilities: acp::ClientCapabilities,
        peer: Arc<Peer>,
        web_fetch: Option<Arc<WebFetchTool>>,
    ) -> Self {
        let roots = std::iter::once(&cwd)
            .chain(additional)
            .filter_map(|root| std::fs::canonicalize(root).ok())
            .collect();
        Self {
            id,
            cwd,
            capabilities,
            peer,
            web_fetch,
            roots,
            mode: Mutex::new(mode),
            permissions: Mutex::default(),
            calls: Mutex::default(),
            builtins: Mutex::default(),
            turn: Mutex::default(),
            permission_gate: tokio::sync::Mutex::new(()),
        }
    }

    pub(crate) fn update(&self, update: acp::SessionUpdate) {
        self.peer.notify(
            "session/update",
            acp::SessionNotification::new(self.id.clone(), update),
        );
    }

    pub(crate) fn start_turn(&self, cancel: CancelToken) {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *self.turn.lock().unwrap_or_else(|e| e.into_inner()) = Some(cancel);
    }

    pub(crate) fn settle(&self) {
        for (id, call) in self.calls.lock().unwrap_or_else(|e| e.into_inner()).drain() {
            if !call.ended {
                self.tool_update(&id, call.fields.status(acp::ToolCallStatus::Failed));
            }
        }
        *self.turn.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub(crate) fn path(&self, path: impl AsRef<Path>) -> PathBuf {
        self.cwd.join(path)
    }

    pub(crate) fn is_inside_roots(&self, path: &Path) -> bool {
        let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.roots.iter().any(|root| resolved.starts_with(root))
    }

    pub(crate) async fn read_ide(
        &self,
        path: &Path,
        line: Option<u32>,
        limit: Option<u32>,
        cancel: &CancelToken,
    ) -> Option<Result<String, acp::Error>> {
        if !self.capabilities.fs.read_text_file {
            return None;
        }
        let request = acp::ReadTextFileRequest::new(self.id.clone(), path)
            .line(line)
            .limit(limit);
        Some(
            self.peer
                .request::<acp::ReadTextFileResponse>("fs/read_text_file", request, cancel)
                .await
                .map(|response| response.content),
        )
    }

    pub(crate) fn wrap(self: &Arc<Self>, tool: Arc<dyn AgentTool>) -> Arc<dyn AgentTool> {
        self.builtins
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(tool.definition().name);
        Arc::new(IdeTool {
            original: tool,
            bridge: self.clone(),
        })
    }

    fn is_builtin(&self, name: &str) -> bool {
        self.builtins
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(name)
    }

    fn local_permission(&self, name: &str) -> Option<Permission> {
        match name {
            "tool_search" | "exec" | "call_tool" => None, // Their delegated tools pass through this middleware.
            _ if !self.is_builtin(name) => Some(Permission::Tool(name.into())),
            "read" | "find" | "grep" | "web_fetch" => None,
            "write" | "edit" | "apply_patch" => Some(Permission::Files),
            _ => Some(Permission::Execute),
        }
    }

    async fn approve(
        &self,
        id: &str,
        permission: &Permission,
        mode: Mode,
        inside: bool,
        cancel: &CancelToken,
    ) -> Result<(), String> {
        if mode == Mode::Plan && !matches!(permission, Permission::WebFetch(_)) {
            return Err("Plan mode allows inspection only. Present a plan without changing files or running commands.".into());
        }
        if mode == Mode::Bypass
            || (mode == Mode::AcceptEdits && *permission == Permission::Files && inside)
        {
            return Ok(());
        }
        let _guard = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err("Operation cancelled.".into()),
            guard = self.permission_gate.lock() => guard,
        };
        let scope = match permission {
            // Remember a workspace approval separately from each external target.
            Permission::Files if !inside => {
                let calls = self.calls.lock().unwrap_or_else(|e| e.into_inner());
                let Some(call) = calls.get(id) else {
                    return Err("Tool call state missing.".into());
                };
                let mut targets: Vec<String> = call
                    .changes
                    .iter()
                    .map(|change| {
                        change
                            .target
                            .as_ref()
                            .unwrap_or(&change.path)
                            .to_string_lossy()
                            .into_owned()
                    })
                    .collect();
                targets.sort();
                Permission::Tool(format!("files:{}", json!(targets)))
            }
            _ => permission.clone(),
        };
        if let Some(allowed) = self
            .permissions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&scope)
            .copied()
        {
            return if allowed {
                Ok(())
            } else {
                Err("Permission was denied.".into())
            };
        }
        let fields = self
            .calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .map(|call| call.fields.clone())
            .ok_or_else(|| "Tool call state missing.".to_string())?;
        let options = vec![
            acp::PermissionOption::new(
                "allow_once",
                "Allow once",
                acp::PermissionOptionKind::AllowOnce,
            ),
            acp::PermissionOption::new(
                "allow_always",
                "Always allow",
                acp::PermissionOptionKind::AllowAlways,
            ),
            acp::PermissionOption::new(
                "reject_once",
                "Deny once",
                acp::PermissionOptionKind::RejectOnce,
            ),
            acp::PermissionOption::new(
                "reject_always",
                "Always deny",
                acp::PermissionOptionKind::RejectAlways,
            ),
        ];
        let request = acp::RequestPermissionRequest::new(
            self.id.clone(),
            acp::ToolCallUpdate::new(id.to_owned(), fields),
            options,
        );
        let response = self
            .peer
            .request::<acp::RequestPermissionResponse>(
                "session/request_permission",
                request,
                cancel,
            )
            .await
            .map_err(|_| {
                if cancel.is_cancelled() {
                    "Operation cancelled.".to_string()
                } else {
                    "Permission was not granted.".to_string()
                }
            })?;
        if cancel.is_cancelled() {
            return Err("Operation cancelled.".into());
        }
        match response.outcome {
            acp::RequestPermissionOutcome::Cancelled => {
                if let Some(turn) = self.turn.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                    turn.cancel();
                }
                cancel.cancel();
                Err("Operation cancelled.".into())
            }
            acp::RequestPermissionOutcome::Selected(choice) => match choice.option_id.0.as_ref() {
                "allow_once" => Ok(()),
                "allow_always" => {
                    self.permissions
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(scope, true);
                    Ok(())
                }
                "reject_always" => {
                    self.permissions
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(scope, false);
                    Err("Permission was denied.".into())
                }
                _ => Err("Permission was denied.".into()),
            },
            _ => Err("Permission was not granted.".into()),
        }
    }

    async fn prepare(
        &self,
        id: &str,
        name: &str,
        args: &Value,
        cancel: &CancelToken,
    ) -> Result<(), String> {
        if matches!(name, "call_tool" | "exec") {
            return Ok(());
        }
        let mode = *self.mode.lock().unwrap_or_else(|e| e.into_inner());
        let permission = self.local_permission(name);
        if mode == Mode::Plan && permission.is_some() {
            self.begin(id, name, args, Vec::new());
            return Err("Plan mode allows inspection only. Present a plan without changing files or running commands.".into());
        }
        let (changes, failure) = if permission == Some(Permission::Files) {
            match edits::preview(self, name, args, cancel).await {
                Ok(changes) => (changes, None),
                Err(error) => (Vec::new(), Some(error)),
            }
        } else {
            (Vec::new(), None)
        };
        let inside = changes.iter().all(|change| {
            change
                .target
                .as_ref()
                .is_some_and(|target| self.roots.iter().any(|root| target.starts_with(root)))
        });
        self.begin(id, name, args, changes);
        if let Some(failure) = failure {
            return Err(failure);
        }
        if let Some(permission) = permission {
            self.approve(id, &permission, mode, inside, cancel).await?;
        }
        if let Some(web_fetch) = self.web_fetch.as_ref().filter(|_| name == "web_fetch") {
            let url = args["url"].as_str().ok_or("Enter an HTTP or HTTPS URL")?;
            if let Some(origin) = web_fetch
                .private_origin(url, cancel)
                .await
                .map_err(|error| error.to_string())?
            {
                self.approve(
                    id,
                    &Permission::WebFetch(origin.clone()),
                    mode,
                    inside,
                    cancel,
                )
                .await?;
                let mut calls = self.calls.lock().unwrap_or_else(|e| e.into_inner());
                let call = calls.get_mut(id).ok_or("Tool call state missing.")?;
                call.private_origin = Some(origin);
            }
        }
        if cancel.is_cancelled() {
            return Err("Operation cancelled.".into());
        }
        self.tool_update(
            id,
            acp::ToolCallUpdateFields::new().status(acp::ToolCallStatus::InProgress),
        );
        Ok(())
    }

    fn begin(&self, id: &str, name: &str, args: &Value, changes: Vec<Change>) {
        let (name, args) = delegated(name, args, &Value::Null);
        let (kind, title) = presentation(name);
        let content: Vec<_> = changes.iter().map(Change::diff).collect();
        let mut locations: Vec<_> = changes.iter().map(Change::location).collect();
        if locations.is_empty()
            && let Some(path) = args.get("path").and_then(Value::as_str)
        {
            locations.push(acp::ToolCallLocation::new(self.path(path)));
        }
        let call = acp::ToolCall::new(id.to_owned(), title)
            .name(name.to_owned())
            .kind(kind)
            .status(acp::ToolCallStatus::Pending)
            .content(content.clone())
            .locations(locations.clone())
            .raw_input(args.clone());
        let fields = acp::ToolCallUpdateFields::new()
            .title(title.to_string())
            .name(name.to_owned())
            .kind(kind)
            .status(acp::ToolCallStatus::Pending)
            .content(content)
            .locations(locations)
            .raw_input(args.clone());
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).insert(
            id.into(),
            Call {
                fields,
                changes,
                private_origin: None,
                ended: false,
            },
        );
        self.update(acp::SessionUpdate::ToolCall(call));
    }

    fn tool_update(&self, id: &str, fields: acp::ToolCallUpdateFields) {
        self.update(acp::SessionUpdate::ToolCallUpdate(
            acp::ToolCallUpdate::new(id.to_owned(), fields),
        ));
    }

    fn progress(&self, id: &str, result: &AgentToolResult) {
        if let Some(call) = self.calls.lock().unwrap_or_else(|e| e.into_inner()).get(id)
            && !call.ended
            && call.fields.content.as_ref().is_none_or(Vec::is_empty)
        {
            let content: Vec<_> = result
                .content
                .iter()
                .map(|part| acp::ToolCallContent::Content(acp::Content::new(content_block(part))))
                .collect();
            self.tool_update(
                id,
                acp::ToolCallUpdateFields::new()
                    .status(acp::ToolCallStatus::InProgress)
                    .content(content),
            );
        }
    }

    fn finish(&self, id: &str, result: &AgentToolResult) {
        if let Some(call) = self
            .calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(id)
        {
            if call.ended {
                return;
            }
            call.ended = true;
            let mut fields = call
                .fields
                .clone()
                .status(if result.is_error {
                    acp::ToolCallStatus::Failed
                } else {
                    acp::ToolCallStatus::Completed
                })
                .raw_output(
                    result
                        .structured_content
                        .clone()
                        .unwrap_or_else(|| json!(result.content)),
                );
            if fields.content.as_ref().is_none_or(Vec::is_empty) {
                let content: Vec<_> = result
                    .content
                    .iter()
                    .map(|part| {
                        acp::ToolCallContent::Content(acp::Content::new(content_block(part)))
                    })
                    .collect();
                fields = fields.content(content);
            }
            self.tool_update(id, fields);
        }
    }

    async fn terminal(
        &self,
        id: &str,
        args: &Value,
        cancel: &CancelToken,
    ) -> Result<AgentToolResult, String> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .filter(|command| !command.trim().is_empty() && !command.contains('\0'))
            .ok_or("A shell command is required.")?;
        let cwd = args
            .get("cwd")
            .and_then(Value::as_str)
            .map_or_else(|| self.cwd.clone(), |path| self.path(path));
        let shell = Shell::default();
        let timeout = args
            .get("timeout_ms")
            .and_then(|value| {
                value.as_u64().or_else(|| {
                    value
                        .as_f64()
                        .filter(|v| v.is_finite() && *v >= 0.0)
                        .map(|v| v.round() as u64)
                })
            })
            .map_or(Duration::from_secs(120), |ms| {
                Duration::from_millis(ms.clamp(1, 86_400_000))
            });
        if cancel.is_cancelled() {
            return Err("Operation cancelled.".into());
        }
        let create = acp::CreateTerminalRequest::new(self.id.clone(), shell.program)
            .args(vec![shell.flag.into(), command.into()])
            .cwd(cwd)
            .output_byte_limit(1_048_576);
        // Obtain the terminal ID even if the turn is cancelled during creation, so we can release it.
        let cleanup = CancelToken::new();
        let terminal = self
            .peer
            .request::<acp::CreateTerminalResponse>("terminal/create", create, &cleanup)
            .await
            .map_err(|_| "The editor could not start a terminal.")?
            .terminal_id;
        let mut terminal_guard =
            TerminalCleanupGuard::new(self.peer.clone(), self.id.clone(), terminal.clone());
        let content = vec![acp::ToolCallContent::Terminal(acp::Terminal::new(
            terminal.clone(),
        ))];
        if let Some(call) = self
            .calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(id)
        {
            call.fields.content = Some(content.clone());
        }
        self.tool_update(
            id,
            acp::ToolCallUpdateFields::new()
                .status(acp::ToolCallStatus::InProgress)
                .content(content),
        );
        let wait = acp::WaitForTerminalExitRequest::new(self.id.clone(), terminal.clone());
        let status = self
            .peer
            .request_with_timeout::<acp::WaitForTerminalExitResponse>(
                "terminal/wait_for_exit",
                wait,
                cancel,
                timeout,
            )
            .await;
        let release = acp::ReleaseTerminalRequest::new(self.id.clone(), terminal.clone());
        let kill = acp::KillTerminalRequest::new(self.id.clone(), terminal.clone());
        if self.peer.closed.is_cancelled() {
            terminal_guard.disarm();
            self.peer.send_cleanup("terminal/kill", kill);
            self.peer.send_cleanup("terminal/release", release);
            return Ok(AgentToolResult::error("Command cancelled."));
        }
        if status.is_err() {
            let _ = self
                .peer
                .request_with_timeout::<acp::KillTerminalResponse>(
                    "terminal/kill",
                    kill,
                    &cleanup,
                    Duration::from_secs(5),
                )
                .await;
        }
        let output = self
            .peer
            .request_with_timeout::<acp::TerminalOutputResponse>(
                "terminal/output",
                acp::TerminalOutputRequest::new(self.id.clone(), terminal),
                &cleanup,
                Duration::from_secs(5),
            )
            .await;
        terminal_guard.disarm();
        if self.peer.closed.is_cancelled() {
            self.peer.send_cleanup("terminal/release", release);
        } else {
            let _ = self
                .peer
                .request_with_timeout::<acp::ReleaseTerminalResponse>(
                    "terminal/release",
                    release,
                    &cleanup,
                    Duration::from_secs(5),
                )
                .await;
        }
        let raw_text = output
            .as_ref()
            .map(|output| output.output.as_str())
            .unwrap_or_default();
        let trunc = pikra_tools::truncate_tail_spillover(
            raw_text,
            pikra_tools::DEFAULT_MAX_OUTPUT_BYTES,
            pikra_tools::DEFAULT_MAX_OUTPUT_LINES,
        );
        let text = trunc.text.as_str();
        if cancel.is_cancelled() {
            return Ok(AgentToolResult::error(format!(
                "Command cancelled.\n{text}"
            )));
        }
        let exit = match status {
            Ok(status) => status.exit_status,
            Err(error) => {
                let message = if error.code == CLIENT_TIMEOUT.into() {
                    "Command timed out."
                } else {
                    "Command did not finish."
                };
                return Ok(AgentToolResult::error(format!("{message}\n{text}")));
            }
        };
        let mut result = AgentToolResult::text(format!(
            "Exit code: {}{}\n{text}",
            exit.exit_code
                .map_or_else(|| "none".into(), |code| code.to_string()),
            exit.signal
                .map_or_else(String::new, |signal| format!(" ({signal})"))
        ));
        result.is_error = exit.exit_code != Some(0) || output.is_err();
        if !trunc.truncated && output.is_ok_and(|output| output.truncated) {
            result
                .content
                .push(pikra_ai::ContentPart::Text(pikra_ai::TextContent::plain(
                    "Output was truncated.",
                )));
        }
        Ok(result)
    }
}

struct TerminalCleanupGuard {
    peer: Arc<Peer>,
    session_id: acp::SessionId,
    terminal_id: Option<acp::TerminalId>,
}

impl TerminalCleanupGuard {
    fn new(peer: Arc<Peer>, session_id: acp::SessionId, terminal_id: acp::TerminalId) -> Self {
        Self {
            peer,
            session_id,
            terminal_id: Some(terminal_id),
        }
    }

    fn disarm(&mut self) {
        self.terminal_id = None;
    }
}

impl Drop for TerminalCleanupGuard {
    fn drop(&mut self) {
        let Some(terminal_id) = self.terminal_id.take() else {
            return;
        };
        let peer = self.peer.clone();
        let session_id = self.session_id.clone();
        let kill = acp::KillTerminalRequest::new(session_id.clone(), terminal_id.clone());
        let release = acp::ReleaseTerminalRequest::new(session_id, terminal_id);
        if peer.closed.is_cancelled() {
            peer.send_cleanup("terminal/kill", kill);
            peer.send_cleanup("terminal/release", release);
            return;
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let cleanup = CancelToken::new();
                let _ = peer
                    .request_with_timeout::<acp::KillTerminalResponse>(
                        "terminal/kill",
                        kill,
                        &cleanup,
                        Duration::from_secs(5),
                    )
                    .await;
                let _ = peer
                    .request_with_timeout::<acp::ReleaseTerminalResponse>(
                        "terminal/release",
                        release,
                        &cleanup,
                        Duration::from_secs(5),
                    )
                    .await;
            });
        } else {
            peer.send_cleanup("terminal/kill", kill);
            peer.send_cleanup("terminal/release", release);
        }
    }
}

struct Shell {
    program: &'static str,
    flag: &'static str,
}
impl Default for Shell {
    fn default() -> Self {
        if cfg!(windows) {
            Self {
                program: "cmd",
                flag: "/C",
            }
        } else {
            Self {
                program: "/bin/sh",
                flag: "-lc",
            }
        }
    }
}

struct IdeTool {
    original: Arc<dyn AgentTool>,
    bridge: Arc<Bridge>,
}

impl AgentTool for IdeTool {
    fn definition(&self) -> ToolDefinition {
        self.original.definition()
    }
    fn execution_mode(&self) -> ToolExecutionMode {
        self.original.execution_mode()
    }
    fn execute(
        &self,
        id: String,
        args: Value,
        cancel: CancelToken,
        on_update: ToolProgressCallback,
    ) -> BoxFuture<'_, Result<AgentToolResult, String>> {
        Box::pin(async move {
            let name = self.definition().name;
            match name.as_str() {
                "read" => {
                    let path = self.bridge.path(edits::string_arg(&args, "path")?);
                    let is_image =
                        path.extension()
                            .and_then(|ext| ext.to_str())
                            .is_some_and(|ext| {
                                matches!(
                                    ext.to_ascii_lowercase().as_str(),
                                    "png" | "jpg" | "jpeg" | "gif" | "webp"
                                )
                            });
                    let number = |field: &str| {
                        args[field]
                            .as_u64()
                            .and_then(|value| u32::try_from(value).ok())
                    };
                    if !is_image
                        && let Some(result) = self
                            .bridge
                            .read_ide(&path, number("offset"), number("limit"), &cancel)
                            .await
                    {
                        return match result {
                            Ok(content) => {
                                let clipped: String = content
                                    .lines()
                                    .map(|line| {
                                        if line.len() > 2_000 && line.chars().nth(2_000).is_some() {
                                            let total = line.chars().count();
                                            let cut = line
                                                .char_indices()
                                                .nth(2_000)
                                                .map_or(line.len(), |(idx, _)| idx);
                                            format!(
                                                "{} ... [line truncated, {total} chars]",
                                                &line[..cut]
                                            )
                                        } else {
                                            line.to_owned()
                                        }
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                let trunc = pikra_tools::truncate_head(
                                    &clipped,
                                    pikra_tools::DEFAULT_MAX_OUTPUT_BYTES,
                                    pikra_tools::DEFAULT_MAX_OUTPUT_LINES,
                                );
                                Ok(AgentToolResult::text(trunc.text))
                            }
                            Err(_) if cancel.is_cancelled() => Err("Operation cancelled.".into()),
                            Err(err) => Err(err.message),
                        };
                    }
                }
                "write" | "edit" | "apply_patch" => {
                    let changes = self
                        .bridge
                        .calls
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get_mut(&id)
                        .map(|call| std::mem::take(&mut call.changes))
                        .ok_or("The edit was not approved.")?;
                    return edits::apply(&self.bridge, &changes, &cancel).await;
                }
                "web_fetch" => {
                    let origin = self
                        .bridge
                        .calls
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get_mut(&id)
                        .and_then(|call| call.private_origin.take());
                    if let (Some(origin), Some(tool)) = (origin, &self.bridge.web_fetch) {
                        return tool
                            .as_ref()
                            .clone()
                            .with_private_origin(origin)
                            .execute(id, args, cancel, on_update)
                            .await;
                    }
                }
                "exec_command"
                    if self.bridge.capabilities.terminal
                        && !args["tty"].as_bool().unwrap_or(false)
                        && !args["background"].as_bool().unwrap_or(false)
                        && args.get("yield_time_ms").is_none() =>
                {
                    return self.bridge.terminal(&id, &args, &cancel).await;
                }
                _ => {}
            }
            let bridge = self.bridge.clone();
            let progress_id = id.clone();
            let progress = Arc::new(move |result: AgentToolResult| {
                bridge.progress(&progress_id, &result);
                on_update(result);
            });
            self.original.execute(id, args, cancel, progress).await
        })
    }
}

impl AgentMiddleware for Bridge {
    fn before_tool_call<'a>(
        &'a self,
        ctx: BeforeToolCallCtx<'a>,
        cancel: CancelToken,
    ) -> BoxFuture<'a, Option<BeforeToolCallResult>> {
        Box::pin(async move {
            let id = &ctx.tool_call.id;
            match self
                .prepare(id, &ctx.tool_call.name, ctx.args, &cancel)
                .await
            {
                Ok(()) => None,
                Err(error) => {
                    self.finish(id, &AgentToolResult::error(&error));
                    Some(BeforeToolCallResult {
                        block: true,
                        reason: Some(error),
                        ..Default::default()
                    })
                }
            }
        })
    }

    fn after_tool_call<'a>(
        &'a self,
        ctx: AfterToolCallCtx<'a>,
        _cancel: CancelToken,
    ) -> BoxFuture<'a, Option<AfterToolCallResult>> {
        Box::pin(async move {
            let id = &ctx.tool_call.id;
            if !self
                .calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(id)
            {
                let (name, args) = delegated(&ctx.tool_call.name, ctx.args, &ctx.result.details);
                self.begin(id, name, args, Vec::new());
            }
            self.finish(id, ctx.result);
            None
        })
    }

    fn finish_turn<'a>(
        &'a self,
        ctx: FinishTurnCtx<'a>,
        _cancel: CancelToken,
    ) -> BoxFuture<'a, Option<TurnDecision>> {
        Box::pin(async move {
            // Validation failures are produced before the before-tool middleware runs.
            for result in ctx.tool_results {
                if !self
                    .calls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains_key(&result.tool_call_id)
                    && let Some(call) = ctx
                        .assistant_message
                        .tool_calls()
                        .into_iter()
                        .find(|call| call.id == result.tool_call_id)
                {
                    self.begin(
                        &call.id,
                        &call.name,
                        &Value::Object(call.arguments.clone()),
                        Vec::new(),
                    );
                }
                let mut completed = AgentToolResult::text("");
                completed.content.clone_from(&result.content);
                completed.is_error = result.is_error;
                self.finish(&result.tool_call_id, &completed);
            }
            None
        })
    }
}

pub(crate) fn delegated<'a>(
    name: &'a str,
    args: &'a Value,
    details: &'a Value,
) -> (&'a str, &'a Value) {
    let delegated = &details["delegated_tool"];
    if let Some(name) = delegated["name"].as_str() {
        return (name, &delegated["arguments"]);
    }
    if name == "call_tool"
        && let Some(name) = args["name"].as_str()
    {
        return (name, &args["arguments"]);
    }
    (name, args)
}

pub(crate) fn presentation(name: &str) -> (acp::ToolKind, &str) {
    use acp::ToolKind::*;
    match name {
        "read" => (Read, "Read file"),
        "find" | "grep" => (Search, "Search workspace"),
        "write" => (Edit, "Write file"),
        "edit" => (Edit, "Edit file"),
        "apply_patch" => (Edit, "Apply patch"),
        "exec_command" => (Execute, "Run command"),
        "write_stdin" => (Execute, "Send terminal input"),
        "monitor" => (Execute, "Monitor command"),
        "web_fetch" => (Fetch, "Open webpage"),
        "tool_search" => (Search, "Find tools"),
        "exec" => (Execute, "Run tools"),
        _ => (Other, name),
    }
}
