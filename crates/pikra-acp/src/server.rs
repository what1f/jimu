use crate::{
    AcpConfig, args,
    auth::Auth,
    rpc::{Peer, Result, encode, failed, invalid, params},
    schema::v1 as acp,
    session::{self, Metadata, Mode, Session, Sessions, branch},
    turn,
};
use base64::Engine;
use pikra_ai::{CancelToken, Client, HttpTransport, StreamProvider};
use pikra_http::ReqwestTransport;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

/// Shared configuration and session catalog for ACP connections.
pub struct AcpServer {
    pub(crate) config: AcpConfig,
    pub(crate) auth: Arc<Auth>,
    pub(crate) mode: Mode,
    pub(crate) sessions: Sessions,
    pub(crate) transport: Arc<dyn HttpTransport>,
    pub(crate) provider: Arc<dyn StreamProvider>,
}

impl AcpServer {
    /// Open the session catalog and configure the model transport.
    pub fn new(config: AcpConfig) -> std::result::Result<Self, String> {
        if !config.cwd.is_absolute() {
            return Err("The workspace directory must be an absolute path.".into());
        }
        if config
            .base_url
            .as_deref()
            .is_some_and(|url| crate::auth::base_url(url).is_none())
        {
            return Err("Enter a base URL without credentials, query, or fragment.".into());
        }
        let mode = Mode::parse(&config.initial_mode).ok_or("Unknown session mode.")?;
        let transport: Arc<dyn HttpTransport> = match &config.transport_override {
            Some(transport) => transport.clone(),
            None => Arc::new(
                ReqwestTransport::new()
                    .map_err(|_| "Could not configure the network connection.")?,
            ),
        };
        let provider = config
            .provider_override
            .clone()
            .unwrap_or_else(|| Arc::new(Client::new(transport.clone())));
        let sessions = Sessions::new(
            config
                .persist_sessions
                .then(|| config.pikra_home.join("acp/sessions")),
        )?;
        Ok(Self {
            auth: Arc::new(Auth::new(config.clone())),
            config,
            mode,
            sessions,
            transport,
            provider,
        })
    }
}

pub(crate) struct Connection {
    pub(crate) server: Arc<AcpServer>,
    pub(crate) peer: Arc<Peer>,
    capabilities: Mutex<Option<acp::ClientCapabilities>>,
    auth_status: Mutex<Option<acp::AuthStatusResponse>>,
    live: Mutex<HashMap<String, Arc<Session>>>,
    lifecycle: tokio::sync::Mutex<()>,
}

struct OpenRequest {
    cwd: PathBuf,
    additional: Vec<PathBuf>,
    mcp: Vec<acp::McpServer>,
    prompt: Option<acp::SystemPrompt>,
    options: Vec<acp::InitialSessionConfigOption>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyModel {
    session_id: acp::SessionId,
    model_id: String,
}

impl Connection {
    pub(crate) fn new(server: Arc<AcpServer>, peer: Arc<Peer>) -> Self {
        Self {
            server,
            peer,
            capabilities: Mutex::default(),
            auth_status: Mutex::default(),
            live: Mutex::default(),
            lifecycle: tokio::sync::Mutex::new(()),
        }
    }

