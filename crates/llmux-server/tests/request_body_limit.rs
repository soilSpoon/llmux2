use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use http::{header, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use llmux_config::RuntimeConfig;
use llmux_testkit::TestServer;
use serde_json::{json, Value};
use std::{
    fs,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::time::timeout;

async fn upstream_chat(State(calls): State<Arc<AtomicUsize>>, body: Bytes) -> impl IntoResponse {
    calls.fetch_add(1, Ordering::Relaxed);
    let request: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(request["model"], "upstream-model");
    Json(json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "ok"},
            "finish_reason": "stop"
        }]
    }))
}

struct Fixture {
    calls: Arc<AtomicUsize>,
    gateway: TestServer,
    _upstream: TestServer,
    _dir: tempfile::TempDir,
}

impl Fixture {
    async fn start(max_request_bytes: Option<u64>) -> Self {
        let calls = Arc::new(AtomicUsize::new(0));
        let upstream = TestServer::start(
            Router::new()
                .route("/v1/chat/completions", post(upstream_chat))
                .with_state(calls.clone())
                .layer(DefaultBodyLimit::disable()),
        )
        .await
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("client"), "client-key\n").unwrap();
        fs::write(dir.path().join("upstream"), "upstream-key\n").unwrap();
        let base = dir.path().display();
        let limit = max_request_bytes
            .map(|value| format!("max_request_bytes = {value}\n"))
            .unwrap_or_default();
        let config = format!(
            r#"
[server]
client_keys = ["file:{base}/client"]
{limit}
[[credentials]]
id = "upstream-credential"
kind = "api_key"
provider = "openai_compat"
key = "file:{base}/upstream"

[[models]]
name = "large-context"
[[models.deployments]]
id = "upstream"
credential = "upstream-credential"
base_url = "{}/v1"
upstream_model = "upstream-model"
"#,
            upstream.url()
        );
        let gateway = TestServer::start(
            llmux_server::app(RuntimeConfig::parse(&config).unwrap())
                .await
                .unwrap(),
        )
        .await
        .unwrap();
        Self {
            calls,
            gateway,
            _upstream: upstream,
            _dir: dir,
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    async fn send(&self, path: &str, payload: String) -> (StatusCode, String) {
        let request = Request::post(format!("{}{path}", self.gateway.url()))
            .header(header::AUTHORIZATION, "Bearer client-key")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(payload)))
            .unwrap();
        let response = timeout(
            Duration::from_secs(5),
            Client::builder(TokioExecutor::new())
                .build_http()
                .request(request),
        )
        .await
        .unwrap()
        .unwrap();
        let status = response.status();
        let bytes = timeout(Duration::from_secs(5), response.into_body().collect())
            .await
            .unwrap()
            .unwrap()
            .to_bytes();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }
}

fn payload_with_pad(pad: usize) -> String {
    json!({
        "model": "large-context",
        "messages": [{"role": "user", "content": "hi"}],
        "pad": "x".repeat(pad)
    })
    .to_string()
}

fn payload_of_len(len: usize) -> String {
    let empty = payload_with_pad(0);
    let payload = payload_with_pad(len - empty.len());
    assert_eq!(payload.len(), len);
    payload
}

#[tokio::test]
async fn configured_request_body_limit_accepts_at_limit_and_rejects_over_without_upstream() {
    let fixture = Fixture::start(Some(192)).await;
    let within = payload_of_len(192);
    let (status, body) = fixture.send("/v1/chat/completions", within).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(fixture.calls(), 1);

    let over = payload_of_len(193);
    let (status, body) = fixture.send("/v1/chat/completions", over).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert!(body.contains("Request body too large"));
    assert_eq!(fixture.calls(), 1);

    for path in ["/v1/responses", "/v1/messages"] {
        let over = payload_of_len(193);
        let (status, body) = fixture.send(path, over).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{path}: {body}");
        assert!(body.contains("Request body too large"), "{path}: {body}");
    }
    assert_eq!(fixture.calls(), 1);
}

#[tokio::test]
async fn default_request_body_limit_accepts_multi_mebibyte_prompts() {
    let fixture = Fixture::start(None).await;
    let payload = payload_of_len(3 * 1024 * 1024);
    let (status, body) = fixture.send("/v1/chat/completions", payload).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(fixture.calls(), 1);
}
