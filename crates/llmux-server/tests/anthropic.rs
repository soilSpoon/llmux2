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
    Exhausted,
    Limit,
    Timeout,
    NotFound(&'static str),
    BadRequest(&'static str),
    Image,
    Document,
    BrokenStream,
    ProtocolHeaders,
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
        match self.reply {
            Reply::Limit => {
                Some((StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, "30")]).into_response())
            }
            Reply::Timeout => Some(StatusCode::REQUEST_TIMEOUT.into_response()),
            Reply::NotFound(code) => Some(
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({
                        "type":"error",
                        "error":{"type":"not_found_error","code":code,"message":"Model unavailable"}
                    })),
                )
                    .into_response(),
            ),
            Reply::BadRequest(message) => Some(
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "type":"error",
                        "error":{"type":"invalid_request_error","message":message}
                    })),
                )
                    .into_response(),
            ),
            _ => None,
        }
    }
}

async fn messages(State(up): State<Arc<Upstream>>, headers: HeaderMap, body: Bytes) -> Response {
    let request: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(headers["x-api-key"], "test-anthropic");
    if matches!(up.reply, Reply::ProtocolHeaders) {
        assert_eq!(headers["anthropic-version"], "2024-01-01");
        assert_eq!(
            headers.get_all("anthropic-beta").iter().collect::<Vec<_>>(),
            ["first-beta", "second-beta"]
        );
    } else {
        assert_eq!(headers["anthropic-version"], "2023-06-01");
        assert!(!headers.contains_key("anthropic-beta"));
    }
    assert!(!headers.contains_key(header::AUTHORIZATION));
    assert_eq!(request["model"], "claude-upstream");
    assert_eq!(request["max_tokens"], 4096);
    assert_eq!(request["system"], "Be brief");
    if matches!(up.reply, Reply::Image) {
        let block = &request["messages"].as_array().unwrap().last().unwrap()["content"][0];
        let source = if block["type"] == "tool_result" {
            &block["content"][0]["source"]
        } else {
            &block["source"]
        };
        assert_eq!(source["type"], "base64");
        assert_eq!(source["data"].as_str().unwrap().len(), 32_000);
    } else if matches!(up.reply, Reply::Document) {
        assert_eq!(request["messages"][0]["content"][0]["type"], "document");
    } else {
        assert_eq!(
            request["messages"],
            json!([{"role": "user", "content": [{"type": "text", "text": "Hello"}]}])
        );
    }
    if let Some(limited) = up.limited() {
        return limited;
    }
    if request["stream"] == true {
        if matches!(up.reply, Reply::BrokenStream) {
            return (
                [(header::CONTENT_TYPE, "text/event-stream")],
                ANTHROPIC_SSE.split_once("event: message_stop").unwrap().0,
            )
                .into_response();
        }
        return ([(header::CONTENT_TYPE, "text/event-stream")], ANTHROPIC_SSE).into_response();
    }
    let mut response = Json(json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "claude-upstream",
        "content": [{"type": "text", "text": "Hi from Claude"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 3, "output_tokens": 4},
    }))
    .into_response();
    if matches!(up.reply, Reply::Exhausted) {
        response.headers_mut().insert(
            "anthropic-ratelimit-requests-remaining",
            "0".parse().unwrap(),
        );
        response.headers_mut().insert(
            "anthropic-ratelimit-requests-reset",
            "2099-01-01T00:00:00Z".parse().unwrap(),
        );
    }
    response
}

