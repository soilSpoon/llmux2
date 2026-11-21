//! Nous Portal subscription credentials (device-code login, rotating refresh token).
//!
//! The access token the portal issues for scope `inference:invoke` is itself
//! the bearer for the inference API. Refresh tokens are single-use, so every
//! rotation is persisted to the state file before the new token is used.

use arc_swap::ArcSwapOption;
use async_trait::async_trait;
use bytes::Bytes;
use http::{header, HeaderValue, Request, Uri};
use http_body_util::{BodyExt, Full, Limited};
use llmux_types::{
    http_client, is_https_or_loopback_http, AuthError, AuthMaterial, CredentialSource, HttpClient,
    NousState,
};
use serde::Deserialize;
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Weak,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{Mutex, Notify},
    task::JoinHandle,
    time::Instant,
};

pub struct NousPortalConfig {
    pub state_path: PathBuf,
    pub refresh_skew: Duration,
    pub request_timeout: Duration,
}

impl NousPortalConfig {
    pub fn new(state_path: PathBuf) -> Self {
        Self {
            state_path,
            refresh_skew: Duration::from_secs(120),
            request_timeout: Duration::from_secs(30),
        }
    }
}

struct Token {
    auth: AuthMaterial,
    expires: Instant,
    refresh_at: Instant,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    scope: Option<String>,
    expires_in: Option<u64>,
    expires_at: Option<u64>,
}

struct RefreshState {
    state: NousState,
    succeeded: bool,
}

struct Inner {
    config: NousPortalConfig,
    uri: Uri,
    http: HttpClient,
    token: ArcSwapOption<Token>,
    generation: AtomicU64,
    state: Mutex<RefreshState>,
    changed: Notify,
}

/// Dropping the source aborts its background refresh task.
pub struct NousPortalSource {
    inner: Arc<Inner>,
    task: JoinHandle<()>,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl NousPortalSource {
    pub async fn start(config: NousPortalConfig) -> Result<Self, AuthError> {
        if config.request_timeout.is_zero() {
            return Err(AuthError::RefreshFailed);
        }
        let state = NousState::load(&config.state_path).map_err(|_| AuthError::Unavailable)?;
        let uri: Uri = format!(
            "{}/api/oauth/token",
            state.portal_base_url.trim_end_matches('/')
        )
        .parse()
        .map_err(|_| AuthError::RefreshFailed)?;
        if uri.authority().is_none()
            || uri.authority().is_some_and(|a| a.as_str().contains('@'))
            || !is_https_or_loopback_http(&uri)
        {
            return Err(AuthError::RefreshFailed);
        }
        let remaining = state.expires_at.saturating_sub(unix_now());
        let initial = (!state.access_token.is_empty() && remaining > 0)
            .then(|| make_token(&state.access_token, remaining, config.refresh_skew))
            .transpose()?;
        let usable = initial
            .as_ref()
            .is_some_and(|t| t.refresh_at > Instant::now());
        let inner = Arc::new(Inner {
            config,
            uri,
            http: http_client(),
            token: ArcSwapOption::from(initial.map(Arc::new)),
            generation: AtomicU64::new(0),
            changed: Notify::new(),
            state: Mutex::new(RefreshState {
                state,
                succeeded: false,
            }),
        });
        if !usable {
            inner.refresh().await?;
        }
        let task = tokio::spawn(background(Arc::downgrade(&inner)));
        Ok(Self { inner, task })
    }
}

impl Drop for NousPortalSource {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[async_trait]
impl CredentialSource for NousPortalSource {
    fn current(&self) -> Result<AuthMaterial, AuthError> {
        self.inner
            .token
            .load()
            .as_ref()
            .filter(|t| Instant::now() < t.expires)
            .map(|t| t.auth.clone())
            .ok_or(AuthError::Unavailable)
    }

    async fn refresh(&self) -> Result<(), AuthError> {
        self.inner.refresh().await
    }
}

fn make_token(access: &str, lifetime_secs: u64, skew: Duration) -> Result<Token, AuthError> {
    let mut value =
        HeaderValue::from_str(&format!("Bearer {access}")).map_err(|_| AuthError::RefreshFailed)?;
    value.set_sensitive(true);
    let lifetime = Duration::from_secs(lifetime_secs);
    let expires = Instant::now()
        .checked_add(lifetime)
        .ok_or(AuthError::RefreshFailed)?;
    let skew = skew.min(lifetime / 2);
    Ok(Token {
        auth: AuthMaterial::Header {
            name: header::AUTHORIZATION,
            value,
        },
        expires,
        refresh_at: expires - skew,
    })
}

impl Inner {
    async fn refresh(self: &Arc<Self>) -> Result<(), AuthError> {
        let generation = self.generation.load(Ordering::Acquire);
        let inner = self.clone();
        tokio::spawn(async move { inner.refresh_generation(generation).await })
            .await
            .map_err(|_| AuthError::RefreshFailed)?
    }