    fn initialize(&self, req: acp::InitializeRequest) -> Result<Value> {
        let mut caps = self.capabilities.lock().unwrap();
        if caps.is_some() {
            return Err(acp::Error::invalid_request());
        }
        let terminal_auth = req.client_capabilities.auth.terminal;
        *caps = Some(req.client_capabilities);
        *self.auth_status.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(self.server.auth.status());
        let session = acp::SessionCapabilities::new()
            .list(acp::SessionListCapabilities::new())
            .delete(acp::SessionDeleteCapabilities::new())
            .resume(acp::SessionResumeCapabilities::new())
            .close(acp::SessionCloseCapabilities::new())
            .fork(acp::SessionForkCapabilities::new())
            .archive(acp::SessionArchiveCapabilities::new())
            .status(acp::SessionStatusCapabilities::new())
            .options(acp::SessionOptionsCapabilities::new())
            .title(acp::SessionTitleCapabilities::new())
            .additional_directories(acp::SessionAdditionalDirectoriesCapabilities::new())
            .system_prompt(
                acp::SessionSystemPromptCapabilities::new().mode(
                    acp::SessionSystemPromptModeCapabilities::new()
                        .override_(acp::SessionSystemPromptOverrideCapabilities::new()),
                ),
            );
        let capabilities = acp::AgentCapabilities::new()
            .load_session(true)
            .prompt_capabilities(
                acp::PromptCapabilities::new()
                    .image(true)
                    .embedded_context(true),
            )
            .mcp_capabilities(acp::McpCapabilities::new().http(true))
            .session_capabilities(session)
            .auth(
                acp::AgentAuthCapabilities::new()
                    .logout(acp::LogoutCapabilities::new())
                    .status(acp::AuthStatusCapabilities::new()),
            )
            .providers(acp::ProvidersCapabilities::new());
        let mut auth_methods = Vec::new();
        if terminal_auth {
            let terminal =
                acp::AuthMethodTerminal::new("codex-oauth", "ChatGPT / Codex Subscription")
                    .args(vec!["login".into()])
                    .env(args::auth_env(&self.server.config));
            auth_methods.push(acp::AuthMethod::Terminal(terminal));
        }
        let key = acp::AuthMethodAgent::new("api-key", "API Key (Environment or Provider Config)");
        auth_methods.push(acp::AuthMethod::Agent(key));
        encode(
            acp::InitializeResponse::new(crate::schema::ProtocolVersion::V1)
                .agent_info(
                    acp::Implementation::new("pikra-acp", env!("CARGO_PKG_VERSION")).title("Pikra"),
                )
                .agent_capabilities(capabilities)
                .auth_methods(auth_methods)
                .meta(meta(json!({"steering":{"supported":true}}))),
        )
    }

    fn update_auth(&self, status: &acp::AuthStatusResponse) {
        let mut previous = self.auth_status.lock().unwrap();
        if previous.as_ref() != Some(status) {
            *previous = Some(status.clone());
            self.peer.notify(
                "auth/status_update",
                acp::AuthStatusUpdateNotification::new(status.state.clone())
                    .accounts(status.accounts.clone()),
            );
        }
    }

    fn live(&self, id: &str) -> Result<Arc<Session>> {
        self.live
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| invalid("Open the session before using it."))
    }

    pub(crate) fn order_steering(&self, id: &str) -> Option<session::SteeringOrder> {
        self.live(id).ok().map(|session| session.order_steering())
    }

    pub(crate) fn cancel_turn(&self, id: &str) {
        if let Ok(session) = self.live(id) {
            session.cancel();
        }
    }

    pub(crate) fn cancel_all(&self) {
        for session in self.live.lock().unwrap().values() {
            session.cancel();
        }
    }

    pub(crate) async fn shutdown(&self) {
        let sessions = std::mem::take(&mut *self.live.lock().unwrap_or_else(|e| e.into_inner()));
        futures_util::future::join_all(sessions.values().map(|session| session.close())).await;
    }

    pub(crate) fn commands_after(
        &self,
        method: &str,
        source_session: Option<&Value>,
        result: &Value,
    ) -> Option<acp::SessionNotification> {
        let id = match method {
            "session/new" | "session/fork" => result.get("sessionId"),
            "session/load" | "session/resume" => source_session,
            _ => None,
        }?
        .as_str()?;
        let session = self.live(id).ok()?;
        Some(acp::SessionNotification::new(
            session.bridge.id.clone(),
            session.commands(),
        ))
    }