async fn chat(State(up): State<Arc<Upstream>>, headers: HeaderMap, body: Bytes) -> Response {
    let request: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(headers[header::AUTHORIZATION], "Bearer test-openai");
    assert!(!headers.contains_key("x-api-key"));
    assert!(!headers.contains_key("anthropic-beta"));
    assert!(!headers.contains_key("anthropic-version"));
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
        Self::with_config(anthropic, (openai.0, Some(openai.1)), "").await
    }

    async fn with_config(
        anthropic: (Reply, i32),
        openai: (Reply, Option<i32>),
        capabilities: &str,
    ) -> Self {
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
        let openai_deployment = openai_priority
            .map(|priority| {
                format!(
                    r#"
[[models.deployments]]
id = "gpt"
credential = "openai-key"
base_url = "{}/v1"
upstream_model = "gpt-upstream"
priority = {priority}
quota = {{ rpm = 1 }}
"#,
                    o.url()
                )
            })
            .unwrap_or_default();
        let config = format!(
            r#"
[server]
client_keys = ["file:{base}/test-client"]
request_timeout_ms = 4000

[routing]
max_attempts = 2

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
{capabilities}

[[models.deployments]]
id = "claude"
credential = "claude-key"
base_url = "{}/v1"
upstream_model = "claude-upstream"
priority = {anthropic_priority}

{openai_deployment}
"#,
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
        self.send_to(
            "/v1/chat/completions",
            header::AUTHORIZATION,
            "Bearer test-client",
            payload,
        )
        .await
    }

    async fn send_to(
        &self,
        path: &str,
        auth_header: header::HeaderName,
        key: &str,
        payload: Value,
    ) -> (Response<Body>, String) {
        self.send_request(
            Request::post(format!("{}{path}", self.gateway.url())).header(auth_header, key),
            payload,
        )
        .await
    }

    async fn send_request(
        &self,
        request: http::request::Builder,
        payload: Value,
    ) -> (Response<Body>, String) {
        let request = request
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
async fn anthropic_reset_header_skips_an_exhausted_deployment() {
    let fixture = Fixture::start((Reply::Exhausted, 0), (Reply::Ok, 10)).await;
    let payload = json!({
        "model":"assistant","max_tokens":4096,"system":"Be brief",
        "messages":[{"role":"user","content":[{"type":"text","text":"Hello"}]}]
    });
    for expected in ["claude", "gpt"] {
        let (response, _) = fixture
            .send_to(
                "/v1/messages",
                header::AUTHORIZATION,
                "Bearer test-client",
                payload.clone(),
            )
            .await;
        check_route(&response, expected, "1");
    }
    assert_eq!(fixture.counts(), [1, 1]);
}

#[tokio::test]
async fn native_messages_authentication_and_passthrough() {
    let fixture = Fixture::start((Reply::Ok, 0), (Reply::Ok, 10)).await;
    let mut payload = json!({
        "model":"assistant","max_tokens":4096,"system":"Be brief",
        "messages":[{"role":"user","content":[{"type":"text","text":"Hello"}]}]
    });
    let api_key = header::HeaderName::from_static("x-api-key");
    let (response, body) = fixture
        .send_to("/v1/messages", api_key.clone(), "invalid", payload.clone())
        .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let error: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(error["type"], "error");
    assert_eq!(error["error"]["type"], "authentication_error");
    assert_eq!(fixture.counts(), [0, 0]);

    let (response, body) = fixture
        .send_to("/v1/messages", api_key, "test-client", payload.clone())
        .await;
    check_route(&response, "claude", "1");
    let message: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(message["type"], "message");
    assert_eq!(message["content"][0]["text"], "Hi from Claude");
    assert_eq!(message["usage"]["output_tokens"], 4);

    payload["stream"] = json!(true);
    let (response, body) = fixture
        .send_to(
            "/v1/messages",
            header::AUTHORIZATION,
            "Bearer test-client",
            payload,
        )
        .await;
    check_route(&response, "claude", "1");
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    for prefix in ["event: ", "data: "] {
        assert_eq!(
            body.lines()
                .filter(|line| line.starts_with(prefix))
                .collect::<Vec<_>>(),
            ANTHROPIC_SSE
                .lines()
                .filter(|line| line.starts_with(prefix))
                .collect::<Vec<_>>(),
        );
    }
    assert_eq!(fixture.counts(), [2, 0]);
}

#[tokio::test]
async fn native_messages_forwards_anthropic_protocol_headers() {
    let fixture = Fixture::start((Reply::ProtocolHeaders, 0), (Reply::Ok, 10)).await;
    let request = |fixture: &Fixture, path| {
        Request::post(format!("{}{path}", fixture.gateway.url()))
            .header(header::AUTHORIZATION, "Bearer test-client")
            .header("anthropic-version", "2024-01-01")
            .header("anthropic-beta", "first-beta")
            .header("anthropic-beta", "second-beta")
    };
    let (response, _) = fixture
        .send_request(
            request(&fixture, "/v1/messages"),
            json!({"model":"assistant","max_tokens":4096,"system":"Be brief",
            "messages":[{"role":"user","content":[{"type":"text","text":"Hello"}]}]}),
        )
        .await;
    check_route(&response, "claude", "1");
    assert_eq!(fixture.counts(), [1, 0]);

    let fixture = Fixture::start((Reply::Ok, 10), (Reply::Ok, 0)).await;
    let (response, _) = fixture
        .send_request(
            request(&fixture, "/v1/chat/completions"),
            json!({"model":"assistant","messages":[{"role":"user","content":"Hello"}]}),
        )
        .await;
    check_route(&response, "gpt", "1");
    assert_eq!(fixture.counts(), [0, 1]);
}

#[tokio::test]
async fn anthropic_http_408_fails_over() {
    let fixture = Fixture::start((Reply::Timeout, 0), (Reply::Ok, 10)).await;
    let (response, text) = fixture.request(false).await;
    check_route(&response, "gpt", "2");
    assert!(text.contains("Hi from OpenAI"));
    assert_eq!(fixture.counts(), [1, 1]);
}

#[tokio::test]
async fn only_explicit_anthropic_model_not_found_fails_over() {
    for code in ["not_found", "model_not_found"] {
        let fixture = Fixture::start((Reply::NotFound(code), 0), (Reply::Ok, 10)).await;
        let (response, text) = fixture.request(false).await;
        if code == "model_not_found" {
            check_route(&response, "gpt", "2");
            assert!(text.contains("Hi from OpenAI"));
            assert_eq!(fixture.counts(), [1, 1]);
        } else {
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            assert_eq!(fixture.counts(), [1, 0]);
        }
    }
}

#[tokio::test]
async fn anthropic_insufficient_credit_fails_over_but_invalid_request_does_not() {
    let fixture = Fixture::start(
        (Reply::BadRequest("messages: invalid input"), 0),
        (Reply::Ok, 10),
    )
    .await;
    let (response, _) = fixture.request(false).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(fixture.counts(), [1, 0]);

    let fixture = Fixture::start(
        (
            Reply::BadRequest(
                "Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits.",
            ),
            0,
        ),
        (Reply::Ok, 10),
    )
    .await;
    let (response, text) = fixture.request(false).await;
    check_route(&response, "gpt", "2");
    assert!(text.contains("Hi from OpenAI"));
    assert_eq!(fixture.counts(), [1, 1]);

    let (response, _) = fixture.request(false).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(fixture.counts(), [1, 1]);
}

#[tokio::test]
async fn terminal_anthropic_model_not_found_uses_the_chat_error_envelope() {
    let fixture = Fixture::start(
        (Reply::NotFound("model_not_found"), 10),
        (Reply::NotFound("model_not_found"), 0),
    )
    .await;
    let (response, text) = fixture.request(false).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(fixture.counts(), [1, 1]);
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap(),
        json!({
            "error":{"type":"not_found_error","code":"model_not_found","message":"Model unavailable","param":null}
        })
    );
}

#[tokio::test]
async fn native_messages_base64_image_does_not_use_prompt_text_budget() {
    let fixture = Fixture::with_config(
        (Reply::Image, 0),
        (Reply::Ok, Some(10)),
        "capabilities = { max_context = 4500 }",
    )
    .await;
    let (response, _) = fixture.send_to(
        "/v1/messages", header::AUTHORIZATION, "Bearer test-client",
        json!({"model":"assistant","max_tokens":4096,"system":"Be brief",
            "messages":[{"role":"user","content":[{"type":"image",
                "source":{"type":"base64","media_type":"image/png","data":"A".repeat(32_000)}}]}]
        }),
    ).await;
    check_route(&response, "claude", "1");
    assert_eq!(fixture.counts(), [1, 0]);
}

#[tokio::test]
async fn native_messages_nested_base64_image_does_not_use_prompt_text_budget() {
    let fixture = Fixture::with_config(
        (Reply::Image, 0),
        (Reply::Ok, Some(10)),
        "capabilities = { max_context = 4500 }",
    )
    .await;
    let (response, _) = fixture.send_to(
        "/v1/messages", header::AUTHORIZATION, "Bearer test-client",
        json!({"model":"assistant","max_tokens":4096,"system":"Be brief",
            "messages":[
                {"role":"user","content":"Hello"},
                {"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"lookup","input":{}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1",
                    "content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"A".repeat(32_000)}}]}]}
            ]
        }),
    ).await;
    check_route(&response, "claude", "1");
    assert_eq!(fixture.counts(), [1, 0]);
}

#[tokio::test]
async fn native_messages_preserves_anthropic_rate_limit_error() {
    let fixture = Fixture::with_config((Reply::Limit, 0), (Reply::Ok, None), "").await;
    let (response, body) = fixture
        .send_to(
            "/v1/messages",
            header::AUTHORIZATION,
            "Bearer test-client",
            json!({"model":"assistant","max_tokens":4096,"system":"Be brief",
                "messages":[{"role":"user","content":[{"type":"text","text":"Hello"}]}]
            }),
        )
        .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()[header::RETRY_AFTER], "30");
    let error: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(error["type"], "error");
    assert_eq!(error["error"]["type"], "rate_limit_error");
    assert_eq!(fixture.counts(), [1, 0]);
}

#[tokio::test]
async fn native_messages_emits_anthropic_broken_stream_error() {
    let fixture = Fixture::start((Reply::BrokenStream, 0), (Reply::Ok, 10)).await;
    let (response, body) = fixture
        .send_to(
            "/v1/messages",
            header::AUTHORIZATION,
            "Bearer test-client",
            json!({"model":"assistant","stream":true,"max_tokens":4096,"system":"Be brief",
                "messages":[{"role":"user","content":[{"type":"text","text":"Hello"}]}]
            }),
        )
        .await;
    check_route(&response, "claude", "1");
    assert!(body.contains("event: message_start"));
    let last = body.trim_end().split("\n\n").last().unwrap();
    assert!(last.lines().any(|line| line == "event: error"));
    let data = last
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .unwrap();
    let error: Value = serde_json::from_str(data).unwrap();
    assert_eq!(error["type"], "error");
    assert_eq!(error["error"]["type"], "api_error");
    assert!(!body.contains("event: message_stop"));
    assert_eq!(fixture.counts(), [1, 0]);
}

#[tokio::test]
async fn native_messages_skips_conversion_that_cannot_handle_documents() {
    let fixture = Fixture::start((Reply::Document, 10), (Reply::Ok, 0)).await;
    for stream in [false, true] {
        let (response, body) = fixture.send_to(
            "/v1/messages", header::AUTHORIZATION, "Bearer test-client",
            json!({"model":"assistant","max_tokens":4096,"system":"Be brief","stream":stream,
                "messages":[{"role":"user","content":[{"type":"document",
                    "source":{"type":"base64","media_type":"application/pdf","data":"JVBERg=="}}]}]
            }),
        ).await;
        check_route(&response, "claude", "1");
        assert!(body.contains("Hi from Claude"));
    }
    assert_eq!(fixture.counts(), [2, 0]);
    let (response, _) = fixture.request(false).await;
    check_route(&response, "gpt", "1");
    assert_eq!(fixture.counts(), [2, 1]);
}

#[tokio::test]
async fn chat_n_two_returns_one_choice_for_anthropic() {
    let fixture = Fixture::start((Reply::Ok, 0), (Reply::Ok, 10)).await;
    for stream in [false, true] {
        let (response, body) = fixture.send(json!({
            "model":"assistant","max_tokens":4096,"stream":stream,"n":2,
            "messages":[{"role":"system","content":"Be brief"},{"role":"user","content":"Hello"}]
        })).await;
        check_route(&response, "claude", "1");
        if stream {
            assert!(body.ends_with("data: [DONE]\n\n"));
            for chunk in body
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .filter(|data| *data != "[DONE]")
            {
                let chunk: Value = serde_json::from_str(chunk).unwrap();
                assert_eq!(chunk["choices"].as_array().unwrap().len(), 1);
                assert_eq!(chunk["choices"][0]["index"], 0);
            }
        } else {
            let completion: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(completion["choices"].as_array().unwrap().len(), 1);
        }
    }
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
async fn anthropic_chat_stream_sends_requested_usage_in_its_own_final_chunk() {
    let fixture = Fixture::start((Reply::Ok, 0), (Reply::Ok, 10)).await;
    for include_usage in [true, false] {
        let (response, body) = fixture
            .send(json!({
                "model":"assistant",
                "messages":[{"role":"system","content":"Be brief"},{"role":"user","content":"Hello"}],
                "stream":true,"stream_options":{"include_usage":include_usage}
            }))
            .await;
        check_route(&response, "claude", "1");
        let events: Vec<_> = body
            .split("\n\n")
            .filter_map(|event| event.strip_prefix("data: "))
            .collect();
        assert_eq!(events.last(), Some(&"[DONE]"));
        let mut chunks: Vec<Value> = events[..events.len() - 1]
            .iter()
            .map(|event| serde_json::from_str(event).unwrap())
            .collect();
        if include_usage {
            let usage = chunks.pop().unwrap();
            assert_eq!(usage["choices"], json!([]));
            assert_eq!(
                usage["usage"],
                json!({"prompt_tokens":3,"completion_tokens":4,"total_tokens":7})
            );
            assert!(chunks
                .iter()
                .all(|chunk| chunk.get("usage") == Some(&Value::Null)));
        } else {
            assert!(chunks.iter().all(|chunk| chunk.get("usage").is_none()));
        }
        assert_eq!(
            chunks.last().unwrap()["choices"][0]["finish_reason"],
            "stop"
        );
    }
    assert_eq!(fixture.counts(), [2, 0]);
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
            "model": "assistant",
            "messages": [{"role": "user", "content": [{"type":"audio"}]}],
        }))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(error["error"]["type"], "invalid_request_error");
    assert_eq!(fixture.counts(), [0, 0]);
}

