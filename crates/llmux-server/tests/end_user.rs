use axum::{body::Bytes, extract::State, response::IntoResponse, routing::post, Json, Router};
use http::{header, HeaderMap, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use llmux_config::RuntimeConfig;
use llmux_testkit::TestServer;
use serde_json::{json, Value};
use std::{
    fs,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::timeout;

const HASH: &str = "31ca399bfc61dac0b442a6bef7c00cd06cf9682c11b9192fb26a918b4a9130f8";
const RAW: &str = "user-one";

struct Upstream {
    bodies: Mutex<Vec<Value>>,
}

async fn messages(State(up): State<Arc<Upstream>>, body: Bytes) -> impl IntoResponse {
    let request: Value = serde_json::from_slice(&body).unwrap();
    up.bodies.lock().unwrap().push(request);
    Json(json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "claude-upstream",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    }))
}

struct Fixture {
    upstream: Arc<Upstream>,
    gateway: TestServer,
    _upstream_server: TestServer,
    _dir: tempfile::TempDir,
}

impl Fixture {
    async fn start() -> Self {
        let upstream = Arc::new(Upstream {
            bodies: Mutex::new(Vec::new()),
        });
        let server = TestServer::start(
            Router::new()
                .route("/v1/messages", post(messages))
                .with_state(upstream.clone()),
        )
        .await
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("client"), "client-key\n").unwrap();
        fs::write(dir.path().join("anthropic"), "test-anthropic\n").unwrap();
        fs::write(dir.path().join("salt"), "salt-one\n").unwrap();
        let base = dir.path().display();
        let config = format!(
            r#"
[server]
client_keys = ["file:{base}/client"]
end_user_header = "x-llmux-end-user"
end_user_salt = "file:{base}/salt"
remote_images = "reject"
max_image_bytes = 1

[[credentials]]
id = "claude-key"
kind = "api_key"
provider = "anthropic"
key = "file:{base}/anthropic"

[[models]]
name = "assistant"
[[models.deployments]]
id = "claude"
credential = "claude-key"
base_url = "{}/v1"
upstream_model = "claude-upstream"
"#,
            server.url()
        );
        let gateway = TestServer::start(
            llmux_server::app(RuntimeConfig::parse(&config).unwrap())
                .await
                .unwrap(),
        )
        .await
        .unwrap();
        Self {
            upstream,
            gateway,
            _upstream_server: server,
            _dir: dir,
        }
    }

    fn calls(&self) -> usize {
        self.upstream.bodies.lock().unwrap().len()
    }

    fn last(&self) -> Value {
        self.upstream.bodies.lock().unwrap().last().unwrap().clone()
    }

    async fn send(
        &self,
        path: &str,
        extra: &[(&str, &str)],
        payload: Value,
    ) -> (StatusCode, HeaderMap, String) {
        let mut request = Request::post(format!("{}{path}", self.gateway.url()))
            .header(header::AUTHORIZATION, "Bearer client-key")
            .header(header::CONTENT_TYPE, "application/json");
        for (name, value) in extra {
            request = request.header(*name, *value);
        }
        let request = request
            .body(Full::new(Bytes::from(payload.to_string())))
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
        let headers = response.headers().clone();
        let bytes = timeout(Duration::from_secs(5), response.into_body().collect())
            .await
            .unwrap()
            .unwrap()
            .to_bytes();
        (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
    }
}

#[tokio::test]
async fn header_hash_reaches_anthropic_and_the_response() {
    let fixture = Fixture::start().await;
    let (status, headers, body) = fixture
        .send(
            "/v1/chat/completions",
            &[("x-llmux-end-user", RAW)],
            json!({
                "model": "assistant",
                "user": "body-user-SHOULD-NOT-LEAK",
                "messages": [{"role": "user", "content": "hi"}]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-llmux-end-user"], HASH);
    assert!(!body.contains(RAW));
    assert!(!body.contains("body-user-SHOULD-NOT-LEAK"));
    assert!(!body.contains("salt-one"));
    let sent = fixture.last();
    assert_eq!(sent["metadata"]["user_id"], HASH);
    let recorded = sent.to_string();
    assert!(!recorded.contains(RAW));
    assert!(!recorded.contains("body-user-SHOULD-NOT-LEAK"));
    assert!(!recorded.contains("salt-one"));
    assert_eq!(fixture.calls(), 1);

    let (status, headers, _) = fixture
        .send(
            "/v1/messages",
            &[("x-llmux-end-user", RAW)],
            json!({
                "model": "assistant",
                "max_tokens": 8,
                "metadata": {"user_id": "client-raw-SHOULD-NOT-LEAK", "account": "acme"},
                "messages": [{"role": "user", "content": "hi"}]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-llmux-end-user"], HASH);
    let sent = fixture.last();
    assert_eq!(sent["metadata"]["user_id"], HASH);
    assert_eq!(sent["metadata"]["account"], "acme");
    assert!(!sent.to_string().contains("client-raw-SHOULD-NOT-LEAK"));
}

#[tokio::test]
async fn body_user_is_hashed_when_the_header_is_absent() {
    let fixture = Fixture::start().await;
    let (status, headers, _) = fixture
        .send(
            "/v1/chat/completions",
            &[],
            json!({
                "model": "assistant",
                "user": RAW,
                "messages": [{"role": "user", "content": "hi"}]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-llmux-end-user"], HASH);
    assert_eq!(fixture.last()["metadata"]["user_id"], HASH);
    assert!(!fixture.last().to_string().contains(RAW));
}

#[tokio::test]
async fn remote_images_and_oversized_data_urls_never_call_upstream() {
    let fixture = Fixture::start().await;
    let (status, headers, body) = fixture
        .send(
            "/v1/chat/completions",
            &[("x-llmux-end-user", RAW)],
            json!({
                "model": "assistant",
                "messages": [{"role": "user", "content": [
                    {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}}
                ]}]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(headers["x-llmux-end-user"], HASH);
    assert!(body.contains("remote image URLs are not accepted; send a data: URL"));
    assert!(!body.contains("example.com"));
    assert_eq!(fixture.calls(), 0);

    let (status, _, body) = fixture
        .send(
            "/v1/messages",
            &[],
            json!({
                "model": "assistant",
                "max_tokens": 8,
                "messages": [{"role": "user", "content": [
                    {"type": "image", "source": {"type": "url", "url": "http://example.com/a.png"}}
                ]}]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("remote image URLs are not accepted; send a data: URL"));
    assert_eq!(fixture.calls(), 0);

    let (status, _, body) = fixture
        .send(
            "/v1/chat/completions",
            &[],
            json!({
                "model": "assistant",
                "messages": [{"role": "user", "content": [
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,YQI="}}
                ]}]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(body.contains("image exceeds max_image_bytes"));
    assert_eq!(fixture.calls(), 0);

    let (status, _, _) = fixture
        .send(
            "/v1/chat/completions",
            &[],
            json!({
                "model": "assistant",
                "messages": [{"role": "user", "content": [
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,YQ=="}}
                ]}]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fixture.calls(), 1);
}
