use super::*;
use async_trait::async_trait;
use http::{HeaderMap, Request, Response};
use hyper::body::Incoming;
use llmux_types::{
    AuthError, AuthMaterial, BuildError, CanonicalModel, Capabilities, InboundFormat, ProviderKind,
    RequestId, Strategy,
};
use std::{collections::VecDeque, sync::Mutex};

struct Credential {
    refreshes: AtomicUsize,
    fail: bool,
}

#[async_trait]
impl CredentialSource for Credential {
    fn current(&self) -> Result<AuthMaterial, AuthError> {
        Ok(AuthMaterial::Bearer(
            self.refreshes.load(Ordering::Relaxed).to_string().into(),
        ))
    }

    async fn refresh(&self) -> Result<(), AuthError> {
        self.refreshes.fetch_add(1, Ordering::Relaxed);
        if self.fail {
            Err(AuthError::RefreshFailed)
        } else {
            Ok(())
        }
    }
}

struct Adapter {
    kind: ProviderKind,
    errors: Mutex<VecDeque<UpstreamError>>,
    calls: Mutex<Vec<(Arc<str>, Arc<str>)>>,
}

#[async_trait]
impl ProviderAdapter for Adapter {
    fn kind(&self) -> &ProviderKind {
        &self.kind
    }

    fn supports(&self, _: InboundFormat) -> Support {
        Support::Passthrough
    }

    async fn build_request(
        &self,
        _: &GatewayRequest,
        dep: &Deployment,
        auth: &AuthMaterial,
    ) -> Result<Request<Bytes>, BuildError> {
        let AuthMaterial::Bearer(token) = auth else {
            panic!("expected bearer")
        };
        self.calls
            .lock()
            .unwrap()
            .push((dep.id.clone(), token.clone()));
        Ok(Request::post(dep.base_url.clone()).body(Bytes::new())?)
    }

    async fn parse_response(
        &self,
        _: &GatewayRequest,
        _: &Deployment,
        _: Response<Incoming>,
    ) -> Result<GatewayResponse, UpstreamError> {
        if let Some(error) = self.errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        Ok(GatewayResponse::Complete {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Bytes::new(),
            usage: None,
        })
    }
}

async fn fixture(
    errors: Vec<UpstreamError>,
    refresh_fails: bool,
) -> (
    Executor,
    Arc<Adapter>,
    Arc<Credential>,
    llmux_testkit::TestServer,
) {
    let server = llmux_testkit::TestServer::start(axum::Router::new().fallback(|| async { "ok" }))
        .await
        .unwrap();
    let adapter = Arc::new(Adapter {
        kind: ProviderKind("test".into()),
        errors: Mutex::new(errors.into()),
        calls: Mutex::new(Vec::new()),
    });
    let credential = Arc::new(Credential {
        refreshes: AtomicUsize::new(0),
        fail: refresh_fails,
    });
    let deployments = (0..3)
        .map(|i| Deployment {
            idx: DeploymentIdx(i),
            id: format!("dep{i}").into(),
            provider: adapter.kind.clone(),
            credential: if i < 2 {
                "first".into()
            } else {
                "second".into()
            },
            base_url: server.url().parse().unwrap(),
            upstream_model: "upstream".into(),
            priority: i as i32,
            weight: 1,
            billing: Default::default(),
            quota: Default::default(),
            timeout: Duration::from_secs(1),
        })
        .collect();
    let cfg = RuntimeConfig {
        server: llmux_config::ServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            client_keys: Box::new([]),
            request_timeout: Duration::from_secs(5),
            expose_routing_headers: true,
        },
        routing: llmux_config::RoutingConfig {
            max_attempts: 4,
            fallback_on: Box::new([ErrorKind::RateLimited, ErrorKind::QuotaExhausted]),
        },
        cooldown: llmux_config::CooldownConfig {
            default: Duration::from_secs(1),
            max: Duration::from_secs(5),
        },
        api_keys: HashMap::new(),
        oauth: HashMap::new(),
        model_names: HashMap::from([("model".into(), 0)]),
        models: Box::new([CanonicalModel {
            name: "model".into(),
            strategy: Strategy::Priority,
            capabilities: Capabilities::default(),
            deployments: Box::new([DeploymentIdx(0), DeploymentIdx(1), DeploymentIdx(2)]),
        }]),
        deployments,
    };
    let mut registry = ProviderRegistry::default();
    registry.register(adapter.clone());
    let credentials: HashMap<Arc<str>, Arc<dyn CredentialSource>> = HashMap::from([
        (
            Arc::from("first"),
            credential.clone() as Arc<dyn CredentialSource>,
        ),
        (
            Arc::from("second"),
            Arc::new(Credential {
                refreshes: AtomicUsize::new(0),
                fail: false,
            }),
        ),
    ]);
    (
        Executor::new(Arc::new(cfg), &registry, &credentials).unwrap(),
        adapter,
        credential,
        server,
    )
}

fn request() -> GatewayRequest {
    GatewayRequest {
        format: InboundFormat::OpenAiChat,
        model: "model".into(),
        stream: false,
        body: Bytes::new(),
        parsed: Default::default(),
        est_input_tokens: 0,
        request_id: RequestId("test".into()),
    }
}

