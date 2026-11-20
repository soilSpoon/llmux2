use axum::{
    body::{Body, Bytes},
    extract::State,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use futures_util::{stream, StreamExt};
use http::{header, HeaderMap, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use llmux_config::RuntimeConfig;
use llmux_testkit::TestServer;
use serde_json::{json, Value};
use std::{
    convert::Infallible,
    fs,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    sync::Semaphore,
    time::{sleep_until, timeout, Instant},
};

#[derive(Clone)]
enum Reply {
    Limit(u64),
    Quota(Option<u64>),
    Complete,
    ResponseStream { complete: bool },
    AbortStream,
    GatedStream(Arc<Semaphore>),
    BadRequest,
    ContextLength,
    HtmlPayloadTooLarge,
}

struct Mock {
    calls: AtomicUsize,
    key: &'static str,
    model: &'static str,
    first: Reply,
    next: Reply,
}

async fn mock_chat(State(mock): State<Arc<Mock>>, headers: HeaderMap, body: Bytes) -> Response {
    let request: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(request["model"], mock.model);
    assert_eq!(request["messages"][0]["content"], "Hello");
    if request.get("max_completion_tokens").is_some() {
        assert_eq!(request["max_completion_tokens"], 16);
        assert!(request.get("input").is_none());
        assert!(request.get("max_output_tokens").is_none());
    } else {
        assert_eq!(request["vendor_option"]["model"], "leave-me-alone");
    }
    assert_eq!(
        headers[header::AUTHORIZATION],
        format!("Bearer {}", mock.key)
    );
    let reply = if mock.calls.fetch_add(1, Ordering::Relaxed) == 0 {
        mock.first.clone()
    } else {
        mock.next.clone()
    };
    match reply {
        Reply::Quota(seconds) => {
            let mut response = (StatusCode::TOO_MANY_REQUESTS, Json(json!({"error": {"code": "insufficient_quota"}}))).into_response();
            if let Some(seconds) = seconds {
                response.headers_mut().insert(header::RETRY_AFTER, seconds.into());
            }
            response
        }
        Reply::Limit(seconds) => (StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, seconds.to_string())]).into_response(),
        Reply::Complete if request["stream"] == true => (
            [(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")],
            "data: {\"id\":\"chunk-1\"}\r\n\r\ndata: [DONE]\n\n",
        ).into_response(),
        Reply::Complete => Json(json!({
            "id": mock.model,
            "object": "chat.completion",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "Hello!"}, "finish_reason": "stop"}],
        })).into_response(),
        Reply::ResponseStream { complete } => {
            assert_eq!(request["stream"], true);
            assert_eq!(request["stream_options"]["include_usage"], true);
            let chunk = r#"{"id":"chatcmpl-1","model":"vendor-primary","choices":[{"index":0,"delta":{"content":"Hello!"},"finish_reason":"stop"}]}"#;
            let tail = if complete { "data: [DONE]\n\n" } else { "" };
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                format!("data: {chunk}\n\n{tail}"),
            ).into_response()
        }
        Reply::AbortStream => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            "data: {\"id\":\"partial\"}\n\n",
        ).into_response(),
        Reply::GatedStream(gate) => {
            let events = stream::once(async { Ok::<_, Infallible>(Bytes::from_static(b"data: {\"id\":\"chunk-1\"}\n\n")) })
                .chain(stream::once(async move {
                    let _permit = gate.acquire().await.unwrap();
                    Ok::<_, Infallible>(Bytes::from_static(b"data: [DONE]\n\n"))
                }));
            ([(header::CONTENT_TYPE, "text/event-stream")], Body::from_stream(events)).into_response()
        }
        Reply::ContextLength => (
            StatusCode::BAD_REQUEST, Json(json!({"error": {"code": "context_length_exceeded"}})),
        ).into_response(),
        Reply::BadRequest => (
            StatusCode::BAD_REQUEST, Json(json!({"error": {"message": "bad input"}})),
        ).into_response(),
        Reply::HtmlPayloadTooLarge => (
            StatusCode::PAYLOAD_TOO_LARGE, axum::response::Html("<h1>Proxy error</h1>"),
        ).into_response(),
    }
}

struct Fixture {
    primary: Arc<Mock>,
    secondary: Arc<Mock>,
    gateway: TestServer,
    _upstreams: [TestServer; 2],
}

impl Fixture {
    async fn start(first: [Reply; 2], next: [Reply; 2], primary_quota: &str) -> Self {
        Self::start_with_routing(first, next, primary_quota, 4, true).await
    }

