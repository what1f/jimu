use crate::{
    AcpServer,
    auth::Auth,
    rpc::{Result, failed, invalid},
    schema::v1 as acp,
    session::{Mode, Session, branch},
    tools_bridge::{Bridge, delegated, presentation},
};
use base64::Engine;
use futures_util::{StreamExt, stream};
use pikra_agent::{AgentEvent, AgentMessage};
use pikra_ai::{
    AssistantBlock, AssistantEventStream, AssistantMessage, AssistantMessageEvent, BoxFuture,
    CancelToken, ContentPart, HttpTransport, ImageContent, Message, Model, SimpleStreamOptions,
    StopReason, StreamProvider, TextContent, TranscriptContext, Usage,
};
use pikra_kit::KitEvent;
use pikra_session::{
    CompactionInput, CompactionOutput, CompactionStrategy, DefaultCompactionStrategy,
    SessionEntryPayload, SessionError,
};
use serde_json::Value;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Notify;
use uuid::Uuid;

#[derive(Default)]
pub(crate) struct TurnSlot {
    inner: Mutex<Slot>,
    change: Notify,
}

#[derive(Default)]
struct Slot {
    closed: bool,
    progress: Option<Arc<TurnState>>,
}

#[derive(Default)]
pub(crate) struct TurnState {
    pub(crate) cancel: CancelToken,
    pub(crate) running: AtomicBool,
}

pub(crate) struct TurnGuard {
    slot: Arc<TurnSlot>,
    pub(crate) state: Arc<TurnState>,
}
impl Drop for TurnGuard {
    fn drop(&mut self) {
        self.slot
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .progress = None;
        self.slot.change.notify_waiters();
    }
}

impl TurnSlot {
    pub(crate) fn begin(self: &Arc<Self>) -> Result<TurnGuard> {
        let mut slot = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if slot.closed {
            return Err(invalid("The session is closed."));
        }
        if slot.progress.is_some() {
            return Err(failed("A turn is already running."));
        }
        let state = Arc::new(TurnState::default());
        slot.progress = Some(state.clone());
        Ok(TurnGuard {
            slot: self.clone(),
            state,
        })
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).closed
    }

    pub(crate) fn current(&self) -> Option<Arc<TurnState>> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .progress
            .clone()
    }
    pub(crate) fn cancel(&self) {
        if let Some(turn) = self.current() {
            turn.cancel.cancel();
        }
    }
    pub(crate) fn close(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.closed = true;
        if let Some(turn) = &inner.progress {
            turn.cancel.cancel();
        }
    }
    pub(crate) async fn wait(&self) {
        loop {
            let changed = self.change.notified();
            if self.current().is_none() {
                return;
            }
            changed.await;
        }
    }
}

pub(crate) struct Compaction {
    bridge: Arc<Bridge>,
    turn: Arc<TurnSlot>,
    active: Mutex<Option<String>>,
    focus: Mutex<Option<String>>,
}

impl Compaction {
    pub(crate) fn new(bridge: Arc<Bridge>, turn: Arc<TurnSlot>) -> Self {
        Self {
            bridge,
            turn,
            active: Mutex::default(),
            focus: Mutex::default(),
        }
    }

    fn complete(&self, status: acp::CompactionStatus) {
        if let Some(id) = self.active.lock().unwrap().take() {
            self.notify(id, status);
        }
        self.focus.lock().unwrap().take();
    }

    fn notify(&self, id: String, status: acp::CompactionStatus) {
        if self
            .bridge
            .capabilities
            .session
            .as_ref()
            .is_some_and(|session| session.compaction.is_some())
        {
            self.bridge.update(acp::SessionUpdate::CompactionUpdate(
                acp::CompactionUpdate::new(id, status),
            ));
        } else if status == acp::CompactionStatus::Completed {
            self.bridge.update(acp::SessionUpdate::AgentMessageChunk(
                acp::ContentChunk::new(acp::ContentBlock::from("Conversation summarized.")),
            ));
        }
    }
}

