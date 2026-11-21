use agent_client_protocol::{
    AcpAgent, AcpAgentConfig, Agent, Client,
    schema::ProtocolVersion,
    v1::{ConnectionTo, schema::*},
};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    task::JoinHandle,
};

fn tool(id: &str, name: &str, args: Value) -> Vec<Value> {
    vec![json!({"tool_calls":[{"index":0,"id":id,"type":"function",
        "function":{"name":name,"arguments":args.to_string()}}]})]
}
fn text(text: &str) -> Vec<Value> {
    vec![json!({"content":text})]
}

struct ModelFixture {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    server: JoinHandle<()>,
}

impl ModelFixture {
    async fn start(replies: Vec<Vec<Value>>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        let server = tokio::spawn(async move {
            for deltas in replies {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut length = None;
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).await.unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((header, value)) = line.split_once(':')
                        && header.eq_ignore_ascii_case("content-length")
                    {
                        length = Some(value.trim().parse().unwrap());
                    }
                }
                let mut body = vec![0; length.unwrap()];
                reader.read_exact(&mut body).await.unwrap();
                log.lock()
                    .unwrap()
                    .push(serde_json::from_slice(&body).unwrap());

                let has_tool = deltas.iter().any(|delta| delta.get("tool_calls").is_some());
                let mut body = String::new();
                for delta in deltas {
                    body += &format!(
                        "data: {}\n\n",
                        json!({"id":"fixture","choices":[{"index":0,"delta":delta}]})
                    );
                }
                body += &format!(
                    "data: {}\n\ndata: [DONE]\n\n",
                    json!({
                        "id":"fixture",
                        "choices":[{"index":0,"delta":{},"finish_reason":if has_tool {"tool_calls"} else {"stop"}}],
                        "usage":{"prompt_tokens":42,"completion_tokens":8,"total_tokens":50}
                    })
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        Self {
            url,
            requests,
            server,
        }
    }

    fn agent(&self, home: &Path, ephemeral: bool) -> AcpAgent {
        let config = AcpAgentConfig::new(env!("CARGO_BIN_EXE_pikra-acp"))
            .args([
                "--provider",
                "openai",
                "--api",
                "openai-completions",
                "--base-url",
                &self.url,
                "--pikra-home",
                home.to_str().unwrap(),
                "--api-key-env",
                "PIKRA_E2E_KEY",
            ])
            .env("PIKRA_E2E_KEY", "fixture-key-keep-private")
            .env("HOME", home.to_str().unwrap())
            .env("CODEX_HOME", home.join("codex").to_str().unwrap())
            .env("NO_PROXY", "127.0.0.1");
        let config = if ephemeral {
            config.arg("--no-session")
        } else {
            config
        };
        AcpAgent::new(config)
    }

    async fn finish(self) -> Vec<Value> {
        self.server.await.unwrap();
        Arc::try_unwrap(self.requests)
            .unwrap()
            .into_inner()
            .unwrap()
    }
}

type Updates = Arc<Mutex<Vec<SessionNotification>>>;
type Trace = Arc<Mutex<Vec<&'static str>>>;

fn approved() -> RequestPermissionResponse {
    RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
        SelectedPermissionOutcome::new("allow_once"),
    ))
}

async fn ask(
    cx: &ConnectionTo<Agent>,
    id: &SessionId,
    prompt: &str,
) -> Result<PromptResponse, agent_client_protocol::Error> {
    cx.send_request(PromptRequest::new(id.clone(), vec![prompt.into()]))
        .block_task()
        .await
}