    async fn start_with_routing(
        first: [Reply; 2],
        next: [Reply; 2],
        primary_quota: &str,
        max_attempts: usize,
        fallback_on_429: bool,
    ) -> Self {
        let [primary_first, secondary_first] = first;
        let [primary_next, secondary_next] = next;
        let primary = Arc::new(Mock {
            key: "test-primary",
            model: "vendor-primary",
            first: primary_first,
            next: primary_next,
            calls: AtomicUsize::new(0),
        });
        let secondary = Arc::new(Mock {
            key: "test-secondary",
            model: "vendor-secondary",
            first: secondary_first,
            next: secondary_next,
            calls: AtomicUsize::new(0),
        });
        let a = TestServer::start(
            Router::new()
                .route("/vendor/v1/chat/completions", post(mock_chat))
                .with_state(primary.clone()),
        )
        .await
        .unwrap();
        let b = TestServer::start(
            Router::new()
                .route("/v1/chat/completions", post(mock_chat))
                .with_state(secondary.clone()),
        )
        .await
        .unwrap();
        let secrets = tempfile::tempdir().unwrap();
        for key in ["test-primary", "test-secondary", "test-client"] {
            fs::write(secrets.path().join(key), format!("{key}\n")).unwrap();
        }
        let base = secrets.path().display();
        let fallback_on = if fallback_on_429 {
            r#"["rate_limited", "quota_exhausted"]"#
        } else {
            "[]"
        };
        let config = format!(
            r#"
[server]
client_keys = ["file:{base}/test-client"]
request_timeout_ms = 4000

[routing]
max_attempts = {max_attempts}
fallback_on = {fallback_on}

[[credentials]]
id = "primary"
kind = "api_key"
provider = "openai"
key = "file:{base}/test-primary"

[[credentials]]
id = "secondary"
kind = "api_key"
provider = "openai_compat"
key = "file:{base}/test-secondary"

[[models]]
name = "shared"
aliases = ["alias"]
capabilities = {{ stream = true, max_context = 100000 }}

[[models.deployments]]
id = "d2"
credential = "secondary"
base_url = "{}/v1"
upstream_model = "vendor-secondary"
priority = 10

[[models.deployments]]
id = "d1"
credential = "primary"
base_url = "{}/vendor/v1/"
upstream_model = "vendor-primary"
priority = 0
{primary_quota}
"#,
            b.url(),
            a.url()
        );
        let gateway = TestServer::start(
            llmux_server::app(RuntimeConfig::parse(&config).unwrap())
                .await
                .unwrap(),
        )
        .await
        .unwrap();
        Self {
            primary,
            secondary,
            gateway,
            _upstreams: [a, b],
        }
    }

    fn counts(&self) -> [usize; 2] {
        [
            self.primary.calls.load(Ordering::Relaxed),
            self.secondary.calls.load(Ordering::Relaxed),
        ]
    }

    async fn request(&self, key: &str, model: &str, streaming: bool) -> Response<Body> {
        let payload = json!({
            "model": model, "stream": streaming,
            "messages": [{"role": "user", "content": "Hello"}],
            "vendor_option": {"model": "leave-me-alone"},
        });
        self.post("/v1/chat/completions", key, payload.to_string())
            .await
    }

    async fn responses(&self, key: &str, model: &str, stream: bool) -> Response {
        let payload = json!({
            "model": model, "stream": stream, "input": "Hello", "max_output_tokens": 16,
        });
        self.post("/v1/responses", key, payload.to_string()).await
    }

