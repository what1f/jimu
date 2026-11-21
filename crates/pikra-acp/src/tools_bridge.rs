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
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

struct Call {
    fields: acp::ToolCallUpdateFields,
    changes: Vec<Change>,
    ended: bool,
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum Permission {
    Files,
    Execute,
    Tool(String),
}

pub(crate) struct Bridge {
    pub(crate) id: acp::SessionId,
    pub(crate) cwd: PathBuf,
    pub(crate) capabilities: acp::ClientCapabilities,
    pub(crate) peer: Arc<Peer>,
    pub(crate) mode: Mutex<Mode>,
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
        self.calls.lock().unwrap().clear();
        *self.turn.lock().unwrap() = Some(cancel);
    }

    pub(crate) fn settle(&self) {
        for (id, call) in self.calls.lock().unwrap().drain() {
            if !call.ended {
                self.tool_update(&id, call.fields.status(acp::ToolCallStatus::Failed));
            }
        }
        *self.turn.lock().unwrap() = None;
    }

    pub(crate) fn path(&self, path: impl AsRef<Path>) -> PathBuf {
        self.cwd.join(path)
    }

    pub(crate) async fn read_ide(
        &self,
        path: &Path,
        line: Option<u32>,
        limit: Option<u32>,
        cancel: &CancelToken,
    ) -> Option<String> {
        if !self.capabilities.fs.read_text_file {
            return None;
        }
        let request = acp::ReadTextFileRequest::new(self.id.clone(), path)
            .line(line)
            .limit(limit);
        self.peer
            .request::<acp::ReadTextFileResponse>("fs/read_text_file", request, cancel)
            .await
            .ok()
            .map(|response| response.content)
    }

    pub(crate) fn wrap(self: &Arc<Self>, tool: Arc<dyn AgentTool>) -> Arc<dyn AgentTool> {
        self.builtins.lock().unwrap().insert(tool.definition().name);
        Arc::new(IdeTool {
            original: tool,
            bridge: self.clone(),
        })
    }

    fn is_builtin(&self, name: &str) -> bool {
        self.builtins.lock().unwrap().contains(name)
    }

    fn local_permission(&self, name: &str) -> Option<Permission> {
        match name {
            "skill_search" | "tool_search" | "exec" | "call_tool" => None, // Their delegated tools pass through this middleware.
            _ if !self.is_builtin(name) => Some(Permission::Tool(name.into())),
            "read" | "find" | "grep" | "web_fetch" | "sleep" => None,
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
        if mode == Mode::Plan {
            return Err("Plan mode allows inspection only. Present a plan without changing files or running commands.".into());
        }
        if mode == Mode::Bypass
            || (mode == Mode::AcceptEdits && *permission == Permission::Files && inside)
        {
            return Ok(());
        }
        let _guard = tokio::select! {
            guard = self.permission_gate.lock() => guard,
            () = cancel.cancelled() => return Err("Operation cancelled.".into()),
        };
        let scope = match permission {
            // Remember a workspace approval separately from each external target.
            Permission::Files if !inside => {
                let calls = self.calls.lock().unwrap();
                let mut targets: Vec<String> = calls[id]
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
        if let Some(allowed) = self.permissions.lock().unwrap().get(&scope).copied() {
            return if allowed {
                Ok(())
            } else {
                Err("Permission was denied.".into())
            };
        }
        let fields = self.calls.lock().unwrap()[id].fields.clone();
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
            .map_err(|_| "Permission was not granted.".to_string())?;
        match response.outcome {
            acp::RequestPermissionOutcome::Cancelled => {
                if let Some(turn) = self.turn.lock().unwrap().as_ref() {
                    turn.cancel();
                }
                cancel.cancel();
                Err("Operation cancelled.".into())
            }
            acp::RequestPermissionOutcome::Selected(choice) => match choice.option_id.0.as_ref() {
                "allow_once" => Ok(()),
                "allow_always" => {
                    self.permissions.lock().unwrap().insert(scope, true);
                    Ok(())
                }
                "reject_always" => {
                    self.permissions.lock().unwrap().insert(scope, false);
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
        let mode = *self.mode.lock().unwrap();
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
        self.calls.lock().unwrap().insert(
            id.into(),
            Call {
                fields,
                changes,
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
        if let Some(call) = self.calls.lock().unwrap().get(id)
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
        if let Some(call) = self.calls.lock().unwrap().get_mut(id) {
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
            .or_else(|| args.get("cmd"))
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
            .and_then(Value::as_u64)
            .or_else(|| {
                args.get("timeout")
                    .and_then(Value::as_u64)
                    .map(|seconds| seconds.saturating_mul(1000))
            })
            .map_or(Duration::from_secs(600), |ms| {
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
        let content = vec![acp::ToolCallContent::Terminal(acp::Terminal::new(
            terminal.clone(),
        ))];
        if let Some(call) = self.calls.lock().unwrap().get_mut(id) {
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
        let text = output
            .as_ref()
            .map(|output| output.output.as_str())
            .unwrap_or_default();
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
        if output.is_ok_and(|output| output.truncated) {
            result
                .content
                .push(pikra_ai::ContentPart::Text(pikra_ai::TextContent::plain(
                    "Output was truncated.",
                )));
        }
        Ok(result)
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
                        && let Some(content) = self
                            .bridge
                            .read_ide(&path, number("offset"), number("limit"), &cancel)
                            .await
                    {
                        return Ok(AgentToolResult::text(content));
                    }
                }
                "write" | "edit" | "apply_patch" => {
                    let changes = self
                        .bridge
                        .calls
                        .lock()
                        .unwrap()
                        .get_mut(&id)
                        .map(|call| std::mem::take(&mut call.changes))
                        .ok_or("The edit was not approved.")?;
                    return edits::apply(&self.bridge, &changes, &cancel).await;
                }
                "bash" | "exec_command" if self.bridge.capabilities.terminal => {
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
            if !self.calls.lock().unwrap().contains_key(id) {
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
                    .unwrap()
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
        "bash" | "exec_command" => (Execute, "Run command"),
        "write_stdin" => (Execute, "Send terminal input"),
        "monitor" => (Execute, "Monitor command"),
        "sleep" => (Other, "Wait"),
        "web_fetch" => (Fetch, "Open webpage"),
        "skill_search" => (Search, "Find skills"),
        "tool_search" => (Search, "Find tools"),
        "exec" => (Execute, "Run tools"),
        _ => (Other, name),
    }
}