    pub(crate) async fn dispatch(
        &self,
        method: &str,
        payload: Value,
        cancel: &CancelToken,
        session_cancel: &CancelToken,
    ) -> Result<Value> {
        if method == "initialize" {
            return self.initialize(params(payload)?);
        }
        let caps = self
            .capabilities
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(acp::Error::invalid_request)?;
        match method {
            "auth/status" => {
                let _: acp::AuthStatusRequest = params(payload)?;
                let status = self.server.auth.status();
                self.update_auth(&status);
                encode(status)
            }
            "authenticate" => {
                let req: acp::AuthenticateRequest = params(payload)?;
                if req.method_id.0.as_ref() != "api-key" {
                    return Err(invalid("Unknown sign-in method."));
                }
                let status = self.server.auth.status();
                self.update_auth(&status);
                if status.state == acp::AuthState::Unauthenticated {
                    return Err(acp::Error::auth_required());
                }
                encode(acp::AuthenticateResponse::new())
            }
            "logout" => {
                let _: acp::LogoutRequest = params(payload)?;
                self.server.auth.logout().await?;
                self.update_auth(&self.server.auth.status());
                encode(acp::LogoutResponse::new())
            }
            "providers/list" => {
                let _: acp::ListProvidersRequest = params(payload)?;
                encode(self.server.auth.providers())
            }
            "providers/set" => {
                self.server.auth.set(params(payload)?)?;
                self.update_auth(&self.server.auth.status());
                encode(acp::SetProviderResponse::new())
            }
            "providers/disable" => {
                let req: acp::DisableProviderRequest = params(payload)?;
                self.server.auth.disable(req.provider_id.0.as_ref())?;
                self.update_auth(&self.server.auth.status());
                encode(acp::DisableProviderResponse::new())
            }
            "session/new" => {
                let req: acp::NewSessionRequest = params(payload)?;
                let request = OpenRequest {
                    cwd: req.cwd,
                    additional: req.additional_directories,
                    mcp: req.mcp_servers,
                    prompt: req.system_prompt,
                    options: req.config_options,
                };
                let session = self.create(request, &caps, None, cancel).await?;
                let meta = session.metadata();
                encode(
                    acp::NewSessionResponse::new(session.bridge.id.clone())
                        .modes(meta.mode.state())
                        .config_options(meta.options(&self.server.auth, &caps)),
                )
            }
            "session/load" => {
                let req: acp::LoadSessionRequest = params(payload)?;
                let request = OpenRequest {
                    cwd: req.cwd,
                    additional: req.additional_directories,
                    mcp: req.mcp_servers,
                    prompt: req.system_prompt,
                    options: req.config_options,
                };
                let meta = self
                    .reopen(req.session_id.0.as_ref(), request, &caps, true, cancel)
                    .await?;
                encode(
                    acp::LoadSessionResponse::new()
                        .modes(meta.mode.state())
                        .config_options(meta.options(&self.server.auth, &caps)),
                )
            }
            "session/resume" => {
                let req: acp::ResumeSessionRequest = params(payload)?;
                let request = OpenRequest {
                    cwd: req.cwd,
                    additional: req.additional_directories,
                    mcp: req.mcp_servers,
                    prompt: req.system_prompt,
                    options: req.config_options,
                };
                let meta = self
                    .reopen(req.session_id.0.as_ref(), request, &caps, false, cancel)
                    .await?;
                encode(
                    acp::ResumeSessionResponse::new()
                        .modes(meta.mode.state())
                        .config_options(meta.options(&self.server.auth, &caps)),
                )
            }
            "session/fork" => {
                let req: acp::ForkSessionRequest = params(payload)?;
                if req.cursor_id.is_some() {
                    return Err(invalid("Only the current session history can be copied."));
                }
                let request = OpenRequest {
                    cwd: req.cwd,
                    additional: req.additional_directories,
                    mcp: req.mcp_servers,
                    prompt: req.system_prompt,
                    options: req.config_options,
                };
                let session = self
                    .create(request, &caps, Some(req.session_id.0.as_ref()), cancel)
                    .await?;
                let meta = session.metadata();
                encode(
                    acp::ForkSessionResponse::new(session.bridge.id.clone())
                        .modes(meta.mode.state())
                        .config_options(meta.options(&self.server.auth, &caps)),
                )
            }
            "session/options" => {
                let req: acp::SessionOptionsRequest = params(payload)?;
                session::validate_roots(&req.cwd, &req.additional_directories)?;
                let meta = if let Some(id) = req.session_id {
                    self.server
                        .sessions
                        .get(id.0.as_ref())?
                        .metadata
                        .lock()
                        .unwrap()
                        .clone()
                } else {
                    Metadata::new(&self.server, req.cwd, req.additional_directories)
                };
                encode(
                    acp::SessionOptionsResponse::new()
                        .modes(meta.mode.state())
                        .config_options(meta.options(&self.server.auth, &caps)),
                )
            }
            "session/list" => self.list(params(payload)?),
            "session/close" => {
                let req: acp::CloseSessionRequest = params(payload)?;
                let _operation = self.lifecycle.lock().await;
                check_cancel(cancel, &self.peer)?;
                self.close(req.session_id.0.as_ref()).await?;
                encode(acp::CloseSessionResponse::new())
            }
            "session/delete" => {
                let req: acp::DeleteSessionRequest = params(payload)?;
                let id = req.session_id.0.as_ref();
                let _operation = self.lifecycle.lock().await;
                check_cancel(cancel, &self.peer)?;
                let _record = self.server.sessions.get(id)?;
                if self.live(id).is_ok() {
                    self.close(id).await?;
                }
                let _lease = Sessions::claim(&self.server, id)?;
                self.server.sessions.delete(id)?;
                encode(acp::DeleteSessionResponse::new())
            }
            "session/archive" => {
                let req: acp::ArchiveSessionRequest = params(payload)?;
                self.archive(req.session_id.0.as_ref(), true)?;
                encode(acp::ArchiveSessionResponse::new())
            }
            "session/unarchive" => {
                let req: acp::UnarchiveSessionRequest = params(payload)?;
                self.archive(req.session_id.0.as_ref(), false)?;
                encode(acp::UnarchiveSessionResponse::new())
            }
            "session/status" => {
                let req: acp::SessionStatusRequest = params(payload)?;
                let id = req.session_id.0.as_ref();
                self.server.sessions.get(id)?;
                let live = self.live(id).ok();
                let status = if self.server.sessions.is_open(id) {
                    acp::SessionStatus::Open
                } else {
                    acp::SessionStatus::Saved
                };
                let activity = if live.is_some_and(|session| session.turn.current().is_some()) {
                    "busy"
                } else {
                    "idle"
                };
                encode(
                    acp::SessionStatusResponse::new(status)
                        .meta(meta(json!({"activity":activity}))),
                )
            }
            "session/set_title" => {
                let req: acp::SetSessionTitleRequest = params(payload)?;
                let id = req.session_id.0.as_ref();
                let record = self.server.sessions.get(id)?;
                let title = req.title.split_whitespace().collect::<Vec<_>>().join(" ");
                if title.is_empty() {
                    return Err(invalid("Enter a session title."));
                }
                let meta = self
                    .server
                    .sessions
                    .update(&record, |meta| meta.title = Some(title.clone()))?;
                if let Ok(session) = self.live(id) {
                    session.notify_info(&meta);
                }
                encode(acp::SetSessionTitleResponse::new(title))
            }
            "session/set_mode" => {
                let req: acp::SetSessionModeRequest = params(payload)?;
                self.set_option(
                    req.session_id.0.as_ref(),
                    "mode",
                    acp::SessionConfigOptionValue::value_id(req.mode_id.0),
                    cancel,
                )
                .await?;
                encode(acp::SetSessionModeResponse::new())
            }
            "session/set_config_option" => {
                let req: acp::SetSessionConfigOptionRequest = params(payload)?;
                let options = self
                    .set_option(
                        req.session_id.0.as_ref(),
                        req.config_id.0.as_ref(),
                        req.value,
                        cancel,
                    )
                    .await?;
                encode(acp::SetSessionConfigOptionResponse::new(options))
            }
            "session/set_model" => {
                let req: LegacyModel = params(payload)?;
                self.set_option(
                    req.session_id.0.as_ref(),
                    "model",
                    acp::SessionConfigOptionValue::value_id(req.model_id),
                    cancel,
                )
                .await?;
                Ok(json!({}))
            }
            "session/prompt" => {
                let req: acp::PromptRequest = params(payload)?;
                let session = self.live(req.session_id.0.as_ref())?;
                let guard = {
                    let _operation = session.steering.lock().await;
                    session.turn.begin()?
                };
                let is_meta = is_meta_prompt(&req);
                encode(
                    turn::prompt(
                        &self.server,
                        &session,
                        guard,
                        req.prompt,
                        is_meta,
                        cancel,
                        session_cancel,
                    )
                    .await?,
                )
            }
            "_session/steering" => {
                let req: acp::PromptRequest = params(payload)?;
                self.steer(req, cancel).await
            }
            _ => Err(acp::Error::method_not_found()),
        }
    }

