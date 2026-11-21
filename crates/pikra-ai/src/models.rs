//! Unified streaming `Client`, `Models` registry/catalog, and `complete()` helper.

use crate::providers::{
    anthropic_messages, drive_event_stream, emit_protocol_error, google_generative_ai,
    new_assistant_message, now_ms, openai_codex_responses, openai_completions, openai_responses,
};
use crate::{
    ApiKind, AssistantEventStream, AssistantMessage, AssistantMessageEvent, HttpTransport, Model,
    SimpleStreamOptions, StopReason, StreamProvider, TranscriptContext,
};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;

/// Stateless multi-protocol streaming client dispatching by `Model.api` over an injected `HttpTransport`.
#[derive(Clone)]
pub struct Client {
    transport: Arc<dyn HttpTransport>,
}

impl Client {
    pub fn new(transport: Arc<dyn HttpTransport>) -> Self {
        Self { transport }
    }

    pub fn transport(&self) -> &Arc<dyn HttpTransport> {
        &self.transport
    }

    pub async fn complete(
        &self,
        model: &Model,
        context: &TranscriptContext,
        options: SimpleStreamOptions,
    ) -> AssistantMessage {
        complete(self, model, context, options).await
    }
}

impl StreamProvider for Client {
    fn stream(
        &self,
        model: &Model,
        context: &TranscriptContext,
        options: SimpleStreamOptions,
    ) -> AssistantEventStream {
        match &model.api {
            ApiKind::OpenaiResponses => openai_responses::stream(
                self.transport.clone(),
                model.clone(),
                context.clone(),
                options,
            ),
            ApiKind::OpenaiCodexResponses => openai_codex_responses::stream(
                self.transport.clone(),
                model.clone(),
                context.clone(),
                options,
            ),
            ApiKind::OpenaiCompletions => openai_completions::stream(
                self.transport.clone(),
                model.clone(),
                context.clone(),
                options,
            ),
            ApiKind::AnthropicMessages => anthropic_messages::stream(
                self.transport.clone(),
                model.clone(),
                context.clone(),
                options,
            ),
            ApiKind::GoogleGenerativeAi => google_generative_ai::stream(
                self.transport.clone(),
                model.clone(),
                context.clone(),
                options,
            ),
            ApiKind::Custom(api_name) => {
                let model_clone = model.clone();
                let api_name = api_name.clone();
                let cancel = options.cancel.clone();
                let reasoning = options.reasoning;
                drive_event_stream(move |sink| async move {
                    let started_at = now_ms();
                    let output = new_assistant_message(&model_clone, reasoning);
                    emit_protocol_error(
                        &sink,
                        output,
                        "unsupported_api",
                        format!("unsupported custom API kind: {api_name}"),
                        &cancel,
                        started_at,
                    );
                })
            }
        }
    }
}

/// Provider-level defaults (credentials, endpoint override, default headers) registered on `Models`.
#[derive(Debug, Clone, Default)]
pub struct ProviderConfig {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub headers: Vec<(String, String)>,
}

impl ProviderConfig {
    pub fn with_api_key(api_key: impl Into<String>) -> Self {
        Self {
            api_key: Some(api_key.into()),
            base_url: None,
            headers: Vec::new(),
        }
    }
}

/// Model catalog & provider credential registry implementing `StreamProvider`.
#[derive(Clone)]
pub struct Models {
    client: Client,
    models: Vec<Model>,
    providers: HashMap<String, ProviderConfig>,
}

impl Models {
    pub fn new(transport: Arc<dyn HttpTransport>) -> Self {
        Self {
            client: Client::new(transport),
            models: Vec::new(),
            providers: HashMap::new(),
        }
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn register_provider(
        &mut self,
        provider: impl Into<String>,
        config: ProviderConfig,
    ) -> &mut Self {
        self.providers.insert(provider.into(), config);
        self
    }

    pub fn register_model(&mut self, model: Model) -> &mut Self {
        if let Some(existing) = self
            .models
            .iter_mut()
            .find(|m| m.provider == model.provider && m.id == model.id)
        {
            *existing = model;
        } else {
            self.models.push(model);
        }
        self
    }

    pub fn get(&self, provider: &str, id: &str) -> Option<&Model> {
        self.models
            .iter()
            .find(|m| m.provider == provider && m.id == id)
    }

    pub fn get_by_id(&self, id: &str) -> Option<&Model> {
        self.models.iter().find(|m| m.id == id)
    }

    pub fn list(&self) -> &[Model] {
        &self.models
    }

    pub fn provider_config(&self, provider: &str) -> Option<&ProviderConfig> {
        self.providers.get(provider)
    }

    pub async fn complete(
        &self,
        model: &Model,
        context: &TranscriptContext,
        options: SimpleStreamOptions,
    ) -> AssistantMessage {
        complete(self, model, context, options).await
    }
}

impl StreamProvider for Models {
    fn stream(
        &self,
        model: &Model,
        context: &TranscriptContext,
        mut options: SimpleStreamOptions,
    ) -> AssistantEventStream {
        let mut effective_model = model.clone();
        if let Some(cfg) = self.providers.get(&model.provider) {
            if options.api_key.is_none() {
                options.api_key = cfg.api_key.clone();
            }
            if let Some(base_url) = &cfg.base_url {
                effective_model.base_url = base_url.clone();
            }
            if !cfg.headers.is_empty() {
                let mut merged = cfg.headers.clone();
                merged.extend(effective_model.headers);
                effective_model.headers = merged;
            }
        }
        self.client.stream(&effective_model, context, options)
    }
}

/// Consume an entire `AssistantEventStream` and return its terminal `AssistantMessage`.
pub async fn complete(
    provider: &dyn StreamProvider,
    model: &Model,
    context: &TranscriptContext,
    options: SimpleStreamOptions,
) -> AssistantMessage {
    let mut event_stream = provider.stream(model, context, options.clone());
    let mut last_partial: Option<AssistantMessage> = None;

    while let Some(ev) = event_stream.next().await {
        match ev {
            AssistantMessageEvent::Start { partial } => last_partial = Some(partial),
            AssistantMessageEvent::Done { message, .. } => return message,
            AssistantMessageEvent::Error { error, .. } => return error,
            _ => {}
        }
    }

    let mut fallback =
        last_partial.unwrap_or_else(|| new_assistant_message(model, options.reasoning));
    fallback.stop_reason = if options.cancel.is_cancelled() {
        StopReason::Aborted
    } else {
        StopReason::Error
    };
    fallback.error_message = Some("stream terminated without Done or Error event".into());
    fallback
}