#[tokio::test]
async fn unregistered_provider_fails_at_startup() {
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
    let err = llmux_server::app(config).await.err().unwrap();
    assert_eq!(
        err.to_string(),
        "no registered adapter for provider anthropik"
    );
}

struct Sampling {
    bodies: std::sync::Mutex<Vec<Value>>,
    reject: &'static [&'static str],
}

async fn sampling_messages(State(up): State<Arc<Sampling>>, body: Bytes) -> Response {
    let request: Value = serde_json::from_slice(&body).unwrap();
    up.bodies.lock().unwrap().push(request.clone());
    for field in up.reject {
        if request.get(field).is_some() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "type":"error",
                    "error":{"type":"invalid_request_error","message":format!("`{field}` is deprecated for this model.")}
                })),
            )
                .into_response();
        }
    }
    Json(json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-upstream",
        "content": [{"type": "text", "text": "Hi from Claude"}],
        "stop_reason": "end_turn", "usage": {"input_tokens": 3, "output_tokens": 4},
    }))
    .into_response()
}

struct SamplingFixture {
    upstream: Arc<Sampling>,
    gateway: TestServer,
    _server: TestServer,
    _secrets: TempDir,
}

impl SamplingFixture {
    async fn start(reject: &'static [&'static str], strip: &str) -> Self {
        let upstream = Arc::new(Sampling {
            bodies: Default::default(),
            reject,
        });
        let server = TestServer::start(
            Router::new()
                .route("/v1/messages", post(sampling_messages))
                .with_state(upstream.clone()),
        )
        .await
        .unwrap();
        let secrets = tempfile::tempdir().unwrap();
        for key in ["test-anthropic", "test-client"] {
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

[[models]]
name = "assistant"

[[models.deployments]]
id = "claude"
credential = "claude-key"
base_url = "{}/v1"
upstream_model = "claude-upstream"
{strip}
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
            _server: server,
            _secrets: secrets,
        }
    }

    async fn post(&self, path: &str, payload: Value) -> (StatusCode, HeaderMap, String) {
        let request = Request::post(format!("{}{path}", self.gateway.url()))
            .header(header::AUTHORIZATION, "Bearer test-client")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(payload.to_string())))
            .unwrap();
        let response = Client::builder(TokioExecutor::new())
            .build_http()
            .request(request)
            .await
            .unwrap();
        let (parts, body) = response.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        (
            parts.status,
            parts.headers,
            String::from_utf8(bytes.to_vec()).unwrap(),
        )
    }

    fn bodies(&self) -> Vec<Value> {
        self.upstream.bodies.lock().unwrap().clone()
    }
}