    async fn create(
        &self,
        req: OpenRequest,
        caps: &acp::ClientCapabilities,
        parent: Option<&str>,
        cancel: &CancelToken,
    ) -> Result<Arc<Session>> {
        session::validate_roots(&req.cwd, &req.additional)?;
        session::validate_prompt(req.prompt.as_ref())?;
        let _operation = self.lifecycle.lock().await;
        check_cancel(cancel, &self.peer)?;
        let (mut meta, entries) = if let Some(id) = parent {
            let source = self.server.sessions.get(id)?;
            let current = self.live(id).ok();
            let _source_guard = match &current {
                Some(session) => Some(session.steering.lock().await),
                None => None,
            };
            if let Some(session) = &current {
                if session.turn.is_closed() {
                    return Err(invalid("The session is closed."));
                }
                if session.turn.current().is_some() || session.kit.is_running() {
                    return Err(failed("Wait for the current turn to finish."));
                }
            } else if self.server.sessions.is_open(id) {
                return Err(failed("This session is open in another connection."));
            }
            let _source_lock = if current.is_none() {
                self.server.sessions.try_lock_file(id)?
            } else {
                None
            };
            let mut meta = source
                .metadata
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            meta.session_id = uuid::Uuid::now_v7().to_string();
            meta.cwd = req.cwd.clone();
            meta.additional_directories = req.additional.clone();
            meta.created_at = crate::auth::now_ms();
            meta.updated_at = meta.created_at;
            meta.archived = false;
            let raw_entries = match &current {
                Some(session) => session.kit.entries(),
                None => {
                    source
                        .store
                        .load()
                        .map_err(|_| failed("Could not copy the session history."))?
                        .1
                }
            };
            let entries = branch(raw_entries);
            (meta, entries)
        } else {
            (
                Metadata::new(&self.server, req.cwd, req.additional),
                Vec::new(),
            )
        };
        session::initial_options(&mut meta, &self.server.auth, caps, &req.options)?;
        check_cancel(cancel, &self.peer)?;
        let lease = Arc::new(Sessions::claim(&self.server, &meta.session_id)?);
        let record = self
            .server
            .sessions
            .prepare(meta.clone(), parent, &entries)?;
        let opened = Session::open(
            self,
            caps.clone(),
            record.clone(),
            &meta,
            req.mcp,
            lease,
            cancel,
        )
        .await;
        let result = match opened {
            Ok(session) => {
                let valid = check_cancel(cancel, &self.peer)
                    .and_then(|()| {
                        session
                            .apply_prompt(req.prompt, self.server.config.system_prompt.as_deref())
                    })
                    .and_then(|()| self.server.sessions.register(record));
                match valid {
                    Ok(()) => {
                        let session = Arc::new(session);
                        self.live
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(meta.session_id.clone(), session.clone());
                        Ok(session)
                    }
                    Err(error) => {
                        session.close().await;
                        Err(error)
                    }
                }
            }
            Err(error) => Err(error),
        };
        if result.is_err() {
            self.server.sessions.discard(&meta.session_id);
        }
        result
    }