    async fn post(&self, path: &str, key: &str, payload: impl Into<Bytes>) -> Response {
        let request = Request::post(format!("{}{path}", self.gateway.url()))
            .header(header::AUTHORIZATION, format!("Bearer {key}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Full::new(payload.into()))
            .unwrap();
        timeout(
            Duration::from_secs(5),
            Client::builder(TokioExecutor::new())
                .build_http()
                .request(request),
        )
        .await
        .unwrap()
        .unwrap()
        .map(Body::new)
    }
}

fn check_route(response: &Response, deployment: &str, attempts: &str) {
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-llmux-deployment"], deployment);
    assert_eq!(response.headers()["x-llmux-attempts"], attempts);
}

async fn body(response: Response) -> String {
    let bytes = timeout(Duration::from_secs(5), response.into_body().collect())
        .await
        .unwrap()
        .unwrap()
        .to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn responses_json_uses_alias_and_fails_over() {
    let fixture = Fixture::start(
        [Reply::Limit(5), Reply::Complete],
        [Reply::Complete, Reply::Complete],
        "",
    )
    .await;
    let response = fixture.responses("test-client", "alias", false).await;
    check_route(&response, "d2", "2");
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    let output: Value = serde_json::from_str(&body(response).await).unwrap();
    assert_eq!(output["object"], "response");
    assert_eq!(output["status"], "completed");
    assert_eq!(output["output"][0]["content"][0]["text"], "Hello!");
    assert_eq!(fixture.counts(), [1, 1]);
}

#[tokio::test]
async fn responses_sse_preserves_event_names_and_reports_interrupted_streams() {
    for complete in [true, false] {
        let fixture = Fixture::start(
            [Reply::ResponseStream { complete }, Reply::Complete],
            [Reply::Complete, Reply::Complete],
            "",
        )
        .await;
        let response = fixture.responses("test-client", "shared", true).await;
        check_route(&response, "d1", "1");
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/event-stream"
        );
        let text = body(response).await;
        assert!(!text.contains("[DONE]"));
        let events: Vec<_> = text.trim_end().split("\n\n").collect();
        assert!(events[0]
            .lines()
            .any(|line| line == "event: response.created"));
        assert!(events.iter().any(|event| {
            event
                .lines()
                .any(|line| line == "event: response.output_text.delta")
                && event.contains(r#""delta":"Hello!""#)
        }));
        let name = if complete {
            "response.completed"
        } else {
            "error"
        };
        let terminal = events.last().unwrap();
        assert_eq!(
            terminal
                .lines()
                .find_map(|line| line.strip_prefix("event: ")),
            Some(name)
        );
        let payload: Value = serde_json::from_str(
            terminal
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(payload["type"], name);
        assert_eq!(payload["sequence_number"], events.len() - 1);
        if complete {
            assert_eq!(
                payload["response"]["output"][0]["content"][0]["text"],
                "Hello!"
            );
        } else {
            assert_eq!(payload["code"], "stream_broken");
        }
        assert_eq!(fixture.counts(), [1, 0]);
    }
}

#[tokio::test]
async fn responses_rejects_invalid_requests_before_contacting_upstream() {
    let fixture = Fixture::start(
        [Reply::Complete, Reply::Complete],
        [Reply::Complete, Reply::Complete],
        "",
    )
    .await;
    let unauthorized = fixture.responses("wrong", "shared", false).await;
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(unauthorized.headers()[header::WWW_AUTHENTICATE], "Bearer");
    let unknown = fixture.responses("test-client", "missing", false).await;
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    for payload in [
        r#"{"model":"shared","#,
        r#"{"model":"shared","input":123}"#,
        r#"{"model":"shared","input":"Hello","previous_response_id":"resp_old"}"#,
    ] {
        let response = fixture.post("/v1/responses", "test-client", payload).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let payload: Value = serde_json::from_str(&body(response).await).unwrap();
        assert_eq!(payload["error"]["code"], "invalid_request_error");
    }
    let oversized = fixture
        .post(
            "/v1/responses",
            "test-client",
            vec![b' '; 2 * 1024 * 1024 + 1],
        )
        .await;
    assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(fixture.counts(), [0, 0]);
}

async fn verify_failover(streaming: bool) {
    let fixture = Fixture::start(
        [Reply::Limit(5), Reply::Complete],
        [Reply::Complete, Reply::Complete],
        "",
    )
    .await;
    let first = fixture.request("test-client", "alias", streaming).await;
    let resume_at = Instant::now() + Duration::from_secs(5);
    check_route(&first, "d2", "2");
    assert_eq!(fixture.counts(), [1, 1]);
    if streaming {
        assert_eq!(first.headers()[header::CONTENT_TYPE], "text/event-stream");
        assert_eq!(
            body(first).await,
            "data: {\"id\":\"chunk-1\"}\n\ndata: [DONE]\n\n"
        );
    } else {
        let completion: Value = serde_json::from_str(&body(first).await).unwrap();
        assert_eq!(completion["id"], "vendor-secondary");
        assert_eq!(completion["choices"][0]["message"]["content"], "Hello!");
    }
    let during_cooldown = fixture.request("test-client", "shared", streaming).await;
    check_route(&during_cooldown, "d2", "1");
    drop(body(during_cooldown).await);
    assert_eq!(fixture.counts(), [1, 2]);

    sleep_until(resume_at).await;
    let after_cooldown = fixture.request("test-client", "shared", streaming).await;
    check_route(&after_cooldown, "d1", "1");
    drop(body(after_cooldown).await);
    assert_eq!(fixture.counts(), [2, 2]);
    println!("stream={streaming}: d1 429 -> d2 200; d1 skipped during Retry-After: 5; d1 200 after cooldown");
}

#[tokio::test]
async fn quota_exhaustion_cools_down_and_obeys_fallback_policy() {
    for reset in [Some(5), None, Some(u64::MAX)] {
        for fallback in [true, false] {
            let fixture = Fixture::start_with_routing(
                [Reply::Quota(reset), Reply::Complete],
                [Reply::Complete, Reply::Complete],
                "",
                4,
                fallback,
            )
            .await;
            let response = fixture.request("test-client", "shared", false).await;
            if fallback {
                check_route(&response, "d2", "2");
            } else {
                assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
            }
            drop(body(response).await);
            assert_eq!(fixture.counts(), [1, usize::from(fallback)]);
            let response = fixture.request("test-client", "shared", false).await;
            check_route(&response, "d2", "1");
            drop(body(response).await);
            assert_eq!(fixture.counts(), [1, 1 + usize::from(fallback)]);
        }
    }
}

#[tokio::test]
async fn context_length_error_returns_an_openai_client_error() {
    let fixture = Fixture::start(
        [Reply::ContextLength, Reply::Complete],
        [Reply::Complete, Reply::Complete],
        "",
    )
    .await;
    let response = fixture.request("test-client", "shared", false).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: Value = serde_json::from_str(&body(response).await).unwrap();
    assert_eq!(error["error"]["code"], "context_length_exceeded");
    assert_eq!(error["error"]["type"], "invalid_request_error");
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("maximum context length"));
    assert_eq!(fixture.counts(), [1, 0]);
}

#[tokio::test]
async fn non_streaming_429_fails_over_and_recovers() {
    verify_failover(false).await;
}

#[tokio::test]
async fn streaming_429_fails_over_and_recovers() {
    verify_failover(true).await;
}

#[tokio::test]
async fn both_rate_limited_returns_earliest_retry_after() {
    let fixture = Fixture::start(
        [Reply::Limit(9), Reply::Limit(5)],
        [Reply::Complete, Reply::Complete],
        "",
    )
    .await;
    for _ in 0..2 {
        let response = fixture.request("test-client", "shared", false).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[header::RETRY_AFTER], "5");
        let error: Value = serde_json::from_str(&body(response).await).unwrap();
        assert_eq!(error["error"]["code"], "rate_limit_exceeded");
        assert_eq!(fixture.counts(), [1, 1]);
    }
    println!("both deployments returned 429: gateway returned 429 with earliest Retry-After: 5");
}

#[tokio::test]
async fn retry_after_checks_cooldown_and_rpm_together() {
    let fixture = Fixture::start(
        [Reply::Limit(5), Reply::Limit(90)],
        [Reply::Complete, Reply::Complete],
        "quota = { rpm = 1 }",
    )
    .await;
    for _ in 0..2 {
        let response = fixture.request("test-client", "shared", false).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[header::RETRY_AFTER], "60");
        assert_eq!(fixture.counts(), [1, 1]);
    }
}

#[tokio::test]
async fn retry_after_is_zero_if_attempt_budget_left_a_ready_deployment() {
    verify_fresh_request_routing(1, true).await;
}

#[tokio::test]
async fn retry_after_is_zero_if_failover_is_disabled_but_backup_is_ready() {
    verify_fresh_request_routing(4, false).await;
}

async fn verify_fresh_request_routing(max_attempts: usize, fallback_on_429: bool) {
    let fixture = Fixture::start_with_routing(
        [Reply::Limit(5), Reply::Complete],
        [Reply::Complete, Reply::Complete],
        "quota = { rpm = 1 }",
        max_attempts,
        fallback_on_429,
    )
    .await;
    let response = fixture.request("test-client", "shared", false).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()[header::RETRY_AFTER], "0");
    assert_eq!(fixture.counts(), [1, 0]);
    let retry = fixture.request("test-client", "shared", false).await;
    check_route(&retry, "d2", "1");
    assert_eq!(fixture.counts(), [1, 1]);
}

