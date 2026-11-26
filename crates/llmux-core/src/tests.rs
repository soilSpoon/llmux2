use super::*;
use async_trait::async_trait;
use http::{HeaderMap, Request, Response};
use hyper::body::Incoming;
use llmux_types::{
    AuthError, AuthMaterial, Billing, BuildError, CanonicalModel, Capabilities, ContextOverflow,
    InboundFormat, LimitObservation, ProviderKind, RequestId, SseEvent,
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
    build_errors: Mutex<VecDeque<BuildError>>,
    observed: Mutex<Option<LimitObservation>>,
    status: Arc<std::sync::atomic::AtomicU16>,
    stream_errors: Mutex<VecDeque<UpstreamError>>,
    calls: Mutex<Vec<(Arc<str>, Arc<str>)>>,
    cool_on_build: Mutex<Option<Arc<DeploymentLimits>>>,
    cool_on_repair: Mutex<Option<Arc<DeploymentLimits>>>,
    wire_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ProviderAdapter for Adapter {
    fn kind(&self) -> &ProviderKind {
        &self.kind
    }

    fn observe_limits(&self, _: &HeaderMap) -> Option<LimitObservation> {
        self.observed.lock().unwrap().take()
    }

    fn supports(&self, format: InboundFormat) -> Support {
        match format {
            InboundFormat::AnthropicMessages => Support::Convert,
            _ => Support::Passthrough,
        }
    }

    async fn build_request(
        &self,
        req: &GatewayRequest,
        dep: &Deployment,
        auth: &AuthMaterial,
    ) -> Result<Request<Bytes>, BuildError> {
        if let Some(error) = self.build_errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        if req.format == InboundFormat::AnthropicMessages
            && (req.body == "document" || req.body == "image" && dep.idx == DeploymentIdx(2))
        {
            return Err(BuildError::UnsupportedConversion);
        }
        if let Some(limits) = self.cool_on_build.lock().unwrap().take() {
            limits.cool_down(Duration::from_secs(30));
        }
        let token = match auth {
            AuthMaterial::Bearer(token) => token.clone(),
            AuthMaterial::None => "anon".into(),
            _ => panic!("expected bearer or anonymous auth"),
        };
        self.calls
            .lock()
            .unwrap()
            .push((dep.id.clone(), token.clone()));
        Ok(Request::post(dep.base_url.clone()).body(Bytes::new())?)
    }

    async fn parse_response(
        &self,
        req: &GatewayRequest,
        _: &Deployment,
        _: Response<Incoming>,
    ) -> Result<GatewayResponse, UpstreamError> {
        if let Some(error) = self.errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        if req.stream {
            let event = SseEvent {
                event: None,
                data: "{}".into(),
            };
            let tail = self.stream_errors.lock().unwrap().pop_front();
            return Ok(GatewayResponse::Stream(Box::pin(
                futures_util::stream::iter(std::iter::once(Ok(event)).chain(tail.map(Err))),
            )));
        }
        Ok(GatewayResponse::Complete {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        })
    }

    fn repair_rejected(&self, _: &Bytes, _: &UpstreamError) -> Option<Bytes> {
        let limits = self.cool_on_repair.lock().unwrap().take()?;
        limits.cool_down(Duration::from_secs(30));
        Some(Bytes::from_static(b"repaired"))
    }
}

type Fixture = (
    Executor,
    Arc<Adapter>,
    Arc<Credential>,
    llmux_testkit::TestServer,
);

async fn fixture(errors: Vec<UpstreamError>, refresh_fails: bool) -> Fixture {
    fixture_with_context(errors, refresh_fails, [None; 3]).await
}

async fn fixture_with_context(
    errors: Vec<UpstreamError>,
    refresh_fails: bool,
    max_context: [Option<u32>; 3],
) -> Fixture {
    fixture_with_models(
        errors,
        refresh_fails,
        max_context,
        &[("model", &[0, 1, 2])],
        Default::default(),
    )
    .await
}

async fn fixture_with_models(
    errors: Vec<UpstreamError>,
    refresh_fails: bool,
    max_context: [Option<u32>; 3],
    models: &[(&str, &[u32])],
    shared_quota: llmux_types::QuotaSpec,
) -> Fixture {
    fixture_full(
        errors,
        refresh_fails,
        max_context,
        models,
        shared_quota,
        Default::default(),
        false,
        None,
        [None; 3],
        [None; 3],
    )
    .await
}

/// `second_quota` is the quota of credential "second", used only by dep2.
#[allow(clippy::too_many_arguments)]
async fn fixture_full(
    errors: Vec<UpstreamError>,
    refresh_fails: bool,
    max_context: [Option<u32>; 3],
    models: &[(&str, &[u32])],
    shared_quota: llmux_types::QuotaSpec,
    second_quota: llmux_types::QuotaSpec,
    split_routes: bool,
    request_log: Option<std::path::PathBuf>,
    expiry: [Option<SystemTime>; 3],
    boost: [Option<(i32, SystemTime)>; 3],
) -> Fixture {
    let status = Arc::new(std::sync::atomic::AtomicU16::new(200));
    let served_status = status.clone();
    let wire_calls = Arc::new(AtomicUsize::new(0));
    let received = wire_calls.clone();
    let server = llmux_testkit::TestServer::start(axum::Router::new().fallback(move || {
        received.fetch_add(1, Ordering::Relaxed);
        let status = StatusCode::from_u16(served_status.load(Ordering::Relaxed)).unwrap();
        async move { (status, [("request-id", "req_fixture_1")], "ok") }
    }))
    .await
    .unwrap();
    let adapter = Arc::new(Adapter {
        kind: ProviderKind("test".into()),
        errors: Mutex::new(errors.into()),
        build_errors: Mutex::new(VecDeque::new()),
        observed: Mutex::new(None),
        status,
        stream_errors: Mutex::new(VecDeque::new()),
        calls: Mutex::new(Vec::new()),
        cool_on_build: Mutex::new(None),
        cool_on_repair: Mutex::new(None),
        wire_calls,
    });
    let credential = Arc::new(Credential {
        refreshes: AtomicUsize::new(0),
        fail: refresh_fails,
    });
    let deployments = (0..3)
        .map(|i| Deployment {
            headers: Default::default(),
            fallback: false,
            idx: DeploymentIdx(i),
            id: format!("dep{i}").into(),
            provider: adapter.kind.clone(),
            credential: if i < 2 {
                "first".into()
            } else {
                "second".into()
            },
            base_url: server.url().parse().unwrap(),
            upstream_model: if split_routes && i == 1 {
                "upstream-1"
            } else {
                "upstream"
            }
            .into(),
            strip_request_fields: Box::default(),
            priority: i as i32,
            quota: if i < 2 {
                shared_quota.clone()
            } else {
                Default::default()
            },
            shared_quota: None,
            timeout: Duration::from_secs(1),
            max_context: max_context[i as usize],
            expires_at: expiry[i as usize],
            priority_boost: boost[i as usize].map(|(priority, _)| priority),
            priority_boost_until: boost[i as usize].map(|(_, until)| until),
            billing: Billing::Billed,
        })
        .collect();
    let cfg = RuntimeConfig {
        model_allowed_keys: Default::default(),
        credential_disabled: Default::default(),
        server: llmux_config::ServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            client_keys: Box::new([]),
            key_policies: Box::new([]),
            deny: Box::new([]),
            request_timeout: Duration::from_secs(5),
            max_request_bytes: 32 * 1024 * 1024,
            shutdown_timeout: Duration::from_secs(60),
            drain_grace: Duration::from_secs(2),
            expose_routing_headers: true,
            dashboard_key: None,
            request_log,
            end_user_header: None,
            end_user_salt: None,
            end_user_from_body: false,
            remote_images: llmux_config::RemoteImages::Allow,
            max_image_bytes: None,
        },
        routing: llmux_config::RoutingConfig {
            max_attempts: 4,
            fallback_on: Box::new([ErrorKind::RateLimited, ErrorKind::QuotaExhausted]),
        },
        cooldown: llmux_config::CooldownConfig {
            default: Duration::from_secs(1),
            max: Duration::from_secs(5),
            quota_max: Duration::from_secs(10 * 86_400),
        },
        api_keys: HashMap::new(),
        shared_quotas: HashMap::new(),
        credential_quotas: HashMap::from([("second".into(), second_quota)]),
        credential_meta: HashMap::new(),
        oauth: HashMap::new(),
        nous: HashMap::new(),
        acp_agents: HashMap::new(),
        catch_all: None,
        catch_all_exclude: Vec::new(),
        model_names: models
            .iter()
            .enumerate()
            .map(|(i, (name, _))| (Arc::from(*name), i))
            .collect(),
        models: models
            .iter()
            .map(|(name, deployments)| CanonicalModel {
                name: Arc::from(*name),
                capabilities: Capabilities::default(),
                context_overflow: ContextOverflow::default(),
                deployments: deployments.iter().copied().map(DeploymentIdx).collect(),
            })
            .collect(),
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
    let log = cfg
        .server
        .request_log
        .as_deref()
        .map(crate::RequestLog::open)
        .transpose()
        .unwrap();
    (
        Executor::new(Arc::new(cfg), &registry, &credentials, log).unwrap(),
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
        est_input_tokens: 0,
        anthropic_headers: HeaderMap::new(),
        max_output_tokens: None,
        context_overflow: None,
        request_id: RequestId("test".into()),
        end_user: None,
        client_key: Some("key_testfp".into()),
        image_count: 2,
        free_only: false,
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
async fn expired_deployments_are_skipped() {
    let (executor, adapter, _, _server) = fixture_full(
        vec![],
        false,
        [None; 3],
        &[("model", &[0, 1])],
        Default::default(),
        Default::default(),
        false,
        None,
        [
            Some(SystemTime::now() - Duration::from_secs(60)),
            Some(SystemTime::now() + Duration::from_secs(3600)),
            None,
        ],
        [None; 3],
    )
    .await;
    let response = executor.execute(request()).await.unwrap();
    assert_eq!(response.deployment.as_ref(), "dep1");
    assert_eq!(response.attempts, 1);
    assert_eq!(
        *adapter.calls.lock().unwrap(),
        vec![("dep1".into(), "0".into())]
    );
}

#[tokio::test]
async fn priority_boost_promotes_until_deadline_then_reverts() {
    let future = SystemTime::now() + Duration::from_secs(3600);
    let past = SystemTime::now() - Duration::from_secs(60);
    // While dep1's deadline is ahead, its boosted rank wins over dep0's lower
    // configured priority; dep2's lapsed boost keeps it last.
    let (executor, adapter, _, _server) = fixture_full(
        vec![],
        false,
        [None; 3],
        &[("model", &[0, 1, 2])],
        Default::default(),
        Default::default(),
        false,
        None,
        [None; 3],
        [None, Some((-1, future)), Some((-1, past))],
    )
    .await;
    let response = executor.execute(request()).await.unwrap();
    assert_eq!(response.deployment.as_ref(), "dep1");
    assert_eq!(
        *adapter.calls.lock().unwrap(),
        vec![("dep1".into(), "0".into())]
    );

    // With every deadline behind it, the configured order returns.
    let (executor, adapter, _, _server) = fixture_full(
        vec![],
        false,
        [None; 3],
        &[("model", &[0, 1, 2])],
        Default::default(),
        Default::default(),
        false,
        None,
        [None; 3],
        [None, Some((-1, past)), Some((-1, past))],
    )
    .await;
    let response = executor.execute(request()).await.unwrap();
    assert_eq!(response.deployment.as_ref(), "dep0");
    assert_eq!(
        *adapter.calls.lock().unwrap(),
        vec![("dep0".into(), "0".into())]
    );
}

#[tokio::test]
async fn exhaustion_retry_hint_ignores_expired_deployments() {
    let (executor, _, _, _server) = fixture_full(
        vec![UpstreamError::RateLimited {
            retry_after: Some(Duration::from_secs(30)),
        }],
        false,
        [None; 3],
        &[("model", &[0, 1])],
        Default::default(),
        Default::default(),
        false,
        None,
        [
            None,
            Some(SystemTime::now() - Duration::from_secs(60)),
            None,
        ],
        [None; 3],
    )
    .await;
    let Err(GatewayError::AllCandidatesExhausted { retry_after, .. }) =
        executor.execute(request()).await
    else {
        panic!("expected exhaustion")
    };
    assert!(retry_after.is_some_and(|wait| wait > Duration::from_secs(2)));
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
async fn retry_after_excludes_a_deployment_that_does_not_have_the_model() {
    let (executor, adapter, _, _server) = fixture_full(
        vec![UpstreamError::ModelNotFound {
            status: StatusCode::NOT_FOUND,
            body: Bytes::new(),
        }],
        false,
        [None; 3],
        &[("model", &[0, 2])],
        Default::default(),
        llmux_types::QuotaSpec {
            rpm: Some(1),
            ..Default::default()
        },
        false,
        None,
        [None; 3],
        [None; 3],
    )
    .await;
    drop(executor.bindings[2].limits.admit(1).unwrap());

    let Err(GatewayError::AllCandidatesExhausted {
        attempts,
        retry_after,
    }) = executor.execute(request()).await
    else {
        panic!("expected the backup's quota to block routing");
    };
    assert_eq!(attempts, 1);
    assert_eq!(adapter.calls.lock().unwrap()[0].0.as_ref(), "dep0");
    assert!(
        retry_after.is_some_and(|wait| !wait.is_zero()),
        "retry_after = {retry_after:?}"
    );
}

#[tokio::test]
async fn cooldown_before_repair_returns_original_rejection_without_resending() {
    let rejection = Bytes::from_static(b"deprecated parameter");
    let (executor, adapter, _, _server) = fixture_with_models(
        vec![UpstreamError::BadRequest {
            status: StatusCode::BAD_REQUEST,
            body: rejection.clone(),
        }],
        false,
        [None; 3],
        &[("model", &[0])],
        llmux_types::QuotaSpec {
            rpm: Some(2),
            ..Default::default()
        },
    )
    .await;
    adapter.status.store(400, Ordering::Relaxed);
    *adapter.cool_on_repair.lock().unwrap() = Some(executor.bindings[0].limits.clone());

    let result = executor.execute_attributed(request()).await;
    assert_eq!(adapter.wire_calls.load(Ordering::Relaxed), 1);
    let Err(error) = result else {
        panic!("expected the original rejection");
    };
    assert_eq!(error.attempts, 1);
    assert!(matches!(
        error.error,
        GatewayError::Client(UpstreamError::BadRequest {
            status: StatusCode::BAD_REQUEST,
            body,
        }) if body == rejection
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

fn sized_request(prompt_tokens: u32, max_output_tokens: Option<u32>) -> GatewayRequest {
    GatewayRequest {
        est_input_tokens: prompt_tokens,
        max_output_tokens,
        ..request()
    }
}

#[tokio::test]
async fn twenty_thousand_token_request_routes_to_the_larger_window() {
    let (executor, adapter, _, _server) =
        fixture_with_context(vec![], false, [Some(8_192), Some(32_768), Some(8_192)]).await;
    let response = executor.execute(sized_request(20_000, None)).await.unwrap();
    assert_eq!(response.deployment.as_ref(), "dep1");
    assert_eq!(response.attempts, 1);
    assert_eq!(adapter.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn skips_deployments_whose_context_is_too_small() {
    let (executor, adapter, _, _server) =
        fixture_with_context(vec![], false, [Some(100), None, Some(1000)]).await;
    let response = executor.execute(sized_request(80, Some(30))).await.unwrap();
    assert_eq!(response.deployment.as_ref(), "dep1");
    assert_eq!(response.attempts, 1);
    let response = executor.execute(sized_request(80, Some(20))).await.unwrap();
    assert_eq!(response.deployment.as_ref(), "dep0");
    assert_eq!(adapter.calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn rejects_requests_that_fit_no_deployment_without_calling_upstream() {
    let (executor, adapter, _, _server) =
        fixture_with_context(vec![], false, [Some(100), Some(4000), Some(1000)]).await;
    assert!(matches!(
        executor.execute(sized_request(3990, Some(20))).await,
        Err(GatewayError::ContextLengthExceeded {
            limit: 4000,
            prompt_tokens: 3990,
            max_output_tokens: Some(20),
            middle_out: false,
        })
    ));
    assert!(matches!(
        executor
            .execute(sized_request(u32::MAX, Some(u32::MAX)))
            .await,
        Err(GatewayError::ContextLengthExceeded { limit: 4000, .. })
    ));
    let middle_out = GatewayRequest {
        context_overflow: Some(ContextOverflow::MiddleOut),
        ..sized_request(4001, None)
    };
    assert!(matches!(
        executor.execute(middle_out).await,
        Err(GatewayError::ContextLengthExceeded {
            middle_out: true,
            ..
        })
    ));
    assert!(adapter.calls.lock().unwrap().is_empty());
    let response = executor.execute(sized_request(3990, None)).await.unwrap();
    assert_eq!(response.deployment.as_ref(), "dep1");
}

#[tokio::test]
async fn context_preflight_ignores_parked_deployments() {
    // dep0's 4000-token window is parked by a missing allowance; a request
    // that fits only it must be rejected by the preflight instead of
    // routing into legs that cannot serve it.
    // split_routes gives dep1 its own upstream model so dep0's missing
    // allowance parks only dep0's leg.
    let (executor, adapter, _, _server) = fixture_full(
        vec![UpstreamError::NotProvisioned],
        false,
        [Some(4000), Some(100), Some(100)],
        &[("model", &[0, 1])],
        Default::default(),
        Default::default(),
        true,
        None,
        [None; 3],
        [None; 3],
    )
    .await;
    executor.execute(sized_request(50, Some(10))).await.unwrap();
    assert!(executor.bindings[0].limits.not_provisioned());
    assert!(matches!(
        executor.execute(sized_request(200, Some(10))).await,
        Err(GatewayError::ContextLengthExceeded { limit: 100, .. })
    ));
    assert_eq!(adapter.calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn exhaustion_retry_hint_ignores_deployments_that_do_not_fit() {
    let (executor, _, _, _server) = fixture_with_context(
        vec![UpstreamError::RateLimited {
            retry_after: Some(Duration::from_secs(3)),
        }],
        false,
        [None, Some(10), Some(10)],
    )
    .await;
    let Err(GatewayError::AllCandidatesExhausted {
        attempts: 1,
        retry_after,
    }) = executor.execute(sized_request(50, None)).await
    else {
        panic!("expected exhaustion")
    };
    assert!(retry_after.is_some_and(|wait| wait > Duration::from_secs(2)));
}

#[tokio::test]
async fn repeated_rate_limits_lengthen_cooldown_until_a_success() {
    let limited = || UpstreamError::RateLimited { retry_after: None };
    let (executor, adapter, _, _server) = fixture(vec![limited()], false).await;
    let cooldown = |executor: &Executor| {
        let now = Instant::now();
        executor.bindings[0]
            .limits
            .cooldown_until(now)
            .map_or(Duration::ZERO, |end| end - now)
    };
    let served = |adapter: &Adapter| adapter.calls.lock().unwrap().last().unwrap().0.clone();
    let elapse = |duration| async move {
        tokio::time::pause();
        tokio::time::advance(duration).await;
        tokio::time::resume();
    };

    executor.execute(request()).await.unwrap();
    assert_eq!(served(&adapter).as_ref(), "dep2");
    assert!(cooldown(&executor) <= Duration::from_secs(1));

    elapse(Duration::from_millis(1100)).await;
    adapter.errors.lock().unwrap().push_back(limited());
    executor.execute(request()).await.unwrap();
    assert_eq!(served(&adapter).as_ref(), "dep2");
    let second = cooldown(&executor);
    assert!(
        second > Duration::from_millis(1500) && second <= Duration::from_secs(2),
        "second consecutive 429 should double the cooldown, got {second:?}"
    );

    elapse(second + Duration::from_millis(100)).await;
    executor.execute(request()).await.unwrap();
    assert_eq!(served(&adapter).as_ref(), "dep0");
    adapter.errors.lock().unwrap().push_back(limited());
    executor.execute(request()).await.unwrap();
    assert!(
        cooldown(&executor) <= Duration::from_secs(1),
        "a success resets the backoff"
    );
}

#[tokio::test]
async fn rate_limit_cools_down_every_model_sharing_the_upstream_route() {
    // dep0 and dep1 share provider, credential, base_url and upstream model.
    let (executor, adapter, _, _server) = fixture_with_models(
        vec![UpstreamError::RateLimited {
            retry_after: Some(Duration::from_secs(3)),
        }],
        false,
        [None; 3],
        &[("a", &[0, 2]), ("b", &[1, 2])],
        Default::default(),
    )
    .await;
    let first = executor
        .execute(GatewayRequest {
            model: "a".into(),
            ..request()
        })
        .await
        .unwrap();
    assert_eq!(first.deployment.as_ref(), "dep2");
    let second = executor
        .execute(GatewayRequest {
            model: "b".into(),
            ..request()
        })
        .await
        .unwrap();
    assert_eq!(second.deployment.as_ref(), "dep2");
    let calls: Vec<_> = adapter
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|(dep, _)| dep.to_string())
        .collect();
    assert_eq!(calls, ["dep0", "dep2", "dep2"]);
}

#[tokio::test]
async fn local_rpm_budget_is_shared_by_models_on_the_same_upstream_route() {
    let (executor, adapter, _, _server) = fixture_with_models(
        vec![],
        false,
        [None; 3],
        &[("a", &[0, 2]), ("b", &[1, 2])],
        llmux_types::QuotaSpec {
            rpm: Some(1),
            ..Default::default()
        },
    )
    .await;
    for model in ["a", "b"] {
        executor
            .execute(GatewayRequest {
                model: model.into(),
                ..request()
            })
            .await
            .unwrap();
    }
    let calls: Vec<_> = adapter
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|(dep, _)| dep.to_string())
        .collect();
    assert_eq!(calls, ["dep0", "dep2"]);
}

/// `fams[i]` binds dep i to the `[[shared_quotas]]` family of that name;
/// `creds[i]` sets its credential ("" is anonymous), `upstreams[i]` its
/// upstream model, and `dep2_base_url` moves dep2 to a second upstream.
async fn fixture_shared_group(
    models: &[(&str, &[u32])],
    groups: &[(&'static str, llmux_types::QuotaSpec)],
    fams: [Option<&'static str>; 3],
    creds: [&str; 3],
    upstreams: [&str; 3],
    quotas: [llmux_types::QuotaSpec; 3],
    dep2_base_url: Option<&str>,
) -> Fixture {
    let status = Arc::new(std::sync::atomic::AtomicU16::new(200));
    let served_status = status.clone();
    let wire_calls = Arc::new(AtomicUsize::new(0));
    let received = wire_calls.clone();
    let server = llmux_testkit::TestServer::start(axum::Router::new().fallback(move || {
        received.fetch_add(1, Ordering::Relaxed);
        let status = StatusCode::from_u16(served_status.load(Ordering::Relaxed)).unwrap();
        async move { (status, [("request-id", "req_fixture_1")], "ok") }
    }))
    .await
    .unwrap();
    let adapter = Arc::new(Adapter {
        kind: ProviderKind("test".into()),
        errors: Mutex::new(VecDeque::new()),
        build_errors: Mutex::new(VecDeque::new()),
        observed: Mutex::new(None),
        status,
        stream_errors: Mutex::new(VecDeque::new()),
        calls: Mutex::new(Vec::new()),
        cool_on_build: Mutex::new(None),
        cool_on_repair: Mutex::new(None),
        wire_calls,
    });
    let credential = Arc::new(Credential {
        refreshes: AtomicUsize::new(0),
        fail: false,
    });
    let deployments = (0..3)
        .map(|i| {
            let base_url = if i == 2 {
                dep2_base_url.map_or_else(|| server.url(), str::to_string)
            } else {
                server.url()
            };
            Deployment {
                headers: Default::default(),
                fallback: false,
                idx: DeploymentIdx(i),
                id: format!("dep{i}").into(),
                provider: adapter.kind.clone(),
                credential: creds[i as usize].into(),
                base_url: base_url.parse().unwrap(),
                upstream_model: upstreams[i as usize].into(),
                strip_request_fields: Box::default(),
                priority: i as i32,
                quota: quotas[i as usize].clone(),
                shared_quota: fams[i as usize].map(Into::into),
                timeout: Duration::from_secs(1),
                max_context: None,
                expires_at: None,
                priority_boost: None,
                priority_boost_until: None,
                billing: Billing::Billed,
            }
        })
        .collect();
    let cfg = RuntimeConfig {
        model_allowed_keys: Default::default(),
        credential_disabled: Default::default(),
        server: llmux_config::ServerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            client_keys: Box::new([]),
            key_policies: Box::new([]),
            deny: Box::new([]),
            request_timeout: Duration::from_secs(5),
            max_request_bytes: 32 * 1024 * 1024,
            shutdown_timeout: Duration::from_secs(60),
            drain_grace: Duration::from_secs(2),
            expose_routing_headers: true,
            dashboard_key: None,
            request_log: None,
            end_user_header: None,
            end_user_salt: None,
            end_user_from_body: false,
            remote_images: llmux_config::RemoteImages::Allow,
            max_image_bytes: None,
        },
        routing: llmux_config::RoutingConfig {
            max_attempts: 4,
            fallback_on: Box::new([ErrorKind::RateLimited, ErrorKind::QuotaExhausted]),
        },
        cooldown: llmux_config::CooldownConfig {
            default: Duration::from_secs(1),
            max: Duration::from_secs(5),
            quota_max: Duration::from_secs(10 * 86_400),
        },
        api_keys: HashMap::new(),
        shared_quotas: groups
            .iter()
            .map(|(name, spec)| (Arc::from(*name), spec.clone()))
            .collect(),
        credential_quotas: HashMap::new(),
        credential_meta: HashMap::new(),
        oauth: HashMap::new(),
        nous: HashMap::new(),
        acp_agents: HashMap::new(),
        catch_all: None,
        catch_all_exclude: Vec::new(),
        model_names: models
            .iter()
            .enumerate()
            .map(|(i, (name, _))| (Arc::from(*name), i))
            .collect(),
        models: models
            .iter()
            .map(|(name, deployments)| CanonicalModel {
                name: Arc::from(*name),
                capabilities: Capabilities::default(),
                context_overflow: ContextOverflow::default(),
                deployments: deployments.iter().copied().map(DeploymentIdx).collect(),
            })
            .collect(),
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
        Executor::new(Arc::new(cfg), &registry, &credentials, None).unwrap(),
        adapter,
        credential,
        server,
    )
}

#[tokio::test]
async fn shared_quota_binds_legs_on_one_scope_to_one_window() {
    // dep0 (model a → upstream) and dep1 (model b → upstream-1) sit on the
    // same credential and both bind the "fam" shared quota: the family is one
    // account-wide window across different upstream models.
    let (executor, adapter, _, _server) = fixture_shared_group(
        &[("a", &[0, 2]), ("b", &[1, 2])],
        &[(
            "fam",
            llmux_types::QuotaSpec {
                rpm: Some(1),
                ..Default::default()
            },
        )],
        [Some("fam"), Some("fam"), None],
        ["first", "first", "second"],
        ["upstream", "upstream-1", "upstream"],
        std::array::from_fn(|_| Default::default()),
        None,
    )
    .await;
    for model in ["a", "b"] {
        executor
            .execute(GatewayRequest {
                model: model.into(),
                ..request()
            })
            .await
            .unwrap();
    }
    let calls: Vec<_> = adapter
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|(dep, _)| dep.to_string())
        .collect();
    assert_eq!(calls, ["dep0", "dep2"]);
}

#[tokio::test]
async fn shared_quota_scopes_anonymous_legs_by_provider_and_base_url() {
    // Anonymous legs carry no credential, so the family scope is provider +
    // base URL: dep1 mirrors dep0's scope and call 1 spends the fam rpm=1
    // window, while dep2 names the same group on a different base URL — a
    // different scope — and keeps its own window.
    let other = llmux_testkit::TestServer::start(
        axum::Router::new()
            .fallback(|| async { (StatusCode::OK, [("request-id", "req_fixture_2")], "ok") }),
    )
    .await
    .unwrap();
    let other_url = other.url();
    let (executor, adapter, _, _server) = fixture_shared_group(
        &[("a", &[0, 2]), ("b", &[1, 2])],
        &[(
            "fam",
            llmux_types::QuotaSpec {
                rpm: Some(1),
                ..Default::default()
            },
        )],
        [Some("fam"), Some("fam"), Some("fam")],
        ["", "", ""],
        ["upstream", "upstream-1", "upstream"],
        std::array::from_fn(|_| Default::default()),
        Some(&other_url),
    )
    .await;
    for model in ["a", "b"] {
        executor
            .execute(GatewayRequest {
                model: model.into(),
                ..request()
            })
            .await
            .unwrap();
    }
    let calls: Vec<_> = adapter
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|(dep, _)| dep.to_string())
        .collect();
    assert_eq!(calls, ["dep0", "dep2"]);
}

#[tokio::test]
async fn unbound_leg_shares_route_local_window_with_bound_sibling() {
    // dep0 (unbound) and dep1 (bound to fam) sit on the same route — same
    // provider, credential, base URL, upstream model, and local rpm=1.
    // The family binding must not split the route's local budget: dep0's
    // spend drains the window dep1 draws on, so call 2 exhausts.
    let (executor, adapter, _, _server) = fixture_shared_group(
        &[("a", &[0, 1])],
        &[(
            "fam",
            llmux_types::QuotaSpec {
                rpm: Some(100),
                ..Default::default()
            },
        )],
        [None, Some("fam"), Some("fam")],
        ["first", "first", "first"],
        ["upstream", "upstream", "upstream-2"],
        [
            llmux_types::QuotaSpec {
                rpm: Some(1),
                ..Default::default()
            },
            llmux_types::QuotaSpec {
                rpm: Some(1),
                ..Default::default()
            },
            Default::default(),
        ],
        None,
    )
    .await;
    executor
        .execute(GatewayRequest {
            model: "a".into(),
            ..request()
        })
        .await
        .unwrap();
    let Err(GatewayError::AllCandidatesExhausted { .. }) = executor
        .execute(GatewayRequest {
            model: "a".into(),
            ..request()
        })
        .await
    else {
        panic!("expected exhaustion")
    };
    assert_eq!(adapter.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn bound_leg_shares_route_local_window_with_unbound_sibling() {
    // Mirror order: dep0 is bound, dep1 unbound. The shared local window
    // must hold in both processing directions.
    let (executor, adapter, _, _server) = fixture_shared_group(
        &[("a", &[0, 1])],
        &[(
            "fam",
            llmux_types::QuotaSpec {
                rpm: Some(100),
                ..Default::default()
            },
        )],
        [Some("fam"), None, Some("fam")],
        ["first", "first", "first"],
        ["upstream", "upstream", "upstream-2"],
        [
            llmux_types::QuotaSpec {
                rpm: Some(1),
                ..Default::default()
            },
            llmux_types::QuotaSpec {
                rpm: Some(1),
                ..Default::default()
            },
            Default::default(),
        ],
        None,
    )
    .await;
    executor
        .execute(GatewayRequest {
            model: "a".into(),
            ..request()
        })
        .await
        .unwrap();
    let Err(GatewayError::AllCandidatesExhausted { .. }) = executor
        .execute(GatewayRequest {
            model: "a".into(),
            ..request()
        })
        .await
    else {
        panic!("expected exhaustion")
    };
    assert_eq!(adapter.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn bound_leg_still_spends_family_window_alongside_shared_local() {
    // dep0 (bound, fam rpm=1) shares its route-local window with unbound
    // dep1, but its calls must still charge the family: dep2 binds the same
    // fam on a different route and finds the allowance already spent.
    let (executor, adapter, _, _server) = fixture_shared_group(
        &[("a", &[0, 1]), ("b", &[2])],
        &[(
            "fam",
            llmux_types::QuotaSpec {
                rpm: Some(1),
                ..Default::default()
            },
        )],
        [Some("fam"), None, Some("fam")],
        ["first", "first", "first"],
        ["upstream", "upstream", "upstream-2"],
        [
            llmux_types::QuotaSpec {
                rpm: Some(2),
                ..Default::default()
            },
            llmux_types::QuotaSpec {
                rpm: Some(2),
                ..Default::default()
            },
            Default::default(),
        ],
        None,
    )
    .await;
    for _ in 0..2 {
        executor
            .execute(GatewayRequest {
                model: "a".into(),
                ..request()
            })
            .await
            .unwrap();
    }
    let Err(GatewayError::AllCandidatesExhausted { .. }) = executor
        .execute(GatewayRequest {
            model: "b".into(),
            ..request()
        })
        .await
    else {
        panic!("expected exhaustion")
    };
    // Call 2 lands on unbound dep1 — dep0's fam window is already spent —
    // drawing only on the shared local window, never the family.
    let calls: Vec<_> = adapter
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|(dep, _)| dep.to_string())
        .collect();
    assert_eq!(calls, ["dep0", "dep1"]);
}

#[tokio::test]
async fn shared_quota_family_cap_rejects_oversized_request_on_empty_window() {
    // A family allowance is an account-wide ceiling like a credential cap:
    // a single request whose cost alone exceeds it must be refused even
    // when nothing has been spent. Soft deployment windows would admit one
    // oversized call to an idle scope and blow the allowance.
    let (executor, adapter, _, _server) = fixture_shared_group(
        &[("a", &[0, 2])],
        &[(
            "fam",
            llmux_types::QuotaSpec {
                tpm: Some(5),
                ..Default::default()
            },
        )],
        [Some("fam"), None, Some("fam")],
        ["first", "first", "second"],
        ["upstream", "upstream-1", "upstream"],
        std::array::from_fn(|_| Default::default()),
        None,
    )
    .await;
    let Err(GatewayError::AllCandidatesExhausted { .. }) = executor
        .execute(GatewayRequest {
            model: "a".into(),
            est_input_tokens: 10,
            ..request()
        })
        .await
    else {
        panic!("expected exhaustion")
    };
    assert!(adapter.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn unbound_legs_on_the_same_credential_keep_independent_windows() {
    // dep0 alone binds the drained "fam" window; dep1 on the same credential
    // is not in the group, so a paid leg on a shared key is never throttled
    // by the family's allowance.
    let (executor, adapter, _, _server) = fixture_shared_group(
        &[("a", &[0, 2]), ("b", &[1, 2])],
        &[(
            "fam",
            llmux_types::QuotaSpec {
                rpm: Some(1),
                ..Default::default()
            },
        )],
        [Some("fam"), None, None],
        ["first", "first", "second"],
        ["upstream", "upstream-1", "upstream"],
        std::array::from_fn(|_| Default::default()),
        None,
    )
    .await;
    for model in ["a", "b"] {
        executor
            .execute(GatewayRequest {
                model: model.into(),
                ..request()
            })
            .await
            .unwrap();
    }
    let calls: Vec<_> = adapter
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|(dep, _)| dep.to_string())
        .collect();
    assert_eq!(calls, ["dep0", "dep1"]);
}

#[tokio::test]
async fn bound_then_unbound_siblings_on_one_route_do_not_share_family_windows() {
    // dep0 and dep1 sit on the same upstream route except dep0 binds "fam":
    // processing dep0 first must not hand dep1 the family's drained window.
    let (executor, adapter, _, _server) = fixture_shared_group(
        &[("a", &[0, 1, 2])],
        &[(
            "fam",
            llmux_types::QuotaSpec {
                rpm: Some(1),
                ..Default::default()
            },
        )],
        [Some("fam"), None, None],
        ["first", "first", "second"],
        ["upstream", "upstream", "upstream"],
        std::array::from_fn(|_| Default::default()),
        None,
    )
    .await;
    for _ in 0..2 {
        executor
            .execute(GatewayRequest {
                model: "a".into(),
                ..request()
            })
            .await
            .unwrap();
    }
    let calls: Vec<_> = adapter
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|(dep, _)| dep.to_string())
        .collect();
    assert_eq!(calls, ["dep0", "dep1"]);
}

#[tokio::test]
async fn unbound_then_bound_siblings_on_one_route_keep_the_family_cap() {
    // Mirror order: dep0 unbound processes first, dep1 same route bound
    // "fam". dep1 must still count against the family window it shares with
    // dep2 — a detached limits object would let the cap double silently.
    let (executor, _, _, _server) = fixture_shared_group(
        &[("a", &[1]), ("b", &[2])],
        &[(
            "fam",
            llmux_types::QuotaSpec {
                rpm: Some(2),
                ..Default::default()
            },
        )],
        [None, Some("fam"), Some("fam")],
        ["first", "first", "first"],
        ["upstream", "upstream", "upstream-2"],
        std::array::from_fn(|_| Default::default()),
        None,
    )
    .await;
    for model in ["a", "b"] {
        executor
            .execute(GatewayRequest {
                model: model.into(),
                ..request()
            })
            .await
            .unwrap();
    }
    // dep1 and dep2 spend the fam rpm=2 window; a second request on model a
    // must find dep1 out of family allowance, not serve on dep0's free pass.
    assert!(matches!(
        executor
            .execute(GatewayRequest {
                model: "a".into(),
                ..request()
            })
            .await,
        Err(GatewayError::AllCandidatesExhausted { .. })
    ));
}

#[tokio::test]
async fn legs_bound_to_different_families_on_one_route_do_not_conflate() {
    // dep0 binds "fam-a", dep1 the same route bound "fam-b": dep1 draws on
    // fam-b's fresh window, never dep0's spent fam-a window.
    let spec = llmux_types::QuotaSpec {
        rpm: Some(1),
        ..Default::default()
    };
    let (executor, adapter, _, _server) = fixture_shared_group(
        &[("a", &[0, 1, 2])],
        &[("fam-a", spec.clone()), ("fam-b", spec)],
        [Some("fam-a"), Some("fam-b"), None],
        ["first", "first", "second"],
        ["upstream", "upstream", "upstream"],
        std::array::from_fn(|_| Default::default()),
        None,
    )
    .await;
    for _ in 0..2 {
        executor
            .execute(GatewayRequest {
                model: "a".into(),
                ..request()
            })
            .await
            .unwrap();
    }
    let calls: Vec<_> = adapter
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|(dep, _)| dep.to_string())
        .collect();
    assert_eq!(calls, ["dep0", "dep1"]);
}

#[tokio::test]
async fn credentialed_leg_does_not_claim_the_anonymous_scope_window() {
    // dep0 credentialed, dep1/dep2 anonymous — all bound "fam" on one
    // base_url. The anonymous legs' per-IP scope must not merge into dep0's
    // credential scope when dep0 is processed first.
    let (executor, adapter, _, _server) = fixture_shared_group(
        &[("a", &[0, 1, 2])],
        &[(
            "fam",
            llmux_types::QuotaSpec {
                rpm: Some(1),
                ..Default::default()
            },
        )],
        [Some("fam"), Some("fam"), Some("fam")],
        ["first", "", ""],
        ["upstream", "upstream", "upstream"],
        std::array::from_fn(|_| Default::default()),
        None,
    )
    .await;
    for _ in 0..2 {
        executor
            .execute(GatewayRequest {
                model: "a".into(),
                ..request()
            })
            .await
            .unwrap();
    }
    let calls: Vec<_> = adapter
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|(dep, _)| dep.to_string())
        .collect();
    assert_eq!(calls, ["dep0", "dep1"]);
}

#[tokio::test]
async fn anonymous_leg_does_not_claim_the_credential_scope_window() {
    // Mirror order: anonymous dep0 first; credentialed dep1/dep2 on "first"
    // share the credential scope, never dep0's anonymous window.
    let (executor, adapter, _, _server) = fixture_shared_group(
        &[("a", &[0, 1, 2])],
        &[(
            "fam",
            llmux_types::QuotaSpec {
                rpm: Some(1),
                ..Default::default()
            },
        )],
        [Some("fam"), Some("fam"), Some("fam")],
        ["", "first", "first"],
        ["upstream", "upstream", "upstream"],
        std::array::from_fn(|_| Default::default()),
        None,
    )
    .await;
    for _ in 0..2 {
        executor
            .execute(GatewayRequest {
                model: "a".into(),
                ..request()
            })
            .await
            .unwrap();
    }
    let calls: Vec<_> = adapter
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|(dep, _)| dep.to_string())
        .collect();
    assert_eq!(calls, ["dep0", "dep1"]);
}

#[tokio::test]
async fn exhausted_rate_limit_headers_skip_the_deployment_until_reset() {
    for observed in [
        LimitObservation {
            remaining_requests: Some(0),
            reset_requests: Some(Duration::from_secs(3)),
            ..LimitObservation::default()
        },
        LimitObservation {
            remaining_tokens: Some(0),
            reset_tokens: Some(Duration::from_secs(3)),
            ..LimitObservation::default()
        },
    ] {
        let (executor, adapter, _, _server) = fixture(Vec::new(), false).await;
        *adapter.observed.lock().unwrap() = Some(observed);
        let first = executor.execute(request()).await.unwrap();
        assert_eq!(first.deployment.as_ref(), "dep0");
        let second = executor.execute(request()).await.unwrap();
        assert_eq!(
            second.deployment.as_ref(),
            "dep2",
            "a success reporting zero remaining requests or tokens must not be retried before reset"
        );
    }
}

#[tokio::test]
async fn remaining_rate_limit_headers_keep_the_deployment() {
    let (executor, adapter, _, _server) = fixture(Vec::new(), false).await;
    *adapter.observed.lock().unwrap() = Some(LimitObservation {
        remaining_requests: Some(1),
        reset_requests: Some(Duration::from_secs(3)),
        ..LimitObservation::default()
    });
    executor.execute(request()).await.unwrap();
    let second = executor.execute(request()).await.unwrap();
    assert_eq!(second.deployment.as_ref(), "dep0");
}

#[tokio::test]
async fn rate_limit_headers_on_error_responses_do_not_cool_down() {
    let (executor, adapter, _, _server) = fixture(Vec::new(), false).await;
    adapter.status.store(400, Ordering::Relaxed);
    *adapter.observed.lock().unwrap() = Some(LimitObservation {
        remaining_requests: Some(0),
        reset_requests: Some(Duration::from_secs(3)),
        ..LimitObservation::default()
    });
    executor.execute(request()).await.unwrap();
    assert!(executor.bindings[0]
        .limits
        .cooldown_until(Instant::now())
        .is_none());
}

#[tokio::test]
async fn limit_error_after_stream_start_cools_down_the_deployment() {
    for error in [
        UpstreamError::RateLimited {
            retry_after: Some(Duration::from_secs(3)),
        },
        UpstreamError::QuotaExhausted {
            reset_at: Some(SystemTime::now() + Duration::from_secs(3)),
        },
    ] {
        let (executor, adapter, _, _server) = fixture(Vec::new(), false).await;
        adapter.stream_errors.lock().unwrap().push_back(error);
        let streaming = || GatewayRequest {
            stream: true,
            ..request()
        };
        let routed = executor.execute(streaming()).await.unwrap();
        assert_eq!(routed.deployment.as_ref(), "dep0");
        let GatewayResponse::Stream(events) = routed.response else {
            panic!("expected stream")
        };
        let events: Vec<_> = events.collect().await;
        assert!(matches!(events.as_slice(), [Ok(_), Err(_)]));

        let next = executor.execute(streaming()).await.unwrap();
        assert_eq!(
            next.deployment.as_ref(),
            "dep2",
            "the deployment whose stream ended in a limit error must cool down"
        );
    }
}

#[tokio::test]
async fn committed_stream_quota_cools_shared_credential_without_replay() {
    for (error, expected) in [
        (UpstreamError::QuotaExhausted { reset_at: None }, "dep2"),
        (
            UpstreamError::QuotaExhausted {
                reset_at: Some(SystemTime::now() + Duration::from_secs(30)),
            },
            "dep2",
        ),
        (
            UpstreamError::RateLimited {
                retry_after: Some(Duration::from_secs(30)),
            },
            "dep1",
        ),
    ] {
        let kind = error.kind();
        let (executor, adapter, _, _server) = fixture_full(
            Vec::new(),
            false,
            [None; 3],
            &[("a", &[0, 2]), ("b", &[1, 2])],
            Default::default(),
            Default::default(),
            true,
            None,
            [None; 3],
            [None; 3],
        )
        .await;
        adapter.stream_errors.lock().unwrap().push_back(error);
        let routed = executor
            .execute(GatewayRequest {
                model: "a".into(),
                stream: true,
                ..request()
            })
            .await
            .unwrap();
        assert_eq!(routed.deployment.as_ref(), "dep0");
        let GatewayResponse::Stream(events) = routed.response else {
            panic!("expected stream")
        };
        let events: Vec<_> = events.collect().await;
        assert!(matches!(events.as_slice(), [Ok(_), Err(err)] if err.kind() == kind));
        assert_eq!(adapter.calls.lock().unwrap().len(), 1, "no stream replay");

        let next = executor
            .execute(GatewayRequest {
                model: "b".into(),
                ..request()
            })
            .await
            .unwrap();
        assert_eq!(next.deployment.as_ref(), expected, "{kind:?}");
        assert_eq!(adapter.calls.lock().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn committed_stream_not_provisioned_parks_the_leg_without_replay() {
    // The plan-missing error can arrive after the response commits: it must
    // reach the client unreplayed, park only that leg, and leave the sibling
    // on the same credential eligible.
    let (executor, adapter, _, _server) = fixture_full(
        Vec::new(),
        false,
        [None; 3],
        &[("a", &[0, 2]), ("b", &[1, 2])],
        Default::default(),
        Default::default(),
        true,
        None,
        [None; 3],
        [None; 3],
    )
    .await;
    adapter
        .stream_errors
        .lock()
        .unwrap()
        .push_back(UpstreamError::NotProvisioned);
    let routed = executor
        .execute(GatewayRequest {
            model: "a".into(),
            stream: true,
            ..request()
        })
        .await
        .unwrap();
    assert_eq!(routed.deployment.as_ref(), "dep0");
    let GatewayResponse::Stream(events) = routed.response else {
        panic!("expected stream")
    };
    let events: Vec<_> = events.collect().await;
    assert!(matches!(
        events.as_slice(),
        [Ok(_), Err(UpstreamError::NotProvisioned)]
    ));
    assert_eq!(adapter.calls.lock().unwrap().len(), 1, "no stream replay");
    assert!(executor.bindings[0].limits.not_provisioned());

    let next = executor
        .execute(GatewayRequest {
            model: "b".into(),
            ..request()
        })
        .await
        .unwrap();
    assert_eq!(
        next.deployment.as_ref(),
        "dep1",
        "the parked leg is skipped; the sibling stays eligible"
    );
    assert_eq!(adapter.calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn repeated_limit_errors_after_stream_start_back_off() {
    let (executor, adapter, _, _server) = fixture(Vec::new(), false).await;
    let streaming = || GatewayRequest {
        stream: true,
        ..request()
    };
    let mut cooldowns = Vec::new();
    for _ in 0..2 {
        adapter
            .stream_errors
            .lock()
            .unwrap()
            .push_back(UpstreamError::RateLimited { retry_after: None });
        let routed = executor.execute(streaming()).await.unwrap();
        assert_eq!(routed.deployment.as_ref(), "dep0");
        let GatewayResponse::Stream(events) = routed.response else {
            panic!("expected stream")
        };
        let _: Vec<_> = events.collect().await;
        let now = Instant::now();
        let until = executor.bindings[0].limits.cooldown_until(now).unwrap();
        cooldowns.push(until - now);
        tokio::time::sleep(until - now + Duration::from_millis(50)).await;
    }
    assert!(
        cooldowns[1] > Duration::from_millis(1500),
        "a stream that started then hit a limit is not a success: {cooldowns:?}"
    );
}

#[tokio::test]
async fn conversion_rejection_distinguishes_rate_limited_compatible_deployments() {
    for cooldown in [false, true] {
        let (executor, adapter, _, _server) = fixture_with_models(
            Vec::new(),
            false,
            [None; 3],
            &[("model", &[0, 2])],
            llmux_types::QuotaSpec {
                rpm: (!cooldown).then_some(1),
                ..Default::default()
            },
        )
        .await;
        let result = executor.execute(request()).await.unwrap();
        assert_eq!(result.deployment.as_ref(), "dep0");
        if cooldown {
            executor.bindings[0]
                .limits
                .cool_down(Duration::from_secs(30));
        }
        let messages = |body| GatewayRequest {
            format: InboundFormat::AnthropicMessages,
            body: Bytes::from_static(body),
            ..request()
        };
        assert!(matches!(
            executor.execute(messages(b"document")).await,
            Err(GatewayError::InvalidRequest)
        ));
        assert_eq!(adapter.calls.lock().unwrap().len(), 1);
        assert!(matches!(
            executor.execute(messages(b"image")).await,
            Err(GatewayError::AllCandidatesExhausted { attempts: 0, retry_after })
                if retry_after.is_some_and(|wait| !wait.is_zero())
        ));
    }
}

#[tokio::test]
async fn token_quota_skips_a_deployment_without_calling_it() {
    let (executor, adapter, _, _server) = fixture_with_models(
        Vec::new(),
        false,
        [None; 3],
        &[("model", &[0, 1, 2])],
        llmux_types::QuotaSpec {
            tpm: Some(100),
            ..Default::default()
        },
    )
    .await;
    let sized = || GatewayRequest {
        est_input_tokens: 60,
        ..request()
    };
    executor.execute(sized()).await.unwrap();
    executor.execute(sized()).await.unwrap();
    let served: Vec<_> = adapter
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|(dep, _)| dep.to_string())
        .collect();
    assert_eq!(
        served,
        ["dep0", "dep2"],
        "60 + 60 tokens exceed tpm = 100 on dep0"
    );
}

#[tokio::test]
async fn after_a_cooldown_other_requests_fall_back_while_a_probe_is_open() {
    let (executor, adapter, _, _server) = fixture(
        vec![UpstreamError::RateLimited {
            retry_after: Some(Duration::from_millis(50)),
        }],
        false,
    )
    .await;
    assert_eq!(
        executor
            .execute(request())
            .await
            .unwrap()
            .deployment
            .as_ref(),
        "dep2"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    let streaming = GatewayRequest {
        stream: true,
        ..request()
    };
    let probe = executor.execute(streaming).await.unwrap();
    assert_eq!(probe.deployment.as_ref(), "dep0");
    let held_back = executor.execute(request()).await.unwrap();
    assert_eq!(
        held_back.deployment.as_ref(),
        "dep2",
        "dep0 has a probe open"
    );
    drop(probe);
    assert_eq!(adapter.calls.lock().unwrap().len(), 4);
}

/// A credential window is a hard ceiling: a request whose estimate exceeds
/// the remaining budget is skipped even when the window is empty. dep2 is
/// routed first but its credential `tpd` is smaller than the request, so it
/// falls through to dep0, whose credential has no quota.
#[tokio::test]
async fn oversized_request_skips_deployment_on_a_full_or_empty_credential_window() {
    let (executor, adapter, _, _server) = fixture_full(
        Vec::new(),
        false,
        [None; 3],
        &[("model", &[2, 0])],
        Default::default(),
        llmux_types::QuotaSpec {
            tpd: Some(100),
            ..Default::default()
        },
        false,
        None,
        [None; 3],
        [None; 3],
    )
    .await;
    let oversized = || GatewayRequest {
        est_input_tokens: 150,
        ..request()
    };
    executor.execute(oversized()).await.unwrap();
    executor.execute(oversized()).await.unwrap();
    let served: Vec<_> = adapter
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|(dep, _)| dep.to_string())
        .collect();
    assert_eq!(
        served,
        ["dep0", "dep0"],
        "dep2's credential tpd = 100 is a hard ceiling: 150 never reaches it"
    );
}

#[tokio::test]
async fn credential_quota_limits_the_credentials_deployments() {
    let (executor, adapter, _, _server) = fixture_full(
        Vec::new(),
        false,
        [None; 3],
        &[("model", &[2])],
        Default::default(),
        llmux_types::QuotaSpec {
            rpm: Some(1),
            ..Default::default()
        },
        false,
        None,
        [None; 3],
        [None; 3],
    )
    .await;
    executor.execute(request()).await.unwrap();
    assert!(matches!(
        executor.execute(request()).await,
        Err(GatewayError::AllCandidatesExhausted { .. })
    ));
    assert_eq!(adapter.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn exhausted_quota_cools_every_deployment_on_the_credential() {
    let (executor, adapter, _, _server) = fixture_full(
        vec![UpstreamError::QuotaExhausted { reset_at: None }],
        false,
        [None; 3],
        &[("a", &[0]), ("b", &[1, 2])],
        Default::default(),
        Default::default(),
        true,
        None,
        [None; 3],
        [None; 3],
    )
    .await;
    let to = |model: &str| GatewayRequest {
        model: model.into(),
        ..request()
    };
    assert!(executor.execute(to("a")).await.is_err());
    executor.execute(to("b")).await.unwrap();
    let served: Vec<_> = adapter
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|(dep, _)| dep.to_string())
        .collect();
    assert_eq!(
        served,
        ["dep0", "dep2"],
        "dep1 shares dep0's exhausted credential"
    );
}

#[tokio::test]
async fn credential_peers_cool_only_until_the_quota_resets() {
    let (executor, adapter, _, _server) = fixture_full(
        vec![UpstreamError::QuotaExhausted {
            reset_at: Some(SystemTime::now() + Duration::from_millis(50)),
        }],
        false,
        [None; 3],
        &[("a", &[0]), ("b", &[1, 2])],
        Default::default(),
        Default::default(),
        true,
        None,
        [None; 3],
        [None; 3],
    )
    .await;
    *adapter.observed.lock().unwrap() = Some(LimitObservation {
        remaining_requests: Some(0),
        reset_requests: Some(Duration::from_secs(10)),
        ..LimitObservation::default()
    });
    let to = |model: &str| GatewayRequest {
        model: model.into(),
        ..request()
    };
    assert!(executor.execute(to("a")).await.is_err());
    tokio::time::sleep(Duration::from_millis(100)).await;
    executor.execute(to("b")).await.unwrap();
    let served: Vec<_> = adapter
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|(dep, _)| dep.to_string())
        .collect();
    assert_eq!(
        served,
        ["dep0", "dep1"],
        "dep0's own route limit is not the key's"
    );
}

#[tokio::test]
async fn an_explicit_quota_reset_outlasts_the_backoff_cap() {
    // fixture cooldown: default 1s, max 5s, quota_max 10d. A provider reset
    // three days out is authoritative and must not clamp to `max`.
    // split_routes gives dep1 its own upstream model so the credential peer
    // path (not the shared-limits alias) carries the cooldown.
    let (executor, adapter, _, _server) = fixture_full(
        vec![UpstreamError::QuotaExhausted {
            reset_at: Some(SystemTime::now() + Duration::from_secs(3 * 86_400)),
        }],
        false,
        [None; 3],
        &[("model", &[0, 1, 2])],
        Default::default(),
        Default::default(),
        true,
        None,
        [None; 3],
        [None; 3],
    )
    .await;
    // dep0's credential quota is exhausted; dep1 shares the credential so dep2 serves.
    let response = executor.execute(request()).await.unwrap();
    assert_eq!(response.deployment.as_ref(), "dep2");
    let now = Instant::now();
    for idx in [0, 1] {
        let until = executor.bindings[idx]
            .limits
            .cooldown_until(now)
            .expect("the credential's deployments cool together");
        assert!(
            until.duration_since(now) > Duration::from_secs(86_400),
            "an authoritative reset is honored beyond the backoff cap"
        );
        assert_eq!(
            executor.bindings[idx].limits.cooldown_scope(now),
            Some("credential"),
            "the cooldown reports the credential's shared reset"
        );
    }
    assert!(executor.bindings[2].limits.cooldown_until(now).is_none());
    assert_eq!(adapter.calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_quota_reset_beyond_the_quota_cap_is_clamped_there() {
    let (executor, _, _, _server) = fixture_full(
        vec![UpstreamError::QuotaExhausted {
            reset_at: Some(SystemTime::now() + Duration::from_secs(40 * 86_400)),
        }],
        false,
        [None; 3],
        &[("model", &[0])],
        Default::default(),
        Default::default(),
        false,
        None,
        [None; 3],
        [None; 3],
    )
    .await;
    assert!(executor.execute(request()).await.is_err());
    let now = Instant::now();
    let until = executor.bindings[0].limits.cooldown_until(now).unwrap();
    let wait = until.duration_since(now);
    assert!(
        wait > Duration::from_secs(9 * 86_400) && wait <= Duration::from_secs(10 * 86_400),
        "a reset beyond the quota cap clamps there, not at the backoff cap: {wait:?}"
    );
}

#[tokio::test]
async fn a_rate_limit_never_cools_credential_peers() {
    // split_routes gives dep1 its own upstream model: dep0 and dep1 draw on
    // the same credential but not the same rate-limit bucket.
    let (executor, adapter, _, _server) = fixture_full(
        vec![UpstreamError::RateLimited {
            retry_after: Some(Duration::from_secs(60)),
        }],
        false,
        [None; 3],
        &[("model", &[0, 1, 2])],
        Default::default(),
        Default::default(),
        true,
        None,
        [None; 3],
        [None; 3],
    )
    .await;
    let response = executor.execute(request()).await.unwrap();
    assert_eq!(response.deployment.as_ref(), "dep1");
    let now = Instant::now();
    // The failing deployment cools — capped at the backoff cap, not the hint.
    let until = executor.bindings[0].limits.cooldown_until(now).unwrap();
    assert!(until.duration_since(now) <= Duration::from_secs(5));
    assert_eq!(
        executor.bindings[0].limits.cooldown_scope(now),
        Some("deployment")
    );
    // A rate limit is the deployment's own: the credential's other legs stay ready.
    assert!(executor.bindings[1].limits.cooldown_until(now).is_none());
    assert!(executor.bindings[2].limits.cooldown_until(now).is_none());
    // And dep1 serves the next request without waiting.
    executor.execute(request()).await.unwrap();
    let served: Vec<_> = adapter
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|(dep, _)| dep.to_string())
        .collect();
    assert_eq!(served, ["dep0", "dep1", "dep1"]);
}

#[tokio::test]
async fn a_not_provisioned_deployment_is_skipped_until_reload() {
    // split_routes gives dep1 its own upstream model: a missing allowance on
    // dep0 parks only dep0, even though both draw on credential "first".
    let (mut executor, adapter, _, _server) = fixture_full(
        vec![UpstreamError::NotProvisioned],
        false,
        [None; 3],
        &[("model", &[0, 1, 2])],
        Default::default(),
        Default::default(),
        true,
        None,
        [None; 3],
        [None; 3],
    )
    .await;
    // dep0's plan does not cover it; the same credential's dep1 still serves.
    let response = executor.execute(request()).await.unwrap();
    assert_eq!(response.deployment.as_ref(), "dep1");
    assert!(executor.bindings[0].limits.not_provisioned());
    assert!(!executor.bindings[1].limits.not_provisioned());
    // The next request skips dep0 without spending an attempt on it.
    let response = executor.execute(request()).await.unwrap();
    assert_eq!(response.deployment.as_ref(), "dep1");
    assert_eq!(response.attempts, 1);
    let served: Vec<_> = adapter
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|(dep, _)| dep.to_string())
        .collect();
    assert_eq!(served, ["dep0", "dep1", "dep1"]);
    // Stats record the distinct class, not a transient 429.
    for binding in executor.bindings.iter_mut() {
        binding.stats = Some(Arc::default());
    }
    executor.execute(request()).await.unwrap();
    let stats = serde_json::to_value(executor.deployment_stats()).unwrap();
    let dep0 = stats
        .as_array()
        .unwrap()
        .iter()
        .find(|dep| dep["id"] == "dep0")
        .unwrap();
    assert_eq!(dep0["not_provisioned"], true);
    assert_eq!(dep0["skip_reasons"]["not_provisioned"], 1);
}

#[tokio::test]
async fn an_all_not_provisioned_model_exhausts_immediately() {
    let (executor, adapter, _, _server) = fixture_full(
        vec![UpstreamError::NotProvisioned],
        false,
        [None; 3],
        &[("model", &[0])],
        Default::default(),
        Default::default(),
        false,
        None,
        [None; 3],
        [None; 3],
    )
    .await;
    assert!(matches!(
        executor.execute(request()).await,
        Err(GatewayError::Client(UpstreamError::NotProvisioned))
    ));
    // Later requests short-circuit: no attempt, no retry hint.
    assert!(matches!(
        executor.execute(request()).await,
        Err(GatewayError::AllCandidatesExhausted {
            attempts: 0,
            retry_after: None
        })
    ));
    assert_eq!(adapter.calls.lock().unwrap().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn request_log_records_each_request() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("requests.jsonl");
    let (executor, _adapter, _, _server) = fixture_full(
        vec![UpstreamError::RateLimited {
            retry_after: Some(Duration::from_secs(1)),
        }],
        false,
        [Some(1024); 3],
        &[("model", &[0])],
        Default::default(),
        Default::default(),
        false,
        Some(path.clone()),
        [None; 3],
        [None; 3],
    )
    .await;

    assert!(matches!(
        executor.execute(request()).await,
        Err(GatewayError::AllCandidatesExhausted { .. })
    ));
    // The exhausted request reached dep0 before its cooldown; the next one
    // cannot even start an attempt.
    assert!(matches!(
        executor.execute(request()).await,
        Err(GatewayError::AllCandidatesExhausted { .. })
    ));
    tokio::time::advance(Duration::from_secs(2)).await;
    executor.execute(request()).await.unwrap();
    // A middle-out context rejection is a preliminary probe the server retries;
    // it must not produce a record.
    let probe = GatewayRequest {
        est_input_tokens: 10_000,
        context_overflow: Some(ContextOverflow::MiddleOut),
        ..request()
    };
    assert!(matches!(
        executor.execute(probe).await,
        Err(GatewayError::ContextLengthExceeded {
            middle_out: true,
            ..
        })
    ));
    drop(executor);

    let lines: Vec<serde_json::Value> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["outcome"], "error");
    assert_eq!(lines[0]["error_kind"], "all_candidates_exhausted");
    assert_eq!(lines[0]["deployment"], "dep0");
    assert_eq!(lines[0]["provider"], "test");
    assert_eq!(lines[0]["attempts"], 1);
    assert_eq!(lines[1]["outcome"], "error");
    assert_eq!(lines[1]["attempts"], 0);
    assert!(lines[1].get("deployment").is_none());
    assert_eq!(lines[2]["outcome"], "routed");
    assert_eq!(lines[2]["model"], "model");
    assert_eq!(lines[2]["deployment"], "dep0");
    assert_eq!(lines[2]["attempts"], 1);
    assert!(lines[2]["latency_ms"].is_number());
    assert!(lines[2]["ts_ms"].is_number());
    // Client key, image inputs and the provider request id are recorded on
    // every record that reached an upstream.
    for line in &lines {
        assert_eq!(line["client_key"], "key_testfp");
        assert_eq!(line["image_count"], 2);
    }
    assert_eq!(lines[0]["upstream_request_id"], "req_fixture_1");
    assert!(lines[1].get("upstream_request_id").is_none());
    assert_eq!(lines[2]["upstream_request_id"], "req_fixture_1");
}

#[tokio::test(start_paused = true)]
async fn request_log_lists_every_attempt_leg() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("requests.jsonl");
    let (executor, _adapter, _, _server) = fixture_full(
        vec![UpstreamError::RateLimited {
            retry_after: Some(Duration::from_secs(1)),
        }],
        false,
        [Some(1024); 3],
        &[("model", &[0, 2])],
        Default::default(),
        Default::default(),
        false,
        Some(path.clone()),
        [None; 3],
        [None; 3],
    )
    .await;

    let response = executor.execute(request()).await.unwrap();
    assert_eq!(response.deployment.as_ref(), "dep2");
    assert_eq!(response.attempts, 2);
    drop(executor);

    let lines: Vec<serde_json::Value> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 1);
    let record = &lines[0];
    // The record names the serving deployment as before; the legs list keeps
    // the failed leg's provider and deployment too.
    assert_eq!(record["outcome"], "routed");
    assert_eq!(record["deployment"], "dep2");
    assert_eq!(record["attempts"], 2);
    assert_eq!(
        record["legs"],
        serde_json::json!([
            {"deployment": "dep0", "provider": "test", "outcome": "rate_limited"},
            {"deployment": "dep2", "provider": "test", "outcome": "routed"},
        ])
    );
}

#[tokio::test(start_paused = true)]
async fn request_log_omits_legs_when_nothing_was_sent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("requests.jsonl");
    let (executor, _adapter, _, _server) = fixture_full(
        vec![UpstreamError::RateLimited {
            retry_after: Some(Duration::from_secs(60)),
        }],
        false,
        [Some(1024); 3],
        &[("model", &[0])],
        Default::default(),
        Default::default(),
        false,
        Some(path.clone()),
        [None; 3],
        [None; 3],
    )
    .await;

    assert!(matches!(
        executor.execute(request()).await,
        Err(GatewayError::AllCandidatesExhausted { .. })
    ));
    // dep0 is now cooling past the request deadline's budget: the next request
    // makes no upstream attempt at all.
    assert!(matches!(
        executor.execute(request()).await,
        Err(GatewayError::AllCandidatesExhausted { .. })
    ));
    drop(executor);

    let lines: Vec<serde_json::Value> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[1]["attempts"], 0);
    assert!(lines[1].get("deployment").is_none());
    assert_eq!(lines[1]["legs"], serde_json::json!([]));
}

#[tokio::test]
async fn request_log_lists_repair_resends_for_one_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("requests.jsonl");
    let (executor, adapter, _, _server) = fixture_full(
        vec![UpstreamError::BadRequest {
            status: StatusCode::BAD_REQUEST,
            body: Bytes::new(),
        }],
        false,
        [Some(1024); 3],
        &[("model", &[0])],
        Default::default(),
        Default::default(),
        false,
        Some(path.clone()),
        [None; 3],
        [None; 3],
    )
    .await;
    // Repair without cooling dep0 itself.
    *adapter.cool_on_repair.lock().unwrap() = Some(Arc::new(DeploymentLimits::new(
        llmux_types::QuotaSpec::default(),
    )));

    let response = executor.execute(request()).await.unwrap();
    assert_eq!(response.deployment.as_ref(), "dep0");
    assert_eq!(response.attempts, 1);
    assert_eq!(adapter.wire_calls.load(Ordering::Relaxed), 2);
    drop(executor);

    let lines: Vec<serde_json::Value> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 1);
    assert_eq!(
        lines[0]["legs"],
        serde_json::json!([
            {"deployment": "dep0", "provider": "test", "outcome": "bad_request"},
            {"deployment": "dep0", "provider": "test", "outcome": "routed"},
        ])
    );
}

#[tokio::test]
async fn request_log_omits_legs_when_the_build_never_sent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("requests.jsonl");
    let (executor, adapter, _, _server) = fixture_full(
        Vec::new(),
        false,
        [Some(1024); 3],
        &[("model", &[0])],
        Default::default(),
        Default::default(),
        false,
        Some(path.clone()),
        [None; 3],
        [None; 3],
    )
    .await;
    adapter
        .build_errors
        .lock()
        .unwrap()
        .push_back(BuildError::InvalidAuth);

    assert!(matches!(
        executor.execute(request()).await,
        Err(GatewayError::Internal)
    ));
    drop(executor);

    let lines: Vec<serde_json::Value> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["attempts"], 1);
    assert_eq!(lines[0]["error_kind"], "internal");
    assert_eq!(lines[0]["legs"], serde_json::json!([]));
}

#[tokio::test]
async fn deployment_stats_count_outgoing_handoffs() {
    let (mut executor, _adapter, _, _server) = fixture_full(
        vec![
            UpstreamError::Server {
                status: StatusCode::BAD_GATEWAY,
            },
            UpstreamError::RateLimited {
                retry_after: Some(Duration::from_secs(1)),
            },
        ],
        false,
        [Some(1024); 3],
        &[("model", &[0, 1, 2])],
        Default::default(),
        Default::default(),
        false,
        None,
        [None; 3],
        [None; 3],
    )
    .await;
    for binding in &mut executor.bindings {
        binding.stats = Some(Default::default());
    }
    let response = executor.execute(request()).await.unwrap();
    assert_eq!(response.deployment.as_ref(), "dep2");
    let handoffs = |executor: &Executor| {
        executor
            .deployment_stats()
            .iter()
            .map(|dep| serde_json::to_value(dep).unwrap()["handoffs"].clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        serde_json::json!(handoffs(&executor)),
        serde_json::json!([1, 1, 0])
    );
    executor.execute(request()).await.unwrap();
    assert_eq!(
        serde_json::json!(handoffs(&executor)),
        serde_json::json!([1, 1, 0]),
        "a first-attempt route hands nothing off"
    );
}

#[tokio::test]
async fn request_log_prefers_response_header_over_error_body() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("requests.jsonl");
    let (executor, _adapter, _, _server) = fixture_full(
        vec![UpstreamError::BadRequest {
            status: StatusCode::BAD_REQUEST,
            body: Bytes::from_static(br#"{"request_id":"req_body_9"}"#),
        }],
        false,
        [Some(1024); 3],
        &[("model", &[0])],
        Default::default(),
        Default::default(),
        false,
        Some(path.clone()),
        [None; 3],
        [None; 3],
    )
    .await;

    assert!(matches!(
        executor.execute(request()).await,
        Err(GatewayError::Client(UpstreamError::BadRequest { .. }))
    ));
    drop(executor);

    let lines: Vec<serde_json::Value> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["upstream_request_id"], "req_fixture_1");
}

#[tokio::test]
async fn convert_deployment_cooled_during_build_is_skipped_at_the_gate() {
    let (executor, adapter, _, _server) = fixture_with_models(
        Vec::new(),
        false,
        [None; 3],
        &[("model", &[0, 2])],
        Default::default(),
    )
    .await;
    *adapter.cool_on_build.lock().unwrap() = Some(executor.bindings[0].limits.clone());
    let result = executor
        .execute(GatewayRequest {
            format: InboundFormat::AnthropicMessages,
            body: Bytes::from_static(b"plain"),
            ..request()
        })
        .await
        .unwrap();
    assert_eq!(result.deployment.as_ref(), "dep2");
    assert_eq!(adapter.calls.lock().unwrap().len(), 2);
}

/// Unlike a convert deployment, a passthrough deployment already holds its
/// permit when the build starts — the gate must recall even a granted permit
/// when a cooldown lands during the build, or the cooling deployment is sent
/// traffic the spec forbids.
#[tokio::test]
async fn passthrough_deployment_cooled_during_build_is_recalled_at_the_gate() {
    let (executor, adapter, _, _server) = fixture_with_models(
        Vec::new(),
        false,
        [None; 3],
        &[("model", &[0, 2])],
        Default::default(),
    )
    .await;
    *adapter.cool_on_build.lock().unwrap() = Some(executor.bindings[0].limits.clone());
    let result = executor.execute(request()).await.unwrap();
    assert_eq!(result.deployment.as_ref(), "dep2");
}