    async fn reopen(
        &self,
        id: &str,
        req: OpenRequest,
        caps: &acp::ClientCapabilities,
        replay: bool,
        cancel: &CancelToken,
    ) -> Result<Metadata> {
        session::validate_roots(&req.cwd, &req.additional)?;
        session::validate_prompt(req.prompt.as_ref())?;
        let _operation = self.lifecycle.lock().await;
        check_cancel(cancel, &self.peer)?;
        let record = self.server.sessions.get(id)?;
        let current = self.live(id).ok();
        let _session_control = match &current {
            Some(session) => Some(session.steering.lock().await),
            None => None,
        };
        if current
            .as_ref()
            .is_some_and(|session| session.turn.current().is_some())
        {
            return Err(failed("Wait for the current turn to finish."));
        }
        let old = record.metadata.lock().unwrap().clone();
        if req.cwd != old.cwd {
            return Err(invalid(
                "Use the original workspace directory to reopen the session.",
            ));
        }
        let mut next = old.clone();
        next.cwd = req.cwd;
        next.additional_directories = req.additional;
        session::initial_options(&mut next, &self.server.auth, caps, &req.options)?;
        if let Some(session) = &current
            && old.cwd == next.cwd
            && old.additional_directories == next.additional_directories
            && session.mcp_config == req.mcp
        {
            session.apply_prompt(req.prompt, self.server.config.system_prompt.as_deref())?;
            if !req.options.is_empty() {
                session.apply_metadata(&self.server, &next)?;
            }
            session.kit.reload_instructions();
            if replay {
                turn::replay(session, cancel).await?;
            }
            return Ok(session.metadata());
        }
        let lease = match &current {
            Some(session) => session.lease(),
            None => Arc::new(Sessions::claim(&self.server, id)?),
        };
        let opened = Session::open(
            self,
            caps.clone(),
            record.clone(),
            &next,
            req.mcp,
            lease,
            cancel,
        )
        .await?;
        let setup = async {
            check_cancel(cancel, &self.peer)?;
            opened.apply_prompt(req.prompt, self.server.config.system_prompt.as_deref())?;
            if replay {
                turn::replay(&opened, cancel).await?;
            }
            self.server.sessions.update(&record, |meta| {
                let (title, archived) = (meta.title.clone(), meta.archived);
                *meta = next;
                meta.title = title;
                meta.archived = archived;
            })
        }
        .await;
        match setup {
            Ok(next) => {
                if let Some(current) = &current {
                    current.shutdown().await;
                }
                self.live
                    .lock()
                    .unwrap()
                    .insert(id.into(), Arc::new(opened));
                Ok(next)
            }
            Err(error) => {
                if current.is_none() {
                    opened.close().await;
                } else {
                    opened.shutdown().await;
                }
                Err(error)
            }
        }
    }

