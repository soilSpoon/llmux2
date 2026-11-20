//! OpenAI Sign in with ChatGPT authorization-code flow.

mod http;
mod storage;

use axum::{
    extract::{RawQuery, State},
    routing::get,
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use jsonwebtoken::{decode, decode_header, jwk::JwkSet, Algorithm, DecodingKey, Validation};
use rand::{rngs::SysRng, TryRng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use subtle::ConstantTimeEq;
use time::{macros::format_description, OffsetDateTime};
use tokio::{
    net::TcpListener,
    sync::{oneshot, Mutex},
    task::JoinHandle,
};
use url::Url;

const RESOURCE: &str = "https://api.openai.com/v1";
const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const DYNAMIC_CLIENT: &str = "dynamic_agent_client";
const XAI_CLIENT: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const XAI_SCOPES: &str = "openid profile email offline_access grok-cli:access api:access";

pub type Result<T> = std::result::Result<T, Error>;

/// Errors never include endpoint response bodies, authorization URLs, or tokens.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not read or atomically save protected login files")]
    Storage,
    #[error("OAuth endpoints must use HTTPS (HTTP is permitted only for loopback tests)")]
    Endpoint,
    #[error("could not start the loopback callback listener")]
    Listener,
    #[error("sign-in timed out or was interrupted")]
    Timeout,
    #[error("invalid callback state or parameters")]
    Callback,
    #[error("ChatGPT authorization was denied")]
    AccessDenied,
    #[error("OAuth authorization failed")]
    Authorization,
    #[error("registration incomplete: callback did not supply an issued client_id")]
    Incomplete,
    #[error("returning sign-in changed the selected client or account identity")]
    AccountMismatch,
    #[error("OAuth discovery or token request failed; start a fresh sign-in attempt")]
    Http,
    #[error("authorization code expired or was already used; retry with the issued client ID")]
    InvalidGrant { client_id: String },
    #[error("ID token signature or required claims are invalid")]
    IdToken,
    #[error("ChatGPT plan permission chatgpt.tokens.use.direct was not granted; authorize plan usage and try again")]
    PlanPermission,
    #[error("required OAuth scope {0} was not granted")]
    MissingScope(&'static str),
}

impl Error {
    /// The issued registration survives an expired code within this attempt.
    pub fn retry_client_id(&self) -> Option<&str> {
        match self {
            Self::InvalidGrant { client_id } => Some(client_id),
            _ => None,
        }
    }
}

/// Programmatic overrides for tests; the CLI always uses the official endpoints.
#[derive(Clone)]
pub struct Endpoints {
    pub authorize: Url,
    pub token: Url,
    pub issuer: Url,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            authorize: Url::parse("https://auth.openai.com/api/accounts/authorize").unwrap(),
            token: Url::parse("https://auth.openai.com/api/accounts/oauth/token").unwrap(),
            issuer: Url::parse("https://auth.openai.com").unwrap(),
        }
    }
}

/// Which provider a sign-in attempt registers credentials for.
/// `codex` uses dynamic client registration and the Sign in with ChatGPT
/// parameters; `xai` uses the public Grok CLI client on auth.x.ai.
#[derive(Clone)]
pub struct Flow {
    endpoints: Endpoints,
    client: &'static str,
    scopes: &'static str,
    resource: Option<&'static str>,
    /// Extra authorize-query parameters beyond the standard PKCE set.
    extra_params: &'static [(&'static str, &'static str)],
    /// Scope the token response must grant, when the provider defines one.
    required_scope: Option<&'static str>,
    id_token_alg: Algorithm,
    /// Whether the authorization request carries ext_agent_host_id.
    host_binding: bool,
    port: u16,
    callback_path: &'static str,
    verifier_bytes: usize,
    credential_id: &'static str,
    provider: &'static str,
}

