//! OpenAI Sign in with ChatGPT authorization-code flow.

mod http;
mod storage;

use axum::{
    extract::{RawQuery, State},
    routing::get,
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{SecondsFormat, Utc};
use jsonwebtoken::{decode, decode_header, jwk::JwkSet, Algorithm, DecodingKey, Validation};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use subtle::ConstantTimeEq;
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
    endpoints: Endpoints,
    listener: TcpListener,
    authorization: Url,
    redirect: String,
    state: String,
    nonce: String,
    verifier: String,
}

impl PendingLogin {
    pub async fn start(dir: impl AsRef<Path>, endpoints: Endpoints) -> Result<Self> {
        Self::start_inner(dir.as_ref(), endpoints, None).await
    }

    /// Retry an invalid_grant with the callback's issued client ID, never the old
    /// code or PKCE values. The registration is not saved until identity validates.
    pub async fn retry(dir: impl AsRef<Path>, endpoints: Endpoints, client: &str) -> Result<Self> {
        if client.is_empty() || client == DYNAMIC_CLIENT {
            return Err(Error::Incomplete);
        }
        Self::start_inner(dir.as_ref(), endpoints, Some(client)).await
    }

    async fn start_inner(dir: &Path, endpoints: Endpoints, retry: Option<&str>) -> Result<Self> {
        for endpoint in [&endpoints.authorize, &endpoints.token, &endpoints.issuer] {
            http::validate_url(endpoint)?;
        }
        if endpoints.issuer.query().is_some() {
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
                || saved.issuer != endpoints.issuer.as_str().trim_end_matches('/')
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
            .unwrap_or(DYNAMIC_CLIENT)
            .to_owned();
        let listener = match TcpListener::bind("127.0.0.1:1455").await {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                TcpListener::bind("127.0.0.1:0")
                    .await
                    .map_err(|_| Error::Listener)?
            }
            Err(_) => return Err(Error::Listener),
        };
        let redirect = format!(
            "http://127.0.0.1:{}/auth/callback",
            listener.local_addr().map_err(|_| Error::Listener)?.port()
        );
        let state = random()?;
        let nonce = random()?;
        let verifier = random()?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(&verifier));
        let mut authorization = endpoints.authorize.clone();
        {
            let mut query = authorization.query_pairs_mut();
            query
                .append_pair("client_id", &client)
                .append_pair("ext_agent_host_id", &host)
                .append_pair("response_type", "code")
                .append_pair("redirect_uri", &redirect)
                .append_pair("scope", SCOPES)
                .append_pair("resource", RESOURCE)
                .append_pair("state", &state)
                .append_pair("nonce", &nonce)
                .append_pair("code_challenge_method", "S256")
                .append_pair("code_challenge", &challenge);
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
            endpoints,
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
            .route("/auth/callback", get(handle_callback))
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
        let form = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "authorization_code")
            .append_pair("client_id", &callback.client)
            .append_pair("code", &callback.code)
            .append_pair("code_verifier", &self.verifier)
            .append_pair("redirect_uri", &self.redirect)
            .append_pair("resource", RESOURCE)
            .finish();
        let tokens: TokenResponse = http
            .exchange(&self.endpoints.token, form, &callback.client)
            .await?;
        let saved_at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        let identity = validate_identity(
            &http,
            &self.endpoints,
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
        if !scopes
            .iter()
            .any(|scope| scope == "chatgpt.tokens.use.direct")
        {
            return Err(Error::PlanPermission);
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
        let snippet = snippet(&credentials, &self.dir, &self.endpoints);
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

fn random() -> Result<String> {
    let mut bytes = [0; 32];
    OsRng
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
    let state = fields.get("state").ok_or(Error::Callback)?;
    if !bool::from(state.as_bytes().ct_eq(pending.expected_state.as_bytes())) {
        return Err(Error::Callback);
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
    let result = parse_callback(query.as_deref().unwrap_or(""), &state);
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
    nonce: String,
    exp: i64,
}

async fn validate_identity(
    http: &http::Http,
    endpoints: &Endpoints,
    token: &str,
    client: &str,
    nonce: &str,
) -> Result<Identity> {
    #[derive(Deserialize)]
    struct Discovery {
        issuer: String,
        jwks_uri: Url,
    }
    let issuer = endpoints.issuer.as_str().trim_end_matches('/');
    let discovery: Discovery = http
        .json(
            &endpoints
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
    if header.alg != Algorithm::RS256 {
        return Err(Error::IdToken);
    }
    let key = jwks
        .find(header.kid.as_deref().ok_or(Error::IdToken)?)
        .ok_or(Error::IdToken)?;
    let key = DecodingKey::from_jwk(key).map_err(|_| Error::IdToken)?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_audience(&[client]);
    validation.set_issuer(&[issuer]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    validation.leeway = 0;
    validation.validate_nbf = true;
    let identity = decode::<Identity>(token, &key, &validation)
        .map_err(|_| Error::IdToken)?
        .claims;
    if identity.sub.is_empty()
        || identity.exp <= Utc::now().timestamp()
        || !bool::from(identity.nonce.as_bytes().ct_eq(nonce.as_bytes()))
    {
        return Err(Error::IdToken);
    }
    Ok(identity)
}

fn snippet(credentials: &Credentials, dir: &Path, endpoints: &Endpoints) -> String {
    let quote = |value: &str| toml::Value::String(value.to_owned()).to_string();
    format!("[[credentials]]\nid = \"codex\"\nkind = \"oauth\"\nprovider = \"openai_responses\"\ntoken_url = {}\nclient_id = {}\nrefresh_token = {}\nresource = {}\n", quote(endpoints.token.as_str()), quote(&credentials.client_id), quote(&format!("file:{}", dir.join("refresh_token").display())), quote(RESOURCE))
}
