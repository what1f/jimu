//! Native OpenAI Codex (ChatGPT Subscription) OAuth PKCE flow, JWT claims decoding,
//! credential refresh, and Codex model catalog fetching over `HttpTransport`.

use crate::providers::{now_ms, send_cancellable};
use crate::{CancelToken, HttpRequest, HttpTransport, TransportError};
use bytes::Bytes;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const CODEX_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const CODEX_DEFAULT_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
pub const CODEX_DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api";
pub const CODEX_MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models";

const CODEX_SCOPE: &str = "openid profile email offline_access";
const CODEX_ORIGINATOR: &str = "jimu";
const JWT_AUTH_CLAIM: &str = "https://api.openai.com/auth";
const JWT_PROFILE_CLAIM: &str = "https://api.openai.com/profile";
const MAX_OAUTH_BODY_BYTES: usize = 4 * 1024 * 1024;

/// PKCE verifier & S256 challenge pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PkcePair {
    pub verifier: String,
    pub challenge: String,
}

/// Outbound OAuth authorization request state for OpenAI Codex login.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexAuthorizationRequest {
    pub url: String,
    pub state: String,
    pub code_verifier: String,
    pub redirect_uri: String,
}

/// Parsed fields from an OAuth callback URL, `code#state` token, query string, or raw code.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedAuthInput {
    pub code: Option<String>,
    pub state: Option<String>,
    /// True when the input was structured (URL, query string, or `code#state`) and MUST carry a matching `state`.
    pub requires_state: bool,
}

/// OpenAI Codex OAuth credentials (ChatGPT subscription).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexCredentials {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
}

/// Model entry returned by the Codex `/codex/models` catalog endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexCatalogModel {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default)]
    pub reasoning_levels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning_level: Option<String>,
    #[serde(default)]
    pub supports_images: bool,
    #[serde(default)]
    pub supports_fast: bool,
}

// ============================================================================
// Minimal RFC 4648 base64url, URL encoding/decoding, and OS random bytes
// ============================================================================

const B64URL_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn base64url_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity((input.len() * 4).div_ceil(3));
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = if chunk.len() > 1 { chunk[1] } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] } else { 0 };
        out.push(B64URL_ALPHABET[(b0 >> 2) as usize] as char);
        out.push(B64URL_ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64URL_ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(B64URL_ALPHABET[(b2 & 0x3f) as usize] as char);
        }
    }
    out
}

fn base64url_val(b: u8) -> Option<u8> {
    match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'-' | b'+' => Some(62),
        b'_' | b'/' => Some(63),
        _ => None,
    }
}

fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    let bytes = input.trim().trim_end_matches('=').as_bytes();
    if bytes.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity((bytes.len() * 3) / 4);
    for chunk in bytes.chunks(4) {
        let v0 = base64url_val(chunk[0])?;
        let v1 = base64url_val(chunk[1])?;
        out.push((v0 << 2) | (v1 >> 4));
        if chunk.len() > 2 {
            let v2 = base64url_val(chunk[2])?;
            out.push((v1 << 4) | (v2 >> 2));
            if chunk.len() > 3 {
                let v3 = base64url_val(chunk[3])?;
                out.push((v2 << 6) | v3);
            }
        }
    }
    Some(out)
}

