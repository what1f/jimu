//! Global provider presets, `~/.jimu/providers.json` configuration, and `~/.jimu/auth.json`
//! credential management shared across TUI, ACP, RPC, and one-shot (`-p`) modes.

use futures_util::stream::{self, StreamExt};
use jimu_ai::{
    ApiKind, AssistantEventStream, AssistantMessage, AssistantMessageEvent, CancelToken,
    HttpTransport, Model, ModelCost, SimpleStreamOptions, StopReason, StreamProvider,
    ThinkingLevel, TranscriptContext, Usage,
    oauth::{self, CodexCredentials},
};
use jimu_http::ReqwestTransport;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Mutex,
};
use url::Url;

pub type Headers = BTreeMap<String, String>;

/// Built-in provider connection preset (endpoint, wire API, and default env vars only — no
/// hardcoded model lists).
#[derive(Debug, Clone)]
pub struct ProviderPreset {
    pub id: &'static str,
    pub title: &'static str,
    pub api: ApiKind,
    pub base_url: &'static str,
    pub env: &'static [&'static str],
    pub default_context_window: u32,
    pub default_max_output_tokens: u32,
}

pub static PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        id: "anthropic",
        title: "Anthropic",
        api: ApiKind::AnthropicMessages,
        base_url: "https://api.anthropic.com",
        env: &["ANTHROPIC_API_KEY"],
        default_context_window: 1_000_000,
        default_max_output_tokens: 128_000,
    },
    ProviderPreset {
        id: "openai",
        title: "OpenAI",
        api: ApiKind::OpenaiResponses,
        base_url: "https://api.openai.com/v1",
        env: &["OPENAI_API_KEY"],
        default_context_window: 272_000,
        default_max_output_tokens: 128_000,
    },
    ProviderPreset {
        id: "openai-codex",
        title: "ChatGPT / Codex Subscription",
        api: ApiKind::OpenaiCodexResponses,
        base_url: oauth::CODEX_DEFAULT_BASE_URL,
        env: &[],
        default_context_window: 272_000,
        default_max_output_tokens: 128_000,
    },
    ProviderPreset {
        id: "google",
        title: "Google",
        api: ApiKind::GoogleGenerativeAi,
        base_url: "https://generativelanguage.googleapis.com/v1beta",
        env: &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        default_context_window: 1_048_576,
        default_max_output_tokens: 65_536,
    },
    ProviderPreset {
        id: "deepseek",
        title: "DeepSeek",
        api: ApiKind::OpenaiResponses,
        base_url: "https://api.deepseek.com/v1",
        env: &["DEEPSEEK_API_KEY"],
        default_context_window: 1_000_000,
        default_max_output_tokens: 384_000,
    },
    ProviderPreset {
        id: "openrouter",
        title: "OpenRouter",
        api: ApiKind::OpenaiCompletions,
        base_url: "https://openrouter.ai/api/v1",
        env: &["OPENROUTER_API_KEY"],
        default_context_window: 200_000,
        default_max_output_tokens: 64_000,
    },
];

pub fn preset(id: &str) -> Option<&'static ProviderPreset> {
    let normalized = match id {
        "gemini" => "google",
        other => other,
    };
    PRESETS.iter().find(|entry| entry.id == normalized)
}

#[derive(Clone)]
pub struct ProviderStoreConfig {
    pub jimu_home: PathBuf,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api: Option<ApiKind>,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub context_window: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub provider_override: bool,
}

