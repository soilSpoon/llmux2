//! OAuth 2.0 refresh-token grant credentials (RFC 6749 section 6).

use arc_swap::ArcSwapOption;
use async_trait::async_trait;
use bytes::Bytes;
use http::{header, HeaderValue, Request, Uri};
use http_body_util::{BodyExt, Full, Limited};
use llmux_types::{
    http_client, is_https_or_loopback_http, AuthError, AuthMaterial, CredentialSource, HttpClient,
};
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Weak,
    },
    time::Duration,
};
use tokio::{
    sync::{Mutex, Notify},
    task::JoinHandle,
    time::Instant,
};

/// Secrets are intentionally not Debug. No endpoint response is included in errors.
pub struct OAuthConfig {
    pub token_url: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub refresh_token: String,
    pub scopes: Option<Vec<String>>,
    /// Additional form fields; standard OAuth refresh fields cannot be overridden.
    pub extra_params: Vec<(String, String)>,
    /// Token response field used as the bearer credential. Defaults to `access_token`;
    /// OIDC issuers may require `id_token` instead.
    pub token_field: String,
    /// Best-effort atomic storage for rotated refresh tokens. Failures retain the
    /// in-memory token, but the application may need to reauthorize after restart.
    pub refresh_token_path: Option<PathBuf>,
    pub refresh_skew: Duration,
    pub request_timeout: Duration,
}