    async fn refresh_generation(&self, generation: u64) -> Result<(), AuthError> {
        let mut guard = self.state.lock().await;
        if self.generation.load(Ordering::Acquire) != generation {
            return if guard.succeeded {
                Ok(())
            } else {
                Err(AuthError::RefreshFailed)
            };
        }
        guard.succeeded = false;
        let result =
            tokio::time::timeout(self.config.request_timeout, self.fetch(&guard.state)).await;
        if let Ok(Ok(response)) = result {
            if let Some(access) = response.access_token.filter(|t| !t.is_empty()) {
                let lifetime = response
                    .expires_in
                    .or_else(|| response.expires_at.map(|at| at.saturating_sub(unix_now())))
                    .unwrap_or(0);
                // A rotated refresh token is persisted even when the access token is
                // rejected below: the old one is already spent.
                let previous = guard.state.clone();
                if let Some(rotated) = response.refresh_token.filter(|t| !t.is_empty()) {
                    guard.state.refresh_token = rotated;
                }
                guard.state.access_token = access.clone();
                guard.state.expires_at = unix_now() + lifetime;
                if let Some(scope) = response.scope.filter(|s| !s.is_empty()) {
                    guard.state.scope = scope;
                }
                let snapshot = guard.state.clone();
                let path = self.config.state_path.clone();
                // A save failure is not fatal: the in-memory state already carries the
                // rotated token, so this process keeps working. It only costs a refresh
                // on the next start, because the state file still holds the spent token.
                match tokio::task::spawn_blocking(move || snapshot.save(&path)).await {
                    Ok(Ok(())) => {}
                    _ => eprintln!("llmux: could not persist rotated Nous refresh token"),
                }
                match make_token(&access, lifetime, self.config.refresh_skew) {
                    Ok(token) if lifetime > 0 => {
                        self.token.store(Some(Arc::new(token)));
                        self.changed.notify_one();
                        guard.succeeded = true;
                    }
                    _ => guard.state.access_token = previous.access_token,
                }
            }
        }
        self.generation.fetch_add(1, Ordering::Release);
        if guard.succeeded {
            Ok(())
        } else {
            Err(AuthError::RefreshFailed)
        }
    }

    async fn fetch(&self, state: &NousState) -> Result<TokenResponse, AuthError> {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "refresh_token")
            .append_pair("client_id", &state.client_id)
            .finish();
        let mut refresh =
            HeaderValue::from_str(&state.refresh_token).map_err(|_| AuthError::RefreshFailed)?;
        refresh.set_sensitive(true);
        let request = Request::post(self.uri.clone())
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::ACCEPT, "application/json")
            .header(header::USER_AGENT, llmux_types::USER_AGENT)
            .header("x-nous-refresh-token", refresh)
            .body(Full::new(Bytes::from(body)))
            .map_err(|_| AuthError::RefreshFailed)?;
        let response = self
            .http
            .request(request)
            .await
            .map_err(|_| AuthError::RefreshFailed)?;
        let success = response.status().is_success();
        let bytes = Limited::new(response.into_body(), 64 * 1024)
            .collect()
            .await
            .map_err(|_| AuthError::RefreshFailed)?
            .to_bytes();
        if !success {
            return Err(AuthError::RefreshFailed);
        }
        serde_json::from_slice(&bytes).map_err(|_| AuthError::RefreshFailed)
    }
}