fn url_encode_component(input: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn form_decode_component(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                    out.push((hi << 4) | lo);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn encode_form_pairs(pairs: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (idx, (k, v)) in pairs.iter().enumerate() {
        if idx > 0 {
            out.push('&');
        }
        out.push_str(&url_encode_component(k));
        out.push('=');
        out.push_str(&url_encode_component(v));
    }
    out
}

fn fill_random_bytes(dest: &mut [u8]) -> Result<(), String> {
    getrandom::fill(dest).map_err(|e| format!("failed to obtain OS cryptographic randomness: {e}"))
}

fn encode_hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

// ============================================================================
// PKCE & Authorization URL
// ============================================================================

/// Deterministically derive a PKCE verifier & S256 challenge from 32 random bytes.
pub fn generate_pkce_from_bytes(bytes: &[u8; 32]) -> PkcePair {
    let verifier = base64url_encode(bytes);
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = base64url_encode(&digest);
    PkcePair {
        verifier,
        challenge,
    }
}

/// Generate a fresh PKCE verifier & S256 challenge using OS randomness.
pub fn generate_pkce() -> Result<PkcePair, String> {
    let mut bytes = [0u8; 32];
    fill_random_bytes(&mut bytes)?;
    Ok(generate_pkce_from_bytes(&bytes))
}

/// Build an OpenAI Codex OAuth PKCE authorization request & URL.
pub fn start_codex_authorization(
    redirect_uri: Option<&str>,
) -> Result<CodexAuthorizationRequest, String> {
    let pkce = generate_pkce()?;
    let mut state_bytes = [0u8; 16];
    fill_random_bytes(&mut state_bytes)?;
    let state = encode_hex_lower(&state_bytes);
    let redirect = redirect_uri
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(CODEX_DEFAULT_REDIRECT_URI)
        .to_string();

    let query = encode_form_pairs(&[
        ("response_type", "code"),
        ("client_id", CODEX_CLIENT_ID),
        ("redirect_uri", &redirect),
        ("scope", CODEX_SCOPE),
        ("code_challenge", &pkce.challenge),
        ("code_challenge_method", "S256"),
        ("state", &state),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", CODEX_ORIGINATOR),
    ]);

    Ok(CodexAuthorizationRequest {
        url: format!("{CODEX_AUTHORIZE_URL}?{query}"),
        state,
        code_verifier: pkce.verifier,
        redirect_uri: redirect,
    })
}

fn parse_query_params(query: &str) -> ParsedAuthInput {
    let mut out = ParsedAuthInput {
        requires_state: true,
        ..ParsedAuthInput::default()
    };
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (form_decode_component(k), form_decode_component(v)),
            None => (form_decode_component(pair), String::new()),
        };
        match k.as_str() {
            "code" if !v.is_empty() => out.code = Some(v),
            "state" if !v.is_empty() => out.state = Some(v),
            _ => {}
        }
    }
    out
}

/// Parse an authorization callback URL, `code#state` string, query string, or raw code.
pub fn parse_authorization_input(input: &str) -> ParsedAuthInput {
    let value = input.trim();
    if value.is_empty() {
        return ParsedAuthInput::default();
    }

    if let Some((_, after_q)) = value.split_once('?') {
        let query = after_q.split('#').next().unwrap_or(after_q);
        return parse_query_params(query);
    }

    if value.contains("://") {
        return ParsedAuthInput {
            code: None,
            state: None,
            requires_state: true,
        };
    }

    if let Some((code, state)) = value.split_once('#') {
        return ParsedAuthInput {
            code: (!code.is_empty()).then(|| code.to_string()),
            state: (!state.is_empty()).then(|| state.to_string()),
            requires_state: true,
        };
    }

    if value.contains("code=") || value.contains("state=") {
        return parse_query_params(value);
    }

    ParsedAuthInput {
        code: Some(value.to_string()),
        state: None,
        requires_state: false,
    }
}

// ============================================================================
// JWT Claims & Credentials
// ============================================================================

/// Decode the middle payload segment of a JWT into JSON claims.
pub fn decode_jwt_claims(token: &str) -> Option<Value> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let _sig = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let decoded = base64url_decode(payload)?;
    serde_json::from_slice::<Value>(&decoded).ok()
}

/// Extract `chatgpt_account_id` from a ChatGPT/Codex OAuth JWT access token.
pub fn extract_account_id(token: &str) -> Option<String> {
    decode_jwt_claims(token)?
        .get(JWT_AUTH_CLAIM)?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

fn extract_email_from_jwt(token: &str) -> Option<String> {
    let claims = decode_jwt_claims(token)?;
    claims
        .get(JWT_PROFILE_CLAIM)
        .and_then(|p| p.get("email"))
        .and_then(Value::as_str)
        .or_else(|| claims.get("email").and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

impl CodexCredentials {
    pub fn decode_jwt_claims(token: &str) -> Option<Value> {
        decode_jwt_claims(token)
    }

    pub fn extract_account_id(token: &str) -> Option<String> {
        extract_account_id(token)
    }

    pub fn extract_email(&self) -> Option<String> {
        self.id_token
            .as_deref()
            .and_then(extract_email_from_jwt)
            .or_else(|| extract_email_from_jwt(&self.access_token))
    }

    pub fn is_expiring_within(&self, now_ms: u64, margin_ms: u64) -> bool {
        match self.expires_at_ms {
            Some(exp) => now_ms.saturating_add(margin_ms) >= exp,
            None => false,
        }
    }

    pub fn normalized(mut self) -> Result<Self, String> {
        self.access_token = self.access_token.trim().to_string();
        if self.access_token.is_empty() {
            return Err("Codex credentials missing access_token".into());
        }
        self.refresh_token = self
            .refresh_token
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        self.account_id = self
            .account_id
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        self.id_token = self
            .id_token
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        if self.account_id.is_none() {
            self.account_id = extract_account_id(&self.access_token);
        }
        if self.expires_at_ms.is_none()
            && let Some(claims) = decode_jwt_claims(&self.access_token)
            && let Some(exp_secs) = claims
                .get("exp")
                .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f.max(0.0) as u64)))
        {
            self.expires_at_ms = Some(exp_secs.saturating_mul(1000));
        }

        Ok(self)
    }
}

