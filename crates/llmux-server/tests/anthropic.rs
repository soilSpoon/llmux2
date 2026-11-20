use axum::{
    body::{Body, Bytes},
    extract::State,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use http::{header, HeaderMap, Request, StatusCode};
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
use tempfile::TempDir;
use tokio::time::timeout;

const ANTHROPIC_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-upstream\",\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi from Claude\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":4}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

#[derive(Clone, Copy)]
enum Reply {
    Ok,
    Limit,
}

struct Upstream {
    calls: AtomicUsize,
    reply: Reply,
}

impl Upstream {
    fn new(reply: Reply) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            reply,
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    fn limited(&self) -> Option<Response> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        matches!(self.reply, Reply::Limit)
            .then(|| (StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, "30")]).into_response())
    }
}

async fn messages(State(up): State<Arc<Upstream>>, headers: HeaderMap, body: Bytes) -> Response {
    let request: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(headers["x-api-key"], "test-anthropic");
    assert_eq!(headers["anthropic-version"], "2023-06-01");
    assert!(!headers.contains_key(header::AUTHORIZATION));
    assert_eq!(request["model"], "claude-upstream");
    assert_eq!(request["max_tokens"], 4096);
    assert_eq!(request["system"], "Be brief");
    assert_eq!(
        request["messages"],
        json!([{"role": "user", "content": [{"type": "text", "text": "Hello"}]}])
    );
    if let Some(limited) = up.limited() {
        return limited;
    }
    if request["stream"] == true {
        return ([(header::CONTENT_TYPE, "text/event-stream")], ANTHROPIC_SSE).into_response();
    }
    Json(json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "claude-upstream",
        "content": [{"type": "text", "text": "Hi from Claude"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 3, "output_tokens": 4},
    }))
    .into_response()
}

async fn chat(State(up): State<Arc<Upstream>>, headers: HeaderMap, body: Bytes) -> Response {
    let request: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(headers[header::AUTHORIZATION], "Bearer test-openai");
    assert!(!headers.contains_key("x-api-key"));
    assert_eq!(request["model"], "gpt-upstream");
    if let Some(limited) = up.limited() {
        return limited;
    }
    Json(json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "Hi from OpenAI"}, "finish_reason": "stop"}],
    }))
    .into_response()
}

struct Fixture {
    anthropic: Arc<Upstream>,
    openai: Arc<Upstream>,
    gateway: TestServer,
    _upstreams: [TestServer; 2],
    _secrets: TempDir,
}

impl Fixture {
    async fn start(anthropic: (Reply, i32), openai: (Reply, i32)) -> Self {
        let (anthropic_reply, anthropic_priority) = anthropic;
        let (openai_reply, openai_priority) = openai;
        let anthropic = Upstream::new(anthropic_reply);
        let openai = Upstream::new(openai_reply);
        let a = TestServer::start(
            Router::new()
                .route("/v1/messages", post(messages))
                .with_state(anthropic.clone()),
        )
        .await
        .unwrap();
        let o = TestServer::start(
            Router::new()
                .route("/v1/chat/completions", post(chat))
                .with_state(openai.clone()),
        )
        .await
        .unwrap();
        let secrets = tempfile::tempdir().unwrap();
        for key in ["test-anthropic", "test-openai", "test-client"] {
            fs::write(secrets.path().join(key), format!("{key}\n")).unwrap();
        }
        let base = secrets.path().display();
        let config = format!(
            r#"
[server]
client_keys = ["file:{base}/test-client"]
request_timeout_ms = 4000

[[credentials]]
id = "claude-key"
kind = "api_key"
provider = "anthropic"
key = "file:{base}/test-anthropic"

[[credentials]]
id = "openai-key"
kind = "api_key"
provider = "openai"
key = "file:{base}/test-openai"

[[models]]
name = "assistant"

[[models.deployments]]
id = "claude"
credential = "claude-key"
base_url = "{}/v1"
upstream_model = "claude-upstream"
priority = {anthropic_priority}

[[models.deployments]]
id = "gpt"
credential = "openai-key"
base_url = "{}/v1"
upstream_model = "gpt-upstream"
priority = {openai_priority}
"#,
            a.url(),
            o.url()
        );
        let gateway =
            TestServer::start(llmux_server::app(RuntimeConfig::parse(&config).unwrap()).unwrap())
                .await
                .unwrap();
        Self {
            anthropic,
            openai,
            gateway,
            _upstreams: [a, o],
            _secrets: secrets,
        }
    }

    fn counts(&self) -> [usize; 2] {
        [self.anthropic.calls(), self.openai.calls()]
    }

    async fn request(&self, streaming: bool) -> (Response<Body>, String) {
        self.send(json!({
            "model": "assistant", "stream": streaming,
            "messages": [
                {"role": "system", "content": "Be brief"},
                {"role": "user", "content": "Hello"},
            ],
        }))
        .await
    }

