use crate::{
    AcpConfig,
    rpc::{Result, failed, invalid},
    schema::v1 as acp,
};
use pikra_ai::{
    ApiKind, CancelToken, HttpTransport, Model, ModelCost,
    oauth::{self, CodexCredentials},
};
use pikra_http::ReqwestTransport;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Mutex,
};
use url::Url;

struct Builtin {
    id: &'static str,
    title: &'static str,
    api: ApiKind,
    url: &'static str,
    env: &'static [&'static str],
    models: &'static [&'static str],
    context: u32,
    images: bool,
}

static BUILTINS: &[Builtin] = &[
    Builtin {
        id: "anthropic",
        title: "Anthropic",
        api: ApiKind::AnthropicMessages,
        url: "https://api.anthropic.com",
        env: &["ANTHROPIC_API_KEY"],
        models: &["claude-sonnet-4-6", "claude-opus-4-6", "claude-haiku-4-5"],
        context: 200_000,
        images: true,
    },
    Builtin {
        id: "openai",
        title: "OpenAI",
        api: ApiKind::OpenaiResponses,
        url: "https://api.openai.com/v1",
        env: &["OPENAI_API_KEY"],
        models: &["gpt-5.4", "gpt-5.4-mini", "gpt-5.3-codex"],
        context: 272_000,
        images: true,
    },
    Builtin {
        id: "openai-codex",
        title: "ChatGPT / Codex Subscription",
        api: ApiKind::OpenaiCodexResponses,
        url: oauth::CODEX_DEFAULT_BASE_URL,
        env: &[],
        models: &["gpt-5.4", "gpt-5.3-codex"],
        context: 272_000,
        images: true,
    },
    Builtin {
        id: "google",
        title: "Google",
        api: ApiKind::GoogleGenerativeAi,
        url: "https://generativelanguage.googleapis.com/v1beta",
        env: &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        models: &["gemini-2.5-pro", "gemini-2.5-flash"],
        context: 1_048_576,
        images: true,
    },
    Builtin {
        id: "deepseek",
        title: "DeepSeek",
        api: ApiKind::OpenaiCompletions,
        url: "https://api.deepseek.com",
        env: &["DEEPSEEK_API_KEY"],
        models: &["deepseek-chat", "deepseek-reasoner"],
        context: 128_000,
        images: false,
    },
    Builtin {
        id: "openrouter",
        title: "OpenRouter",
        api: ApiKind::OpenaiCompletions,
        url: "https://openrouter.ai/api/v1",
        env: &["OPENROUTER_API_KEY"],
        models: &[
            "anthropic/claude-sonnet-4.6",
            "openai/gpt-5.4",
            "google/gemini-2.5-pro",
        ],
        context: 200_000,
        images: true,
    },
];

type Headers = BTreeMap<String, String>;

#[derive(Clone, Default, Deserialize, Serialize)]
struct Secrets {
    codex: Option<CodexCredentials>,
    #[serde(default)]
    codex_signed_out: bool,
    #[serde(default)]
    headers: BTreeMap<String, ProviderSecret>,
}

#[derive(Clone, Deserialize, Serialize)]
struct ProviderSecret {
    origin: String,
    headers: Headers,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(untagged)]