// ============================================================================
// HTTP Helpers & Async OAuth Operations
// ============================================================================

async fn send_and_collect_json(
    transport: &dyn HttpTransport,
    request: HttpRequest,
    cancel: CancelToken,
) -> Result<Value, TransportError> {
    let response = send_cancellable(transport, request, cancel.clone()).await?;
    let mut body_bytes = Vec::new();
    let mut stream = response.body;
    while let Some(chunk) = stream.next().await {
        if cancel.is_cancelled() {
            return Err(TransportError::Aborted);
        }
        let bytes = chunk?;
        if body_bytes.len().saturating_add(bytes.len()) > MAX_OAUTH_BODY_BYTES {
            return Err(TransportError::Network(
                "OAuth response body exceeded 4 MiB limit".into(),
            ));
        }
        body_bytes.extend_from_slice(&bytes);
    }
    serde_json::from_slice::<Value>(&body_bytes)
        .map_err(|err| TransportError::Network(format!("invalid OAuth JSON response: {err}")))
}

fn credentials_from_token_json(
    val: &Value,
    fallback_refresh: Option<&str>,
    fallback_account: Option<&str>,
    fallback_id_token: Option<&str>,
) -> Result<CodexCredentials, TransportError> {
    let access_token = val
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            TransportError::Network("OpenAI Codex token response missing access_token".into())
        })?
        .to_string();

    let refresh_token = val
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .or_else(|| fallback_refresh.map(ToString::to_string));

    let id_token = val
        .get("id_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .or_else(|| fallback_id_token.map(ToString::to_string));

    let expires_at_ms = val
        .get("expires_in")
        .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f.max(0.0) as u64)))
        .map(|secs| now_ms().saturating_add(secs.saturating_mul(1000)));

    let account_id =
        extract_account_id(&access_token).or_else(|| fallback_account.map(ToString::to_string));

    CodexCredentials {
        access_token,
        refresh_token,
        account_id,
        expires_at_ms,
        id_token,
    }
    .normalized()
    .map_err(TransportError::Network)
}

/// Exchange an OAuth authorization callback (URL, `code#state`, query, or raw code) for credentials.
pub async fn exchange_codex_code(
    transport: &dyn HttpTransport,
    auth: &CodexAuthorizationRequest,
    callback_input: &str,
    cancel: CancelToken,
) -> Result<CodexCredentials, TransportError> {
    let parsed = parse_authorization_input(callback_input);
    if parsed.requires_state {
        if parsed.state.as_deref() != Some(auth.state.as_str()) {
            return Err(TransportError::Network("OAuth state mismatch".into()));
        }
    } else if let Some(state) = parsed.state.as_deref()
        && state != auth.state
    {
        return Err(TransportError::Network("OAuth state mismatch".into()));
    }
    let code = parsed
        .code
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| TransportError::Network("missing OAuth authorization code".into()))?;

    let form = encode_form_pairs(&[
        ("grant_type", "authorization_code"),
        ("client_id", CODEX_CLIENT_ID),
        ("code", code),
        ("code_verifier", &auth.code_verifier),
        ("redirect_uri", &auth.redirect_uri),
    ]);

    let req = HttpRequest {
        method: "POST".into(),
        url: CODEX_TOKEN_URL.into(),
        headers: vec![
            (
                "content-type".into(),
                "application/x-www-form-urlencoded".into(),
            ),
            ("accept".into(), "application/json".into()),
        ],
        body: Bytes::from(form),
        timeout_ms: None,
    };

    let json = send_and_collect_json(transport, req, cancel).await?;
    credentials_from_token_json(&json, None, None, None)
}

/// Refresh expiring OpenAI Codex OAuth credentials using `refresh_token`.
pub async fn refresh_codex_credentials(
    transport: &dyn HttpTransport,
    creds: &CodexCredentials,
    cancel: CancelToken,
) -> Result<CodexCredentials, TransportError> {
    let refresh_token = creds
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| TransportError::Network("missing refresh_token".into()))?;

    let form = encode_form_pairs(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", CODEX_CLIENT_ID),
    ]);

    let req = HttpRequest {
        method: "POST".into(),
        url: CODEX_TOKEN_URL.into(),
        headers: vec![
            (
                "content-type".into(),
                "application/x-www-form-urlencoded".into(),
            ),
            ("accept".into(), "application/json".into()),
        ],
        body: Bytes::from(form),
        timeout_ms: None,
    };

    let json = send_and_collect_json(transport, req, cancel).await?;
    credentials_from_token_json(
        &json,
        creds.refresh_token.as_deref(),
        creds.account_id.as_deref(),
        creds.id_token.as_deref(),
    )
}