    async fn send(&self, payload: Value) -> (Response<Body>, String) {
        let request = Request::post(format!("{}/v1/chat/completions", self.gateway.url()))
            .header(header::AUTHORIZATION, "Bearer test-client")
            .header(header::CONTENT_TYPE, "application/json")
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
        let (parts, body) = response.into_parts();
        let bytes = timeout(Duration::from_secs(5), body.collect())
            .await
            .unwrap()
            .unwrap()
            .to_bytes();
        (
            Response::from_parts(parts, Body::empty()),
            String::from_utf8(bytes.to_vec()).unwrap(),
        )
    }
}

fn check_route(response: &Response, deployment: &str, attempts: &str) {
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-llmux-deployment"], deployment);
    assert_eq!(response.headers()["x-llmux-attempts"], attempts);
}

#[tokio::test]
async fn anthropic_deployment_serves_openai_chat_json() {
    let fixture = Fixture::start((Reply::Ok, 0), (Reply::Ok, 10)).await;
    let (response, body) = fixture.request(false).await;
    check_route(&response, "claude", "1");
    let completion: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(completion["object"], "chat.completion");
    assert_eq!(completion["model"], "claude-upstream");
    assert_eq!(
        completion["choices"][0]["message"],
        json!({"role": "assistant", "content": "Hi from Claude"})
    );
    assert_eq!(completion["choices"][0]["finish_reason"], "stop");
    assert_eq!(
        completion["usage"],
        json!({"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7})
    );
    assert_eq!(fixture.counts(), [1, 0]);
}

#[tokio::test]
async fn anthropic_deployment_streams_openai_chat_chunks() {
    let fixture = Fixture::start((Reply::Ok, 0), (Reply::Ok, 10)).await;
    let (response, body) = fixture.request(true).await;
    check_route(&response, "claude", "1");
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    let events: Vec<&str> = body
        .split("\n\n")
        .filter_map(|event| event.strip_prefix("data: "))
        .collect();
    assert_eq!(events.last(), Some(&"[DONE]"));
    let chunks: Vec<Value> = events[..events.len() - 1]
        .iter()
        .map(|data| serde_json::from_str(data).unwrap())
        .collect();
    assert!(chunks
        .iter()
        .all(|chunk| chunk["object"] == "chat.completion.chunk" && chunk["id"] == "msg_1"));
    let deltas: Vec<&Value> = chunks.iter().map(|c| &c["choices"][0]["delta"]).collect();
    assert_eq!(deltas[0]["role"], "assistant");
    assert_eq!(deltas[1]["content"], "Hi from Claude");
    let last = chunks.last().unwrap();
    assert_eq!(last["choices"][0]["finish_reason"], "stop");
    assert_eq!(last["usage"]["completion_tokens"], 4);
    assert_eq!(fixture.counts(), [1, 0]);
}

#[tokio::test]
async fn rate_limited_anthropic_fails_over_to_openai() {
    let fixture = Fixture::start((Reply::Limit, 0), (Reply::Ok, 10)).await;
    let (response, body) = fixture.request(false).await;
    check_route(&response, "gpt", "2");
    let completion: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        completion["choices"][0]["message"]["content"],
        "Hi from OpenAI"
    );
    assert_eq!(fixture.counts(), [1, 1]);
}

#[tokio::test]
async fn rate_limited_openai_fails_over_to_anthropic() {
    let fixture = Fixture::start((Reply::Ok, 10), (Reply::Limit, 0)).await;
    let (response, body) = fixture.request(true).await;
    check_route(&response, "claude", "2");
    assert!(body.contains("Hi from Claude"));
    assert!(body.ends_with("data: [DONE]\n\n"));
    assert_eq!(fixture.counts(), [1, 1]);
}

#[tokio::test]
async fn unconvertible_request_is_a_client_error() {
    let fixture = Fixture::start((Reply::Ok, 0), (Reply::Ok, 10)).await;
    let (response, body) = fixture
        .send(json!({
            "model": "assistant", "n": 2,
            "messages": [{"role": "user", "content": "Hello"}],
        }))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(error["error"]["type"], "invalid_request_error");
    assert_eq!(fixture.counts(), [0, 0]);
}

#[test]
fn unregistered_provider_fails_at_startup() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("key"), "k").unwrap();
    let base = dir.path().display();
    let config = RuntimeConfig::parse(&format!(
        r#"
[server]
client_keys = ["file:{base}/key"]

[[credentials]]
id = "c"
kind = "api_key"
provider = "anthropik"
key = "file:{base}/key"

[[models]]
name = "m"

[[models.deployments]]
id = "d"
credential = "c"
base_url = "https://api.anthropic.com/v1"
upstream_model = "claude"
"#
    ))
    .unwrap();
    let err = llmux_server::app(config).err().unwrap();
    assert_eq!(
        err.to_string(),
        "no registered adapter for provider anthropik"
    );
}