#[tokio::test]
async fn refresh_once_then_success() {
    let (executor, adapter, credential, _server) =
        fixture(vec![UpstreamError::Auth { refreshable: true }], false).await;
    let response = executor.execute(request()).await.unwrap();
    assert_eq!(response.deployment.as_ref(), "dep0");
    assert_eq!(response.attempts, 2);
    assert_eq!(credential.refreshes.load(Ordering::Relaxed), 1);
    assert_eq!(
        *adapter.calls.lock().unwrap(),
        vec![("dep0".into(), "0".into()), ("dep0".into(), "1".into())]
    );
}

#[tokio::test]
async fn refreshed_retry_obeys_rpm_limit() {
    let (mut executor, adapter, credential, _server) =
        fixture(vec![UpstreamError::Auth { refreshable: true }], false).await;
    executor.bindings[0].limits = Arc::new(DeploymentLimits::new(llmux_types::QuotaSpec {
        rpm: Some(1),
        ..Default::default()
    }));
    let response = executor.execute(request()).await.unwrap();
    assert_eq!(response.deployment.as_ref(), "dep1");
    assert_eq!(response.attempts, 2);
    assert_eq!(credential.refreshes.load(Ordering::Relaxed), 1);
    assert_eq!(
        *adapter.calls.lock().unwrap(),
        vec![("dep0".into(), "0".into()), ("dep1".into(), "1".into())]
    );
}

#[tokio::test]
async fn refreshed_retry_releases_previous_concurrency_permit() {
    let (mut executor, _, _, _server) =
        fixture(vec![UpstreamError::Auth { refreshable: true }], false).await;
    executor.bindings[0].limits = Arc::new(DeploymentLimits::new(llmux_types::QuotaSpec {
        concurrent: Some(1),
        ..Default::default()
    }));
    let response = executor.execute(request()).await.unwrap();
    assert_eq!(response.deployment.as_ref(), "dep0");
    assert_eq!(response.attempts, 2);
}

#[tokio::test]
async fn failed_auth_disables_shared_credential_and_falls_back() {
    for refreshable in [true, false] {
        let (executor, adapter, credential, _server) = fixture(
            vec![
                UpstreamError::Auth { refreshable: true },
                UpstreamError::Auth { refreshable },
            ],
            false,
        )
        .await;
        let response = executor.execute(request()).await.unwrap();
        assert_eq!(response.deployment.as_ref(), "dep2");
        assert_eq!(response.attempts, 3);
        assert_eq!(credential.refreshes.load(Ordering::Relaxed), 1);
        assert_eq!(
            executor
                .execute(request())
                .await
                .unwrap()
                .deployment
                .as_ref(),
            "dep2"
        );
        assert_eq!(adapter.calls.lock().unwrap().len(), 4);
    }
}

#[tokio::test]
async fn failed_refresh_disables_credential() {
    let (executor, _, credential, _server) =
        fixture(vec![UpstreamError::Auth { refreshable: true }], true).await;
    let response = executor.execute(request()).await.unwrap();
    assert_eq!(response.deployment.as_ref(), "dep2");
    assert_eq!(response.attempts, 2);
    assert_eq!(credential.refreshes.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn transient_errors_fall_back() {
    for error in [
        UpstreamError::Server {
            status: StatusCode::BAD_GATEWAY,
        },
        UpstreamError::Timeout,
        UpstreamError::Connect,
    ] {
        let (executor, _, credential, _server) = fixture(vec![error], false).await;
        let response = executor.execute(request()).await.unwrap();
        assert_eq!(response.deployment.as_ref(), "dep1");
        assert_eq!(response.attempts, 2);
        assert_eq!(credential.refreshes.load(Ordering::Relaxed), 0);
    }
}

#[tokio::test]
async fn exhausted_transient_errors_return_last_error() {
    let (executor, _, _, _server) = fixture(
        vec![
            UpstreamError::Server {
                status: StatusCode::BAD_GATEWAY,
            },
            UpstreamError::Connect,
            UpstreamError::Timeout,
        ],
        false,
    )
    .await;
    assert!(matches!(
        executor.execute(request()).await,
        Err(GatewayError::Client(UpstreamError::Timeout))
    ));
}

#[tokio::test]
async fn exhaustion_after_transient_then_quota_returns_limits() {
    let (executor, _, _, _server) = fixture(
        vec![
            UpstreamError::Timeout,
            UpstreamError::QuotaExhausted { reset_at: None },
            UpstreamError::QuotaExhausted { reset_at: None },
        ],
        false,
    )
    .await;
    assert!(matches!(
        executor.execute(request()).await,
        Err(GatewayError::AllCandidatesExhausted { attempts: 3, .. })
    ));
}

#[tokio::test]
async fn client_error_does_not_fall_back() {
    let (executor, adapter, _, _server) = fixture(
        vec![UpstreamError::BadRequest {
            status: StatusCode::BAD_REQUEST,
            body: Bytes::new(),
        }],
        false,
    )
    .await;
    assert!(matches!(
        executor.execute(request()).await,
        Err(GatewayError::Client(UpstreamError::BadRequest { .. }))
    ));
    assert_eq!(adapter.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn no_errors_succeeds_without_refresh() {
    let (executor, _, credential, _server) = fixture(vec![], false).await;
    assert_eq!(executor.execute(request()).await.unwrap().attempts, 1);
    assert_eq!(credential.refreshes.load(Ordering::Relaxed), 0);
}