enum CustomModel {
    Id(String),
    Spec {
        id: String,
        name: Option<String>,
        context_window: Option<u32>,
        supports_images: Option<bool>,
    },
}
impl CustomModel {
    fn id(&self) -> &str {
        match self {
            Self::Id(id) | Self::Spec { id, .. } => id,
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct Route {
    api: ApiKind,
    base_url: String,
    #[serde(default)]
    models: Vec<CustomModel>,
    #[serde(default)]
    disabled: bool,
}

pub(crate) fn read_optional<T: DeserializeOwned>(path: &Path) -> std::io::Result<Option<T>> {
    match File::open(path) {
        Ok(file) => serde_json::from_reader(std::io::BufReader::new(file))
            .map(Some)
            .map_err(std::io::Error::other),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

pub(crate) fn save(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}

fn lock_credentials(home: &Path) -> std::io::Result<File> {
    fs::create_dir_all(home)?;
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(home.join("auth.lock"))?;
    lock.lock()?;
    Ok(lock)
}

fn codex_fallback() -> Option<CodexCredentials> {
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|home| home.join(".codex")))?;
    let value: Value = read_optional(&home.join("auth.json")).ok()??;
    serde_json::from_value::<CodexCredentials>(value.get("tokens").unwrap_or(&value).clone())
        .ok()?
        .normalized()
        .ok()
}

fn usable_codex(secrets: &Secrets) -> Option<CodexCredentials> {
    secrets
        .codex
        .clone()
        .or_else(|| (!secrets.codex_signed_out).then(codex_fallback).flatten())
        .filter(|creds| {
            !creds.access_token.is_empty()
                && (!creds.is_expiring_within(now_ms(), 0) || creds.refresh_token.is_some())
        })
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_millis().min(u128::from(u64::MAX)) as u64
        })
}

pub(crate) struct Auth {
    config: AcpConfig,
    refresh: Mutex<()>,
    redactions: std::sync::RwLock<Vec<String>>,
}

impl Auth {
    pub(crate) fn new(config: AcpConfig) -> Self {
        Self {
            config,
            refresh: Mutex::new(()),
            redactions: std::sync::RwLock::default(),
        }
    }

    fn auth_path(&self) -> PathBuf {
        self.config.pikra_home.join("auth.json")
    }
    fn providers_path(&self) -> PathBuf {
        self.config.pikra_home.join("acp/providers.json")
    }

