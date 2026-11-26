//! Dahl Inference account-key credential source.
//!
//! Dahl account keys are backed by one shared account pool. Rotating a key does
//! not create quota; it allocates part of the existing pool to the replacement
//! key. The state file records only keys this source created, so rotation never
//! revokes a user-managed key.

use arc_swap::ArcSwapOption;
use async_trait::async_trait;
use bytes::Bytes;
use http::{header, HeaderValue, Method, Request, StatusCode, Uri};
use http_body_util::{BodyExt, Full, Limited};
use llmux_types::{
    http_client, is_https_or_loopback_http, AuthError, AuthMaterial, CredentialSource, HttpClient,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;

pub struct DahlConfig {
    pub base_url: String,
    pub fingerprint: Arc<str>,
    pub state_path: PathBuf,
    pub allocation_tokens: Option<u64>,
    pub allocate_entire_pool: bool,
    pub request_timeout: Duration,
}

#[derive(Clone, Serialize, Deserialize)]
struct DahlState {
    key_id: u64,
    token: String,
    owned: bool,
}

impl DahlState {
    fn load(path: &PathBuf) -> io::Result<Option<Self>> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let state: Self = serde_json::from_slice(&bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid Dahl state file"))?;
        if state.key_id == 0 || state.token.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Dahl state file has no usable key",
            ));
        }
        Ok(Some(state))
    }

    fn save(&self, path: &Path) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "could not encode state"))?;
        llmux_types::atomic_write(path, &bytes)
    }
}

struct Token {
    key_id: u64,
    owned: bool,
    auth: AuthMaterial,
}

struct Inner {
    config: DahlConfig,
    http: HttpClient,
    token: ArcSwapOption<Token>,
    refresh_lock: Mutex<()>,
}

pub struct DahlCredentialSource {
    inner: Arc<Inner>,
}

impl DahlCredentialSource {
    pub async fn start(config: DahlConfig) -> Result<Self, AuthError> {
        if config.request_timeout.is_zero() {
            return Err(AuthError::RefreshFailed);
        }
        let uri: Uri = config
            .base_url
            .parse()
            .map_err(|_| AuthError::RefreshFailed)?;
        if uri.authority().is_none()
            || uri.authority().is_some_and(|a| a.as_str().contains('@'))
            || !is_https_or_loopback_http(&uri)
        {
            return Err(AuthError::RefreshFailed);
        }
        let state = DahlState::load(&config.state_path).map_err(|_| AuthError::Unavailable)?;
        let initial = state
            .map(|state| make_token(state.key_id, state.owned, &state.token))
            .transpose()?;
        let inner = Arc::new(Inner {
            config,
            http: http_client(),
            token: ArcSwapOption::from(initial.map(Arc::new)),
            refresh_lock: Mutex::new(()),
        });
        let source = Self { inner };
        if source.inner.token.load().is_none() {
            source.refresh().await?;
        }
        Ok(source)
    }
}

#[async_trait]
impl CredentialSource for DahlCredentialSource {
    fn current(&self) -> Result<AuthMaterial, AuthError> {
        self.inner
            .token
            .load()
            .as_ref()
            .map(|token| token.auth.clone())
            .ok_or(AuthError::Unavailable)
    }

    async fn refresh(&self) -> Result<(), AuthError> {
        let before = self
            .inner
            .token
            .load()
            .as_ref()
            .map(|token| (token.key_id, token.owned));
        let _guard = self.inner.refresh_lock.lock().await;
        if self
            .inner
            .token
            .load()
            .as_ref()
            .map(|token| (token.key_id, token.owned))
            != before
        {
            return Ok(());
        }
        tokio::time::timeout(
            self.inner.config.request_timeout,
            rotate(&self.inner, before),
        )
        .await
        .map_err(|_| AuthError::RefreshFailed)?
    }
}

fn make_token(key_id: u64, owned: bool, key: &str) -> Result<Token, AuthError> {
    let mut value =
        HeaderValue::from_str(&format!("Bearer {key}")).map_err(|_| AuthError::RefreshFailed)?;
    value.set_sensitive(true);
    Ok(Token {
        key_id,
        owned,
        auth: AuthMaterial::Header {
            name: header::AUTHORIZATION,
            value,
        },
    })
}

#[derive(Deserialize)]
struct AccountResponse {
    token_balance: u64,
}

#[derive(Deserialize)]
struct KeysResponse {
    #[serde(default)]
    keys: Vec<KeySummary>,
}

#[derive(Deserialize)]
struct KeySummary {
    id: u64,
    #[serde(default)]
    available_tokens: u64,
}

#[derive(Deserialize)]
struct CreateKeyResponse {
    key: CreatedKey,
}

#[derive(Deserialize)]
struct CreatedKey {
    id: u64,
    token: String,
}