fn resolve_codex_models_url(base_url: Option<&str>, client_version: &str) -> String {
    let base = match base_url.map(str::trim).filter(|s| !s.is_empty()) {
        None => CODEX_MODELS_URL.to_string(),
        Some(raw) => {
            let trimmed = raw.trim_end_matches('/');
            if trimmed.ends_with("/codex/models") {
                trimmed.to_string()
            } else if trimmed.ends_with("/codex") {
                format!("{trimmed}/models")
            } else {
                format!("{trimmed}/codex/models")
            }
        }
    };
    let cv = client_version.trim();
    if cv.is_empty() {
        base
    } else {
        let sep = if base.contains('?') { '&' } else { '?' };
        format!("{base}{sep}client_version={}", url_encode_component(cv))
    }
}

/// Fetch available models from the Codex subscription `/codex/models` catalog endpoint.
pub async fn fetch_codex_models(
    transport: &dyn HttpTransport,
    base_url: Option<&str>,
    creds: &CodexCredentials,
    client_version: &str,
    cancel: CancelToken,
) -> Result<Vec<CodexCatalogModel>, TransportError> {
    let url = resolve_codex_models_url(base_url, client_version);
    let mut headers = vec![
        ("accept".into(), "application/json".into()),
        (
            "authorization".into(),
            format!("Bearer {}", creds.access_token),
        ),
        ("originator".into(), CODEX_ORIGINATOR.into()),
    ];
    let account_id = creds
        .account_id
        .clone()
        .or_else(|| extract_account_id(&creds.access_token));
    if let Some(aid) = account_id.filter(|s| !s.is_empty()) {
        headers.push(("chatgpt-account-id".into(), aid));
    }

    let req = HttpRequest {
        method: "GET".into(),
        url,
        headers,
        body: Bytes::new(),
        timeout_ms: None,
    };

    let json = send_and_collect_json(transport, req, cancel).await?;
    let items = json
        .get("models")
        .and_then(Value::as_array)
        .or_else(|| json.as_array())
        .ok_or_else(|| {
            TransportError::Network("Codex models response missing models array".into())
        })?;

    let mut models = Vec::with_capacity(items.len());
    for item in items {
        let Some(id) = item
            .get("slug")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };

        let display_name = item
            .get("display_name")
            .or_else(|| item.get("displayName"))
            .or_else(|| item.get("name"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string);

        let reasoning_levels: Vec<String> = item
            .get("supported_reasoning_levels")
            .or_else(|| item.get("reasoning_levels"))
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| {
                        v.as_str()
                            .or_else(|| v.get("effort").and_then(Value::as_str))
                            .map(ToString::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default();

        let default_reasoning_level = item
            .get("default_reasoning_level")
            .or_else(|| item.get("defaultReasoningLevel"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string);

        let supports_images = item
            .get("supports_images")
            .or_else(|| item.get("supportsImages"))
            .and_then(Value::as_bool)
            .unwrap_or_else(|| {
                item.get("input_modalities")
                    .and_then(Value::as_array)
                    .is_some_and(|mods| {
                        mods.iter()
                            .any(|m| m.as_str().is_some_and(|s| s.eq_ignore_ascii_case("image")))
                    })
            });

        let supports_fast = item
            .get("supports_fast")
            .or_else(|| item.get("supportsFast"))
            .and_then(Value::as_bool)
            .unwrap_or_else(|| {
                let has_priority_tier = item
                    .get("service_tiers")
                    .and_then(Value::as_array)
                    .is_some_and(|tiers| {
                        tiers.iter().any(|t| {
                            t.as_str()
                                .or_else(|| t.get("id").and_then(Value::as_str))
                                .is_some_and(|s| s.eq_ignore_ascii_case("priority"))
                        })
                    });
                let has_fast_speed_tier = item
                    .get("additional_speed_tiers")
                    .and_then(Value::as_array)
                    .is_some_and(|tiers| {
                        tiers
                            .iter()
                            .any(|t| t.as_str().is_some_and(|s| s.eq_ignore_ascii_case("fast")))
                    });
                has_priority_tier || has_fast_speed_tier
            });

        models.push(CodexCatalogModel {
            id: id.to_string(),
            display_name,
            reasoning_levels,
            default_reasoning_level,
            supports_images,
            supports_fast,
        });
    }

    Ok(models)
}