#[tokio::test]
async fn official_client_drives_the_ide_workflow_over_stdio() {
    let mut read = vec![
        json!({"reasoning_content":"Consider the buffer."}),
        json!({"content":"I will check it."}),
    ];
    read.extend(tool(
        "read-id",
        "read",
        json!({"path":"input.txt","offset":2,"limit":1}),
    ));
    let model = ModelFixture::start(vec![
        read,
        tool("patch-id", "apply_patch", json!({"patch":"*** Begin Patch\n*** Update File: input.txt\n@@\n head\n-unsaved\n+new buffer\n*** End Patch"})),
        tool("bash-id", "exec_command", json!({"command":"printf fixture"})),
        text("Finished."),
    ]).await;
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let path = workspace.path().to_path_buf();
    std::fs::write(path.join("input.txt"), "head\non disk\n").unwrap();
    let skill = path.join(".agents/skills/check-buffer");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: check-buffer\ndescription: Check a buffer\n---\n",
    )
    .unwrap();
    let updates = Updates::default();
    let trace = Trace::default();

    Client
        .v1()
        .on_receive_notification(
            {
                let log = updates.clone();
                async move |notification: SessionNotification, _| {
                    log.lock().unwrap().push(notification);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_notification(
            async |status: AuthStatusUpdateNotification, _| {
                assert!(
                    !serde_json::to_string(&status)
                        .unwrap()
                        .contains("provider-fixture-secret")
                );
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let trace = trace.clone();
                async move |req: ReadTextFileRequest, reply, _| {
                    assert!(req.path.ends_with("input.txt"));
                    let content = if let Some(line) = req.line {
                        assert_eq!((line, req.limit), (2, Some(1)));
                        trace.lock().unwrap().push("read");
                        "unsaved"
                    } else {
                        "head\nunsaved\n"
                    };
                    reply.respond(ReadTextFileResponse::new(content))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let trace = trace.clone();
                async move |req: RequestPermissionRequest, reply, _| {
                    assert_eq!(req.options.len(), 4);
                    let fields = req.tool_call.fields;
                    if req.tool_call.tool_call_id.0.as_ref() == "patch-id" {
                        let Some([ToolCallContent::Diff(diff)]) = fields.content.as_deref() else {
                            panic!("missing preview")
                        };
                        assert_eq!(diff.old_text.as_deref(), Some("head\nunsaved\n"));
                        assert_eq!(diff.new_text, "head\nnew buffer\n");
                        assert_eq!(fields.locations.unwrap()[0].line, Some(2));
                        assert_eq!(fields.status, Some(ToolCallStatus::Pending));
                        trace.lock().unwrap().push("permission:patch");
                    } else {
                        trace.lock().unwrap().push("permission:shell");
                    }
                    reply.respond(approved())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let trace = trace.clone();
                async move |req: WriteTextFileRequest, reply, _| {
                    assert_eq!(req.content, "head\nnew buffer\n");
                    trace.lock().unwrap().push("write");
                    reply.respond(WriteTextFileResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let trace = trace.clone();
                async move |req: CreateTerminalRequest, reply, _| {
                    assert_eq!(req.args.last().map(String::as_str), Some("printf fixture"));
                    assert!(req.cwd.unwrap().is_absolute());
                    trace.lock().unwrap().push("create");
                    reply.respond(CreateTerminalResponse::new("terminal-1"))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let trace = trace.clone();
                async move |req: WaitForTerminalExitRequest, reply, _| {
                    assert_eq!(req.terminal_id.0.as_ref(), "terminal-1");
                    trace.lock().unwrap().push("wait");
                    reply.respond(WaitForTerminalExitResponse::new(
                        TerminalExitStatus::new().exit_code(0),
                    ))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let trace = trace.clone();
                async move |_req: TerminalOutputRequest, reply, _| {
                    trace.lock().unwrap().push("output");
                    reply.respond(TerminalOutputResponse::new("terminal output", false))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let trace = trace.clone();
                async move |_req: ReleaseTerminalRequest, reply, _| {
                    trace.lock().unwrap().push("release");
                    reply.respond(ReleaseTerminalResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(
            model.agent(home.path(), true),
            async |cx: ConnectionTo<Agent>| {
                let caps = ClientCapabilities::new()
                    .fs(FileSystemCapabilities::new()
                        .read_text_file(true)
                        .write_text_file(true))
                    .terminal(true);
                let init = cx
                    .send_request(
                        InitializeRequest::new(ProtocolVersion::V1).client_capabilities(caps),
                    )
                    .block_task()
                    .await?;
                assert_eq!(init.protocol_version, ProtocolVersion::V1);
                assert!(
                    init.agent_capabilities.mcp_capabilities.http
                        && init.agent_capabilities.load_session
                );
                assert!(init.agent_capabilities.prompt_capabilities.image);
                assert!(init.agent_capabilities.session_capabilities.fork.is_some());
                assert!(init.agent_capabilities.auth.status.is_some());
                assert_eq!(init.meta.unwrap()["steering"]["supported"], true);
                let status = cx
                    .send_request(AuthStatusRequest::new())
                    .block_task()
                    .await?;
                assert_eq!(status.state, AuthState::Authenticated);
                assert!(
                    !serde_json::to_string(&status)
                        .unwrap()
                        .contains("fixture-key")
                );
                let provider = SetProviderRequest::new(
                    "fixture",
                    LlmProtocol::OpenAi,
                    "https://fixture.invalid/v1",
                )
                .headers(
                    [(
                        "Authorization".into(),
                        "Bearer provider-fixture-secret".into(),
                    )]
                    .into_iter()
                    .collect(),
                )
                .meta(serde_json::from_value::<Meta>(json!({"models":["fixture-model"]})).unwrap());
                cx.send_request(provider).block_task().await?;
                let providers = cx
                    .send_request(ListProvidersRequest::new())
                    .block_task()
                    .await?;
                assert!(
                    providers
                        .providers
                        .iter()
                        .any(|provider| provider.provider_id.0.as_ref() == "fixture"
                            && provider.current.is_some())
                );
                assert!(
                    !serde_json::to_string(&providers)
                        .unwrap()
                        .contains("provider-fixture-secret")
                );
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let modes = std::fs::metadata(home.path().join("auth.json"))
                        .unwrap()
                        .permissions()
                        .mode();
                    assert_eq!(modes & 0o777, 0o600);
                }
                let routes =
                    std::fs::read_to_string(home.path().join("acp/providers.json")).unwrap();
                assert!(!routes.contains("provider-fixture-secret"));
                let options = cx
                    .send_request(SessionOptionsRequest::new(&path))
                    .block_task()
                    .await?;
                assert!(
                    serde_json::to_string(&options)
                        .unwrap()
                        .contains("fixture/fixture-model")
                );
                cx.send_request(DisableProviderRequest::new("fixture"))
                    .block_task()
                    .await?;
                let session = cx
                    .send_request(NewSessionRequest::new(&path))
                    .block_task()
                    .await?;
                assert_eq!(session.modes.unwrap().current_mode_id.0.as_ref(), "default");
                assert_eq!(session.config_options.unwrap().len(), 4);

                let image = ContentBlock::Image(ImageContent::new("AA==", "image/png"));
                let resource = ContentBlock::Resource(EmbeddedResource::new(
                    EmbeddedResourceResource::TextResourceContents(TextResourceContents::new(
                        "file:///context.txt",
                        "context text",
                    )),
                ));
                let prompt = PromptRequest::new(
                    session.session_id,
                    vec!["Inspect the buffer".into(), image, resource],
                );
                let response = cx.send_request(prompt).block_task().await?;
                assert_eq!(response.stop_reason, StopReason::EndTurn);
                assert_eq!(response.usage.unwrap().total_tokens, 200);
                Ok(())
            },
        )
        .await
        .unwrap();
    assert_eq!(
        *trace.lock().unwrap(),
        [
            "read",
            "permission:patch",
            "write",
            "permission:shell",
            "create",
            "wait",
            "output",
            "release"
        ]
    );
    assert_eq!(
        std::fs::read_to_string(path.join("input.txt")).unwrap(),
        "head\non disk\n"
    );
    let updates = Arc::try_unwrap(updates).unwrap().into_inner().unwrap();
    assert!(updates.iter().any(|n| matches!(&n.update, SessionUpdate::AvailableCommandsUpdate(commands) if commands.available_commands.iter().any(|c| c.name == "check-buffer"))));
    assert!(
        updates
            .iter()
            .any(|n| matches!(&n.update, SessionUpdate::AgentThoughtChunk(_)))
    );
    assert!(updates.iter().any(|n| matches!(&n.update, SessionUpdate::AgentMessageChunk(chunk) if chunk.content == "Finished.".into())));
    assert!(
        updates
            .iter()
            .any(|n| matches!(&n.update, SessionUpdate::UsageUpdate(usage) if usage.used == 50))
    );
    let patch_starts = updates.iter().filter(|n| matches!(&n.update, SessionUpdate::ToolCall(call) if call.tool_call_id.0.as_ref() == "patch-id")).count();
    assert_eq!(patch_starts, 1);
    assert!(updates.iter().any(
        |n| matches!(&n.update, SessionUpdate::ToolCallUpdate(update)
        if update.fields.status == Some(ToolCallStatus::Completed)
        && matches!(update.fields.content.as_deref(), Some([ToolCallContent::Terminal(_)])))
    ));
    let requests = model.finish().await;
    assert!(requests[0].to_string().contains("image_url"));
    assert!(requests[0].to_string().contains("context text"));
    assert!(requests[1].to_string().contains("unsaved"));
    assert!(requests[3].to_string().contains("terminal output"));
}

#[tokio::test]
async fn additional_workspace_instructions_follow_new_load_and_resume() {
    let model = ModelFixture::start(vec![text("OK"); 3]).await;
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let cwd = workspace.path().to_path_buf();
    let scopes = ["new", "load", "resume"].map(|scope| {
        let dir = cwd.join(scope);
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("AGENTS.md"), format!("instructions for {scope}")).unwrap();
        dir
    });
    std::fs::write(
        home.path().join("AGENTS.md"),
        "instructions from data directory",
    )
    .unwrap();
    Client
        .v1()
        .connect_with(
            model.agent(home.path(), false),
            async |cx: ConnectionTo<Agent>| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session = cx
                    .send_request(
                        NewSessionRequest::new(&cwd)
                            .additional_directories(vec![scopes[0].clone()]),
                    )
                    .block_task()
                    .await?
                    .session_id;
                ask(&cx, &session, "First").await?;
                cx.send_request(
                    LoadSessionRequest::new(session.clone(), &cwd)
                        .additional_directories(vec![scopes[1].clone()]),
                )
                .block_task()
                .await?;
                ask(&cx, &session, "Second").await?;
                cx.send_request(
                    ResumeSessionRequest::new(session.clone(), &cwd)
                        .additional_directories(vec![scopes[2].clone()]),
                )
                .block_task()
                .await?;
                ask(&cx, &session, "Third").await?;
                Ok(())
            },
        )
        .await
        .unwrap();
    for (request, scope) in model.finish().await.iter().zip(["new", "load", "resume"]) {
        let latest = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .filter_map(|message| message["content"].as_str())
            .find(|content| content.contains("<project_instructions>"))
            .unwrap();
        assert!(latest.contains("instructions from data directory"));
        assert!(latest.contains(&format!("instructions for {scope}")));
    }
}

#[tokio::test]
async fn persisted_sessions_replay_before_load_response_and_preserve_options() {
    let model = ModelFixture::start(vec![
        tool(
            "blocked",
            "write",
            json!({"path":"test.txt","content":"blocked"}),
        ),
        text("Plan prepared."),
        tool(
            "edit",
            "write",
            json!({"path":"test.txt","content":"accepted"}),
        ),
        tool(
            "shell",
            "exec_command",
            json!({"command":"printf local-shell"}),
        ),
        text("Workspace updated."),
    ])
    .await;
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let cwd = workspace.path().to_path_buf();
    let id = Arc::new(Mutex::new(None));
    let trace = Trace::default();
    Client
        .v1()
        .on_receive_request(
            {
                let trace = trace.clone();
                async move |req: RequestPermissionRequest, reply, _| {
                    assert_eq!(req.tool_call.tool_call_id.0.as_ref(), "shell");
                    trace.lock().unwrap().push("shell");
                    reply.respond(approved())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(
            model.agent(home.path(), false),
            async |cx: ConnectionTo<Agent>| {
                let caps = ClientCapabilities::new().session(
                    ClientSessionCapabilities::new().config_options(
                        SessionConfigOptionsCapabilities::new()
                            .boolean(BooleanConfigOptionCapabilities::new()),
                    ),
                );
                cx.send_request(
                    InitializeRequest::new(ProtocolVersion::V1).client_capabilities(caps),
                )
                .block_task()
                .await?;
                let created = cx
                    .send_request(NewSessionRequest::new(&cwd))
                    .block_task()
                    .await?;
                assert!(
                    created
                        .config_options
                        .unwrap()
                        .iter()
                        .any(|option| matches!(&option.kind, SessionConfigKind::Boolean(_)))
                );
                let session = created.session_id;
                cx.send_request(SetSessionModeRequest::new(session.clone(), "plan"))
                    .block_task()
                    .await?;
                assert_eq!(
                    ask(&cx, &session, "Only plan").await?.stop_reason,
                    StopReason::EndTurn
                );
                assert!(!cwd.join("test.txt").exists());
                cx.send_request(SetSessionModeRequest::new(session.clone(), "acceptEdits"))
                    .block_task()
                    .await?;
                for (id, value) in [
                    (
                        "model",
                        SessionConfigOptionValue::value_id("openai/gpt-5.4-mini"),
                    ),
                    ("thought_level", SessionConfigOptionValue::value_id("high")),
                    ("fast", SessionConfigOptionValue::boolean(true)),
                ] {
                    cx.send_request(SetSessionConfigOptionRequest::new(
                        session.clone(),
                        id,
                        value,
                    ))
                    .block_task()
                    .await?;
                }
                assert_eq!(
                    ask(&cx, &session, "Apply the plan").await?.stop_reason,
                    StopReason::EndTurn
                );
                assert_eq!(
                    std::fs::read_to_string(cwd.join("test.txt")).unwrap(),
                    "accepted"
                );
                cx.send_request(SetSessionTitleRequest::new(
                    session.clone(),
                    "Updated workspace",
                ))
                .block_task()
                .await?;
                *id.lock().unwrap() = Some(session);
                Ok(())
            },
        )
        .await
        .unwrap();
    assert_eq!(*trace.lock().unwrap(), ["shell"]);
    let session = id.lock().unwrap().take().unwrap();
    let updates = Updates::default();
    Client
        .v1()
        .on_receive_notification(
            {
                let log = updates.clone();
                async move |notification: SessionNotification, _| {
                    log.lock().unwrap().push(notification);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(
            model.agent(home.path(), false),
            async |cx: ConnectionTo<Agent>| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let listed = cx
                    .send_request(ListSessionsRequest::new().cwd(cwd.clone()))
                    .block_task()
                    .await?;
                assert_eq!(listed.sessions.len(), 1);
                assert_eq!(
                    listed.sessions[0].title.as_deref(),
                    Some("Updated workspace")
                );
                assert_eq!(listed.sessions[0].session_id, session);
                let loaded = cx
                    .send_request(LoadSessionRequest::new(session.clone(), &cwd))
                    .block_task()
                    .await?;
                let loaded_options = serde_json::to_value(loaded.config_options).unwrap();
                assert_eq!(
                    loaded.modes.unwrap().current_mode_id.0.as_ref(),
                    "acceptEdits"
                );
                for (id, value) in [
                    ("model", "openai/gpt-5.4-mini"),
                    ("thought_level", "high"),
                    ("fast", "on"),
                ] {
                    let option = loaded_options
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|item| item["id"] == id)
                        .unwrap();
                    assert_eq!(option["currentValue"], value);
                }
                let replay_count = || {
                    updates
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|n| {
                            matches!(
                                n.update,
                                SessionUpdate::UserMessageChunk(_)
                                    | SessionUpdate::AgentMessageChunk(_)
                                    | SessionUpdate::ToolCall(_)
                                    | SessionUpdate::ToolCallUpdate(_)
                            )
                        })
                        .count()
                };
                let replayed = replay_count();
                assert!(replayed >= 10);
                cx.send_request(ResumeSessionRequest::new(session.clone(), &cwd))
                    .block_task()
                    .await?;
                assert_eq!(replay_count(), replayed);
                let fork = cx
                    .send_request(ForkSessionRequest::new(session.clone(), &cwd))
                    .block_task()
                    .await?;
                assert_ne!(fork.session_id, session);
                cx.send_request(LoadSessionRequest::new(fork.session_id.clone(), &cwd))
                    .block_task()
                    .await?;
                assert_eq!(replay_count(), replayed * 2);
                cx.send_request(ArchiveSessionRequest::new(session.clone()))
                    .block_task()
                    .await?;
                let archived = cx
                    .send_request(ListSessionsRequest::new().archived(SessionArchiveFilter::Only))
                    .block_task()
                    .await?;
                assert_eq!(archived.sessions[0].session_id, session);
                cx.send_request(UnarchiveSessionRequest::new(session.clone()))
                    .block_task()
                    .await?;
                cx.send_request(CloseSessionRequest::new(fork.session_id.clone()))
                    .block_task()
                    .await?;
                let status = cx
                    .send_request(SessionStatusRequest::new(fork.session_id.clone()))
                    .block_task()
                    .await?;
                assert_eq!(status.status, SessionStatus::Saved);
                cx.send_request(DeleteSessionRequest::new(fork.session_id))
                    .block_task()
                    .await?;
                let listed = cx
                    .send_request(ListSessionsRequest::new())
                    .block_task()
                    .await?;
                assert_eq!(listed.sessions.len(), 1);
                Ok(())
            },
        )
        .await
        .unwrap();
    let requests = model.finish().await;
    assert!(
        requests[1]
            .to_string()
            .contains("Plan mode allows inspection only")
    );
    assert_eq!(requests[2]["model"], "gpt-5.4-mini");
    assert!(requests[4].to_string().contains("local-shell"));
}

#[cfg(unix)]
async fn steer(
    cx: &ConnectionTo<Agent>,
    session: &SessionId,
    text: &str,
) -> Result<Value, agent_client_protocol::Error> {
    let request = agent_client_protocol::UntypedMessage::new(
        "_session/steering",
        json!({
            "sessionId":session, "prompt":[{"type":"text","text":text}],
            "_meta":{"steering":{"idleBehavior":"promptRequired"}}
        }),
    )?;
    cx.send_request(request).block_task().await
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_steering_mcp_and_compaction_use_the_same_session() {
    let model = ModelFixture::start(vec![
        tool(
            "cancelled",
            "exec_command",
            json!({"command":"printf should-not-run"}),
        ),
        tool("echo", "mcp_local_echo", json!({"text":"MCP roundtrip"})),
        text("MCP completed."),
        text("Conversation summary."),
        text("Started idle."),
    ])
    .await;
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let path = workspace.path().join("mcp.sh");
    std::fs::write(&path, r#"
while IFS= read -r line; do
  rest=${line#*'"id":'}
  id=${rest%%,*}
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}}\n' "$id";;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}]}}\n' "$id";;
    *'"method":"tools/call"'*)
      case "$line" in *'MCP roundtrip'*) ;; *) exit 1;; esac
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"MCP fixture received"}]}}\n' "$id";;
  esac
done
"#).unwrap();

    let updates = Updates::default();
    let idle_started = Arc::new(tokio::sync::Notify::new());
    let cwd = workspace.path();
    Client
        .v1()
        .on_receive_notification(
            {
                let log = updates.clone();
                let idle_started = idle_started.clone();
                async move |notification: SessionNotification, _| {
                    if matches!(&notification.update, SessionUpdate::AgentMessageChunk(chunk) if chunk.content == "Started idle.".into()) {
                        idle_started.notify_one();
                    }
                    log.lock().unwrap().push(notification);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |req: RequestPermissionRequest, reply, cx: ConnectionTo<Agent>| {
                if req.tool_call.tool_call_id.0.as_ref() == "cancelled" {
                    cx.send_notification(CancelNotification::new(req.session_id))?;
                    reply.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    ))
                } else {
                    let connection = cx.clone();
                    cx.spawn(async move {
                        assert_eq!(
                            steer(&connection, &req.session_id, "Include the tool result").await?["outcome"],
                            "injected"
                        );
                        reply.respond(approved())
                    })
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(
            model.agent(home.path(), true),
            async |cx: ConnectionTo<Agent>| {
                let caps = ClientCapabilities::new().session(
                    ClientSessionCapabilities::new().compaction(CompactionCapabilities::new()),
                );
                cx.send_request(
                    InitializeRequest::new(ProtocolVersion::V1).client_capabilities(caps),
                )
                .block_task()
                .await?;
                let mcp = McpServer::Stdio(
                    McpServerStdio::new("local", "/bin/sh")
                        .args(vec![path.to_string_lossy().into_owned()]),
                );
                let response = cx
                    .send_request(NewSessionRequest::new(cwd).mcp_servers(vec![mcp]))
                    .block_task()
                    .await?;
                let id = response.session_id;
                assert_eq!(
                    ask(&cx, &id, "Cancel this command").await?.stop_reason,
                    StopReason::Cancelled
                );
                assert_eq!(
                    ask(&cx, &id, "Use the MCP tool").await?.stop_reason,
                    StopReason::EndTurn
                );
                let idle = steer(&cx, &id, "Hold this message").await?;
                assert_eq!(
                    idle,
                    json!({"outcome":"promptRequired","reason":"noRunningTurn"})
                );
                assert_eq!(
                    ask(&cx, &id, "/compact keep the tool outcome")
                        .await?
                        .stop_reason,
                    StopReason::EndTurn
                );
                let request = agent_client_protocol::UntypedMessage::new("_session/steering", json!({
                    "sessionId":id, "prompt":[{"type":"text","text":"Start from idle"}]
                }))?;
                let idle = cx.send_request(request).block_task().await?;
                assert_eq!(idle["outcome"], "startedNewTurn");
                idle_started.notified().await;
                let updates = updates.lock().unwrap();
                let states: Vec<_> = updates
                    .iter()
                    .filter_map(|n| match &n.update {
                        SessionUpdate::CompactionUpdate(update) => {
                            Some((&update.compaction_id, update.status.clone()))
                        }
                        _ => None,
                    })
                    .collect();
                assert_eq!(states.len(), 2);
                assert_eq!(states[0].0, states[1].0);
                assert_eq!(
                    (states[0].1.clone(), states[1].1.clone()),
                    (CompactionStatus::InProgress, CompactionStatus::Completed)
                );
                Ok(())
            },
        )
        .await
        .unwrap();
    let requests = model.finish().await;
    assert!(requests[1]["tools"].to_string().contains("mcp_local_echo"));
    assert!(requests[2].to_string().contains("MCP fixture received"));
    assert!(requests[2].to_string().contains("Include the tool result"));
    assert!(requests[3].to_string().contains("keep the tool outcome"));
    assert!(requests[4].to_string().contains("Start from idle"));
}

#[tokio::test]
async fn acp_replays_visible_turns_and_sends_hidden_history_to_the_model() {
    use futures_util::StreamExt;
    use pikra_ai::{ApiKind, Client as ModelClient, Model};
    use pikra_kit::{BackgroundNotification, KitEvent, KitSession, KitSessionConfig};
    use pikra_session::JsonlSessionStore;
    use pikra_testkit::FauxTransport;

    let model = ModelFixture::start(vec![
        tool(
            "meta-gate",
            "write",
            json!({"path":"meta-fixture.txt","content":"fixture"}),
        ),
        text("Hidden reply"),
        text("Visible reply"),
        text("After load"),
    ])
    .await;
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let cwd = workspace.path();
    std::fs::write(cwd.join("AGENTS.md"), "ACP project context").unwrap();
    let created_id = Arc::new(Mutex::new(None));
    Client
        .v1()
        .on_receive_request(
            async move |req: RequestPermissionRequest, reply, cx: ConnectionTo<Agent>| {
                let connection = cx.clone();
                cx.spawn(async move {
                    let steer = agent_client_protocol::UntypedMessage::new(
                        "_session/steering",
                        json!({
                            "sessionId":req.session_id,
                            "prompt":[{"type":"text","text":"Hidden ACP steer"}],
                            "_meta":{"is_meta":true,"steering":{"idleBehavior":"promptRequired"}}
                        }),
                    )?;
                    let response: Value = connection.send_request(steer).block_task().await?;
                    assert_eq!(response["outcome"], "injected");
                    reply.respond(approved())
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async |notification: SessionNotification, _| {
                assert!(!matches!(
                    notification.update,
                    SessionUpdate::UserMessageChunk(_)
                ));
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(
            model.agent(home.path(), false),
            async |cx: ConnectionTo<Agent>| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let id = cx
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?
                    .session_id;
                let request = PromptRequest::new(id.clone(), vec!["/plan hidden ACP text".into()])
                    .meta(serde_json::from_value::<Meta>(json!({"isMeta":true})).unwrap());
                assert_eq!(
                    cx.send_request(request).block_task().await?.stop_reason,
                    StopReason::EndTurn
                );
                let listed = cx
                    .send_request(ListSessionsRequest::new())
                    .block_task()
                    .await?;
                assert!(listed.sessions[0].title.is_none());
                assert_eq!(
                    ask(&cx, &id, "Visible ACP input").await?.stop_reason,
                    StopReason::EndTurn
                );
                *created_id.lock().unwrap() = Some(id);
                Ok(())
            },
        )
        .await
        .unwrap();
    let id = created_id.lock().unwrap().take().unwrap();

    // A host can write a background turn into the same journal before ACP loads it.
    let http = Arc::new(FauxTransport::new());
    http.enqueue_sse([
        r#"data: {"id":"background","choices":[{"index":0,"delta":{"content":"Background reply"},"finish_reason":"stop"}]}"#,
        "data: [DONE]",
    ].map(|line| format!("{line}\n\n")));
    let journal = home
        .path()
        .join("acp/sessions")
        .join(format!("{}.jsonl", id.0));
    let session = KitSession::new(
        Arc::new(ModelClient::new(http)),
        KitSessionConfig::new(Model {
            id: "gpt-5.4".into(),
            name: "Fixture".into(),
            provider: "openai".into(),
            api: ApiKind::OpenaiCompletions,
            base_url: model.url.clone(),
            context_window: 128_000,
            max_output_tokens: 4_096,
            supports_images: false,
            supports_reasoning: false,
            cost: Default::default(),
            headers: Vec::new(),
            compat: None,
        })
        .with_store(Arc::new(JsonlSessionStore::new(journal))),
    )
    .unwrap();
    session.background_sender().send(BackgroundNotification {
        source_id: "fixture".into(),
        kind: "monitor".into(),
        summary: "Background context".into(),
        xml_payload: "<monitor_event>ACP background context</monitor_event>".into(),
        timestamp_ms: 1,
    });
    let mut turn = session.run_background_turn().unwrap().unwrap();
    while let Some(event) = turn.next().await {
        assert!(!matches!(event, KitEvent::Error(_)), "{event:?}");
    }
    drop(turn);
    drop(session);

    let updates = Updates::default();
    Client
        .v1()
        .on_receive_notification(
            {
                let updates = updates.clone();
                async move |notification: SessionNotification, _| {
                    updates.lock().unwrap().push(notification);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(
            model.agent(home.path(), false),
            async |cx: ConnectionTo<Agent>| {
                cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                cx.send_request(LoadSessionRequest::new(id.clone(), cwd))
                    .block_task()
                    .await?;
                let replayed = updates
                    .lock()
                    .unwrap()
                    .iter()
                    .filter_map(|notification| {
                        Some(match &notification.update {
                            SessionUpdate::UserMessageChunk(chunk) => {
                                ("user", chunk.content.clone())
                            }
                            SessionUpdate::AgentMessageChunk(chunk) => {
                                ("assistant", chunk.content.clone())
                            }
                            _ => return None,
                        })
                    })
                    .collect::<Vec<_>>();
                assert_eq!(
                    replayed,
                    [
                        ("assistant", "Hidden reply".into()),
                        ("user", "Visible ACP input".into()),
                        ("assistant", "Visible reply".into()),
                        ("assistant", "Background reply".into()),
                    ]
                );
                assert_eq!(
                    ask(&cx, &id, "Check history").await?.stop_reason,
                    StopReason::EndTurn
                );
                Ok(())
            },
        )
        .await
        .unwrap();
    let requests = model.finish().await;
    let first = requests[0].to_string();
    assert!(first.contains("ACP project context"));
    assert!(first.contains("/plan hidden ACP text"));
    assert!(requests[1].to_string().contains("Hidden ACP steer"));
    let last = requests.last().unwrap().to_string();
    assert!(last.contains("ACP background context"));
    assert!(last.contains("/plan hidden ACP text"));
    for req in requests {
        let body = req.to_string();
        assert!(!body.contains("is_meta") && !body.contains("isMeta"));
    }
}

#[tokio::test]
async fn review_batch2_acp_security_capabilities_fs_denial_and_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let secret_file = dir.path().join("restricted.txt");
    std::fs::write(&secret_file, "TOP_SECRET_DISK_CONTENT").unwrap();

    // Turn 1: model tries to read restricted.txt (editor denies fs/read_text_file -> must NOT fallback to disk!),
    // then replies mentioning "App" and the bearer token (X-Title: My App must NOT redact "App", Bearer token MUST be redacted).
    let model = ModelFixture::start(vec![
        tool("r1", "read", json!({"path": "restricted.txt"})),
        text("My App leaks secret-token-12345678"),
    ])
    .await;

    let updates: Updates = Arc::default();
    let client_updates = updates.clone();
    let cwd = dir.path().to_path_buf();

    // 1. Initialize WITHOUT auth.terminal -> must NOT advertise codex-oauth; WITH fs.read_text_file returning denial.
    let agent = AcpAgent::new(
        AcpAgentConfig::new(env!("CARGO_BIN_EXE_pikra-acp"))
            .args([
                "--pikra-home",
                home.path().to_str().unwrap(),
                "--system-prompt",
                "BASE_PROMPT",
            ])
            .env("HOME", home.path().to_str().unwrap())
            .env("CODEX_HOME", home.path().join("codex").to_str().unwrap())
            .env("NO_PROXY", "127.0.0.1"),
    );

    Client
        .v1()
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                client_updates.lock().unwrap().push(notification);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async |_req: ReadTextFileRequest, responder, _cx| {
                responder.respond_with_error(agent_client_protocol::Error::new(
                    -32000,
                    "Editor denied file read.",
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, async |cx: ConnectionTo<Agent>| {
            // Default caps: auth.terminal is false -> codex-oauth must be omitted.
            let caps =
                ClientCapabilities::new().fs(FileSystemCapabilities::new().read_text_file(true));
            let init = cx
                .send_request(InitializeRequest::new(ProtocolVersion::V1).client_capabilities(caps))
                .block_task()
                .await?;
            assert!(
                !init
                    .auth_methods
                    .iter()
                    .any(|m| m.id().0.as_ref() == "codex-oauth"),
                "codex-oauth must not be advertised without clientCapabilities.auth.terminal"
            );

            // Configure a custom provider with both non-sensitive X-Title and sensitive Authorization header.
            let mut headers = agent_client_protocol::schema::IndexMap::new();
            headers.insert("X-Title".to_string(), "My App".to_string());
            headers.insert(
                "Authorization".to_string(),
                "Bearer secret-token-12345678".to_string(),
            );
            let mut provider_meta = serde_json::Map::new();
            provider_meta.insert("models".to_string(), json!(["custom-m1"]));
            cx.send_request(
                SetProviderRequest::new(
                    "custom-gw",
                    LlmProtocol::Other("_openai_completions".into()),
                    &model.url,
                )
                .headers(headers)
                .meta(provider_meta),
            )
            .block_task()
            .await?;

            // Create session with SystemPrompt::Append and custom-gw model.
            let created = cx
                .send_request(NewSessionRequest::new(cwd.clone()).system_prompt(
                    SystemPrompt::new(vec!["EXTRA_RULE".into()]).mode(SystemPromptMode::Append),
                ))
                .block_task()
                .await?;
            let sid = created.session_id;
            cx.send_request(SetSessionConfigOptionRequest::new(
                sid.clone(),
                "model",
                "custom-gw/custom-m1",
            ))
            .block_task()
            .await?;

            let reply = ask(&cx, &sid, "read restricted").await?;
            assert_eq!(reply.stop_reason, StopReason::EndTurn);

            // Fork with the same Append prompt -> must NOT duplicate EXTRA_RULE.
            let forked = cx
                .send_request(
                    ForkSessionRequest::new(sid.clone(), cwd.clone()).system_prompt(
                        SystemPrompt::new(vec!["EXTRA_RULE".into()]).mode(SystemPromptMode::Append),
                    ),
                )
                .block_task()
                .await?;
            let fork_id = forked.session_id;

            // Disable the custom provider -> subsequent prompt MUST fail (NOT fallback to public OpenAI!).
            cx.send_request(DisableProviderRequest::new("custom-gw"))
                .block_task()
                .await?;
            let disabled_res = ask(&cx, &fork_id, "should fail").await;
            assert!(
                disabled_res.is_err(),
                "disabled custom provider must fail and never fallback to OpenAI"
            );

            // Immediate close + delete must succeed deterministically (no Lease drop race).
            cx.send_request(CloseSessionRequest::new(fork_id.clone()))
                .block_task()
                .await?;
            cx.send_request(DeleteSessionRequest::new(fork_id))
                .block_task()
                .await?;
            cx.send_request(CloseSessionRequest::new(sid.clone()))
                .block_task()
                .await?;
            cx.send_request(DeleteSessionRequest::new(sid))
                .block_task()
                .await?;
            Ok(())
        })
        .await
        .unwrap();

    // Check model requests: disk content must NEVER have been leaked via fallback.
    let reqs = model.finish().await;
    assert_eq!(reqs.len(), 2);
    let second_body = reqs[1].to_string();
    assert!(
        !second_body.contains("TOP_SECRET_DISK_CONTENT"),
        "denied fs/read_text_file must never fall back to local disk: {second_body}"
    );

    // Check assistant output updates: "App" preserved, bearer secret redacted.
    let assistant_text: String = updates
        .lock()
        .unwrap()
        .iter()
        .filter_map(|n| match &n.update {
            SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert!(
        assistant_text.contains("My App"),
        "non-sensitive X-Title value must not redact 'App': {assistant_text}"
    );
    assert!(
        !assistant_text.contains("secret-token-12345678"),
        "Authorization bearer token must be redacted: {assistant_text}"
    );

    // 2. Initialize WITH auth.terminal = true -> MUST advertise codex-oauth.
    let agent2 = AcpAgent::new(
        AcpAgentConfig::new(env!("CARGO_BIN_EXE_pikra-acp"))
            .args([
                "--pikra-home",
                home.path().to_str().unwrap(),
                "--no-session",
            ])
            .env("HOME", home.path().to_str().unwrap()),
    );
    Client
        .v1()
        .connect_with(agent2, async |cx: ConnectionTo<Agent>| {
            let caps = ClientCapabilities::new().auth(AuthCapabilities::new().terminal(true));
            let init = cx
                .send_request(InitializeRequest::new(ProtocolVersion::V1).client_capabilities(caps))
                .block_task()
                .await?;
            assert!(
                init.auth_methods
                    .iter()
                    .any(|m| m.id().0.as_ref() == "codex-oauth"),
                "codex-oauth must be advertised when clientCapabilities.auth.terminal is true"
            );
            Ok(())
        })
        .await
        .unwrap();
}
