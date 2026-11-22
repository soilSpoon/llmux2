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
    time::{advance, pause, resume, timeout, Instant},
};

#[derive(Clone)]
enum Reply {
    Limit(u64),
    Quota(Option<u64>),
    Complete,
    ExpectedChatRequest(Value),
    NegotiatedStream(Value),
    WithoutGatewayControls,
    InlineUsageStream,
    ResponseStream {
        complete: bool,
    },
    AbortStream,
    EmptyStream,
    GatedStream(Arc<Semaphore>),
    BadRequest,
    Forbidden,
    NotFound(&'static str),
    EmbeddedModelNotFound,
    StructuredBadRequest(Value),
    ContextLength,
    HtmlPayloadTooLarge,
    /// Raw `text/event-stream` body, after the usual request assertions.
    Sse(Arc<str>),
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
        Reply::ExpectedChatRequest(expected) if request != expected => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error":{"message":"unexpected upstream request fields"}})),
        ).into_response(),
        Reply::Quota(seconds) => {
            let mut response = (StatusCode::TOO_MANY_REQUESTS, Json(json!({"error": {"code": "insufficient_quota"}}))).into_response();
            if let Some(seconds) = seconds {
                response.headers_mut().insert(header::RETRY_AFTER, seconds.into());
            }
            response
        }
        Reply::Limit(seconds) => (StatusCode::TOO_MANY_REQUESTS, [
            ("retry-after", seconds.to_string()),
            ("x-ratelimit-remaining-requests", "0".to_owned()),
            ("x-ratelimit-reset-requests", "60s".to_owned()),
        ]).into_response(),
        Reply::Forbidden => StatusCode::FORBIDDEN.into_response(),
        Reply::Complete | Reply::ExpectedChatRequest(_) if request["stream"] == true => (
            [(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")],
            "data: {\"id\":\"chunk-1\"}\r\n\r\ndata: [DONE]\n\n",
        ).into_response(),
        Reply::Complete | Reply::ExpectedChatRequest(_) => Json(json!({
            "id": mock.model,
            "object": "chat.completion",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "Hello!"}, "finish_reason": "stop"}],
        })).into_response(),
        Reply::NegotiatedStream(expected) => {
            assert_eq!(request, expected);
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                concat!(
                    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":null}\n\n",
                    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":2,\"total_tokens\":10}}\n\n",
                    "data: [DONE]\n\n",
                ),
            ).into_response()
        }
        Reply::WithoutGatewayControls => {
            for field in ["min_discount_percent", "zdr", "ranking"] {
                if request.get(field).is_some() {
                    return (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({"error":{"message":"unknown field"}}))).into_response();
                }
            }
            if request.get("max_completion_tokens").is_none() {
                assert_eq!(request["vendor_option"]["zdr"], true);
                assert_eq!(request["vendor_option"]["ranking"], "nested");
            }
            if request["stream"] == true {
                ([(header::CONTENT_TYPE, "text/event-stream")], "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n").into_response()
            } else {
                Json(json!({
                    "id":"c","choices":[{"index":0,"message":{"role":"assistant","content":"Hi"},"finish_reason":"stop"}]
                })).into_response()
            }
        }
        Reply::InlineUsageStream => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            concat!(
                "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"}}]}\n\n",
                "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":2,\"total_tokens\":10}}\n\n",
                "data: {\"id\":\"c\",\"choices\":[],\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":2,\"total_tokens\":10}}\n\n",
                "data: [DONE]\n\n",
            ),
        ).into_response(),
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
        Reply::EmptyStream => ([(header::CONTENT_TYPE, "text/event-stream")], "").into_response(),
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
        Reply::NotFound(code) => (
            StatusCode::NOT_FOUND, Json(json!({"error":{"message":"not found","code":code}})),
        ).into_response(),
        Reply::EmbeddedModelNotFound => Json(json!({"error":{"code":"model_not_found"}})).into_response(),
        Reply::StructuredBadRequest(payload) => (StatusCode::BAD_REQUEST, Json(payload)).into_response(),
        Reply::HtmlPayloadTooLarge => (
            StatusCode::PAYLOAD_TOO_LARGE, axum::response::Html("<h1>Proxy error</h1>"),
        ).into_response(),
        Reply::Sse(body) => (
            [(header::CONTENT_TYPE, "text/event-stream")],
            body.to_string(),
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
        Self::start_with_options(
            first,
            next,
            primary_quota,
            max_attempts,
            fallback_on_429,
            false,
        )
        .await
    }

    async fn start_with_options(
        first: [Reply; 2],
        next: [Reply; 2],
        primary_quota: &str,
        max_attempts: usize,
        fallback_on_429: bool,
        dashboard: bool,
    ) -> Self {
        Self::start_with_exposure(
            first,
            next,
            primary_quota,
            max_attempts,
            fallback_on_429,
            dashboard,
            true,
        )
        .await
    }

    async fn start_with_exposure(
        first: [Reply; 2],
        next: [Reply; 2],
        primary_quota: &str,
        max_attempts: usize,
        fallback_on_429: bool,
        dashboard: bool,
        expose_routing_headers: bool,
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
        let dashboard = if dashboard {
            fs::write(secrets.path().join("dashboard"), "test-dashboard\n").unwrap();
            format!("dashboard_key = \"file:{base}/dashboard\"")
        } else {
            String::new()
        };
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
expose_routing_headers = {expose_routing_headers}
{dashboard}

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

    async fn skip_reasons(&self, id: &str) -> Value {
        let response = self.get("/dashboard/stats", Some("test-dashboard")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let stats: Value = serde_json::from_str(&body(response).await).unwrap();
        stats["deployments"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == id)
            .unwrap()["skip_reasons"]
            .clone()
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

    async fn get(&self, path: &str, key: Option<&str>) -> Response {
        let mut request = Request::get(format!("{}{path}", self.gateway.url()));
        if let Some(key) = key {
            request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
        }
        Client::builder(TokioExecutor::new())
            .build_http()
            .request(request.body(Full::<Bytes>::default()).unwrap())
            .await
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
async fn deployment_custom_headers_reach_upstream_on_every_request() {
    let upstream = TestServer::start(Router::new().route(
        "/v1/chat/completions",
        post(|headers: HeaderMap| async move {
            assert_eq!(headers["x-static"], "literal value");
            assert_eq!(headers["authorization"], "custom-secret");
            Json(json!({"choices":[{"message":{"role":"assistant","content":"ok"}}]}))
        }),
    ))
    .await
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("header"), "custom-secret\n").unwrap();
    let config = format!(
        r#"
[server]
client_keys = ["file:{}/header"]
[[models]]
name = "custom"
[[models.deployments]]
id = "custom"
auth = "none"
provider = "openai_compat"
base_url = "{}/v1"
upstream_model = "custom"
headers = {{ x-static = "literal value", Authorization = "file:{}/header" }}
"#,
        dir.path().display(),
        upstream.url(),
        dir.path().display()
    );
    let gateway = TestServer::start(
        llmux_server::app(RuntimeConfig::parse(&config).unwrap())
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    for _ in 0..2 {
        let request = Request::post(format!("{}/v1/chat/completions", gateway.url()))
            .header(header::AUTHORIZATION, "Bearer custom-secret")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from_static(
                br#"{"model":"custom","messages":[{"role":"user","content":"Hello"}]}"#,
            )))
            .unwrap();
        assert_eq!(
            client.request(request).await.unwrap().status(),
            StatusCode::OK
        );
    }
}

#[tokio::test]
async fn healthz_is_public() {
    let fixture = Fixture::start_with_options(
        [Reply::Complete, Reply::Complete],
        [Reply::Complete, Reply::Complete],
        "",
        2,
        true,
        true,
    )
    .await;
    let before = body(
        fixture
            .get("/dashboard/stats", Some("test-dashboard"))
            .await,
    )
    .await;
    let response = fixture.get("/healthz", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body(response).await, "ok");
    let after = body(
        fixture
            .get("/dashboard/stats", Some("test-dashboard"))
            .await,
    )
    .await;
    let before: Value = serde_json::from_str(&before).unwrap();
    let after: Value = serde_json::from_str(&after).unwrap();
    assert_eq!(
        before.get("responses").unwrap(),
        after.get("responses").unwrap()
    );
    assert_eq!(
        before.get("deployments").unwrap(),
        after.get("deployments").unwrap()
    );
}

#[tokio::test]
async fn model_discovery_lists_exact_routable_ids_and_requires_authentication() {
    let fixture = Fixture::start(
        [Reply::Complete, Reply::Complete],
        [Reply::Complete, Reply::Complete],
        "",
    )
    .await;
    let denied = fixture.get("/v1/models", None).await;
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(denied.headers()[header::WWW_AUTHENTICATE], "Bearer");
    let response = fixture.get("/v1/models", Some("test-client")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let list: Value = serde_json::from_str(&body(response).await).unwrap();
    assert_eq!(
        list,
        json!({"object": "list", "data": [
            {"id":"alias","object":"model","created":0,"owned_by":"llmux2"},
            {"id":"shared","object":"model","created":0,"owned_by":"llmux2"}
        ]})
    );
    assert_eq!(fixture.counts(), [0, 0]);
    for model in list["data"].as_array().unwrap() {
        let response = fixture
            .request("test-client", model["id"].as_str().unwrap(), false)
            .await;
        check_route(&response, "d1", "1");
    }
}

#[tokio::test]
async fn openai_chat_preserves_documented_and_extension_parameters_and_usage() {
    let request = json!({
        "model":"alias","messages":[{"role":"user","content":"Hello"}],
        "stream":true,"stream_options":{"include_usage":true},"max_completion_tokens":16,
        "tools":[{"type":"function","function":{"name":"lookup","parameters":{"type":"object"}}}],
        "tool_choice":"auto","response_format":{"type":"json_object"},"reasoning_effort":"low",
        "reasoning":{"effort":"low"},"user":"opaque-id","seed":7,"n":1,"logprobs":true,
        "vendor_option":{"model":"leave-me-alone"}
    });
    let mut expected = request.clone();
    expected["model"] = json!("vendor-primary");
    let fixture = Fixture::start(
        [Reply::NegotiatedStream(expected), Reply::Complete],
        [Reply::Complete, Reply::Complete],
        "",
    )
    .await;
    let response = fixture
        .post("/v1/chat/completions", "test-client", request.to_string())
        .await;
    check_route(&response, "d1", "1");
    let output = body(response).await;
    assert!(output.ends_with(
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":2,\"total_tokens\":10}}\n\ndata: [DONE]\n\n"
    ));
}

#[tokio::test]
async fn cheaperinference_controls_are_not_sent_to_upstream() {
    for (endpoint, fields) in [
        (
            "/v1/chat/completions",
            json!({"messages":[{"role":"user","content":"Hello"}],
                "vendor_option":{"model":"leave-me-alone","zdr":true,"ranking":"nested"}}),
        ),
        (
            "/v1/responses",
            json!({"input":"Hello","max_output_tokens":16}),
        ),
    ] {
        let fixture = Fixture::start(
            [Reply::WithoutGatewayControls, Reply::Complete],
            [Reply::WithoutGatewayControls, Reply::Complete],
            "",
        )
        .await;
        for stream in [false, true] {
            let mut request = fields.clone();
            request["model"] = json!("alias");
            request["stream"] = json!(stream);
            request["min_discount_percent"] = json!(99.99);
            request["zdr"] = json!(true);
            request["ranking"] = Value::Null;
            let payload = request.to_string().replace("\"zdr\":", r#""\u007adr":"#);
            let response = fixture.post(endpoint, "test-client", payload).await;
            check_route(&response, "d1", "1");
            assert!(body(response).await.contains("Hi"));
        }
    }
}

#[tokio::test]
async fn chat_strips_rejected_fields_only_for_the_configured_deployment() {
    for (strip, stream) in [(true, false), (true, true), (false, false), (false, true)] {
        let mut request = json!({
            "model":"alias", "stream":stream, "messages":[{"role":"user","content":"Hello"}],
            "seed":42, "user":"caller", "safety_identifier":"hashed-id",
            "logprobs":true, "top_logprobs":2, "reasoning":{"effort":"low"},
            "reasoning_effort":"high",
            "vendor_option":{"model":"leave-me-alone","seed":9,"reasoning":{"effort":"keep"}}
        });
        if stream {
            request["reasoning"]["vendor_option"] = json!("keep");
        }
        let mut expected = request.clone();
        expected["model"] = json!("vendor-primary");
        if strip {
            for field in [
                "seed",
                "user",
                "safety_identifier",
                "logprobs",
                "top_logprobs",
            ] {
                expected.as_object_mut().unwrap().remove(field);
            }
            if stream {
                expected["reasoning"] = json!({"vendor_option":"keep"});
            } else {
                expected.as_object_mut().unwrap().remove("reasoning");
            }
        }
        let fixture = Fixture::start(
            [Reply::ExpectedChatRequest(expected), Reply::Complete],
            [Reply::Complete, Reply::Complete],
            if strip {
                r#"strip_request_fields = ["seed", "user", "safety_identifier", "reasoning.effort", "logprobs", "top_logprobs"]"#
            } else {
                ""
            },
        )
        .await;
        let response = fixture
            .post("/v1/chat/completions", "test-client", request.to_string())
            .await;
        check_route(&response, "d1", "1");
        let output = body(response).await;
        assert!(output.contains(if stream { "data: [DONE]" } else { "Hello!" }));
        assert_eq!(fixture.counts(), [1, 0]);
    }
}

#[tokio::test]
async fn openai_chat_enforces_explicit_stream_usage_options() {
    let fixture = Fixture::start(
        [Reply::InlineUsageStream, Reply::Complete],
        [Reply::InlineUsageStream, Reply::Complete],
        "",
    )
    .await;
    for include_usage in [false, true] {
        let response = fixture
            .post(
                "/v1/chat/completions",
                "test-client",
                json!({
                    "model":"alias","messages":[{"role":"user","content":"Hello"}],
                    "stream":true,"stream_options":{"include_usage":include_usage},
                    "vendor_option":{"model":"leave-me-alone"}
                })
                .to_string(),
            )
            .await;
        check_route(&response, "d1", "1");
        let output = body(response).await;
        let mut data: Vec<_> = output
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .collect();
        assert_eq!(data.pop(), Some("[DONE]"));
        let chunks: Vec<Value> = data
            .into_iter()
            .map(|chunk| serde_json::from_str(chunk).unwrap())
            .collect();
        assert_eq!(chunks.len(), if include_usage { 3 } else { 2 });
        assert_eq!(chunks[0]["choices"][0]["delta"]["content"], "Hi");
        assert_eq!(chunks[1]["choices"][0]["finish_reason"], "stop");
        for chunk in &chunks[..2] {
            assert_eq!(chunk.get("usage"), include_usage.then_some(&Value::Null));
        }
        if include_usage {
            assert_eq!(chunks[2]["choices"], json!([]));
            assert_eq!(chunks[2]["usage"]["total_tokens"], 10);
            assert_eq!(chunks[2]["id"], "c");
        }
    }
}

#[tokio::test]
async fn dashboard_routes_are_absent_unless_configured() {
    let fixture = Fixture::start(
        [Reply::Complete, Reply::Complete],
        [Reply::Complete, Reply::Complete],
        "",
    )
    .await;
    for path in [
        "/dashboard",
        "/dashboard/stats",
        "/dashboard/app.css",
        "/dashboard/app.js",
    ] {
        assert_eq!(
            fixture.get(path, None).await.status(),
            StatusCode::NOT_FOUND
        );
    }
}

#[tokio::test]
async fn dashboard_reports_fallback_and_committed_stream_errors_without_exposing_secrets() {
    let fixture = Fixture::start_with_options(
        [Reply::Limit(60), Reply::Complete],
        [Reply::Complete, Reply::AbortStream],
        "quota = { rpm = 20, concurrent = 2 }",
        4,
        true,
        true,
    )
    .await;
    let page = fixture.get("/dashboard", None).await;
    assert_eq!(page.status(), StatusCode::OK);
    assert!(page.headers()[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap()
        .contains("frame-ancestors 'none'"));
    let page = body(page).await;
    assert!(page.contains("<html lang=\"ko\">"));
    assert!(page.contains("게이트웨이 현황"));
    assert!(body(fixture.get("/dashboard/app.js", None).await)
        .await
        .contains("/dashboard/stats"));
    for key in [None, Some("test-client"), Some("wrong")] {
        let response = fixture.get("/dashboard/stats", key).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }
    let mut bad = fixture.request("test-dashboard", "alias", false).await;
    assert_eq!(bad.status(), StatusCode::UNAUTHORIZED);
    bad = fixture.request("test-client", "unknown", false).await;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    check_route(
        &fixture.request("test-client", "alias", false).await,
        "d2",
        "2",
    );
    let stream = fixture.request("test-client", "alias", true).await;
    check_route(&stream, "d2", "1");
    assert!(body(stream).await.contains("stream_broken"));
    assert_eq!(fixture.counts(), [1, 2]);

    let response = fixture
        .get("/dashboard/stats", Some("test-dashboard"))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let output = body(response).await;
    for private in [
        "test-primary",
        "test-secondary",
        "test-client",
        "test-dashboard",
        "127.0.0.1",
        "vendor-primary",
        "Hello",
    ] {
        assert!(!output.contains(private));
    }
    let stats: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(stats["responses"]["ok"], 2);
    assert_eq!(stats["responses"]["rejected"], 1);
    assert_eq!(stats["responses"]["unavailable"], 0);
    assert_eq!(stats["responses"]["failed"], 0);
    assert!(stats["responses"]["ready_us"].as_u64().unwrap() > 0);

    let rows = stats["deployments"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["model"], "shared");
    assert_eq!(rows[0]["id"], "d1");
    assert_eq!(rows[0]["routed"], 0);
    assert_eq!(rows[0]["failed_attempts"], 1);
    assert_eq!(rows[0]["failure_classes"], json!({"rate_limited":1}));
    assert_eq!(rows[0]["skip_reasons"], json!({"cooldown":1}));
    assert_eq!(rows[0]["rpm_limit"], 20);
    assert_eq!(rows[0]["in_flight"], 0);
    assert!(rows[0]["cooldown_ms"].as_u64().unwrap() > 0);
    assert_eq!(rows[1]["id"], "d2");
    assert_eq!(rows[1]["routed"], 2);
    assert_eq!(rows[1]["failed_attempts"], 0);
    assert_eq!(rows[1]["stream_errors"], 1);
    assert_eq!(rows[1]["failure_classes"], json!({"stream_broken":1}));
    assert!(rows[1]["cooldown_ms"].is_null());
}

#[tokio::test]
async fn gateway_error_headers_attribute_attempts_only_when_exposed() {
    for expose in [true, false] {
        let fixture = Fixture::start_with_exposure(
            [Reply::Limit(5), Reply::BadRequest],
            [Reply::Complete, Reply::Complete],
            "",
            4,
            true,
            false,
            expose,
        )
        .await;
        let response = fixture.request("test-client", "shared", false).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(fixture.counts(), [1, 1]);
        assert_eq!(
            response
                .headers()
                .get("x-llmux-deployment")
                .map(|v| v.to_str().unwrap()),
            expose.then_some("d2")
        );
        assert_eq!(
            response
                .headers()
                .get("x-llmux-attempts")
                .map(|v| v.to_str().unwrap()),
            expose.then_some("2")
        );
        let unknown = fixture.request("test-client", "unknown", false).await;
        assert!(!unknown.headers().contains_key("x-llmux-deployment"));
        assert!(!unknown.headers().contains_key("x-llmux-attempts"));
    }
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
async fn responses_sse_429_fails_over() {
    let fixture = Fixture::start(
        [Reply::Limit(60), Reply::ResponseStream { complete: true }],
        [Reply::Complete, Reply::Complete],
        "",
    )
    .await;
    let response = fixture.responses("test-client", "alias", true).await;
    check_route(&response, "d2", "2");
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
    let terminal = events.last().unwrap();
    assert!(terminal
        .lines()
        .any(|line| line == "event: response.completed"));
    let payload: Value = serde_json::from_str(
        terminal
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(payload["type"], "response.completed");
    assert_eq!(payload["sequence_number"], events.len() - 1);
    assert_eq!(payload["response"]["status"], "completed");
    assert_eq!(
        payload["response"]["output"][0]["content"][0]["text"],
        "Hello!"
    );
    assert_eq!(fixture.counts(), [1, 1]);
}

#[tokio::test]
async fn responses_json_skips_primary_during_429_cooldown() {
    let fixture = Fixture::start(
        [Reply::Limit(60), Reply::Complete],
        [Reply::Complete, Reply::Complete],
        "",
    )
    .await;
    let first = fixture.responses("test-client", "alias", false).await;
    check_route(&first, "d2", "2");
    drop(body(first).await);
    assert_eq!(fixture.counts(), [1, 1]);

    let response = fixture.responses("test-client", "shared", false).await;
    check_route(&response, "d2", "1");
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    let output: Value = serde_json::from_str(&body(response).await).unwrap();
    assert_eq!(output["object"], "response");
    assert_eq!(output["status"], "completed");
    assert_eq!(output["output"][0]["content"][0]["text"], "Hello!");
    assert_eq!(fixture.counts(), [1, 2]);
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
        assert!(!response.headers().contains_key("x-llmux-deployment"));
        assert!(!response.headers().contains_key("x-llmux-attempts"));
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

    pause();
    advance(resume_at.saturating_duration_since(Instant::now())).await;
    resume();
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
        assert!(!response
            .headers()
            .contains_key("x-ratelimit-remaining-requests"));
        assert!(!response
            .headers()
            .contains_key("x-ratelimit-reset-requests"));
        let error: Value = serde_json::from_str(&body(response).await).unwrap();
        assert_eq!(error["error"]["code"], "rate_limit_exceeded");
        assert_eq!(fixture.counts(), [1, 1]);
    }
    println!("both deployments returned 429: gateway returned 429 with earliest Retry-After: 5");
}

#[tokio::test]
async fn retry_after_is_omitted_if_no_compatible_deployment_can_be_retried() {
    let fixture = Fixture::start(
        [Reply::Forbidden, Reply::Forbidden],
        [Reply::Complete, Reply::Complete],
        "",
    )
    .await;
    for _ in 0..2 {
        let response = fixture.request("test-client", "shared", false).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(!response.headers().contains_key(header::RETRY_AFTER));
    }
    assert_eq!(fixture.counts(), [1, 1]);
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
async fn only_explicit_upstream_model_not_found_fails_over() {
    for stream in [false, true] {
        for (reply, status) in [
            (Reply::BadRequest, StatusCode::BAD_REQUEST),
            (Reply::NotFound("not_found"), StatusCode::NOT_FOUND),
            (Reply::NotFound("model_not_found"), StatusCode::OK),
        ] {
            let fixture = Fixture::start(
                [reply, Reply::Complete],
                [Reply::Complete, Reply::Complete],
                "",
            )
            .await;
            let response = fixture.request("test-client", "shared", stream).await;
            assert_eq!(response.status(), status);
            if status == StatusCode::OK {
                check_route(&response, "d2", "2");
            }
            drop(body(response).await);
            assert_eq!(fixture.counts(), [1, usize::from(status == StatusCode::OK)]);
        }
    }
}

#[tokio::test]
async fn embedded_upstream_model_not_found_fails_over() {
    let fixture = Fixture::start(
        [Reply::EmbeddedModelNotFound, Reply::Complete],
        [Reply::Complete, Reply::Complete],
        "",
    )
    .await;
    let response = fixture.request("test-client", "shared", false).await;
    check_route(&response, "d2", "2");
    let payload: Value = serde_json::from_str(&body(response).await).unwrap();
    assert_eq!(payload["id"], "vendor-secondary");
    assert_eq!(fixture.counts(), [1, 1]);
}

#[tokio::test]
async fn stream_closed_before_first_event_fails_over() {
    let fixture = Fixture::start(
        [Reply::EmptyStream, Reply::Complete],
        [Reply::Complete, Reply::Complete],
        "",
    )
    .await;
    let response = fixture.request("test-client", "shared", true).await;
    assert_eq!(response.status(), StatusCode::OK);
    check_route(&response, "d2", "2");
    assert!(body(response).await.contains("[DONE]"));
    assert_eq!(fixture.counts(), [1, 1]);
}

#[tokio::test]
async fn chat_reports_omniroute_errors_for_invalid_temperature_and_unknown_model() {
    let fixture = Fixture::start(
        [Reply::Complete, Reply::Complete],
        [Reply::Complete, Reply::Complete],
        "",
    )
    .await;
    for stream in [false, true] {
        for (model, temperature, message) in [
            ("shared", json!(3), "temperature: must be between 0 and 2"),
            ("shared", json!("hot"), "temperature: must be a number"),
            ("unknown", json!(1), "Invalid model format"),
        ] {
            let request = json!({"model":model,"stream":stream,"temperature":temperature,
                "messages":[{"role":"user","content":"Hello"}],"vendor_option":{"model":"leave-me-alone"}});
            let response = fixture
                .post("/v1/chat/completions", "test-client", request.to_string())
                .await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{message}");
            let error: Value = serde_json::from_str(&body(response).await).unwrap();
            assert_eq!(
                error,
                json!({"error":{"message":message,"type":"invalid_request_error","code":"bad_request"}})
            );
        }
    }
    assert_eq!(fixture.counts(), [0, 0]);
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
    assert_eq!(missing.status(), StatusCode::BAD_REQUEST);
    assert_eq!(fixture.counts(), [0, 0]);
    let bad_request = fixture.request("test-client", "shared", false).await;
    assert_eq!(bad_request.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        serde_json::from_str::<Value>(&body(bad_request).await).unwrap(),
        json!({"error":{"message":"bad input","type":"invalid_request_error","param":null,"code":"upstream_client_error"}})
    );
    assert_eq!(fixture.counts(), [1, 0]);
}

#[tokio::test]
async fn upstream_error_preserves_details_without_a_string_message() {
    let first = json!({
        "error": {
            "type":"invalid_request_error","code":"unsupported_parameter",
            "param":"reasoning_effort","provider_detail":{"supported":["low"]}
        },
        "request_id":"vendor-123"
    });
    let mut next = first.clone();
    next["error"]["message"] = json!(123);
    let fixture = Fixture::start(
        [Reply::StructuredBadRequest(first.clone()), Reply::Complete],
        [Reply::StructuredBadRequest(next), Reply::Complete],
        "",
    )
    .await;
    let mut expected = first;
    expected["error"]["message"] = json!("Upstream rejected the request");
    for _ in 0..2 {
        let response = fixture.request("test-client", "shared", false).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            serde_json::from_str::<Value>(&body(response).await).unwrap(),
            expected
        );
    }
    assert_eq!(fixture.counts(), [2, 0]);
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
    let fixture = Fixture::start_with_options(
        [Reply::Complete, Reply::Complete],
        [Reply::Complete, Reply::Complete],
        "quota = { rpm = 1 }",
        4,
        true,
        true,
    )
    .await;
    let first = fixture.request("test-client", "shared", false).await;
    check_route(&first, "d1", "1");
    let fallback = fixture.request("test-client", "shared", false).await;
    check_route(&fallback, "d2", "1");
    assert_eq!(fixture.counts(), [1, 1]);
    assert_eq!(
        fixture.skip_reasons("d1").await,
        json!({"admission_limited":1})
    );
}

#[tokio::test]
async fn converted_requests_record_unsupported_conversion_and_admission_skips() {
    let fixture = Fixture::start_with_options(
        [Reply::Complete, Reply::Complete],
        [Reply::Complete, Reply::Complete],
        "quota = { rpm = 1 }",
        4,
        true,
        true,
    )
    .await;
    let rejected = fixture.post("/v1/messages", "test-client", r#"{
        "model":"shared","max_tokens":16,"messages":[{"role":"user","content":[
            {"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"AA=="}}
        ]}]
    }"#).await;
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    assert_eq!(fixture.counts(), [0, 0]);

    let first = fixture.responses("test-client", "shared", false).await;
    check_route(&first, "d1", "1");
    let fallback = fixture.responses("test-client", "shared", false).await;
    check_route(&fallback, "d2", "1");
    assert_eq!(fixture.counts(), [1, 1]);
    assert_eq!(
        fixture.skip_reasons("d1").await,
        json!({"unsupported_conversion":1,"admission_limited":1})
    );
    assert_eq!(
        fixture.skip_reasons("d2").await,
        json!({"unsupported_conversion":1})
    );
}

#[tokio::test]
async fn expired_deployment_skip_is_attributed_without_an_attempt() {
    let fixture = Fixture::start_with_options(
        [Reply::Complete, Reply::Complete],
        [Reply::Complete, Reply::Complete],
        "expires_at = 2000-01-01T00:00:00Z",
        4,
        true,
        true,
    )
    .await;
    let response = fixture.request("test-client", "shared", false).await;
    check_route(&response, "d2", "1");
    assert_eq!(fixture.counts(), [0, 1]);
    assert_eq!(fixture.skip_reasons("d1").await, json!({"expired":1}));

    let response = fixture
        .post(
            "/v1/chat/completions",
            "test-client",
            r#"{
        "model":"shared","messages":[{"role":"user","content":"Hello"}],"max_tokens":200000
    }"#,
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(fixture.counts(), [0, 1]);
    assert_eq!(fixture.skip_reasons("d1").await, json!({"expired":2}));
    assert_eq!(
        fixture.skip_reasons("d2").await,
        json!({"context_length":1})
    );
}

#[tokio::test]
async fn context_precheck_skips_small_deployments_and_rejects_without_upstream() {
    let fixture = Fixture::start_with_options(
        [Reply::Complete, Reply::Complete],
        [Reply::Complete, Reply::Complete],
        "max_context = 30",
        4,
        true,
        true,
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
    assert_eq!(
        fixture.skip_reasons("d1").await,
        json!({"context_length":2})
    );
    assert_eq!(
        fixture.skip_reasons("d2").await,
        json!({"context_length":1})
    );
}

#[tokio::test]
async fn middle_out_truncation_is_opt_in_and_reported() {
    let fixture = Fixture::start(
        [Reply::Complete, Reply::Complete],
        [Reply::Complete, Reply::Complete],
        "max_context = 30",
    )
    .await;
    let payload = json!({
        "model": "shared",
        "max_tokens": 99_970,
        "messages": [
            {"role": "system", "content": "Hello"},
            {"role": "user", "content": "x".repeat(200)},
            {"role": "assistant", "content": "y".repeat(200)},
            {"role": "user", "content": "latest"},
        ],
        "vendor_option": {"model": "leave-me-alone"},
    })
    .to_string();
    let post = |overflow: Option<&'static str>| {
        let mut request = Request::post(format!("{}/v1/chat/completions", fixture.gateway.url()))
            .header(header::AUTHORIZATION, "Bearer test-client")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(overflow) = overflow {
            request = request.header("x-llmux-context-overflow", overflow);
        }
        let request = request
            .body(Full::new(Bytes::from(payload.clone())))
            .unwrap();
        async move {
            timeout(
                Duration::from_secs(5),
                Client::builder(TokioExecutor::new())
                    .build_http()
                    .request(request),
            )
            .await
            .unwrap()
            .unwrap()
        }
    };
    for overflow in [None, Some("reject")] {
        let response = post(overflow).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(response.headers().get("x-llmux-dropped-messages").is_none());
    }
    assert_eq!(post(Some("drop")).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(fixture.counts(), [0, 0]);

    let response = post(Some("middle-out")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-llmux-dropped-messages"], "2");
    assert_eq!(response.headers()["x-llmux-deployment"], "d2");
    assert_eq!(fixture.counts(), [0, 1]);
}

const KEEPALIVE_FRAME: &str = "data: {\"id\":\"chatcmpl-keepalive\",\"object\":\"chat.completion.chunk\",\"model\":\"gemini-3-flash\",\"choices\":[]}\n\n";
const RATE_LIMIT_FRAME: &str = "data: {\"error\":{\"message\":\"antigravity/gemini-3-flash: rate limit - [429]: Antigravity refused this exact request with 429 RESOURCE_EXHAUSTED\",\"type\":\"rate_limit_error\",\"code\":429}}\n\n";
const ROLE_FRAME: &str = "data: {\"id\":\"chatcmpl-role\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\n";

fn sse(body: &str) -> Reply {
    Reply::Sse(Arc::from(body))
}

fn three_keepalives_then_rate_limit() -> Reply {
    sse(&format!(
        "{KEEPALIVE_FRAME}{KEEPALIVE_FRAME}{KEEPALIVE_FRAME}{RATE_LIMIT_FRAME}"
    ))
}

#[tokio::test]
async fn stream_keepalive_then_rate_limit_frame_fails_over() {
    let fixture = Fixture::start(
        [three_keepalives_then_rate_limit(), Reply::Complete],
        [Reply::Complete, Reply::Complete],
        "",
    )
    .await;
    let response = fixture.request("test-client", "alias", true).await;
    check_route(&response, "d2", "2");
    assert_eq!(
        body(response).await,
        "data: {\"id\":\"chunk-1\"}\n\ndata: [DONE]\n\n"
    );
    assert_eq!(fixture.counts(), [1, 1]);

    let again = fixture.request("test-client", "alias", true).await;
    check_route(&again, "d2", "1");
    drop(body(again).await);
    assert_eq!(
        fixture.counts(),
        [1, 2],
        "the deployment that streamed a 429 frame must cool down"
    );
}

#[tokio::test]
async fn stream_role_preamble_then_rate_limit_fails_over() {
    let content = "data: {\"id\":\"chatcmpl-role\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"}}]}\n\n";
    let backup = format!("{ROLE_FRAME}{content}data: [DONE]\n\n");
    let fixture = Fixture::start(
        [
            sse(&format!("{ROLE_FRAME}{RATE_LIMIT_FRAME}")),
            sse(&backup),
        ],
        [Reply::Complete, Reply::Complete],
        "",
    )
    .await;
    let response = fixture.request("test-client", "alias", true).await;
    check_route(&response, "d2", "2");
    assert_eq!(body(response).await, backup);
    assert_eq!(fixture.counts(), [1, 1]);
}

#[tokio::test]
async fn stream_error_after_content_is_not_retried() {
    let fixture = Fixture::start(
        [
            sse(&format!(
                "{KEEPALIVE_FRAME}data: {{\"id\":\"chatcmpl-1\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"Hi\"}},\"finish_reason\":\"stop\"}}]}}\n\n{RATE_LIMIT_FRAME}"
            )),
            Reply::Complete,
        ],
        [Reply::Complete, Reply::Complete],
        "",
    )
    .await;
    let response = fixture.request("test-client", "alias", true).await;
    check_route(&response, "d1", "1");
    assert_eq!(
        body(response).await,
        concat!(
            "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"error\":{\"message\":\"Upstream stream ended unexpectedly\",\"type\":\"upstream_error\",\"param\":null,\"code\":\"stream_broken\"}}\n\n",
        )
    );
    assert_eq!(fixture.counts(), [1, 0]);

    let again = fixture.request("test-client", "alias", true).await;
    check_route(&again, "d2", "1");
    drop(body(again).await);
    assert_eq!(fixture.counts(), [1, 1]);
}

#[tokio::test]
async fn stream_keepalives_before_content_are_dropped_and_later_ones_forwarded() {
    let fixture = Fixture::start(
        [
            sse(concat!(
                "data: {\"id\":\"chatcmpl-keepalive\",\"object\":\"chat.completion.chunk\",\"model\":\"gemini-3-flash\",\"choices\":[]}\n\n",
                "data: {\"id\":\"chatcmpl-keepalive\",\"object\":\"chat.completion.chunk\",\"model\":\"gemini-3-flash\",\"choices\":[]}\n\n",
                "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"}}]}\n\n",
                "data: {\"id\":\"chatcmpl-keepalive-after\",\"choices\":[]}\n\n",
                "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n",
            )),
            Reply::Complete,
        ],
        [Reply::Complete, Reply::Complete],
        "",
    )
    .await;
    let response = fixture.request("test-client", "alias", true).await;
    check_route(&response, "d1", "1");
    // Leading keepalives are not a committing event and are dropped. Frames after
    // the first content chunk, including a later keepalive, are forwarded unchanged.
    assert_eq!(
        body(response).await,
        concat!(
            "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"}}]}\n\n",
            "data: {\"id\":\"chatcmpl-keepalive-after\",\"choices\":[]}\n\n",
            "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        )
    );
    assert_eq!(fixture.counts(), [1, 0]);
}

#[tokio::test]
async fn stream_error_frames_on_every_deployment_return_classified_429() {
    let failing = three_keepalives_then_rate_limit();
    let fixture = Fixture::start(
        [failing.clone(), failing],
        [Reply::Complete, Reply::Complete],
        "",
    )
    .await;
    let response = fixture.request("test-client", "alias", true).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_ne!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
    let payload: Value = serde_json::from_str(&body(response).await).unwrap();
    assert_eq!(payload["error"]["code"], "rate_limit_exceeded");
    assert_eq!(payload["error"]["type"], "rate_limit_error");
    assert_eq!(fixture.counts(), [1, 1]);
}