impl CompactionStrategy for Compaction {
    fn compact<'a>(
        &'a self,
        provider: &'a dyn StreamProvider,
        mut input: CompactionInput,
    ) -> BoxFuture<'a, std::result::Result<CompactionOutput, SessionError>> {
        Box::pin(async move {
            let id = input
                .branch_entries
                .last()
                .map_or_else(|| Uuid::new_v4().to_string(), |entry| entry.id.clone());
            *self.active.lock().unwrap() = Some(id.clone());
            self.notify(id, acp::CompactionStatus::InProgress);
            if let Some(turn) = self.turn.current() {
                input.options.cancel = turn.cancel.clone();
                turn.running.store(false, Ordering::Release);
            }
            input.config.custom_instructions = self.focus.lock().unwrap().take();
            DefaultCompactionStrategy.compact(provider, input).await
        })
    }
}

pub(crate) struct ModelStream {
    provider: Arc<dyn StreamProvider>,
    transport: Arc<dyn HttpTransport>,
    auth: Arc<Auth>,
    override_provider: bool,
    bridge: Arc<Bridge>,
    turn: Arc<TurnSlot>,
    compaction: Arc<Compaction>,
}

impl ModelStream {
    pub(crate) fn new(
        server: &AcpServer,
        bridge: &Arc<Bridge>,
        turn: Arc<TurnSlot>,
        compaction: Arc<Compaction>,
    ) -> Self {
        Self {
            provider: server.provider.clone(),
            transport: server.transport.clone(),
            auth: server.auth.clone(),
            override_provider: server.config.provider_override.is_some(),
            bridge: bridge.clone(),
            turn,
            compaction,
        }
    }
}

fn error_assistant_message(model: &Model, message: String) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_id: None,
        thinking_level: None,
        usage: Usage::default(),
        stop_reason: StopReason::Error,
        error_message: Some(message),
        diagnostics: Vec::new(),
        timestamp: crate::auth::now_ms(),
        duration_ms: None,
    }
}

impl StreamProvider for ModelStream {
    fn stream(
        &self,
        model: &Model,
        context: &TranscriptContext,
        mut options: SimpleStreamOptions,
    ) -> AssistantEventStream {
        let model = model.clone();
        let context = context.clone();
        let summarizing = self
            .compaction
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some();
        let turn = self.turn.current();
        let bridge = self.bridge.clone();
        let provider = self.provider.clone();
        let transport = self.transport.clone();
        let auth = self.auth.clone();
        let override_provider = self.override_provider;
        Box::pin(
            stream::once(async move {
                let (mut resolved, headers) = match auth.resolve_call(&model, override_provider) {
                    Ok(pair) => pair,
                    Err(message) => {
                        let error = error_assistant_message(&model, message);
                        return Box::pin(stream::once(async move {
                            AssistantMessageEvent::Error {
                                reason: StopReason::Error,
                                error,
                            }
                        })) as AssistantEventStream;
                    }
                };
                if !override_provider {
                    options.api_key = auth.api_key(&resolved, &*transport, &options.cancel).await;
                    resolved.headers.extend(headers);
                }
                if !summarizing
                    && let Some(turn) = &turn
                    && !turn.cancel.is_cancelled()
                {
                    turn.running.store(true, Ordering::Release);
                }
                let upstream = provider.stream(&resolved, &context, options);
                Box::pin(upstream.map(move |event| {
                    if !summarizing {
                        let update = match &event {
                            AssistantMessageEvent::TextDelta { delta, .. } if !delta.is_empty() => {
                                Some(acp::SessionUpdate::AgentMessageChunk(
                                    acp::ContentChunk::new(acp::ContentBlock::from(delta.as_str())),
                                ))
                            }
                            AssistantMessageEvent::ThinkingDelta { delta, .. }
                                if !delta.is_empty() =>
                            {
                                Some(acp::SessionUpdate::AgentThoughtChunk(
                                    acp::ContentChunk::new(acp::ContentBlock::from(delta.as_str())),
                                ))
                            }
                            _ => None,
                        };
                        if let Some(update) = update {
                            bridge.update(update);
                        }
                    }
                    event
                })) as AssistantEventStream
            })
            .flatten(),
        )
    }
}