async fn background(weak: Weak<Inner>) {
    let mut retry = Duration::from_secs(1);
    loop {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        let due = inner
            .token
            .load()
            .as_ref()
            .map(|t| t.refresh_at)
            .unwrap_or_else(Instant::now);
        tokio::select! {
            _ = tokio::time::sleep_until(due) => {},
            _ = inner.changed.notified() => { continue; },
        }
        drop(inner);
        let Some(inner) = weak.upgrade() else {
            return;
        };
        if inner
            .token
            .load()
            .as_ref()
            .is_some_and(|t| t.refresh_at > Instant::now())
        {
            continue;
        }
        if inner.refresh().await.is_err() {
            drop(inner);
            tokio::time::sleep(retry).await;
            retry = (retry * 2).min(Duration::from_secs(60));
        } else {
            retry = Duration::from_secs(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::State, routing::post, Router};
    use llmux_testkit::TestServer;
    use llmux_types::{NOUS_CLIENT_ID, NOUS_SCOPE};
    use std::sync::atomic::AtomicBool;

    #[derive(Default)]
    struct Portal {
        requests: Mutex<Vec<(String, String)>>,
        counter: AtomicU64,
        lifetime: AtomicU64,
        reject: AtomicBool,
    }

    async fn token(
        State(portal): State<Arc<Portal>>,
        headers: http::HeaderMap,
        body: String,
    ) -> (http::StatusCode, String) {
        assert_eq!(
            headers["user-agent"],
            concat!("llmux/", env!("CARGO_PKG_VERSION"))
        );
        let presented = headers["x-nous-refresh-token"].to_str().unwrap().to_owned();
        portal.requests.lock().await.push((presented, body));
        tokio::time::sleep(Duration::from_millis(30)).await;
        if portal.reject.load(Ordering::Relaxed) {
            return (
                http::StatusCode::BAD_REQUEST,
                r#"{"error":"invalid_grant","error_description":"secret"}"#.into(),
            );
        }
        let n = portal.counter.fetch_add(1, Ordering::Relaxed) + 1;
        (
            http::StatusCode::OK,
            serde_json::json!({
                "access_token": format!("access-{n}"),
                "refresh_token": format!("refresh-{n}"),
                "token_type": "Bearer",
                "expires_in": portal.lifetime.load(Ordering::Relaxed),
            })
            .to_string(),
        )
    }

    async fn fixture(
        lifetime: u64,
        expires_in: i64,
    ) -> (TestServer, Arc<Portal>, NousPortalConfig, tempfile::TempDir) {
        let portal = Arc::new(Portal::default());
        portal.lifetime.store(lifetime, Ordering::Relaxed);
        let server = TestServer::start(
            Router::new()
                .route("/api/oauth/token", post(token))
                .with_state(portal.clone()),
        )
        .await
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        NousState {
            portal_base_url: server.url(),
            client_id: NOUS_CLIENT_ID.into(),
            scope: NOUS_SCOPE.into(),
            access_token: "access-0".into(),
            refresh_token: "refresh-0".into(),
            expires_at: (unix_now() as i64 + expires_in).max(0) as u64,
        }
        .save(&path)
        .unwrap();
        (server, portal, NousPortalConfig::new(path), dir)
    }

    fn bearer(source: &NousPortalSource) -> String {
        let AuthMaterial::Header { value, .. } = source.current().unwrap() else {
            panic!("expected header")
        };
        assert!(value.is_sensitive());
        value.to_str().unwrap().to_owned()
    }

    #[tokio::test]
    async fn valid_saved_token_is_used_without_a_portal_call() {
        let (_server, portal, config, _dir) = fixture(3600, 3600).await;
        let source = NousPortalSource::start(config).await.unwrap();
        assert_eq!(bearer(&source), "Bearer access-0");
        assert!(portal.requests.lock().await.is_empty());
    }

    #[tokio::test]
    async fn refresh_rotates_sends_header_and_persists() {
        let (_server, portal, config, dir) = fixture(3600, 3600).await;
        let path = config.state_path.clone();
        let source = NousPortalSource::start(config).await.unwrap();
        source.refresh().await.unwrap();
        source.refresh().await.unwrap();
        assert_eq!(bearer(&source), "Bearer access-2");
        let requests = portal.requests.lock().await;
        assert_eq!(requests[0].0, "refresh-0");
        assert_eq!(requests[1].0, "refresh-1");
        assert_eq!(
            requests[0].1,
            "grant_type=refresh_token&client_id=hermes-cli"
        );
        assert!(!requests[0].1.contains("refresh-0"));
        let saved = NousState::load(&path).unwrap();
        assert_eq!(saved.refresh_token, "refresh-2");
        assert_eq!(saved.access_token, "access-2");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn concurrent_refreshes_hit_the_portal_once() {
        let (_server, portal, config, _dir) = fixture(3600, 3600).await;
        let source = Arc::new(NousPortalSource::start(config).await.unwrap());
        let barrier = Arc::new(tokio::sync::Barrier::new(20));
        let tasks: Vec<_> = (0..20)
            .map(|_| {
                let (source, barrier) = (source.clone(), barrier.clone());
                tokio::spawn(async move {
                    barrier.wait().await;
                    source.refresh().await
                })
            })
            .collect();
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        assert_eq!(portal.requests.lock().await.len(), 1);
        assert_eq!(bearer(&source), "Bearer access-1");
    }

    #[tokio::test]
    async fn expired_saved_token_is_refreshed_at_start_and_again_after_it_lapses() {
        let (_server, portal, config, _dir) = fixture(2, -10).await;
        let source = NousPortalSource::start(config).await.unwrap();
        assert_eq!(bearer(&source), "Bearer access-1");
        assert_eq!(portal.requests.lock().await.len(), 1);
        tokio::time::timeout(Duration::from_secs(5), async {
            while portal.requests.lock().await.len() < 2 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(portal.requests.lock().await[1].0, "refresh-1");
    }

    #[tokio::test]
    async fn rejected_refresh_fails_closed_without_leaking_the_body() {
        let (_server, portal, config, _dir) = fixture(3600, -10).await;
        portal.reject.store(true, Ordering::Relaxed);
        assert!(matches!(
            NousPortalSource::start(config).await,
            Err(AuthError::RefreshFailed)
        ));
    }

    #[tokio::test]
    async fn missing_or_insecure_state_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let missing = NousPortalConfig::new(dir.path().join("none.json"));
        assert!(matches!(
            NousPortalSource::start(missing).await,
            Err(AuthError::Unavailable)
        ));
        let path = dir.path().join("state.json");
        NousState {
            portal_base_url: "http://example.com".into(),
            client_id: NOUS_CLIENT_ID.into(),
            scope: NOUS_SCOPE.into(),
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: 0,
        }
        .save(&path)
        .unwrap();
        assert!(matches!(
            NousPortalSource::start(NousPortalConfig::new(path)).await,
            Err(AuthError::RefreshFailed)
        ));
    }
}