impl From<&crate::acp::AcpConfig> for ProviderStoreConfig {
    fn from(cfg: &crate::acp::AcpConfig) -> Self {
        Self {
            jimu_home: cfg.jimu_home.clone(),
            provider: cfg.provider.clone(),
            model: cfg.model.clone(),
            api: cfg.api.clone(),
            base_url: cfg.base_url.clone(),
            api_key_env: cfg.api_key_env.clone(),
            context_window: cfg.context_window,
            max_output_tokens: cfg.max_output_tokens,
            provider_override: cfg.provider_override.is_some(),
        }
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub(crate) struct Secrets {
    pub(crate) codex: Option<CodexCredentials>,
    #[serde(default)]
    pub(crate) codex_signed_out: bool,
    #[serde(default)]
    pub(crate) headers: BTreeMap<String, ProviderSecret>,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct ProviderSecret {
    pub(crate) origin: String,
    pub(crate) headers: Headers,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum CustomModel {
    Id(String),
    Spec {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_window: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_output_tokens: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        supports_images: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking_levels: Option<Vec<ThinkingLevel>>,
    },
}

impl CustomModel {
    pub fn id(&self) -> &str {
        match self {
            Self::Id(id) | Self::Spec { id, .. } => id,
        }
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// On-disk entry inside `~/.jimu/providers.json`.
///
/// For built-in presets (`anthropic`, `openai`, `openai-codex`, `google`, `deepseek`, `openrouter`),
/// both `api` and `base_url` are optional and inherit the preset defaults.
#[derive(Clone, Default, Debug, Deserialize, Serialize)]
pub struct ProviderRoute {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<ApiKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<CustomModel>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub disabled: bool,
}

/// Validated, preset-merged route loaded from `~/.jimu/providers.json`.
#[derive(Clone, Debug)]
pub struct ResolvedRoute {
    pub api: ApiKind,
    pub base_url: String,
    pub api_key_env: Option<String>,
    pub models: Vec<CustomModel>,
    pub disabled: bool,
}

#[derive(Debug, Clone)]
pub enum ProviderUpdateError {
    Invalid(String),
    Failed(String),
}

impl std::fmt::Display for ProviderUpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(msg) | Self::Failed(msg) => f.write_str(msg),
        }
    }
}

pub fn read_optional<T: DeserializeOwned>(path: &Path) -> std::io::Result<Option<T>> {
    match File::open(path) {
        Ok(file) => serde_json::from_reader(std::io::BufReader::new(file))
            .map(Some)
            .map_err(std::io::Error::other),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

pub fn save(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
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
    serde_json::to_writer_pretty(&mut file, value)?;
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

pub(crate) fn usable_codex(secrets: &Secrets) -> Option<CodexCredentials> {
    if let Some(creds) = &secrets.codex {
        return (!creds.access_token.is_empty()
            && (!creds.is_expiring_within(now_ms(), 0) || creds.refresh_token.is_some()))
        .then(|| creds.clone());
    }
    (!secrets.codex_signed_out)
        .then(codex_fallback)
        .flatten()
        .filter(|creds| !creds.access_token.is_empty() && !creds.is_expiring_within(now_ms(), 0))
}

fn is_sensitive_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "authorization" | "proxy-authorization" | "x-api-key" | "api-key"
    ) || lower.contains("token")
        || lower.contains("secret")
        || lower.contains("key")
        || lower.contains("auth")
        || lower.contains("cookie")
}

fn is_auth_scheme_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("authorization") || name.eq_ignore_ascii_case("proxy-authorization")
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_millis().min(u128::from(u64::MAX)) as u64
        })
}

pub fn parse_base_url(raw: &str) -> Option<Url> {
    Url::parse(raw).ok().filter(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
    })
}

pub fn same_origin(a: &str, b: &str) -> bool {
    Url::parse(a)
        .ok()
        .zip(Url::parse(b).ok())
        .is_some_and(|(a, b)| a.origin() == b.origin())
}