impl OAuthConfig {
    pub fn new(token_url: String, client_id: String, refresh_token: String) -> Self {
        Self {
            token_url,
            client_id,
            refresh_token,
            client_secret: None,
            scopes: None,
            extra_params: Vec::new(),
            token_field: "access_token".into(),
            refresh_token_path: None,
            refresh_skew: Duration::from_secs(60),
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
    token_type: Option<String>,
    expires_in: Option<u64>,
    refresh_token: Option<String>,
    error: Option<String>,
    /// Non-standard fields (for example OIDC `id_token`); the configured
    /// `token_field` is looked up here when it is not `access_token`.
    #[serde(flatten)]
    extra: std::collections::HashMap<String, serde_json::Value>,
}

struct RefreshState {
    refresh_token: String,
    succeeded: bool,
}

struct Inner {
    config: OAuthConfig,
    uri: Uri,
    http: HttpClient,
    token: ArcSwapOption<Token>,
    generation: AtomicU64,
    state: Mutex<RefreshState>,
    changed: Notify,
}

/// A cached credential source. `start` fetches the initial token; subsequent
/// automatic refreshes run only in a background task. Dropping it aborts that task.
pub struct OAuthCredentialSource {
    inner: Arc<Inner>,
    task: JoinHandle<()>,
}

impl OAuthCredentialSource {
    pub async fn start(mut config: OAuthConfig) -> Result<Self, AuthError> {
        let uri: Uri = config
            .token_url
            .parse()
            .map_err(|_| AuthError::RefreshFailed)?;
        if uri.authority().is_none()
            || uri.authority().is_some_and(|a| a.as_str().contains('@'))
            || !is_https_or_loopback_http(&uri)
            || config.request_timeout.is_zero()
            || config.extra_params.iter().any(|(key, _)| {
                matches!(
                    key.as_str(),
                    "grant_type" | "client_id" | "client_secret" | "refresh_token" | "scope"
                )
            })
        {
            return Err(AuthError::RefreshFailed);
        }
        let refresh_token = std::mem::take(&mut config.refresh_token);
        let inner = Arc::new(Inner {
            config,
            uri,
            http: http_client(),
            token: ArcSwapOption::empty(),
            generation: AtomicU64::new(0),
            changed: Notify::new(),
            state: Mutex::new(RefreshState {
                refresh_token,
                succeeded: false,
            }),
        });
        inner.refresh().await?;
        let task = tokio::spawn(background(Arc::downgrade(&inner)));
        Ok(Self { inner, task })
    }
}

impl Drop for OAuthCredentialSource {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[async_trait]
impl CredentialSource for OAuthCredentialSource {
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

impl Inner {
    async fn refresh(self: &Arc<Self>) -> Result<(), AuthError> {
        let generation = self.generation.load(Ordering::Acquire);
        let inner = self.clone();
        tokio::spawn(async move { inner.refresh_generation(generation).await })
            .await
            .map_err(|_| AuthError::RefreshFailed)?
    }

    async fn refresh_generation(&self, generation: u64) -> Result<(), AuthError> {
        let mut state = self.state.lock().await;
        if self.generation.load(Ordering::Acquire) != generation {
            return if state.succeeded {
                Ok(())
            } else {
                Err(AuthError::RefreshFailed)
            };
        }
        let result = tokio::time::timeout(
            self.config.request_timeout,
            self.fetch(&state.refresh_token),
        )
        .await;
        state.succeeded = false;
        if let Ok(Ok((mut response, started, success))) = result {
            if let Some(rotated) = response.refresh_token.take().filter(|t| !t.is_empty()) {
                state.refresh_token = rotated;
                if let Some(path) = self.config.refresh_token_path.clone() {
                    let token = state.refresh_token.clone();
                    let _ = tokio::task::spawn_blocking(move || persist_token(&path, &token)).await;
                }
            }
            if let Ok(token) = self.validate_response(response, started, success) {
                self.token.store(Some(Arc::new(token)));
                self.changed.notify_one();
                state.succeeded = true;
            }
        }
        self.generation.fetch_add(1, Ordering::Release);
        if state.succeeded {
            Ok(())
        } else {
            Err(AuthError::RefreshFailed)
        }
    }

    async fn fetch(
        &self,
        refresh_token: &str,
    ) -> Result<(TokenResponse, Instant, bool), AuthError> {
        let mut fields = vec![
            ("grant_type", "refresh_token"),
            ("client_id", &self.config.client_id),
            ("refresh_token", refresh_token),
        ];
        if let Some(secret) = &self.config.client_secret {
            fields.push(("client_secret", secret));
        }
        let scope = self.config.scopes.as_ref().map(|s| s.join(" "));
        if let Some(scope) = &scope {
            fields.push(("scope", scope));
        }
        fields.extend(
            self.config
                .extra_params
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        );
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields)
            .finish();
        let request = Request::post(self.uri.clone())
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::USER_AGENT, llmux_types::USER_AGENT)
            .body(Full::new(Bytes::from(body)))
            .map_err(|_| AuthError::RefreshFailed)?;
        let started = Instant::now();
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
        let response: TokenResponse =
            serde_json::from_slice(&bytes).map_err(|_| AuthError::RefreshFailed)?;
        Ok((response, started, success))
    }

    fn validate_response(
        &self,
        response: TokenResponse,
        started: Instant,
        success: bool,
    ) -> Result<Token, AuthError> {
        if !success
            || response.error.is_some()
            || !response
                .token_type
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case("Bearer"))
        {
            return Err(AuthError::RefreshFailed);
        }
        let access = if self.config.token_field == "access_token" {
            response.access_token
        } else {
            response
                .extra
                .get(&self.config.token_field)
                .and_then(|value| value.as_str().map(str::to_owned))
        }
        .filter(|t| !t.is_empty())
        .ok_or(AuthError::RefreshFailed)?;
        let mut value = HeaderValue::from_str(&format!("Bearer {access}"))
            .map_err(|_| AuthError::RefreshFailed)?;
        value.set_sensitive(true);
        let lifetime = Duration::from_secs(response.expires_in.unwrap_or(3600));
        let expires = started
            .checked_add(lifetime)
            .filter(|t| *t > Instant::now())
            .ok_or(AuthError::RefreshFailed)?;
        // For short-lived tokens, avoid an immediate successful-refresh loop.
        let skew = self.config.refresh_skew.min(lifetime / 2);
        Ok(Token {
            auth: AuthMaterial::Header {
                name: header::AUTHORIZATION,
                value,
            },
            expires,
            refresh_at: expires - skew,
        })
    }
}

fn persist_token(path: &Path, token: &str) -> std::io::Result<()> {
    llmux_types::atomic_write(path, token.as_bytes())
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
    use std::sync::atomic::AtomicBool;

    #[derive(Default)]
    struct Endpoint {
        bodies: Mutex<Vec<String>>,
        fail: AtomicBool,
        lifetime: AtomicU64,
        mode: AtomicU64,
        release: Notify,
    }

    async fn endpoint(
        State(state): State<Arc<Endpoint>>,
        headers: http::HeaderMap,
        body: String,
    ) -> (http::StatusCode, String) {
        assert_eq!(
            headers[header::CONTENT_TYPE],
            "application/x-www-form-urlencoded"
        );
        state.bodies.lock().await.push(body);
        let mode = state.mode.load(Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(30)).await;
        if state.fail.load(Ordering::Relaxed) {
            return (
                http::StatusCode::BAD_REQUEST,
                r#"{"error":"invalid_grant","error_description":"secret"}"#.into(),
            );
        }
        let body = match mode {
            1 => "x".repeat(65537),
            2 => "{}".into(),
            3 => r#"{"access_token":"access","token_type":"Bearer"}"#.into(),
            4 => {
                state.release.notified().await;
                r#"{"access_token":"access","token_type":"Bearer","refresh_token":"cancel-rotated"}"#.into()
            }
            5 => r#"{"access_token":"access","token_type":"Bearer","expires_in":0,"refresh_token":"rejected-rotated"}"#.into(),
            6 => r#"{"access_token":"","token_type":"Bearer","refresh_token":"rejected-rotated"}"#.into(),
            7 => r#"{"access_token":"access","token_type":"unsupported","refresh_token":"rejected-rotated"}"#.into(),
            _ => serde_json::json!({"access_token":"access", "token_type":"Bearer", "expires_in":state.lifetime.load(Ordering::Relaxed), "refresh_token":"rotated"}).to_string(),
        };
        (http::StatusCode::OK, body)
    }

    async fn fixture(lifetime: u64) -> (TestServer, Arc<Endpoint>, OAuthConfig) {
        let state = Arc::new(Endpoint::default());
        state.lifetime.store(lifetime, Ordering::Relaxed);
        let server = TestServer::start(
            Router::new()
                .route("/token", post(endpoint))
                .with_state(state.clone()),
        )
        .await
        .unwrap();
        let mut config = OAuthConfig::new(
            format!("{}/token", server.url()),
            "id +&".into(),
            "refresh/secret".into(),
        );
        config.client_secret = Some("secret=&".into());
        config.scopes = Some(vec!["one".into(), "two".into()]);
        (server, state, config)
    }

    #[tokio::test]
    async fn accepts_case_insensitive_localhost_endpoint() {
        let (_server, endpoint, mut config) = fixture(3600).await;
        config.token_url = config.token_url.replace("127.0.0.1", "LOCALHOST");
        let source = OAuthCredentialSource::start(config).await.unwrap();
        assert!(source.current().is_ok());
        assert_eq!(endpoint.bodies.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn extra_form_parameters_and_collisions() {
        let (_server, endpoint, mut config) = fixture(3600).await;
        config.scopes = None;
        config.extra_params = vec![("resource".into(), "https://api.openai.com/v1".into())];
        let source = OAuthCredentialSource::start(config).await.unwrap();
        source.refresh().await.unwrap();
        for body in endpoint.bodies.lock().await.iter() {
            assert!(body.ends_with("&resource=https%3A%2F%2Fapi.openai.com%2Fv1"));
            assert!(!body.contains("scope="));
        }
        for key in [
            "grant_type",
            "client_id",
            "client_secret",
            "refresh_token",
            "scope",
        ] {
            let mut config = OAuthConfig::new(
                format!("{}/token", _server.url()),
                "id".into(),
                "refresh".into(),
            );
            config.extra_params = vec![(key.into(), "override".into())];
            assert!(matches!(
                OAuthCredentialSource::start(config).await,
                Err(AuthError::RefreshFailed)
            ));
        }
        assert_eq!(endpoint.bodies.lock().await.len(), 2);
    }

    #[tokio::test]
    async fn rotation_is_persisted_atomically_with_private_permissions() {
        let (_server, endpoint, mut config) = fixture(3600).await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("refresh");
        std::fs::write(&path, "old").unwrap();
        config.refresh_token_path = Some(path.clone());
        let source = OAuthCredentialSource::start(config).await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "rotated");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        endpoint.mode.store(5, Ordering::Relaxed);
        assert!(matches!(
            source.refresh().await,
            Err(AuthError::RefreshFailed)
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "rejected-rotated");
        endpoint.mode.store(3, Ordering::Relaxed);
        source.refresh().await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "rejected-rotated");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn persistence_failure_retains_in_memory_rotation() {
        let (_server, endpoint, mut config) = fixture(3600).await;
        let dir = tempfile::tempdir().unwrap();
        config.refresh_token_path = Some(dir.path().join("missing").join("refresh"));
        let source = OAuthCredentialSource::start(config).await.unwrap();
        source.refresh().await.unwrap();
        assert!(endpoint.bodies.lock().await[1].contains("refresh_token=rotated"));
        assert!(source.current().is_ok());
    }

    #[tokio::test]
    async fn initial_form_rotation_and_single_flight() {
        let (_server, endpoint, config) = fixture(3600).await;
        let source = Arc::new(OAuthCredentialSource::start(config).await.unwrap());
        let AuthMaterial::Header { value, .. } = source.current().unwrap() else {
            panic!("expected header")
        };
        assert_eq!(value, "Bearer access");
        assert!(value.is_sensitive());
        assert_eq!(endpoint.bodies.lock().await[0], "grant_type=refresh_token&client_id=id+%2B%26&refresh_token=refresh%2Fsecret&client_secret=secret%3D%26&scope=one+two");
        let mut tasks = Vec::new();
        let barrier = Arc::new(tokio::sync::Barrier::new(20));
        for _ in 0..20 {
            let source = source.clone();
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                source.refresh().await
            }));
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        let bodies = endpoint.bodies.lock().await;
        assert_eq!(bodies.len(), 2);
        assert!(bodies[1].contains("refresh_token=rotated"));
    }

    #[tokio::test]
    async fn cancelled_caller_preserves_rotated_refresh_token() {
        let (_server, endpoint, config) = fixture(3600).await;
        let source = Arc::new(OAuthCredentialSource::start(config).await.unwrap());
        endpoint.mode.store(4, Ordering::Relaxed);
        let caller_source = source.clone();
        let caller = tokio::spawn(async move { caller_source.refresh().await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while endpoint.bodies.lock().await.len() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        endpoint.release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            while source.inner.generation.load(Ordering::Acquire) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        endpoint.mode.store(0, Ordering::Relaxed);
        source.refresh().await.unwrap();
        let bodies = endpoint.bodies.lock().await;
        assert_eq!(bodies.len(), 3);
        assert!(bodies[2].contains("refresh_token=cancel-rotated"));
    }

    #[tokio::test]
    async fn rejected_access_tokens_preserve_rotated_refresh_token() {
        for mode in [5, 6, 7] {
            let (_server, endpoint, config) = fixture(3600).await;
            let source = OAuthCredentialSource::start(config).await.unwrap();
            endpoint.mode.store(mode, Ordering::Relaxed);
            assert!(matches!(
                source.refresh().await,
                Err(AuthError::RefreshFailed)
            ));
            endpoint.mode.store(0, Ordering::Relaxed);
            source.refresh().await.unwrap();
            let bodies = endpoint.bodies.lock().await;
            assert_eq!(bodies.len(), 3);
            assert!(bodies[2].contains("refresh_token=rejected-rotated"));
        }
    }

    #[tokio::test]
    async fn background_refreshes_and_stops_on_drop() {
        let (_server, endpoint, mut config) = fixture(2).await;
        config.refresh_skew = Duration::from_secs(1);
        let source = OAuthCredentialSource::start(config).await.unwrap();
        tokio::time::timeout(Duration::from_millis(1800), async {
            while endpoint.bodies.lock().await.len() < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(source.current().is_ok());
        drop(source);
        let hits = endpoint.bodies.lock().await.len();
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(endpoint.bodies.lock().await.len(), hits);
    }

    #[tokio::test]
    async fn failed_refresh_expires_and_retries() {
        let (_server, endpoint, config) = fixture(1).await;
        let source = OAuthCredentialSource::start(config).await.unwrap();
        endpoint.fail.store(true, Ordering::Relaxed);
        assert!(matches!(
            source.refresh().await,
            Err(AuthError::RefreshFailed)
        ));
        assert!(source.current().is_ok());
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(matches!(source.current(), Err(AuthError::Unavailable)));
        endpoint.fail.store(false, Ordering::Relaxed);
        tokio::time::timeout(Duration::from_secs(3), async {
            while source.current().is_err() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn rejects_errors_empty_oversized_and_insecure_endpoints() {
        let (_server, endpoint, config) = fixture(3600).await;
        endpoint.fail.store(true, Ordering::Relaxed);
        assert!(matches!(
            OAuthCredentialSource::start(config).await,
            Err(AuthError::RefreshFailed)
        ));
        endpoint.fail.store(false, Ordering::Relaxed);
        for mode in [1, 2] {
            endpoint.mode.store(mode, Ordering::Relaxed);
            let config = OAuthConfig::new(
                format!("{}/token", _server.url()),
                "id".into(),
                "refresh".into(),
            );
            assert!(matches!(
                OAuthCredentialSource::start(config).await,
                Err(AuthError::RefreshFailed)
            ));
        }
        let config = OAuthConfig::new(
            "http://example.com/token".into(),
            "id".into(),
            "refresh".into(),
        );
        assert!(matches!(
            OAuthCredentialSource::start(config).await,
            Err(AuthError::RefreshFailed)
        ));
    }

    #[tokio::test]
    async fn default_expiry_and_timeout() {
        let (_server, endpoint, config) = fixture(3600).await;
        endpoint.mode.store(3, Ordering::Relaxed);
        let source = OAuthCredentialSource::start(config).await.unwrap();
        assert!(
            source.inner.token.load().as_ref().unwrap().expires
                > Instant::now() + Duration::from_secs(3500)
        );
        source.inner.token.store(None);
        assert!(matches!(source.current(), Err(AuthError::Unavailable)));
        let mut config = OAuthConfig::new(
            format!("{}/token", _server.url()),
            "id".into(),
            "refresh".into(),
        );
        config.request_timeout = Duration::from_millis(1);
        assert!(matches!(
            OAuthCredentialSource::start(config).await,
            Err(AuthError::RefreshFailed)
        ));
    }
}