async fn rotate(inner: &Inner, previous_key: Option<(u64, bool)>) -> Result<(), AuthError> {
    let cookie = signin(inner).await?;
    let (status, body, _) =
        request(inner, Method::GET, "/v1/account/keys", None, Some(&cookie)).await?;
    if status != StatusCode::OK {
        return Err(AuthError::RefreshFailed);
    }
    let keys: KeysResponse = serde_json::from_slice(&body).map_err(|_| AuthError::RefreshFailed)?;
    let previous_allocated = previous_key
        .filter(|(id, owned)| *owned && keys.keys.iter().any(|key| key.id == *id))
        .and_then(|(id, _)| {
            keys.keys
                .iter()
                .find(|key| key.id == id)
                .map(|key| key.available_tokens)
        })
        .unwrap_or(0);
    let (status, body, _) = request(inner, Method::GET, "/v1/account", None, Some(&cookie)).await?;
    if status != StatusCode::OK {
        return Err(AuthError::RefreshFailed);
    }
    let account: AccountResponse =
        serde_json::from_slice(&body).map_err(|_| AuthError::RefreshFailed)?;
    let configured = inner.config.allocation_tokens.unwrap_or(u64::MAX);
    let budget = account.token_balance.saturating_add(previous_allocated);
    let amount = if inner.config.allocate_entire_pool {
        budget
    } else {
        configured.min(budget)
    };
    if amount == 0 {
        return Err(AuthError::RefreshFailed);
    }

    // Dahl keeps already allocated tokens on the old key rather than in the
    // account pool. Free those tokens first when they are needed; the key that
    // triggered refresh is already unusable, while a user-managed key is never
    // touched because previous_allocated is zero for it.
    let mut revoked_before_create = false;
    if amount > account.token_balance && previous_allocated > 0 {
        let (id, _) = previous_key.unwrap_or_default();
        let path = format!("/v1/account/keys/{id}");
        let (status, _, _) = request(inner, Method::DELETE, &path, None, Some(&cookie)).await?;
        if status != StatusCode::OK && status != StatusCode::NO_CONTENT {
            return Err(AuthError::RefreshFailed);
        }
        revoked_before_create = true;
    }

    let (status, body, _) = request(
        inner,
        Method::POST,
        "/v1/account/keys",
        Some(json!({})),
        Some(&cookie),
    )
    .await?;
    if status != StatusCode::OK && status != StatusCode::CREATED {
        return Err(AuthError::RefreshFailed);
    }
    let created: CreateKeyResponse =
        serde_json::from_slice(&body).map_err(|_| AuthError::RefreshFailed)?;
    if created.key.id == 0 || created.key.token.is_empty() {
        return Err(AuthError::RefreshFailed);
    }

    let allocate_body = json!({"key_id": created.key.id, "amount": amount});
    let (status, _, _) = request(
        inner,
        Method::POST,
        "/v1/account/allocate",
        Some(allocate_body),
        Some(&cookie),
    )
    .await?;
    if status != StatusCode::OK && status != StatusCode::NO_CONTENT {
        let path = format!("/v1/account/keys/{}", created.key.id);
        let _ = request(inner, Method::DELETE, &path, None, Some(&cookie)).await;
        return Err(AuthError::RefreshFailed);
    }

    let state = DahlState {
        key_id: created.key.id,
        token: created.key.token.clone(),
        owned: true,
    };
    let path = inner.config.state_path.clone();
    let persisted = state.clone();
    tokio::task::spawn_blocking(move || persisted.save(&path))
        .await
        .map_err(|_| AuthError::RefreshFailed)?
        .map_err(|_| AuthError::RefreshFailed)?;
    inner.token.store(Some(Arc::new(make_token(
        created.key.id,
        true,
        &created.key.token,
    )?)));

    if !revoked_before_create
        && previous_key.is_some_and(|(id, owned)| owned && id != created.key.id)
    {
        // Revoking returns the remainder to the shared pool. Only keys marked
        // owned in llmux's state file are revoked; user-managed keys are left alone.
        let (id, _) = previous_key.unwrap_or_default();
        let path = format!("/v1/account/keys/{id}");
        let _ = request(inner, Method::DELETE, &path, None, Some(&cookie)).await;
    }
    Ok(())
}

async fn signin(inner: &Inner) -> Result<String, AuthError> {
    let body = json!({"fingerprint": inner.config.fingerprint.as_ref()});
    let (status, _, cookie) =
        request(inner, Method::POST, "/v1/auth/signin", Some(body), None).await?;
    if status != StatusCode::OK {
        return Err(AuthError::RefreshFailed);
    }
    cookie
        .filter(|cookie| !cookie.is_empty())
        .ok_or(AuthError::RefreshFailed)
}

async fn request(
    inner: &Inner,
    method: Method,
    path: &str,
    body: Option<Value>,
    cookie: Option<&str>,
) -> Result<(StatusCode, Bytes, Option<String>), AuthError> {
    let uri: Uri = format!("{}{}", inner.config.base_url.trim_end_matches('/'), path)
        .parse()
        .map_err(|_| AuthError::RefreshFailed)?;
    let payload = body
        .map(|body| serde_json::to_vec(&body).unwrap_or_default())
        .unwrap_or_default();
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::ACCEPT, "application/json")
        .header(header::USER_AGENT, llmux_types::USER_AGENT);
    if !payload.is_empty() {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    let request = builder
        .body(Full::new(Bytes::from(payload)))
        .map_err(|_| AuthError::RefreshFailed)?;
    let response = inner
        .http
        .request(request)
        .await
        .map_err(|_| AuthError::RefreshFailed)?;
    let status = response.status();
    let cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::to_owned);
    let body = Limited::new(response.into_body(), 1024 * 1024)
        .collect()
        .await
        .map_err(|_| AuthError::RefreshFailed)?
        .to_bytes();
    Ok((status, body, cookie))
}