impl Flow {
    /// Sign in with ChatGPT (OpenAI Responses upstream).
    pub fn codex(endpoints: Endpoints) -> Self {
        Self {
            endpoints,
            client: DYNAMIC_CLIENT,
            scopes: SCOPES,
            resource: Some(RESOURCE),
            extra_params: &[],
            required_scope: Some("chatgpt.tokens.use.direct"),
            id_token_alg: Algorithm::RS256,
            host_binding: true,
            port: 1455,
            callback_path: "/auth/callback",
            verifier_bytes: 32,
            credential_id: "codex",
            provider: "openai_responses",
        }
    }

    /// Sign in with xAI (api.x.ai, OpenAI-compatible upstream). Uses the
    /// public Grok CLI OAuth client, matching OmniRoute's xai-oauth flow.
    pub fn xai(endpoints: Endpoints) -> Self {
        Self {
            endpoints,
            client: XAI_CLIENT,
            scopes: XAI_SCOPES,
            resource: None,
            extra_params: &[("plan", "generic"), ("referrer", "cli-proxy-api")],
            required_scope: Some("api:access"),
            id_token_alg: Algorithm::ES256,
            host_binding: false,
            port: 56121,
            callback_path: "/callback",
            verifier_bytes: 96,
            credential_id: "xai",
            provider: "openai_compat",
        }
    }

    /// Dynamic-registration flows retry an expired code with the issued
    /// client ID; fixed-client flows just restart the whole sign-in.
    pub fn dynamic(&self) -> bool {
        self.client == DYNAMIC_CLIENT
    }
}

/// One registration per directory. Use separate directories for separate accounts
/// or workspaces. Secrets intentionally do not implement Debug.
#[derive(Serialize, Deserialize)]
pub struct Credentials {
    pub email: Option<String>,
    pub issuer: String,
    pub subject: String,
    pub client_id: String,
    pub ext_agent_host_id: String,
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: String,
    pub expires_in: u64,
    pub scopes: Vec<String>,
    pub saved_at: String,
}

pub struct LoginResult {
    pub credentials: Credentials,
    pub snippet: String,
}

/// Owns the bound listener and fresh state/nonce/PKCE for a single attempt.
/// Dropping it (or its `finish` future) closes the listener.
pub struct PendingLogin {
    dir: PathBuf,
    host: String,
    previous: Option<Credentials>,
    client: String,
    flow: Flow,
    listener: TcpListener,
    authorization: Url,
    redirect: String,
    state: String,
    nonce: String,
    verifier: String,
}

impl PendingLogin {
    pub async fn start(dir: impl AsRef<Path>, flow: Flow) -> Result<Self> {
        Self::start_inner(dir.as_ref(), flow, None).await
    }

    /// Retry an invalid_grant with the callback's issued client ID, never the old
    /// code or PKCE values. The registration is not saved until identity validates.
    /// Fixed-client flows (xai) have no issued client and start a fresh sign-in.
    pub async fn retry(dir: impl AsRef<Path>, flow: Flow, client: &str) -> Result<Self> {
        if client.is_empty() || client == DYNAMIC_CLIENT {
            return Err(Error::Incomplete);
        }
        let issued = flow.dynamic().then_some(client);
        Self::start_inner(dir.as_ref(), flow, issued).await
    }

