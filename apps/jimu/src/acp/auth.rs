//! Thin ACP v1 protocol adapter over the global [`crate::providers::ProviderStore`].

use crate::{
    acp::{
        AcpConfig,
        rpc::{Result, failed, invalid},
        schema::v1 as acp,
    },
    providers::{
        CustomModel, Headers, ProviderRoute, ProviderStore, ProviderStoreConfig,
        ProviderUpdateError, preset,
    },
};
use jimu_ai::{ApiKind, CancelToken, HttpTransport, Model, ThinkingLevel};
use serde_json::Value;

pub(crate) use crate::providers::{
    model_key, now_ms, parse_base_url as base_url, read_optional, save,
};

pub(crate) struct Auth {
    store: ProviderStore,
}

impl Auth {
    pub(crate) fn new(config: AcpConfig) -> Self {
        Self {
            store: ProviderStore::new(ProviderStoreConfig::from(&config)),
        }
    }

    pub(crate) fn store(&self) -> &ProviderStore {
        &self.store
    }

    pub(crate) fn status(&self) -> acp::AuthStatusResponse {
        let routes = self.store.routes();
        let secrets = self.store.secrets();
        self.store.remember_secrets(&routes, &secrets);
        let mut accounts = Vec::new();
        if self.store.config().provider_override {
            accounts.push(acp::AuthAccount::new("Configured provider").active(true));
        }
        for id in self.store.ids(&routes) {
            if self.store.has_key(&id, &routes, &secrets) {
                accounts.push(
                    acp::AuthAccount::new(
                        preset(&id).map_or("Custom provider", |entry| entry.title),
                    )
                    .active(true),
                );
            }
        }
        let state = if accounts.is_empty() {
            acp::AuthState::Unauthenticated
        } else {
            acp::AuthState::Authenticated
        };
        acp::AuthStatusResponse::new(state).accounts(accounts)
    }

    pub(crate) fn redact(&self, value: &mut Value) {
        self.store.redact(value);
    }

    pub(crate) fn authenticated_for(&self, model: &Model) -> bool {
        self.store.authenticated_for(model)
    }

    pub(crate) fn default_model(&self) -> Model {
        self.store
            .default_model()
            .unwrap_or_else(|| self.store.unconfigured_placeholder_model())
    }

    pub(crate) fn models(&self) -> Vec<Model> {
        self.store.models()
    }

    pub(crate) fn select_model(&self, key: &str, current_provider: Option<&str>) -> Option<Model> {
        self.store.select_model(key, current_provider)
    }

    pub(crate) fn model(&self, provider: &str, id: &str) -> Model {
        self.store.model(provider, id)
    }

    pub(crate) fn thinking_levels(&self, provider: &str, id: &str) -> Option<Vec<ThinkingLevel>> {
        self.store.thinking_levels(provider, id)
    }

    pub(crate) fn resolve_call(
        &self,
        model: &Model,
        allow_override: bool,
    ) -> std::result::Result<(Model, Headers), String> {
        self.store.resolve_call(model, allow_override)
    }

    pub(crate) fn routed_model(&self, model: &Model) -> Model {
        self.store.routed_model(model)
    }

    pub(crate) async fn api_key(
        &self,
        model: &Model,
        http: &dyn HttpTransport,
        cancel: &CancelToken,
    ) -> Option<String> {
        self.store.api_key(model, http, cancel).await
    }

    pub(crate) async fn logout(&self) -> Result<()> {
        self.store.logout().await.map_err(|msg| failed(&msg))
    }

    pub(crate) fn providers(&self) -> acp::ListProvidersResponse {
        let routes = self.store.routes();
        let mut items = Vec::new();
        for id in self.store.ids(&routes) {
            let route = routes.get(&id);
            let active = !route.is_some_and(|route| route.disabled);
            let model = self.store.build_model(&id, "", preset(&id), route);
            let protocol = llm_protocol(&model.api);
            let supported = vec![
                protocol.clone(),
                acp::LlmProtocol::OpenAi,
                acp::LlmProtocol::Anthropic,
                acp::LlmProtocol::Other("_google".into()),
            ];
            let mut supported_unique = Vec::new();
            for value in supported {
                if !supported_unique.contains(&value) {
                    supported_unique.push(value);
                }
            }
            let current = active.then(|| acp::ProviderCurrentConfig::new(protocol, model.base_url));
            items.push(acp::ProviderInfo::new(id, supported_unique, false, current));
        }
        acp::ListProvidersResponse::new(items)
    }

    pub(crate) fn set(&self, req: acp::SetProviderRequest) -> Result<()> {
        let provider = req.provider_id.0.as_ref();
        if provider.is_empty()
            || !provider
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        {
            return Err(invalid("Invalid provider identifier."));
        }
        let base_url = base_url(&req.base_url)
            .ok_or_else(|| invalid("Enter a base URL without credentials, query, or fragment."))?
            .to_string();
        let api = match req.api_type {
            acp::LlmProtocol::Anthropic => ApiKind::AnthropicMessages,
            acp::LlmProtocol::OpenAi => {
                preset(provider).map_or(ApiKind::OpenaiResponses, |entry| match entry.api {
                    ApiKind::OpenaiCompletions | ApiKind::OpenaiCodexResponses => entry.api.clone(),
                    _ => ApiKind::OpenaiResponses,
                })
            }
            acp::LlmProtocol::Other(name) if name == "_google" => ApiKind::GoogleGenerativeAi,
            acp::LlmProtocol::Other(name) if name == "_openai_completions" => {
                ApiKind::OpenaiCompletions
            }
            _ => return Err(invalid("Unsupported API protocol.")),
        };
        let models: Vec<CustomModel> = req
            .meta
            .as_ref()
            .and_then(|meta| meta.get("models"))
            .map(|value| serde_json::from_value(value.clone()))
            .transpose()
            .map_err(|_| invalid("Invalid model list."))?
            .unwrap_or_default();
        if models.iter().any(|model| model.id().trim().is_empty()) {
            return Err(invalid("Model identifiers cannot be empty."));
        }
        self.store
            .update_provider(
                provider,
                Some(ProviderRoute {
                    api: Some(api),
                    base_url: Some(base_url),
                    api_key_env: None,
                    models,
                    disabled: false,
                }),
                req.headers.into_iter().collect(),
            )
            .map_err(|err| match err {
                ProviderUpdateError::Invalid(msg) => invalid(&msg),
                ProviderUpdateError::Failed(msg) => failed(&msg),
            })
    }

    pub(crate) fn disable(&self, provider: &str) -> Result<()> {
        self.store
            .update_provider(provider, None, Headers::new())
            .map_err(|err| match err {
                ProviderUpdateError::Invalid(msg) => invalid(&msg),
                ProviderUpdateError::Failed(msg) => failed(&msg),
            })
    }
}

fn llm_protocol(api: &ApiKind) -> acp::LlmProtocol {
    match api {
        ApiKind::AnthropicMessages => acp::LlmProtocol::Anthropic,
        ApiKind::GoogleGenerativeAi => acp::LlmProtocol::Other("_google".into()),
        _ => acp::LlmProtocol::OpenAi,
    }
}