pub(crate) async fn convert(
    server: &AcpServer,
    session: &Session,
    items: Vec<acp::ContentBlock>,
    cancel: &CancelToken,
) -> Result<Vec<ContentPart>> {
    let mut parts = Vec::new();
    let images = server
        .auth
        .routed_model(&session.kit.model())
        .supports_images;
    for item in items {
        if cancel.is_cancelled() {
            return Err(acp::Error::request_cancelled());
        }
        let part = match item {
            acp::ContentBlock::Text(text) => ContentPart::Text(TextContent::plain(text.text)),
            acp::ContentBlock::Image(image) => image_part(images, image.data, image.mime_type)?,
            acp::ContentBlock::Resource(resource) => match resource.resource {
                acp::EmbeddedResourceResource::TextResourceContents(resource) => {
                    context_part(&resource.uri, &resource.text)
                }
                acp::EmbeddedResourceResource::BlobResourceContents(resource) => {
                    if let Some(mime_type) =
                        resource.mime_type.filter(|mime| mime.starts_with("image/"))
                    {
                        image_part(images, resource.blob, mime_type)?
                    } else if let Ok(bytes) =
                        base64::engine::general_purpose::STANDARD.decode(resource.blob)
                        && let Ok(text) = std::str::from_utf8(&bytes)
                    {
                        context_part(&resource.uri, text)
                    } else {
                        context_part(&resource.uri, "Binary attachment")
                    }
                }
                _ => return Err(invalid("Unsupported attachment.")),
            },
            acp::ContentBlock::ResourceLink(link) => {
                let path = url::Url::parse(&link.uri)
                    .ok()
                    .and_then(|url| url.to_file_path().ok());
                let text = if let Some(path) = path {
                    match session.bridge.read_ide(&path, None, None, cancel).await {
                        Some(Ok(text)) => Some(text),
                        Some(Err(err)) => return Err(err),
                        None => {
                            if session.bridge.is_inside_roots(&path)
                                && tokio::fs::metadata(&path)
                                    .await
                                    .is_ok_and(|meta| meta.is_file() && meta.len() <= 1_048_576)
                            {
                                tokio::fs::read_to_string(&path).await.ok()
                            } else {
                                None
                            }
                        }
                    }
                } else {
                    None
                };
                let uri = xml(&link.uri);
                let name = xml(&link.name);
                let text = text.as_deref().map(xml).unwrap_or_default();
                ContentPart::Text(TextContent::plain(format!(
                    "<resource_link uri=\"{uri}\" name=\"{name}\">{text}</resource_link>"
                )))
            }
            _ => return Err(invalid("Unsupported prompt content.")),
        };
        parts.push(part);
    }
    Ok(parts)
}