    async fn start_inner(dir: &Path, flow: Flow, retry: Option<&str>) -> Result<Self> {
        for endpoint in [
            &flow.endpoints.authorize,
            &flow.endpoints.token,
            &flow.endpoints.issuer,
        ] {
            http::validate_url(endpoint)?;
        }
        if flow.endpoints.issuer.query().is_some() {
            return Err(Error::Endpoint);
        }
        let dir = dir.to_path_buf();
        let (dir, host, previous) = tokio::task::spawn_blocking(move || storage::prepare(&dir))
            .await
            .map_err(|_| Error::Storage)??;
        if let Some(saved) = &previous {
            if saved.client_id.is_empty()
                || saved.client_id == DYNAMIC_CLIENT
                || saved.subject.is_empty()
                || saved.issuer != flow.endpoints.issuer.as_str().trim_end_matches('/')
                || saved.ext_agent_host_id != host
                || retry.is_some_and(|client| client != saved.client_id)
            {
                return Err(Error::AccountMismatch);
            }
        }
        let client = previous
            .as_ref()
            .map(|p| p.client_id.as_str())
            .or(retry)
            .unwrap_or(flow.client)
            .to_owned();
        let listener = match TcpListener::bind(("127.0.0.1", flow.port)).await {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                TcpListener::bind("127.0.0.1:0")
                    .await
                    .map_err(|_| Error::Listener)?
            }
            Err(_) => return Err(Error::Listener),
        };
        let redirect = format!(
            "http://127.0.0.1:{}{}",
            listener.local_addr().map_err(|_| Error::Listener)?.port(),
            flow.callback_path
        );
        let state = random()?;
        let nonce = random()?;
        let verifier = random_n(flow.verifier_bytes)?;
        let challenge = pkce_challenge(&verifier);
        let mut authorization = flow.endpoints.authorize.clone();
        {
            let mut query = authorization.query_pairs_mut();
            query
                .append_pair("client_id", &client)
                .append_pair("response_type", "code")
                .append_pair("redirect_uri", &redirect)
                .append_pair("scope", flow.scopes)
                .append_pair("state", &state)
                .append_pair("nonce", &nonce)
                .append_pair("code_challenge_method", "S256")
                .append_pair("code_challenge", &challenge);
            if flow.host_binding {
                query.append_pair("ext_agent_host_id", &host);
            }
            if let Some(resource) = flow.resource {
                query.append_pair("resource", resource);
            }
            for (key, value) in flow.extra_params {
                query.append_pair(key, value);
            }
            if let Some(saved) = &previous {
                if !saved.id_token.is_empty() {
                    query.append_pair("id_token_hint", &saved.id_token);
                }
                if let Some(email) = &saved.email {
                    query.append_pair("login_hint", email);
                }
            } else if client == DYNAMIC_CLIENT {
                query.append_pair("agent_name_hint", "llmux");
            }
        }
        Ok(Self {
            dir,
            host,
            previous,
            client,
            flow,
            listener,
            authorization,
            redirect,
            state,
            nonce,
            verifier,
        })
    }

    /// Contains an ID-token hint on returning sign-in. Display only to the user;
    /// never send this URL to logs, analytics, or support transcripts.
    pub fn authorization_url(&self) -> &Url {
        &self.authorization
    }

    pub async fn finish(self) -> Result<LoginResult> {
        let (sender, receiver) = oneshot::channel();
        let callback = Arc::new(CallbackState {
            expected_state: self.state,
            client: (self.client != DYNAMIC_CLIENT).then_some(self.client),
            sender: Mutex::new(Some(sender)),
        });
        let router = Router::new()
            .route(self.flow.callback_path, get(handle_callback))
            .with_state(callback);
        let server = AbortOnDrop(tokio::spawn(async move {
            let _ = axum::serve(self.listener, router).await;
        }));
        let callback = tokio::time::timeout(Duration::from_secs(300), receiver)
            .await
            .map_err(|_| Error::Timeout)?
            .map_err(|_| Error::Timeout)??;
        drop(server);
        let http = http::Http::new();
        let form = {
            let mut form = url::form_urlencoded::Serializer::new(String::new());
            form.append_pair("grant_type", "authorization_code")
                .append_pair("client_id", &callback.client)
                .append_pair("code", &callback.code)
                .append_pair("code_verifier", &self.verifier)
                .append_pair("redirect_uri", &self.redirect);
            if let Some(resource) = self.flow.resource {
                form.append_pair("resource", resource);
            }
            form.finish()
        };
        let tokens: TokenResponse = http
            .exchange(&self.flow.endpoints.token, form, &callback.client)
            .await?;
        let saved_at = saved_at_timestamp(OffsetDateTime::now_utc().unix_timestamp());
        let identity = validate_identity(
            &http,
            &self.flow,
            &tokens.id_token,
            &callback.client,
            &self.nonce,
        )
        .await?;
        if self
            .previous
            .as_ref()
            .is_some_and(|p| p.subject != identity.sub || p.issuer != identity.iss)
        {
            return Err(Error::AccountMismatch);
        }
        let scopes: Vec<String> = tokens.scope.split_whitespace().map(str::to_owned).collect();
        if let Some(required) = self.flow.required_scope {
            if !scopes.iter().any(|scope| scope == required) {
                if required == "chatgpt.tokens.use.direct" {
                    return Err(Error::PlanPermission);
                }
                return Err(Error::MissingScope(required));
            }
        }
        if tokens.access_token.is_empty()
            || tokens.refresh_token.is_empty()
            || tokens.expires_in == 0
            || !tokens.token_type.eq_ignore_ascii_case("Bearer")
        {
            return Err(Error::Http);
        }
        let credentials = Credentials {
            email: identity.email,
            issuer: identity.iss,
            subject: identity.sub,
            client_id: callback.client,
            ext_agent_host_id: self.host,
            id_token: tokens.id_token,
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            token_type: tokens.token_type,
            expires_in: tokens.expires_in,
            scopes,
            saved_at,
        };
        let snippet = snippet(&credentials, &self.dir, &self.flow);
        tokio::task::spawn_blocking(move || {
            storage::save(&self.dir, &credentials)?;
            Ok(LoginResult {
                credentials,
                snippet,
            })
        })
        .await
        .map_err(|_| Error::Storage)?
    }
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier))
}