#[tokio::test]
async fn streams_before_upstream_finishes_and_holds_concurrency_slot() {
    let gate = Arc::new(Semaphore::new(0));
    let fixture = Fixture::start(
        [Reply::GatedStream(gate.clone()), Reply::Complete],
        [Reply::Complete, Reply::Complete],
        "quota = { concurrent = 1 }",
    )
    .await;
    let first = fixture.request("test-client", "shared", true).await;
    check_route(&first, "d1", "1");
    let mut stream = first.into_body().into_data_stream();
    let bytes = timeout(Duration::from_secs(1), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(bytes, "data: {\"id\":\"chunk-1\"}\n\n");

    let fallback = fixture.request("test-client", "shared", true).await;
    check_route(&fallback, "d2", "1");
    drop(body(fallback).await);
    gate.add_permits(1);
    let bytes = timeout(Duration::from_secs(1), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(bytes, "data: [DONE]\n\n");
    assert!(stream.next().await.is_none());

    let next = fixture.request("test-client", "shared", true).await;
    check_route(&next, "d1", "1");
    drop(body(next).await);
    assert_eq!(fixture.counts(), [2, 1]);
}

#[tokio::test]
async fn never_retries_after_stream_event() {
    let fixture = Fixture::start(
        [Reply::AbortStream, Reply::Complete],
        [Reply::Complete, Reply::Complete],
        "",
    )
    .await;
    let response = fixture.request("test-client", "shared", true).await;
    check_route(&response, "d1", "1");
    let output = body(response).await;
    assert!(output.starts_with("data: {\"id\":\"partial\"}\n\n"));
    assert!(output.contains("\"code\":\"stream_broken\""));
    assert!(!output.contains("[DONE]"));
    assert_eq!(fixture.counts(), [1, 0]);
}

#[tokio::test]
async fn invalid_client_key_unknown_model_and_upstream_400_do_not_fail_over() {
    let fixture = Fixture::start(
        [Reply::BadRequest, Reply::Complete],
        [Reply::Complete, Reply::Complete],
        "",
    )
    .await;
    let unauthorized = fixture.request("wrong", "shared", false).await;
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    let missing = fixture.request("test-client", "missing", false).await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert_eq!(fixture.counts(), [0, 0]);
    let bad_request = fixture.request("test-client", "shared", false).await;
    assert_eq!(bad_request.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body(bad_request).await,
        r#"{"error":{"message":"bad input"}}"#
    );
    assert_eq!(fixture.counts(), [1, 0]);
}

#[tokio::test]
async fn non_json_upstream_error_preserves_status_and_returns_a_json_error() {
    let fixture = Fixture::start(
        [Reply::HtmlPayloadTooLarge, Reply::Complete],
        [Reply::Complete, Reply::Complete],
        "",
    )
    .await;
    let response = fixture.request("test-client", "shared", false).await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    let output: Value = serde_json::from_str(&body(response).await).unwrap();
    assert_eq!(output["error"]["code"], "upstream_client_error");
    assert_eq!(fixture.counts(), [1, 0]);
}

#[tokio::test]
async fn rpm_limit_skips_primary_without_sending_another_request() {
    let fixture = Fixture::start(
        [Reply::Complete, Reply::Complete],
        [Reply::Complete, Reply::Complete],
        "quota = { rpm = 1 }",
    )
    .await;
    let first = fixture.request("test-client", "shared", false).await;
    check_route(&first, "d1", "1");
    let fallback = fixture.request("test-client", "shared", false).await;
    check_route(&fallback, "d2", "1");
    assert_eq!(fixture.counts(), [1, 1]);
}

#[tokio::test]
async fn context_precheck_skips_small_deployments_and_rejects_without_upstream() {
    let fixture = Fixture::start(
        [Reply::Complete, Reply::Complete],
        [Reply::Complete, Reply::Complete],
        "max_context = 30",
    )
    .await;
    let payload = |max_tokens: u32| {
        json!({
            "model": "shared",
            "messages": [{"role": "user", "content": "Hello"}],
            "vendor_option": {"model": "leave-me-alone"},
            "max_tokens": max_tokens,
        })
        .to_string()
    };
    let response = fixture
        .post("/v1/chat/completions", "test-client", payload(5))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = fixture
        .post("/v1/chat/completions", "test-client", payload(200))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let calls = fixture.counts();
    assert_eq!(calls.iter().sum::<usize>(), 2);
    assert!(calls[1] >= 1);

    let response = fixture
        .post("/v1/chat/completions", "test-client", payload(200_000))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: Value = serde_json::from_str(&body(response).await).unwrap();
    assert_eq!(error["error"]["code"], "context_length_exceeded");
    assert_eq!(error["error"]["type"], "invalid_request_error");
    let message = error["error"]["message"].as_str().unwrap();
    assert!(message.contains("100000 tokens"), "{message}");
    assert!(message.contains("200000 requested"), "{message}");
    assert_eq!(fixture.counts(), calls);
}