pub fn model_key(model: &Model) -> String {
    format!("{}/{}", model.provider, model.id)
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn dynamic_env_name(provider: &str) -> String {
    format!(
        "{}_API_KEY",
        provider
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect::<String>()
    )
}

pub struct ProviderStore {
    config: ProviderStoreConfig,
    refresh: Mutex<()>,
    redactions: std::sync::RwLock<Vec<String>>,
}

impl ProviderStore {
    pub fn new(config: ProviderStoreConfig) -> Self {
        Self {
            config,
            refresh: Mutex::new(()),
            redactions: std::sync::RwLock::default(),
        }
    }

    pub fn config(&self) -> &ProviderStoreConfig {
        &self.config
    }

    pub fn auth_path(&self) -> PathBuf {
        self.config.jimu_home.join("auth.json")
    }

    pub fn providers_path(&self) -> PathBuf {
        self.config.jimu_home.join("providers.json")
    }

    pub(crate) fn secrets(&self) -> Secrets {
        read_optional(&self.auth_path())
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    /// Load `~/.jimu/providers.json` entry-by-entry so one invalid provider does not discard valid
    /// entries, returning all valid resolved routes plus human-readable warnings.
    pub fn load_routes_with_warnings(&self) -> (BTreeMap<String, ResolvedRoute>, Vec<String>) {
        let path = self.providers_path();
        let raw_text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == ErrorKind::NotFound => return (BTreeMap::new(), Vec::new()),
            Err(err) => {
                return (
                    BTreeMap::new(),
                    vec![format!("Failed to read {}: {err}", path.display())],
                );
            }
        };
        let top: Value = match serde_json::from_str(&raw_text) {
            Ok(val) => val,
            Err(err) => {
                return (
                    BTreeMap::new(),
                    vec![format!("Invalid JSON in {}: {err}", path.display())],
                );
            }
        };
        let Some(entries) = top.as_object() else {
            return (
                BTreeMap::new(),
                vec![format!(
                    "Invalid {}: root must be a JSON object",
                    path.display()
                )],
            );
        };

        let mut routes = BTreeMap::new();
        let mut warnings = Vec::new();
        for (id, value) in entries {
            let route: ProviderRoute = match serde_json::from_value(value.clone()) {
                Ok(parsed) => parsed,
                Err(err) => {
                    warnings.push(format!(
                        "{}: skipping provider '{id}': {err}",
                        path.display()
                    ));
                    continue;
                }
            };
            let entry = preset(id);
            if route.disabled {
                routes.insert(
                    id.clone(),
                    ResolvedRoute {
                        api: route
                            .api
                            .or_else(|| entry.map(|p| p.api.clone()))
                            .unwrap_or(ApiKind::OpenaiResponses),
                        base_url: route
                            .base_url
                            .or_else(|| entry.map(|p| p.base_url.to_string()))
                            .unwrap_or_default(),
                        api_key_env: route.api_key_env,
                        models: route.models,
                        disabled: true,
                    },
                );
                continue;
            }
            let Some(base_url) = route
                .base_url
                .or_else(|| entry.map(|p| p.base_url.to_string()))
            else {
                warnings.push(format!(
                    "{}: skipping custom provider '{id}': missing required field 'base_url'",
                    path.display()
                ));
                continue;
            };
            if parse_base_url(&base_url).is_none() {
                warnings.push(format!(
                    "{}: skipping provider '{id}': invalid base_url '{base_url}'",
                    path.display()
                ));
                continue;
            }
            let api = route
                .api
                .or_else(|| entry.map(|p| p.api.clone()))
                .unwrap_or(ApiKind::OpenaiResponses);
            routes.insert(
                id.clone(),
                ResolvedRoute {
                    api,
                    base_url,
                    api_key_env: route.api_key_env,
                    models: route.models,
                    disabled: false,
                },
            );
        }
        (routes, warnings)
    }

    pub fn routes(&self) -> BTreeMap<String, ResolvedRoute> {
        self.load_routes_with_warnings().0
    }

    pub fn warnings(&self) -> Vec<String> {
        self.load_routes_with_warnings().1
    }

    pub fn ids(&self, routes: &BTreeMap<String, ResolvedRoute>) -> Vec<String> {
        let mut ids: Vec<String> = PRESETS.iter().map(|entry| entry.id.into()).collect();
        for id in routes.keys().chain(&self.config.provider) {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
        ids
    }

    fn cli_provider(&self) -> Option<&str> {
        self.config.provider.as_deref()
    }

    fn cli_matches_provider(&self, id: &str) -> bool {
        self.cli_provider().is_some_and(|p| p == id)
            || (self.cli_provider().is_none() && id == "openai")
    }

    fn trusted_key_origin(
        &self,
        id: &str,
        url: &str,
        routes: &BTreeMap<String, ResolvedRoute>,
    ) -> bool {
        let cli_matches = self.cli_matches_provider(id)
            && self
                .config
                .base_url
                .as_deref()
                .is_some_and(|configured| same_origin(configured, url));
        let route_matches = routes
            .get(id)
            .filter(|route| !route.disabled && !route.base_url.is_empty())
            .is_some_and(|route| same_origin(&route.base_url, url));
        let preset_matches = preset(id).is_some_and(|entry| same_origin(entry.base_url, url));
        cli_matches || route_matches || preset_matches
    }

    fn env_candidate(&self, id: &str, routes: &BTreeMap<String, ResolvedRoute>) -> Option<String> {
        if self.cli_matches_provider(id)
            && let Some(name) = &self.config.api_key_env
        {
            return env_value(name);
        }
        if let Some(name) = routes
            .get(id)
            .and_then(|route| route.api_key_env.as_deref())
        {
            return env_value(name);
        }
        if let Some(entry) = preset(id) {
            return entry.env.iter().find_map(|name| env_value(name));
        }
        env_value(&dynamic_env_name(id))
    }

    fn env_key(&self, id: &str, routes: &BTreeMap<String, ResolvedRoute>) -> Option<String> {
        let target_url = self
            .cli_matches_provider(id)
            .then_some(self.config.base_url.as_deref())
            .flatten()
            .or_else(|| {
                routes
                    .get(id)
                    .filter(|r| !r.base_url.is_empty())
                    .map(|r| r.base_url.as_str())
            })
            .or_else(|| preset(id).map(|p| p.base_url))?;
        if !self.trusted_key_origin(id, target_url, routes) {
            return None;
        }
        self.env_candidate(id, routes)
    }

    pub(crate) fn has_key(
        &self,
        id: &str,
        routes: &BTreeMap<String, ResolvedRoute>,
        secrets: &Secrets,
    ) -> bool {
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

    pub fn authenticated_for(&self, model: &Model) -> bool {
        if self.config.provider_override {
            return !self
                .routes()
                .get(&model.provider)
                .is_some_and(|route| route.disabled);
        }
        let routes = self.routes();
        let Ok((resolved, headers)) = self.resolve_call_inner(model, false, &routes) else {
            return false;
        };
        if headers.values().any(|value| !value.is_empty()) {
            return true;
        }
        (self.trusted_key_origin(&resolved.provider, &resolved.base_url, &routes)
            && self.env_candidate(&resolved.provider, &routes).is_some())
            || (resolved.provider == "openai-codex"
                && same_origin(&resolved.base_url, oauth::CODEX_DEFAULT_BASE_URL)
                && usable_codex(&self.secrets()).is_some())
    }

    pub fn redact(&self, value: &mut Value) {
        redact_value(value, &self.redactions.read().unwrap(), false);
    }

    pub(crate) fn remember_secrets(
        &self,
        routes: &BTreeMap<String, ResolvedRoute>,
        secrets: &Secrets,
    ) {
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
        for (name, text) in secrets
            .headers
            .values()
            .flat_map(|secret| secret.headers.iter())
        {
            if !is_sensitive_header(name) {
                continue;
            }
            values.push(text.clone());
            if is_auth_scheme_header(name)
                && let Some((_, key)) = text.split_once(' ')
                && key.len() >= 8
            {
                values.push(key.into());
            }
        }
        let mut remembered = self.redactions.write().unwrap_or_else(|e| e.into_inner());
        for value in values {
            if !value.is_empty() && !remembered.contains(&value) {
                remembered.push(value);
            }
        }
        remembered.sort_by_key(|text| std::cmp::Reverse(text.len()));
    }

    /// Resolve the startup default model from CLI overrides or configured `providers.json` models.
    ///
    /// Returns `None` when neither CLI `-m` nor any enabled provider in `providers.json` declares a
    /// model (no hardcoded legacy model fallback).
    pub fn default_model(&self) -> Option<Model> {
        let routes = self.routes();
        let secrets = self.secrets();

        if let Some(cli_model) = self.config.model.as_deref() {
            if let Some(provider) = self.config.provider.as_deref() {
                return Some(self.build_model(
                    provider,
                    cli_model,
                    preset(provider),
                    routes.get(provider),
                ));
            }
            // Allow `-m provider/model` shorthand when `--provider` is omitted.
            if let Some((p, m)) = cli_model.split_once('/')
                && !m.is_empty()
                && (routes.contains_key(p) || preset(p).is_some())
            {
                return Some(self.build_model(p, m, preset(p), routes.get(p)));
            }
            // Match a unique configured model ID across providers.json.
            let configured = self.models_from_routes(&routes);
            let mut bare = configured.iter().filter(|m| m.id == cli_model);
            if let Some(first) = bare.next()
                && bare.next().is_none()
            {
                return Some(first.clone());
            }
            let provider = self.preferred_provider(&routes, &secrets);
            return Some(self.build_model(
                &provider,
                cli_model,
                preset(&provider),
                routes.get(&provider),
            ));
        }

        if let Some(provider) = self.config.provider.as_deref() {
            let route = routes.get(provider).filter(|r| !r.disabled)?;
            let first = route.models.first()?;
            return Some(self.build_model(provider, first.id(), preset(provider), Some(route)));
        }

        // No CLI model/provider: honor saved user default in `~/.jimu/settings.json` when still
        // valid and enabled, otherwise fall back to the first enabled provider with configured
        // models (preferring one with active credentials).
        let settings = crate::settings::UserSettings::load(&self.config.jimu_home);
        if let Some(saved_key) = settings.default_model.as_deref()
            && let Some(model) = self.select_model_in_routes(&routes, saved_key, None)
            && !routes
                .get(&model.provider)
                .is_some_and(|route| route.disabled)
        {
            return Some(model);
        }

        let with_models: Vec<(&String, &ResolvedRoute)> = routes
            .iter()
            .filter(|(_, route)| !route.disabled && !route.models.is_empty())
            .collect();
        let chosen = with_models
            .iter()
            .find(|(id, _)| self.has_key(id, &routes, &secrets))
            .or_else(|| with_models.first())
            .copied()?;
        let first_model = chosen.1.models.first()?;
        Some(self.build_model(chosen.0, first_model.id(), preset(chosen.0), Some(chosen.1)))
    }

    /// Placeholder model used by interactive shells (TUI / ACP / RPC) when no model is configured
    /// yet, so the shell can still launch and let the user run `/model` or configure `providers.json`.
    pub fn unconfigured_placeholder_model(&self) -> Model {
        let routes = self.routes();
        let secrets = self.secrets();
        let provider = self
            .config
            .provider
            .clone()
            .unwrap_or_else(|| self.preferred_provider(&routes, &secrets));
        self.build_model(&provider, "", preset(&provider), routes.get(&provider))
    }

    fn preferred_provider(
        &self,
        routes: &BTreeMap<String, ResolvedRoute>,
        secrets: &Secrets,
    ) -> String {
        self.ids(routes)
            .into_iter()
            .find(|id| {
                !routes.get(id).is_some_and(|r| r.disabled) && self.has_key(id, routes, secrets)
            })
            .or_else(|| {
                routes
                    .iter()
                    .find(|(_, r)| !r.disabled)
                    .map(|(id, _)| id.clone())
            })
            .or_else(|| {
                self.ids(routes)
                    .into_iter()
                    .find(|id| !routes.get(id).is_some_and(|r| r.disabled))
            })
            .unwrap_or_else(|| "openai".into())
    }

    fn models_from_routes(&self, routes: &BTreeMap<String, ResolvedRoute>) -> Vec<Model> {
        let mut models = Vec::new();
        for (provider, route) in routes {
            if route.disabled {
                continue;
            }
            for spec in &route.models {
                let model = self.build_model(provider, spec.id(), preset(provider), Some(route));
                if !models
                    .iter()
                    .any(|existing| model_key(existing) == model_key(&model))
                {
                    models.push(model);
                }
            }
        }
        models
    }

    /// Return all selectable models declared in `~/.jimu/providers.json` (plus any explicit CLI
    /// `-m` model). Never injects hardcoded built-in models.
    pub fn models(&self) -> Vec<Model> {
        let routes = self.routes();
        let mut models = self.models_from_routes(&routes);
        if let Some(default) = self.default_model()
            && !default.id.is_empty()
            && !routes
                .get(&default.provider)
                .is_some_and(|route| route.disabled)
            && !models
                .iter()
                .any(|model| model_key(model) == model_key(&default))
        {
            models.insert(0, default);
        }
        models
    }

    /// Resolve a user-supplied model selector (`provider/model` or bare `model`), matching
    /// configured models first and allowing free-form `<provider>/<id>` or `<id>` on an active
    /// provider.
    pub fn select_model(&self, key: &str, current_provider: Option<&str>) -> Option<Model> {
        let routes = self.routes();
        self.select_model_in_routes(&routes, key, current_provider)
    }

    fn select_model_in_routes(
        &self,
        routes: &BTreeMap<String, ResolvedRoute>,
        key: &str,
        current_provider: Option<&str>,
    ) -> Option<Model> {
        let key = key.trim();
        if key.is_empty() {
            return None;
        }
        let list = self.models_from_routes(routes);
        if let Some(model) = list.iter().find(|model| model_key(model) == key) {
            return Some(model.clone());
        }
        {
            let mut bare = list.iter().filter(|model| model.id == key);
            if let Some(candidate) = bare.next()
                && bare.next().is_none()
            {
                return Some(candidate.clone());
            }
        }
        // Free-form `provider/model-id` for any enabled configured or preset provider.
        if let Some((provider, id)) = key.split_once('/')
            && !id.is_empty()
            && !routes.get(provider).is_some_and(|r| r.disabled)
            && (routes.contains_key(provider)
                || preset(provider).is_some()
                || self.config.provider.as_deref() == Some(provider))
        {
            return Some(self.build_model(provider, id, preset(provider), routes.get(provider)));
        }
        // Free-form bare `model-id` under the active provider (if enabled and configured/preset).
        if let Some(provider) = current_provider
            && !provider.is_empty()
            && !routes.get(provider).is_some_and(|r| r.disabled)
            && (routes.contains_key(provider)
                || preset(provider).is_some()
                || self.config.provider.as_deref() == Some(provider))
        {
            return Some(self.build_model(provider, key, preset(provider), routes.get(provider)));
        }
        None
    }

    pub(crate) fn build_model(
        &self,
        provider: &str,
        id: &str,
        entry: Option<&ProviderPreset>,
        route: Option<&ResolvedRoute>,
    ) -> Model {
        let fallback_api = entry.map_or(ApiKind::OpenaiResponses, |e| e.api.clone());
        let fallback_url = entry.map_or("", |e| e.base_url);
        let mut api = route.map_or_else(|| fallback_api.clone(), |r| r.api.clone());
        let mut url = route
            .filter(|r| !r.base_url.is_empty())
            .map_or(fallback_url, |r| r.base_url.as_str());

        // CLI overrides always take highest precedence: CLI > providers.json > preset.
        if self.cli_matches_provider(provider) {
            if let Some(cli_api) = &self.config.api {
                api = cli_api.clone();
            }
            if let Some(cli_url) = self.config.base_url.as_deref() {
                url = cli_url;
            }
        }

        let mut model = Model::new(id, provider, api, url);
        model.supports_images = true;
        model.supports_reasoning = true;
        model.context_window = entry.map_or(272_000, |e| e.default_context_window);
        model.max_output_tokens = entry.map_or(jimu_ai::DEFAULT_MAX_OUTPUT_TOKENS, |e| {
            e.default_max_output_tokens
        });
        model.cost = ModelCost::default();

        if let Some(CustomModel::Spec {
            name,
            context_window,
            max_output_tokens,
            supports_images,
            ..
        }) = route.and_then(|r| r.models.iter().find(|spec| spec.id() == id))
        {
            if let Some(name) = name {
                model.name.clone_from(name);
            }
            if let Some(window) = context_window {
                model.context_window = *window;
            }
            if let Some(max_out) = max_output_tokens {
                model.max_output_tokens = *max_out;
            }
            if let Some(images) = supports_images {
                model.supports_images = *images;
            }
        }

        if let Some(window) = self.config.context_window {
            model.context_window = window;
        }
        if let Some(max_out) = self.config.max_output_tokens {
            model.max_output_tokens = max_out;
        }

        model
    }

    pub fn model(&self, provider: &str, id: &str) -> Model {
        let entry = preset(provider);
        let routes = self.routes();
        let route = routes.get(provider).filter(|route| !route.disabled);
        self.build_model(provider, id, entry, route)
    }

    pub fn thinking_levels(&self, provider: &str, id: &str) -> Option<Vec<ThinkingLevel>> {
        let routes = self.routes();
        let route = routes.get(provider).filter(|route| !route.disabled)?;
        match route.models.iter().find(|spec| spec.id() == id)? {
            CustomModel::Spec {
                thinking_levels: Some(levels),
                ..
            } if !levels.is_empty() => Some(levels.clone()),
            _ => None,
        }
    }

    fn resolve_call_inner(
        &self,
        model: &Model,
        allow_override: bool,
        routes: &BTreeMap<String, ResolvedRoute>,
    ) -> Result<(Model, Headers), String> {
        let secrets = self.secrets();
        self.remember_secrets(routes, &secrets);
        if model.id.trim().is_empty() {
            return Err(
                "No model configured. Pass --provider <id> -m <model>, use /model, or add models to ~/.jimu/providers.json."
                    .into(),
            );
        }
        if routes
            .get(&model.provider)
            .is_some_and(|route| route.disabled)
        {
            return Err(format!("Provider '{}' is disabled.", model.provider));
        }
        let entry = preset(&model.provider);
        let route = routes.get(&model.provider);
        let cli_configured =
            self.cli_matches_provider(&model.provider) && self.config.base_url.is_some();
        if route.is_none() && entry.is_none() && !cli_configured {
            if allow_override || !model.base_url.is_empty() {
                return Ok((model.clone(), Headers::new()));
            }
            return Err(format!("Provider '{}' is not configured.", model.provider));
        }
        let resolved = self.build_model(&model.provider, &model.id, entry, route);
        let headers = if let Some(route) = route
            && same_origin(&resolved.base_url, &route.base_url)
        {
            secrets
                .headers
                .get(&model.provider)
                .filter(|secret| same_origin(&secret.origin, &resolved.base_url))
                .map(|secret| secret.headers.clone())
                .unwrap_or_default()
        } else {
            Headers::new()
        };
        Ok((resolved, headers))
    }

    pub fn resolve_call(
        &self,
        model: &Model,
        allow_override: bool,
    ) -> Result<(Model, Headers), String> {
        let routes = self.routes();
        self.resolve_call_inner(model, allow_override, &routes)
    }

    pub fn routed_model(&self, model: &Model) -> Model {
        self.resolve_call(model, true)
            .map(|(resolved, _)| resolved)
            .unwrap_or_else(|_| model.clone())
    }

    pub async fn api_key(
        &self,
        model: &Model,
        http: &dyn HttpTransport,
        cancel: &CancelToken,
    ) -> Option<String> {
        let provider = model.provider.as_str();
        let routes = self.routes();
        let secrets = self.secrets();
        self.remember_secrets(&routes, &secrets);
        if routes.get(provider).is_some_and(|route| route.disabled) {
            return None;
        }
        let key = self
            .trusted_key_origin(provider, &model.base_url, &routes)
            .then(|| self.env_candidate(provider, &routes))
            .flatten();
        if key.is_some() || provider != "openai-codex" {
            return key;
        }
        if !same_origin(&model.base_url, oauth::CODEX_DEFAULT_BASE_URL) {
            return None;
        }
        // Only Jimu's own persisted Codex login may be refreshed; external ~/.codex/auth.json
        // tokens are used read-only while unexpired so we never rotate/clobber external CLI state.
        if secrets.codex.is_none() {
            return usable_codex(&secrets).map(|creds| creds.access_token);
        }
        let _refresh = tokio::select! {
            guard = self.refresh.lock() => guard,
            () = cancel.cancelled() => return None,
        };
        let home = self.config.jimu_home.clone();
        let lock_task = tokio::task::spawn_blocking(move || lock_credentials(&home));
        let _updates = tokio::select! {
            res = lock_task => res.ok()?.ok()?,
            () = cancel.cancelled() => return None,
        };
        let mut current: Secrets = read_optional(&self.auth_path()).ok()?.unwrap_or_default();
        let Some(creds) = current.codex.clone().filter(|creds| {
            !creds.access_token.is_empty()
                && (!creds.is_expiring_within(now_ms(), 0) || creds.refresh_token.is_some())
        }) else {
            return usable_codex(&current).map(|creds| creds.access_token);
        };
        if !creds.is_expiring_within(now_ms(), 60_000) || creds.refresh_token.is_none() {
            self.remember_secrets(&routes, &current);
            return Some(creds.access_token);
        }
        let refreshed = tokio::time::timeout(
            Duration::from_secs(30),
            oauth::refresh_codex_credentials(http, &creds, cancel.clone()),
        )
        .await
        .ok()?
        .ok()?;
        let access = refreshed.access_token.clone();
        current.codex = Some(refreshed);
        save(&self.auth_path(), &current).ok()?;
        self.remember_secrets(&routes, &current);
        Some(access)
    }

    pub async fn logout(&self) -> Result<(), String> {
        let _refresh = self.refresh.lock().await;
        let _updates = lock_credentials(&self.config.jimu_home)
            .map_err(|_| "Could not update saved credentials.".to_string())?;
        let mut secrets: Secrets = read_optional(&self.auth_path())
            .map_err(|_| "Could not open saved credentials.".to_string())?
            .unwrap_or_default();
        secrets.codex = None;
        secrets.codex_signed_out = true;
        save(&self.auth_path(), &secrets).map_err(|_| "Could not save credentials.".to_string())
    }

    pub fn update_provider(
        &self,
        id: &str,
        route: Option<ProviderRoute>,
        headers: Headers,
    ) -> Result<(), ProviderUpdateError> {
        let _updates = lock_credentials(&self.config.jimu_home).map_err(|_| {
            ProviderUpdateError::Failed("Could not update saved credentials.".into())
        })?;
        let mut raw_routes: BTreeMap<String, Value> =
            match read_optional::<Value>(&self.providers_path()) {
                Ok(Some(Value::Object(map))) => map.into_iter().collect(),
                Ok(None) => BTreeMap::new(),
                _ => {
                    return Err(ProviderUpdateError::Failed(
                        "Could not open providers.".into(),
                    ));
                }
            };
        let mut secrets: Secrets = read_optional(&self.auth_path())
            .map_err(|_| ProviderUpdateError::Failed("Could not open saved credentials.".into()))?
            .unwrap_or_default();
        let previous = secrets.clone();

        let effective_origin = if let Some(route) = route {
            let origin = route
                .base_url
                .clone()
                .or_else(|| preset(id).map(|p| p.base_url.to_string()))
                .unwrap_or_default();
            raw_routes.insert(
                id.into(),
                serde_json::to_value(route).map_err(|_| {
                    ProviderUpdateError::Failed("Could not serialize provider.".into())
                })?,
            );
            origin
        } else if let Some(existing) = raw_routes.get_mut(id) {
            let mut parsed: ProviderRoute =
                serde_json::from_value(existing.clone()).unwrap_or_default();
            parsed.disabled = true;
            let origin = parsed
                .base_url
                .clone()
                .or_else(|| preset(id).map(|p| p.base_url.to_string()))
                .unwrap_or_default();
            *existing = serde_json::to_value(parsed)
                .map_err(|_| ProviderUpdateError::Failed("Could not serialize provider.".into()))?;
            origin
        } else {
            if preset(id).is_none() {
                return Err(ProviderUpdateError::Invalid("Unknown provider.".into()));
            }
            raw_routes.insert(
                id.into(),
                serde_json::to_value(ProviderRoute {
                    disabled: true,
                    ..ProviderRoute::default()
                })
                .map_err(|_| ProviderUpdateError::Failed("Could not serialize provider.".into()))?,
            );
            String::new()
        };

        if headers.is_empty() {
            secrets.headers.remove(id);
        } else {
            secrets.headers.insert(
                id.into(),
                ProviderSecret {
                    origin: effective_origin,
                    headers,
                },
            );
        }
        save(&self.auth_path(), &secrets)
            .map_err(|_| ProviderUpdateError::Failed("Could not save credentials.".into()))?;
        if save(&self.providers_path(), &raw_routes).is_err() {
            let _ = save(&self.auth_path(), &previous);
            return Err(ProviderUpdateError::Failed(
                "Could not save providers.".into(),
            ));
        }
        Ok(())
    }
}

/// Shared `StreamProvider` decorator that resolves `~/.jimu/providers.json` routes, credentials,
/// and custom headers before delegating to the underlying HTTP stream client.
pub struct AuthStreamProvider {
    pub store: Arc<ProviderStore>,
    pub provider: Arc<dyn StreamProvider>,
    pub transport: Arc<dyn HttpTransport>,
    pub allow_override: bool,
    /// When true (RPC mode), preserve an explicit non-empty `model.base_url`/`model.api` set via
    /// dynamic `set_model` instead of snapping back to the built-in preset endpoint.
    pub preserve_explicit_endpoint: bool,
}

impl StreamProvider for AuthStreamProvider {
    fn stream(
        &self,
        model: &Model,
        context: &TranscriptContext,
        mut options: SimpleStreamOptions,
    ) -> AssistantEventStream {
        let model = model.clone();
        let context = context.clone();
        let store = Arc::clone(&self.store);
        let provider = Arc::clone(&self.provider);
        let transport = Arc::clone(&self.transport);
        let allow_override = self.allow_override;
        let preserve_explicit_endpoint = self.preserve_explicit_endpoint;

        Box::pin(
            stream::once(async move {
                let (mut resolved, headers) = match store.resolve_call(&model, allow_override) {
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
                let default_base_url = resolved.base_url.clone();
                let mut cross_origin_override = false;
                // Always honor the active session model's capability and limit fields (e.g.
                // `supports_reasoning`, `supports_images`, `max_output_tokens`, `context_window`).
                resolved.context_window = model.context_window;
                resolved.max_output_tokens = model.max_output_tokens;
                resolved.supports_images = model.supports_images;
                resolved.supports_reasoning = model.supports_reasoning;
                if preserve_explicit_endpoint {
                    resolved.api = model.api.clone();
                    if !model.base_url.is_empty() {
                        cross_origin_override = !default_base_url.is_empty()
                            && !same_origin(&default_base_url, &model.base_url);
                        resolved.base_url = model.base_url.clone();
                    }
                }
                if !store.config().provider_override && !cross_origin_override {
                    if options.api_key.is_none() {
                        options.api_key =
                            store.api_key(&resolved, &*transport, &options.cancel).await;
                    }
                    resolved.headers.extend(headers);
                }
                provider.stream(&resolved, &context, options)
            })
            .flatten(),
        )
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
        timestamp: now_ms(),
        duration_ms: None,
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

pub async fn login(home: &Path) -> Result<(), String> {
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