    async fn close(&self, id: &str) -> Result<()> {
        let session = self
            .live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id)
            .ok_or_else(|| invalid("Open the session before using it."))?;
        session.close().await;
        Ok(())
    }

    fn archive(&self, id: &str, archived: bool) -> Result<()> {
        let record = self.server.sessions.get(id)?;
        let meta = self
            .server
            .sessions
            .update(&record, |meta| meta.archived = archived)?;
        if let Ok(session) = self.live(id) {
            session.notify_info(&meta);
        }
        Ok(())
    }

    fn list(&self, req: acp::ListSessionsRequest) -> Result<Value> {
        if let Some(cwd) = &req.cwd {
            session::validate_roots(cwd, &[])?;
        }
        let codec = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let cursor: Option<(u64, String)> = req
            .cursor
            .map(|cursor| {
                codec
                    .decode(cursor)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                    .ok_or_else(|| invalid("Invalid session cursor."))
            })
            .transpose()?;
        let mut records = self.server.sessions.snapshots();
        records.retain(|record| {
            req.cwd.as_ref().is_none_or(|cwd| cwd == &record.cwd)
                && match &req.archived {
                    Some(acp::SessionArchiveFilter::Include) => true,
                    Some(acp::SessionArchiveFilter::Only) => record.archived,
                    _ => !record.archived,
                }
                && cursor.as_ref().is_none_or(|(updated, id)| {
                    (record.updated_at, &record.session_id) < (*updated, id)
                })
        });
        records.sort_by(|a, b| (b.updated_at, &b.session_id).cmp(&(a.updated_at, &a.session_id)));
        const PAGE: usize = 50;
        let next = if records.len() > PAGE {
            records.truncate(PAGE);
            let last = &records[PAGE - 1];
            Some(
                codec.encode(
                    serde_json::to_vec(&(last.updated_at, &last.session_id))
                        .map_err(|_| acp::Error::internal_error())?,
                ),
            )
        } else {
            None
        };
        encode(
            acp::ListSessionsResponse::new(records.iter().map(Metadata::info).collect())
                .next_cursor(next),
        )
    }

    async fn set_option(
        &self,
        id: &str,
        key: &str,
        value: acp::SessionConfigOptionValue,
        cancel: &CancelToken,
    ) -> Result<Vec<acp::SessionConfigOption>> {
        let session = self.live(id)?;
        let _operation = session.steering.lock().await;
        check_cancel(cancel, &self.peer)?;
        if session.turn.is_closed() {
            return Err(invalid("The session is closed."));
        }
        if session.turn.current().is_some() {
            return Err(failed("Wait for the current turn to finish."));
        }
        let mut next = session.metadata();
        let mode = next.mode;
        if !next.set_option(&self.server.auth, &session.bridge.capabilities, key, &value) {
            return Err(invalid("The option or value is unavailable."));
        }
        session.apply_metadata(&self.server, &next)?;
        Ok(session.notify_options(&self.server, mode != next.mode))
    }

    async fn steer(&self, req: acp::PromptRequest, cancel: &CancelToken) -> Result<Value> {
        let setting = req
            .meta
            .as_ref()
            .and_then(|meta| meta.get("steering"))
            .and_then(|value| value.get("idleBehavior"));
        let prompt_required = match setting {
            None => false,
            Some(value) if value.as_str() == Some("promptRequired") => true,
            _ => return Err(invalid("Unsupported steering idle behavior.")),
        };
        let session = self.live(req.session_id.0.as_ref())?;
        let operation = tokio::select! {
            guard = session.steering.lock() => guard,
            () = cancel.cancelled() => return Err(acp::Error::request_cancelled()),
        };
        check_cancel(cancel, &self.peer)?;
        let is_meta = is_meta_prompt(&req);
        let parts = turn::convert(&self.server, &session, req.prompt, cancel).await?;
        if let Some(state) = session.turn.current() {
            if state.running.load(std::sync::atomic::Ordering::Acquire)
                && session.kit.is_running()
                && !state.cancel.is_cancelled()
            {
                session
                    .kit
                    .steer_parts(uuid::Uuid::new_v4().to_string(), parts, is_meta)
                    .await;
                return Ok(json!({"outcome":"injected"}));
            }
            if !prompt_required {
                return Ok(json!({"outcome":"failed"}));
            }
        }
        if prompt_required {
            return Ok(json!({"outcome":"promptRequired","reason":"noRunningTurn"}));
        }
        if !self.server.auth.authenticated_for(&session.kit.model()) {
            return Err(acp::Error::auth_required());
        }
        let guard = session.turn.begin()?;
        drop(operation);
        let server = self.server.clone();
        let (started, running) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _guard = guard;
            let _ = started.send(());
            let never = CancelToken::new();
            let state = &_guard.state;
            let result = turn::drive(
                &session,
                state,
                &never,
                &never,
                turn::run_prompt(&server, &session, state, parts, is_meta),
            )
            .await;
            if result.is_err() {
                session.bridge.update(acp::SessionUpdate::AgentMessageChunk(
                    acp::ContentChunk::new(acp::ContentBlock::from(
                        "The model could not complete the request.",
                    )),
                ));
            }
        });
        running
            .await
            .map_err(|_| failed("Could not start the turn."))?;
        Ok(json!({"outcome":"startedNewTurn"}))
    }
}

fn check_cancel(cancel: &CancelToken, peer: &Peer) -> Result<()> {
    if cancel.is_cancelled() || peer.closed.is_cancelled() {
        Err(acp::Error::request_cancelled())
    } else {
        Ok(())
    }
}

fn meta(value: Value) -> acp::Meta {
    serde_json::from_value(value).expect("ACP metadata is an object")
}

fn is_meta_prompt(req: &acp::PromptRequest) -> bool {
    req.meta
        .as_ref()
        .and_then(|meta| meta.get("isMeta").or_else(|| meta.get("is_meta")))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}