fn sampling_payloads() -> [(&'static str, Value); 2] {
    [
        (
            "/v1/chat/completions",
            json!({"model":"assistant","temperature":0.7,"top_p":0.9,"top_k":5,"messages":[{"role":"user","content":"Hello"}]}),
        ),
        (
            "/v1/messages",
            json!({"model":"assistant","max_tokens":16,"temperature":0.7,"top_p":0.9,"top_k":5,"messages":[{"role":"user","content":"Hello"}]}),
        ),
    ]
}

#[tokio::test]
async fn strip_request_fields_removes_sampling_parameters_on_both_anthropic_paths() {
    let fixture = SamplingFixture::start(
        &[],
        r#"strip_request_fields = ["temperature", "top_p", "top_k"]"#,
    )
    .await;
    for (path, payload) in sampling_payloads() {
        let (status, headers, _) = fixture.post(path, payload).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(headers["x-llmux-attempts"], "1");
    }
    let bodies = fixture.bodies();
    assert_eq!(bodies.len(), 2);
    for body in bodies {
        for field in ["temperature", "top_p", "top_k"] {
            assert!(body.get(field).is_none(), "{field} reached upstream");
        }
        assert_eq!(body["model"], "claude-upstream");
    }
}

#[tokio::test]
async fn deprecated_sampling_parameters_are_dropped_and_retried_on_the_same_deployment() {
    let fixture = SamplingFixture::start(&["temperature", "top_p"], "").await;
    for (path, payload) in sampling_payloads() {
        let (status, headers, body) = fixture.post(path, payload).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        assert_eq!(headers["x-llmux-deployment"], "claude");
        assert_eq!(headers["x-llmux-attempts"], "1");
    }
    let bodies = fixture.bodies();
    assert_eq!(bodies.len(), 6);
    for (sent, native) in bodies.chunks(3).zip([false, true]) {
        assert!(sent[0].get("temperature").is_some());
        assert!(sent[1].get("temperature").is_none() && sent[1].get("top_p").is_some());
        assert!(sent[2].get("top_p").is_none());
        if native {
            assert_eq!(sent[2]["top_k"], 5, "unrelated fields must survive");
        }
        assert_eq!(sent[2]["messages"], sent[0]["messages"]);
    }
}

#[tokio::test]
async fn deprecated_sampling_repairs_consume_upstream_request_quota() {
    for (path, payload) in sampling_payloads() {
        let limited =
            SamplingFixture::start(&["temperature"], "quota = { rpm = 1, concurrent = 1 }").await;
        let (status, headers, body) = limited.post(path, payload.clone()).await;
        assert_eq!(limited.bodies().len(), 1, "{path}: no quota for a repair");
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
        assert!(body.contains("is deprecated for this model"));
        assert_eq!(headers["x-llmux-attempts"], "1");

        let allowed =
            SamplingFixture::start(&["temperature"], "quota = { rpm = 2, concurrent = 1 }").await;
        let (status, headers, body) = allowed.post(path, payload.clone()).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        assert_eq!(headers["x-llmux-attempts"], "1");
        assert_eq!(allowed.bodies().len(), 2);

        let (status, _, _) = allowed.post(path, payload).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{path}");
        assert_eq!(allowed.bodies().len(), 2, "{path}: both sends were charged");
    }
}

#[tokio::test]
async fn deprecated_parameter_retry_serves_chat_requests() {
    let fixture = SamplingFixture::start(&["temperature"], "").await;
    let (status, _, body) = fixture
        .post(
            "/v1/chat/completions",
            json!({"model":"assistant","temperature":0.2,"messages":[{"role":"user","content":"Hello"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(fixture.bodies().len(), 2);
}

#[tokio::test]
async fn unrelated_bad_requests_are_not_retried() {
    let fixture = SamplingFixture::start(&["max_tokens"], "").await;
    let (status, _, _) = fixture
        .post(
            "/v1/messages",
            json!({"model":"assistant","max_tokens":16,"messages":[{"role":"user","content":"Hello"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(fixture.bodies().len(), 1);
}