fn image_part(supports_images: bool, data: String, mime_type: String) -> Result<ContentPart> {
    if !supports_images {
        return Err(acp::Error::new(
            -32600,
            "The selected model cannot read images.",
        ));
    }
    Ok(ContentPart::Image(ImageContent { data, mime_type }))
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
fn context_part(uri: &str, text: &str) -> ContentPart {
    ContentPart::Text(TextContent::plain(format!(
        "<context uri=\"{}\">\n{}\n</context>",
        xml(uri),
        xml(text)
    )))
}

pub(crate) fn content_block(part: &ContentPart) -> acp::ContentBlock {
    match part {
        ContentPart::Text(text) => acp::ContentBlock::from(text.text.as_str()),
        ContentPart::Image(image) => {
            acp::ContentBlock::Image(acp::ImageContent::new(&image.data, &image.mime_type))
        }
    }
}

fn visible_text(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) async fn prompt(
    server: &AcpServer,
    session: &Session,
    guard: TurnGuard,
    items: Vec<acp::ContentBlock>,
    is_meta: bool,
    rpc_cancel: &CancelToken,
    session_cancel: &CancelToken,
) -> Result<acp::PromptResponse> {
    let run = async {
        let parts = convert(server, session, items, &guard.state.cancel).await?;
        run_prompt(server, session, &guard.state, parts, is_meta).await
    };
    drive(session, &guard.state, rpc_cancel, session_cancel, run).await
}

pub(crate) async fn drive(
    session: &Session,
    state: &TurnState,
    rpc_cancel: &CancelToken,
    session_cancel: &CancelToken,
    run: impl Future<Output = Result<acp::PromptResponse>>,
) -> Result<acp::PromptResponse> {
    let cancelled = || Ok(acp::PromptResponse::new(acp::StopReason::Cancelled));
    if rpc_cancel.is_cancelled() || session_cancel.is_cancelled() || state.cancel.is_cancelled() {
        return cancelled();
    }
    session.bridge.start_turn(state.cancel.clone());
    tokio::pin!(run);
    let result = tokio::select! {
        biased;
        () = rpc_cancel.cancelled() => None,
        () = session_cancel.cancelled() => None,
        () = state.cancel.cancelled() => None,
        () = session.bridge.peer.closed.cancelled() => None,
        result = &mut run => Some(result),
    };
    let result = if let Some(result) = result {
        result
    } else {
        state.cancel.cancel();
        session.kit.abort_now();
        let _ = run.await;
        cancelled()
    };
    state.running.store(false, Ordering::Release);
    session.bridge.settle();
    if state.cancel.is_cancelled() {
        cancelled()
    } else {
        result
    }
}

pub(crate) async fn run_prompt(
    server: &AcpServer,
    session: &Session,
    state: &TurnState,
    mut parts: Vec<ContentPart>,
    is_meta: bool,
) -> Result<acp::PromptResponse> {
    if state.cancel.is_cancelled() {
        return Ok(acp::PromptResponse::new(acp::StopReason::Cancelled));
    }
    let first_text = parts
        .first()
        .filter(|_| !is_meta)
        .and_then(|part| match part {
            ContentPart::Text(text) => Some(text.text.trim_start()),
            _ => None,
        })
        .unwrap_or_default();
    let (command, args) = first_text
        .split_once(char::is_whitespace)
        .map_or((first_text, ""), |(word, rest)| (word, rest.trim()));
    let mut compact = false;
    match command {
        "/reload-skills" => {
            session.kit.reload_skills();
            session.bridge.update(session.commands());
            return Ok(acp::PromptResponse::new(acp::StopReason::EndTurn));
        }
        "/plan" => {
            let mut meta = session.metadata();
            let changed = meta.mode != Mode::Plan;
            meta.mode = Mode::Plan;
            if changed {
                session.apply_metadata(server, &meta)?;
                session.notify_options(server, true);
            }
            if args.is_empty() && parts.len() == 1 {
                return Ok(acp::PromptResponse::new(acp::StopReason::EndTurn));
            }
            parts[0] = ContentPart::Text(TextContent::plain(args));
        }
        "/compact" => {
            compact = true;
            *session.compaction.focus.lock().unwrap() = (!args.is_empty()).then(|| args.to_owned());
        }
        command
            if command.starts_with('/') && session.kit.skills().get(&command[1..]).is_some() =>
        {
            parts[0] = ContentPart::Text(TextContent::plain(format!("${}", &first_text[1..])));
        }
        _ => {}
    }

    let (model, _) = server
        .auth
        .resolve_call(
            &session.kit.model(),
            server.config.provider_override.is_some(),
        )
        .map_err(|err| failed(&err))?;
    if !server.auth.authenticated_for(&model) {
        return Err(acp::Error::auth_required());
    }
    if !compact && let Some(bg_prompt) = session.kit.drain_background_prompt() {
        session.kit.append_meta(bg_prompt);
    }
    let before = session.kit.leaf_id();
    let (stop, failed_turn) = if compact {
        (StopReason::Stop, session.kit.compact().await.is_err())
    } else {
        let stream = if is_meta {
            session.kit.prompt_meta_parts_stream(parts.clone())
        } else {
            session.kit.prompt_parts_stream(parts.clone())
        };
        let mut stop = StopReason::Stop;
        let mut failed_turn = stream.is_err();
        if let Ok(mut stream) = stream {
            while let Some(event) = stream.next().await {
                match event {
                    KitEvent::Agent(AgentEvent::MessageStart {
                        message: AgentMessage::Llm(Message::User(user)),
                    }) if !user.is_meta && user.id.is_some() => {
                        for part in &user.content {
                            session.bridge.update(acp::SessionUpdate::UserMessageChunk(
                                acp::ContentChunk::new(content_block(part)),
                            ));
                        }
                    }
                    KitEvent::Agent(AgentEvent::TurnEnd { message, .. }) => {
                        stop = message.stop_reason
                    }
                    KitEvent::Error(_) => failed_turn = true,
                    _ => {}
                }
            }
        }
        (stop, failed_turn)
    };

    let entries = branch(session.kit.entries());
    let latest = entries
        .iter()
        .rev()
        .take_while(|entry| Some(&entry.id) != before.as_ref());
    let compacted = latest
        .clone()
        .any(|entry| matches!(entry.payload, SessionEntryPayload::Compaction { .. }));
    session.compaction.complete(if compacted {
        acp::CompactionStatus::Completed
    } else if state.cancel.is_cancelled() {
        acp::CompactionStatus::Cancelled
    } else {
        acp::CompactionStatus::Failed
    });

    let meta = session.metadata();
    if (!compact || compacted) && latest.clone().next().is_some() {
        let title = if meta.title.is_none() && !is_meta && !compact && !state.cancel.is_cancelled()
        {
            let plain = visible_text(&parts);
            let line = plain.split_whitespace().collect::<Vec<_>>().join(" ");
            (!line.is_empty()).then(|| line.chars().take(60).collect::<String>())
        } else {
            None
        };
        let updated = server.sessions.update(&session.record, |meta| {
            if meta.title.is_none() {
                meta.title = title;
            }
        })?;
        if updated.title != meta.title {
            session.notify_info(&updated);
        }
    }

    let mut cost = 0.0;
    let mut last_usage = None;
    let mut turn_usage = Usage::default();
    for entry in &entries {
        let usage = match &entry.payload {
            SessionEntryPayload::Message {
                message: AgentMessage::Llm(Message::Assistant(message)),
            } => Some(&message.usage),
            SessionEntryPayload::Compaction { usage, .. } => usage.as_ref(),
            _ => None,
        };
        if let Some(usage) = usage {
            cost += usage.cost_total;
            if usage_tokens(usage) > 0 {
                last_usage = Some(usage);
            }
        }
    }
    for entry in latest {
        let usage = match &entry.payload {
            SessionEntryPayload::Message {
                message: AgentMessage::Llm(Message::Assistant(message)),
            } => Some(&message.usage),
            SessionEntryPayload::Compaction { usage, .. } => usage.as_ref(),
            _ => None,
        };
        if let Some(usage) = usage {
            turn_usage.input = turn_usage.input.saturating_add(usage.input);
            turn_usage.output = turn_usage.output.saturating_add(usage.output);
            turn_usage.cache_read = turn_usage.cache_read.saturating_add(usage.cache_read);
            turn_usage.cache_write = turn_usage.cache_write.saturating_add(usage.cache_write);
            turn_usage.total_tokens = turn_usage.total_tokens.saturating_add(usage_tokens(usage));
            if let Some(thought) = usage.reasoning {
                turn_usage.reasoning =
                    Some(turn_usage.reasoning.unwrap_or(0).saturating_add(thought));
            }
        }
    }
    let used = if compacted {
        pikra_session::estimate_context_tokens(&session.kit.messages()).total_tokens
    } else {
        last_usage.map_or(0, usage_tokens)
    };
    let usage = acp::UsageUpdate::new(used, u64::from(model.context_window))
        .cost((cost.is_finite() && cost > 0.0).then(|| acp::Cost::new(cost, "USD")));
    session
        .bridge
        .update(acp::SessionUpdate::UsageUpdate(usage));

    let reason = if state.cancel.is_cancelled() || stop == StopReason::Aborted {
        acp::StopReason::Cancelled
    } else if failed_turn || stop == StopReason::Error {
        return Err(failed("The model could not complete the request."));
    } else if stop == StopReason::Length {
        acp::StopReason::MaxTokens
    } else {
        acp::StopReason::EndTurn
    };
    let mut response = acp::PromptResponse::new(reason);
    if turn_usage.total_tokens > 0 {
        let usage = acp::Usage::new(
            turn_usage.total_tokens,
            turn_usage
                .input
                .saturating_add(turn_usage.cache_read)
                .saturating_add(turn_usage.cache_write),
            turn_usage.output,
        )
        .thought_tokens(turn_usage.reasoning)
        .cached_read_tokens(Some(turn_usage.cache_read))
        .cached_write_tokens(Some(turn_usage.cache_write));
        response = response.usage(usage);
    }
    Ok(response)
}

fn usage_tokens(usage: &Usage) -> u64 {
    if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        usage
            .input
            .saturating_add(usage.cache_read)
            .saturating_add(usage.cache_write)
            .saturating_add(usage.output)
    }
}