fn random() -> Result<String> {
    random_n(32)
}

fn random_n(n: usize) -> Result<String> {
    let mut bytes = vec![0; n];
    SysRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| Error::Authorization)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

struct AbortOnDrop(JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Callback {
    code: String,
    client: String,
}
struct CallbackState {
    expected_state: String,
    client: Option<String>,
    sender: Mutex<Option<oneshot::Sender<Result<Callback>>>>,
}

fn parse_callback(query: &str, pending: &CallbackState) -> Result<Callback> {
    let mut fields = HashMap::new();
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if fields
            .insert(key.into_owned(), value.into_owned())
            .is_some()
        {
            return Err(Error::Callback);
        }
    }
    if let Some(error) = fields.get("error") {
        return Err(if error == "access_denied" {
            Error::AccessDenied
        } else {
            Error::Authorization
        });
    }
    let supplied = fields.get("client_id");
    let client = if let Some(client) = &pending.client {
        if supplied.is_some_and(|id| id != client) {
            return Err(Error::AccountMismatch);
        }
        client.clone()
    } else {
        supplied
            .filter(|id| !id.is_empty() && *id != DYNAMIC_CLIENT)
            .ok_or(Error::Incomplete)?
            .clone()
    };
    let code = fields
        .get("code")
        .filter(|code| !code.is_empty())
        .ok_or(Error::Callback)?
        .clone();
    Ok(Callback { code, client })
}

async fn handle_callback(
    State(state): State<Arc<CallbackState>>,
    RawQuery(query): RawQuery,
) -> (::http::StatusCode, &'static str) {
    let query = query.as_deref().unwrap_or("");
    let mut states =
        url::form_urlencoded::parse(query.as_bytes()).filter(|(key, _)| key == "state");
    let matches_state = states.next().is_some_and(|(_, value)| {
        bool::from(value.as_bytes().ct_eq(state.expected_state.as_bytes()))
    });
    if !matches_state || states.next().is_some() {
        return (::http::StatusCode::BAD_REQUEST, "Invalid callback state.");
    }
    let result = parse_callback(query, &state);
    let status = if result.is_ok() {
        ::http::StatusCode::OK
    } else {
        ::http::StatusCode::BAD_REQUEST
    };
    if let Some(sender) = state.sender.lock().await.take() {
        let _ = sender.send(result);
    }
    (
        status,
        "Sign-in callback received. Return to the terminal for the result.",
    )
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    id_token: String,
    token_type: String,
    expires_in: u64,
    #[serde(default)]
    scope: String,
}
#[derive(Deserialize)]
struct Identity {
    iss: String,
    sub: String,
    email: Option<String>,
    /// Providers that do not echo the authorize nonce may omit this claim.
    nonce: Option<String>,
    exp: i64,
}