    fn secrets(&self) -> Secrets {
        read_optional(&self.auth_path())
            .ok()
            .flatten()
            .unwrap_or_default()
    }
    fn routes(&self) -> BTreeMap<String, Route> {
        read_optional(&self.providers_path())
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    fn ids(&self, routes: &BTreeMap<String, Route>) -> Vec<String> {
        let mut ids: Vec<String> = BUILTINS.iter().map(|entry| entry.id.into()).collect();
        for id in routes.keys().chain(&self.config.provider) {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
        ids
    }

    fn cli_provider(&self) -> &str {
        self.config.provider.as_deref().unwrap_or("openai")
    }

    fn trusted_key_origin(&self, id: &str, url: &str) -> bool {
        let cli_matches = self.cli_provider() == id
            && self
                .config
                .base_url
                .as_deref()
                .or_else(|| {
                    self.config
                        .api_key_env
                        .as_ref()
                        .map(|_| builtin(id).unwrap_or(&BUILTINS[1]).url)
                })
                .is_some_and(|configured| same_origin(configured, url));
        cli_matches || builtin(id).is_some_and(|entry| same_origin(entry.url, url))
    }

    fn env_key(&self, id: &str, routes: &BTreeMap<String, Route>) -> Option<String> {
        if routes
            .get(id)
            .is_some_and(|route| !self.trusted_key_origin(id, &route.base_url))
        {
            return None;
        }
        self.env_candidate(id)
    }

    fn env_candidate(&self, id: &str) -> Option<String> {
        if self.cli_provider() == id
            && let Some(name) = &self.config.api_key_env
        {
            return env_value(name);
        }
        builtin(id)?.env.iter().find_map(|name| env_value(name))
    }

    fn has_key(&self, id: &str, routes: &BTreeMap<String, Route>, secrets: &Secrets) -> bool {
        if let Some(route) = routes.get(id) {
            if route.disabled {
                return false;
            }
            if secrets.headers.get(id).is_some_and(|secret| {
                same_origin(&secret.origin, &route.base_url)
                    && secret.headers.values().any(|value| !value.is_empty())
            }) {
                return true;
            }
            if id == "openai-codex" && !same_origin(&route.base_url, oauth::CODEX_DEFAULT_BASE_URL)
            {
                return false;
            }
        }
        self.env_key(id, routes).is_some()
            || (id == "openai-codex" && usable_codex(secrets).is_some())
    }

    pub(crate) fn authenticated_for(&self, model: &Model) -> bool {
        if self.config.provider_override.is_some() {
            return true;
        }
        let route = self.routed_model(model);
        let routes = self.routes();
        if routes
            .get(&route.provider)
            .is_some_and(|route| route.disabled)
        {
            return false;
        }
        if self.headers(&route).values().any(|value| !value.is_empty()) {
            return true;
        }
        (self.trusted_key_origin(&route.provider, &route.base_url)
            && self.env_candidate(&route.provider).is_some())
            || (route.provider == "openai-codex"
                && same_origin(&route.base_url, oauth::CODEX_DEFAULT_BASE_URL)
                && usable_codex(&self.secrets()).is_some())
    }

    pub(crate) fn status(&self) -> acp::AuthStatusResponse {
        let (routes, secrets) = (self.routes(), self.secrets());
        self.remember_secrets(&routes, &secrets);
        let mut accounts = Vec::new();
        if self.config.provider_override.is_some() {
            accounts.push(acp::AuthAccount::new("Configured provider").active(true));
        }
        for id in self.ids(&routes) {
            if self.has_key(&id, &routes, &secrets) {
                // Provider IDs are client-controlled. Only the fixed catalog supplies account labels.
                accounts.push(
                    acp::AuthAccount::new(
                        builtin(&id).map_or("Custom provider", |entry| entry.title),
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
        redact_value(value, &self.redactions.read().unwrap(), false);
    }

    fn remember_secrets(&self, routes: &BTreeMap<String, Route>, secrets: &Secrets) {
        let mut values: Vec<String> = self
            .ids(routes)
            .iter()
            .filter_map(|id| self.env_key(id, routes))
            .collect();
        if let Some(creds) = usable_codex(secrets) {
            values.push(creds.access_token);
            values.extend(creds.refresh_token);
            values.extend(creds.id_token);
        }
        for text in secrets
            .headers
            .values()
            .flat_map(|secret| secret.headers.values())
        {
            values.push(text.clone());
            if let Some((_, key)) = text.split_once(' ') {
                values.push(key.into());
            }
        }
        let mut remembered = self.redactions.write().unwrap();
        for value in values {
            if !value.is_empty() && !remembered.contains(&value) {
                remembered.push(value);
            }
        }
        remembered.sort_by_key(|text| std::cmp::Reverse(text.len()));
    }

    pub(crate) fn default_model(&self) -> Model {
        let routes = self.routes();
        let secrets = self.secrets();
        let provider = self.config.provider.clone().unwrap_or_else(|| {
            self.ids(&routes)
                .into_iter()
                .find(|id| self.has_key(id, &routes, &secrets))
                .or_else(|| {
                    routes
                        .get("openai")
                        .is_some_and(|route| route.disabled)
                        .then(|| {
                            self.ids(&routes)
                                .into_iter()
                                .find(|id| !routes.get(id).is_some_and(|route| route.disabled))
                        })
                        .flatten()
                })
                .unwrap_or_else(|| "openai".into())
        });
        let id = self.config.model.clone().unwrap_or_else(|| {
            routes
                .get(&provider)
                .and_then(|route| route.models.first())
                .map(|model| model.id().into())
                .unwrap_or_else(|| {
                    builtin(&provider)
                        .map_or("gpt-5.4", |entry| entry.models[0])
                        .into()
                })
        });
        self.model(&provider, &id)
    }

    pub(crate) fn models(&self) -> Vec<Model> {
        let routes = self.routes();
        let mut models = Vec::new();
        for provider in self.ids(&routes) {
            if routes.get(&provider).is_some_and(|route| route.disabled) {
                continue;
            }
            if let Some(entry) = builtin(&provider) {
                models.extend(entry.models.iter().map(|id| self.model(&provider, id)));
            }
            if let Some(route) = routes.get(&provider) {
                for spec in &route.models {
                    let model = self.model(&provider, spec.id());
                    if !models
                        .iter()
                        .any(|existing| model_key(existing) == model_key(&model))
                    {
                        models.push(model);
                    }
                }
            }
        }
        let default = self.default_model();
        if !models
            .iter()
            .any(|model| model_key(model) == model_key(&default))
            && !routes
                .get(&default.provider)
                .is_some_and(|route| route.disabled)
        {
            models.push(default);
        }
        models
    }

    pub(crate) fn select_model(&self, key: &str) -> Option<Model> {
        let list = self.models();
        if let Some(model) = list.iter().find(|model| model_key(model) == key) {
            return Some(model.clone());
        }
        let mut bare = list.into_iter().filter(|model| model.id == key);
        let candidate = bare.next()?;
        bare.next().is_none().then_some(candidate)
    }

    pub(crate) fn model(&self, provider: &str, id: &str) -> Model {
        let entry = builtin(provider).unwrap_or(&BUILTINS[1]);
        let routes = self.routes();
        let route = routes.get(provider).filter(|route| !route.disabled);
        let api = route.map_or_else(|| entry.api.clone(), |route| route.api.clone());
        let url = route.map_or(entry.url, |route| &route.base_url);
        let (api, url) = if route.is_none() && provider == self.cli_provider() {
            (
                self.config.api.clone().unwrap_or(api),
                self.config.base_url.as_deref().unwrap_or(url),
            )
        } else {
            (api, url)
        };
        let mut model = Model::new(id, provider, api, url);
        model.supports_images = entry.images;
        model.supports_reasoning = true;
        model.context_window = self.config.context_window.unwrap_or(entry.context);
        model.cost = match id {
            "gpt-5.4" if provider == "openai" => ModelCost {
                input: 2.5,
                output: 15.,
                cache_read: 0.25,
                cache_write: 2.5,
            },
            "claude-sonnet-4-6" => ModelCost {
                input: 3.,
                output: 15.,
                cache_read: 0.3,
                cache_write: 3.75,
            },
            _ => ModelCost::default(),
        };
        if let Some(CustomModel::Spec {
            name,
            context_window,
            supports_images,
            ..
        }) = route.and_then(|route| route.models.iter().find(|spec| spec.id() == id))
        {
            if let Some(name) = name {
                model.name.clone_from(name);
            }
            if let Some(window) = context_window {
                model.context_window = *window;
            }
            if let Some(images) = supports_images {
                model.supports_images = *images;
            }
        }
        model
    }

    pub(crate) fn headers(&self, model: &Model) -> Headers {
        let routes = self.routes();
        if routes
            .get(&model.provider)
            .is_some_and(|route| !route.disabled && same_origin(&model.base_url, &route.base_url))
        {
            return self
                .secrets()
                .headers
                .remove(&model.provider)
                .filter(|secret| same_origin(&secret.origin, &model.base_url))
                .map(|secret| secret.headers)
                .unwrap_or_default();
        }
        Headers::new()
    }

    pub(crate) fn routed_model(&self, model: &Model) -> Model {
        // Active sessions look up routing again on every model request.
        if self.routes().contains_key(&model.provider) {
            return self.model(&model.provider, &model.id);
        }
        model.clone()
    }

    pub(crate) async fn api_key(
        &self,
        model: &Model,
        http: &dyn HttpTransport,
        cancel: &CancelToken,
    ) -> Option<String> {
        let provider = model.provider.as_str();
        let routes = self.routes();
        self.remember_secrets(&routes, &self.secrets());
        if routes.get(provider).is_some_and(|route| route.disabled) {
            return None;
        }
        let key = self
            .trusted_key_origin(provider, &model.base_url)
            .then(|| self.env_candidate(provider))
            .flatten();
        if key.is_some() || provider != "openai-codex" {
            return key;
        }
        if !same_origin(&model.base_url, oauth::CODEX_DEFAULT_BASE_URL) {
            return None;
        }
        let _refresh = tokio::select! {
            guard = self.refresh.lock() => guard,
            () = cancel.cancelled() => return None,
        };
        let creds = usable_codex(&self.secrets())?;
        if !creds.is_expiring_within(now_ms(), 60_000) || creds.refresh_token.is_none() {
            return Some(creds.access_token);
        }
        let refreshed = tokio::time::timeout(
            Duration::from_secs(30),
            oauth::refresh_codex_credentials(http, &creds, cancel.clone()),
        )
        .await
        .ok()?
        .ok()?;
        let _updates = lock_credentials(&self.config.pikra_home).ok()?;
        let mut current: Secrets = read_optional(&self.auth_path()).ok()?.unwrap_or_default();
        if usable_codex(&current).as_ref() != Some(&creds) {
            return usable_codex(&current).map(|other| other.access_token);
        }
        let access = refreshed.access_token.clone();
        current.codex = Some(refreshed);
        save(&self.auth_path(), &current).ok()?;
        self.remember_secrets(&routes, &current);
        Some(access)
    }

    pub(crate) async fn logout(&self) -> Result<()> {
        let _refresh = self.refresh.lock().await;
        let _updates = lock_credentials(&self.config.pikra_home)
            .map_err(|_| failed("Could not update saved credentials."))?;
        let mut secrets: Secrets = read_optional(&self.auth_path())
            .map_err(|_| failed("Could not open saved credentials."))?
            .unwrap_or_default();
        secrets.codex = None;
        secrets.codex_signed_out = true;
        save(&self.auth_path(), &secrets).map_err(|_| failed("Could not save credentials."))
    }

    pub(crate) fn providers(&self) -> acp::ListProvidersResponse {
        let routes = self.routes();
        let mut items = Vec::new();
        for id in self.ids(&routes) {
            let model = self.model(&id, "");
            let active = !routes.get(&id).is_some_and(|route| route.disabled);
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
                builtin(provider).map_or(ApiKind::OpenaiResponses, |entry| match entry.api {
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
        self.update_provider(
            provider,
            Some(Route {
                api,
                base_url,
                models,
                disabled: false,
            }),
            req.headers.into_iter().collect(),
        )
    }

    pub(crate) fn disable(&self, provider: &str) -> Result<()> {
        self.update_provider(provider, None, Headers::new())
    }

    fn update_provider(&self, id: &str, route: Option<Route>, headers: Headers) -> Result<()> {
        let _updates = lock_credentials(&self.config.pikra_home)
            .map_err(|_| failed("Could not update saved credentials."))?;
        let mut routes: BTreeMap<String, Route> = read_optional(&self.providers_path())
            .map_err(|_| failed("Could not open providers."))?
            .unwrap_or_default();
        let mut secrets: Secrets = read_optional(&self.auth_path())
            .map_err(|_| failed("Could not open saved credentials."))?
            .unwrap_or_default();
        let previous = secrets.clone();
        if let Some(route) = route {
            routes.insert(id.into(), route);
        } else if let Some(route) = routes.get_mut(id) {
            route.disabled = true;
        } else {
            let entry = builtin(id).ok_or_else(|| invalid("Unknown provider."))?;
            routes.insert(
                id.into(),
                Route {
                    api: entry.api.clone(),
                    base_url: entry.url.into(),
                    models: Vec::new(),
                    disabled: true,
                },
            );
        }
        if headers.is_empty() {
            secrets.headers.remove(id);
        } else {
            let origin = routes[id].base_url.clone();
            secrets
                .headers
                .insert(id.into(), ProviderSecret { origin, headers });
        }
        save(&self.auth_path(), &secrets).map_err(|_| failed("Could not save credentials."))?;
        if save(&self.providers_path(), &routes).is_err() {
            let _ = save(&self.auth_path(), &previous);
            return Err(failed("Could not save providers."));
        }
        Ok(())
    }
}

fn redact_value(value: &mut Value, secrets: &[String], freeform: bool) {
    match value {
        Value::String(text) => {
            for secret in secrets {
                if text.contains(secret) {
                    *text = text.replace(secret, "[redacted]");
                }
            }
        }
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| redact_value(item, secrets, freeform)),
        Value::Object(fields) => {
            for (key, item) in fields {
                if freeform
                    || !matches!(
                        key.as_str(),
                        "id" | "requestId"
                            | "sessionId"
                            | "toolCallId"
                            | "terminalId"
                            | "compactionId"
                            | "protocolVersion"
                            | "sessionUpdate"
                            | "stopReason"
                            | "status"
                            | "state"
                            | "role"
                            | "type"
                            | "kind"
                            | "optionId"
                            | "currentModeId"
                            | "currentValue"
                            | "value"
                            | "nextCursor"
                            | "mimeType"
                    )
                {
                    let freeform =
                        freeform || matches!(key.as_str(), "rawInput" | "rawOutput" | "_meta");
                    redact_value(item, secrets, freeform);
                }
            }
        }
        _ => {}
    }
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}
fn builtin(id: &str) -> Option<&'static Builtin> {
    BUILTINS.iter().find(|entry| entry.id == id)
}
pub(crate) fn model_key(model: &Model) -> String {
    format!("{}/{}", model.provider, model.id)
}

pub(crate) fn base_url(raw: &str) -> Option<Url> {
    Url::parse(raw).ok().filter(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
    })
}

fn same_origin(a: &str, b: &str) -> bool {
    Url::parse(a)
        .ok()
        .zip(Url::parse(b).ok())
        .is_some_and(|(a, b)| a.origin() == b.origin())
}

fn llm_protocol(api: &ApiKind) -> acp::LlmProtocol {
    match api {
        ApiKind::AnthropicMessages => acp::LlmProtocol::Anthropic,
        ApiKind::GoogleGenerativeAi => acp::LlmProtocol::Other("_google".into()),
        _ => acp::LlmProtocol::OpenAi,
    }
}

pub(crate) async fn login(home: &Path) -> std::result::Result<(), String> {
    let auth = oauth::start_codex_authorization(None).map_err(|_| "Could not start sign-in.")?;
    let listener = TcpListener::bind(("127.0.0.1", 1455)).await.ok();
    eprintln!("Open this link to sign in with ChatGPT:\n{}", auth.url);
    eprintln!("You can also paste the callback URL or code here and press Enter.");
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_ok() && !line.trim().is_empty() {
            let _ = tx.send(line);
        }
    });
    let (input, mut callback) = tokio::time::timeout(Duration::from_secs(300), async {
        tokio::select! {
            result = rx => match result {
                Ok(input) => (input, None),
                Err(_) => callback_input(listener.as_ref(), &auth.state).await,
            },
            result = callback_input(listener.as_ref(), &auth.state) => result,
        }
    })
    .await
    .map_err(|_| "Sign-in timed out.")?;
    let http = ReqwestTransport::new().map_err(|_| "Could not start sign-in.")?;
    let exchange = tokio::time::timeout(
        Duration::from_secs(30),
        oauth::exchange_codex_code(&http, &auth, input.trim(), CancelToken::new()),
    )
    .await;
    let result = match exchange {
        Ok(Ok(creds)) => (|| {
            let _updates =
                lock_credentials(home).map_err(|_| "Could not update saved credentials.")?;
            let path = home.join("auth.json");
            let mut secrets: Secrets = read_optional(&path)
                .map_err(|_| "Could not open saved credentials.")?
                .unwrap_or_default();
            secrets.codex = Some(creds);
            secrets.codex_signed_out = false;
            save(&path, &secrets).map_err(|_| "Could not save credentials.")
        })(),
        _ => Err("ChatGPT sign-in failed."),
    };
    if let Some(stream) = &mut callback {
        callback_reply(
            stream,
            if result.is_ok() {
                "200 OK"
            } else {
                "400 Bad Request"
            },
            result
                .as_ref()
                .err()
                .copied()
                .unwrap_or("Signed in. You can close this tab."),
        )
        .await;
    }
    result.map_err(str::to_owned)?;
    eprintln!("Signed in with ChatGPT.");
    Ok(())
}

async fn callback_input(
    listener: Option<&TcpListener>,
    state: &str,
) -> (String, Option<TcpStream>) {
    let Some(listener) = listener else {
        return std::future::pending().await;
    };
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            return std::future::pending().await;
        };
        let request = tokio::time::timeout(Duration::from_secs(3), async {
            let mut buf = Vec::new();
            for _ in 0..16_384 {
                let byte = stream.read_u8().await.ok()?;
                buf.push(byte);
                if buf.ends_with(b"\r\n\r\n") {
                    return String::from_utf8(buf).ok();
                }
            }
            None
        })
        .await
        .ok()
        .flatten();
        let target = request
            .as_deref()
            .and_then(|req| req.lines().next())
            .and_then(|line| line.strip_prefix("GET "))
            .and_then(|line| line.split_whitespace().next())
            .filter(|target| target.starts_with("/auth/callback?"));
        if let Some(target) = target {
            let input = format!("http://localhost:1455{target}");
            let parsed = oauth::parse_authorization_input(&input);
            if parsed.state.as_deref() == Some(state) && parsed.code.is_some() {
                return (input, Some(stream));
            }
        }
        callback_reply(&mut stream, "400 Bad Request", "Invalid sign-in callback.").await;
    }
}

async fn callback_reply(stream: &mut TcpStream, status: &str, text: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{text}",
        text.len()
    );
    let _ = tokio::time::timeout(
        Duration::from_secs(3),
        stream.write_all(response.as_bytes()),
    )
    .await;
}