pub(crate) async fn replay(session: &Session, cancel: &CancelToken) -> Result<()> {
    let store = session.record.store.clone();
    let loaded =
        tokio::task::spawn_blocking(move || store.load().map(|(_, entries)| branch(entries)));
    let entries = tokio::select! {
        result = loaded => result.ok().and_then(std::result::Result::ok)
            .ok_or_else(|| failed("Could not read the session history."))?,
        () = cancel.cancelled() => return Err(acp::Error::request_cancelled()),
        () = session.bridge.peer.closed.cancelled() => return Err(acp::Error::request_cancelled()),
    };
    let bridge = &session.bridge;
    for (index, entry) in entries.into_iter().enumerate() {
        if index.is_multiple_of(64) {
            tokio::task::yield_now().await;
        }
        if cancel.is_cancelled() || bridge.peer.closed.is_cancelled() {
            return Err(acp::Error::request_cancelled());
        }
        match entry.payload {
            SessionEntryPayload::Message {
                message: AgentMessage::Llm(Message::User(user)),
            } if !user.is_meta => {
                for part in &user.content {
                    bridge.update(acp::SessionUpdate::UserMessageChunk(
                        acp::ContentChunk::new(content_block(part)),
                    ));
                }
            }
            SessionEntryPayload::Message {
                message: AgentMessage::Llm(Message::Assistant(assistant)),
            } => {
                for block in assistant.content {
                    match block {
                        AssistantBlock::Text(text) => {
                            bridge.update(acp::SessionUpdate::AgentMessageChunk(
                                acp::ContentChunk::new(acp::ContentBlock::from(text.text.as_str())),
                            ))
                        }
                        AssistantBlock::Thinking(text) => bridge.update(
                            acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(
                                acp::ContentBlock::from(text.thinking.as_str()),
                            )),
                        ),
                        AssistantBlock::ToolCall(call) => {
                            let input = Value::Object(call.arguments);
                            let (name, args) = delegated(&call.name, &input, &Value::Null);
                            let (kind, title) = presentation(name);
                            bridge.update(acp::SessionUpdate::ToolCall(
                                acp::ToolCall::new(call.id, title)
                                    .name(name.to_owned())
                                    .kind(kind)
                                    .status(acp::ToolCallStatus::Pending)
                                    .raw_input(args.clone()),
                            ));
                        }
                    }
                }
            }
            SessionEntryPayload::Message {
                message: AgentMessage::Llm(Message::ToolResult(result)),
            } => {
                let fields = acp::ToolCallUpdateFields::new()
                    .status(if result.is_error {
                        acp::ToolCallStatus::Failed
                    } else {
                        acp::ToolCallStatus::Completed
                    })
                    .raw_output(serde_json::json!(result.content))
                    .content(
                        result
                            .content
                            .iter()
                            .map(|part| {
                                acp::ToolCallContent::Content(acp::Content::new(content_block(
                                    part,
                                )))
                            })
                            .collect::<Vec<_>>(),
                    );
                bridge.update(acp::SessionUpdate::ToolCallUpdate(
                    acp::ToolCallUpdate::new(result.tool_call_id, fields),
                ));
            }
            SessionEntryPayload::Compaction { .. }
                if bridge
                    .capabilities
                    .session
                    .as_ref()
                    .is_some_and(|caps| caps.compaction.is_some()) =>
            {
                let id = entry.parent_id.unwrap_or(entry.id);
                bridge.update(acp::SessionUpdate::CompactionUpdate(
                    acp::CompactionUpdate::new(id, acp::CompactionStatus::Completed),
                ));
            }
            _ => {}
        }
    }
    Ok(())
}