async fn validate_identity(
    http: &http::Http,
    flow: &Flow,
    token: &str,
    client: &str,
    nonce: &str,
) -> Result<Identity> {
    #[derive(Deserialize)]
    struct Discovery {
        issuer: String,
        jwks_uri: Url,
    }
    let issuer = flow.endpoints.issuer.as_str().trim_end_matches('/');
    let discovery: Discovery = http
        .json(
            &flow
                .endpoints
                .issuer
                .join(".well-known/openid-configuration")
                .map_err(|_| Error::Endpoint)?,
        )
        .await?;
    if discovery.issuer != issuer {
        return Err(Error::IdToken);
    }
    let jwks: JwkSet = http.json(&discovery.jwks_uri).await?;
    let header = decode_header(token).map_err(|_| Error::IdToken)?;
    if header.alg != flow.id_token_alg {
        return Err(Error::IdToken);
    }
    let key = jwks
        .find(header.kid.as_deref().ok_or(Error::IdToken)?)
        .ok_or(Error::IdToken)?;
    let key = DecodingKey::from_jwk(key).map_err(|_| Error::IdToken)?;
    let mut validation = Validation::new(flow.id_token_alg);
    validation.set_audience(&[client]);
    validation.set_issuer(&[issuer]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    validation.leeway = 0;
    validation.validate_nbf = true;
    let identity = decode::<Identity>(token, &key, &validation)
        .map_err(|_| Error::IdToken)?
        .claims;
    // Sign in with ChatGPT always echoes the nonce; when another provider
    // echoes one it must match, but absence is not itself a rejection.
    let nonce_ok = match (&identity.nonce, flow.dynamic()) {
        (Some(claim), _) => bool::from(claim.as_bytes().ct_eq(nonce.as_bytes())),
        (None, true) => false,
        (None, false) => true,
    };
    if identity.sub.is_empty()
        || token_expired(identity.exp, OffsetDateTime::now_utc().unix_timestamp())
        || !nonce_ok
    {
        return Err(Error::IdToken);
    }
    Ok(identity)
}

fn snippet(credentials: &Credentials, dir: &Path, flow: &Flow) -> String {
    let quote = |value: &str| toml::Value::String(value.to_owned()).to_string();
    let resource = flow
        .resource
        .map(|resource| format!("resource = {}\n", quote(resource)))
        .unwrap_or_default();
    format!("[[credentials]]\nid = {}\nkind = \"oauth\"\nprovider = {}\ntoken_url = {}\nclient_id = {}\nrefresh_token = {}\n{}", quote(flow.credential_id), quote(flow.provider), quote(flow.endpoints.token.as_str()), quote(&credentials.client_id), quote(&format!("file:{}", dir.join("refresh_token").display())), resource)
}

fn saved_at_timestamp(seconds: i64) -> String {
    OffsetDateTime::from_unix_timestamp(seconds)
        .unwrap()
        .format(format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second]Z"
        ))
        .unwrap()
}

fn token_expired(exp: i64, now: i64) -> bool {
    exp <= now
}

#[cfg(test)]
mod pkce_tests {
    use super::*;

    #[test]
    fn pkce_s256_matches_rfc7636_appendix_b() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}

#[cfg(test)]
mod timestamp_tests {
    use super::*;

    #[test]
    fn saved_at_is_utc_without_fractional_seconds() {
        assert_eq!(saved_at_timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(saved_at_timestamp(1_704_164_645), "2024-01-02T03:04:05Z");
    }

    #[test]
    fn expiry_is_inclusive_and_has_no_leeway() {
        assert!(token_expired(99, 100));
        assert!(token_expired(100, 100));
        assert!(!token_expired(101, 100));
    }
}
