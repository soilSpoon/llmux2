use axum::{body::Bytes, extract::State, response::IntoResponse, routing::post, Json, Router};
use http::{header, Request, StatusCode};
use http_body_util::Full;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use llmux_config::RuntimeConfig;
use llmux_testkit::TestServer;
use serde_json::{json, Value};
use std::time::Duration;
use std::{
    fs,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};
use tokio::time::timeout;

struct Upstream {
    bodies: Mutex<Vec<Value>>,
    calls: AtomicUsize,
}

async fn messages(State(up): State<Arc<Upstream>>, body: Bytes) -> impl IntoResponse {
    let request: Value = serde_json::from_slice(&body).unwrap();
    up.bodies.lock().unwrap().push(request);
    if up.calls.fetch_add(1, Ordering::Relaxed) == 1 {
        // A rejected request carries its provider id only in the error body.
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "type": "error",
                "request_id": "req_err_99",
                "error": {"type": "invalid_request_error", "message": "bad request"}
            })),
        )
            .into_response();
    }
    Json(json!({
        "id": "msg_01XFDUD",
        "type": "message",
        "role": "assistant",
        "model": "claude-upstream",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    }))
    .into_response()
}

#[tokio::test]
async fn request_log_attributes_key_images_and_upstream_id() {
    let upstream = Arc::new(Upstream {
        bodies: Mutex::new(Vec::new()),
        calls: AtomicUsize::new(0),
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
    let base = dir.path().display();
    let log_path = dir.path().join("requests.jsonl");
    let config = format!(
        r#"
[server]
client_keys = ["file:{base}/client"]
request_log = "{}"

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
        log_path.display(),
        server.url(),
    );
    let gateway = TestServer::start(
        llmux_server::app(RuntimeConfig::parse(&config).unwrap())
            .await
            .unwrap(),
    )
    .await
    .unwrap();

    let body = json!({
        "model": "assistant",
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "describe it"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGk="}}
            ]
        }],
        "max_tokens": 8
    });
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let response = timeout(
        Duration::from_secs(10),
        client.request(
            Request::post(format!("{}/v1/messages", gateway.url()))
                .header(header::AUTHORIZATION, "Bearer client-key")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Full::new(Bytes::from(body.to_string())))
                .unwrap(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // A rejected request still records the provider id from the error body.
    let response = timeout(
        Duration::from_secs(10),
        client.request(
            Request::post(format!("{}/v1/messages", gateway.url()))
                .header(header::AUTHORIZATION, "Bearer client-key")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Full::new(Bytes::from(body.to_string())))
                .unwrap(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_ne!(response.status(), StatusCode::OK);
    drop(gateway);
    drop(server);

    // The writer thread flushes each line itself; poll briefly for it.
    let lines: Vec<Value> = {
        let mut found = Vec::new();
        for _ in 0..50 {
            if let Ok(text) = fs::read_to_string(&log_path) {
                found = text
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                if found.len() == 2 {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        found
    };
    assert_eq!(lines.len(), 2);
    let record = &lines[0];
    assert_eq!(record["upstream_request_id"], "msg_01XFDUD");
    assert_eq!(record["image_count"], 1);
    let fingerprint = record["client_key"].as_str().unwrap();
    assert!(
        fingerprint.starts_with("key_") && fingerprint.len() == "key_".len() + 16,
        "fingerprint: {fingerprint}"
    );
    assert_ne!(fingerprint, "client-key");
    assert_eq!(record["outcome"], "routed");
    assert_eq!(lines[1]["outcome"], "error");
    assert_eq!(lines[1]["upstream_request_id"], "req_err_99");
}

async fn chat_completions(State(up): State<Arc<Upstream>>, body: Bytes) -> impl IntoResponse {
    let request: Value = serde_json::from_slice(&body).unwrap();
    up.bodies.lock().unwrap().push(request);
    up.calls.fetch_add(1, Ordering::Relaxed);
    Json(json!({
        "id": "chatcmpl-up1",
        "object": "chat.completion",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    }))
    .into_response()
}

#[tokio::test]
async fn request_log_image_count_survives_middle_out_retry() {
    let upstream = Arc::new(Upstream {
        bodies: Mutex::new(Vec::new()),
        calls: AtomicUsize::new(0),
    });
    let server = TestServer::start(
        Router::new()
            .route("/v1/chat/completions", post(chat_completions))
            .with_state(upstream.clone()),
    )
    .await
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("client"), "client-key\n").unwrap();
    fs::write(dir.path().join("openai"), "test-openai\n").unwrap();
    fs::write(dir.path().join("tight"), "test-tight\n").unwrap();
    let base = dir.path().display();
    let log_path = dir.path().join("requests.jsonl");
    let config = format!(
        r#"
[server]
client_keys = ["file:{base}/client"]
request_log = "{}"

[[credentials]]
id = "openai-key"
kind = "api_key"
provider = "openai"
key = "file:{base}/openai"

[[credentials]]
id = "tight-key"
kind = "api_key"
provider = "openai"
key = "file:{base}/tight"

[[models]]
name = "shared"
capabilities = {{ stream = true, max_context = 100000 }}
[[models.deployments]]
id = "o1"
credential = "openai-key"
base_url = "{}/v1"
upstream_model = "gpt-x"
priority = 10

[[models.deployments]]
id = "o2"
credential = "tight-key"
base_url = "{}/v1"
upstream_model = "gpt-x"
priority = 0
max_context = 30
"#,
        log_path.display(),
        server.url(),
        server.url(),
    );
    let gateway = TestServer::start(
        llmux_server::app(RuntimeConfig::parse(&config).unwrap())
            .await
            .unwrap(),
    )
    .await
    .unwrap();

    let body = json!({
        "model": "shared",
        "max_tokens": 99_970,
        "messages": [
            {"role": "system", "content": "Hello"},
            {"role": "user", "content": [
                {"type": "text", "text": "x".repeat(200)},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,aGk="}}
            ]},
            {"role": "assistant", "content": "y".repeat(200)},
            {"role": "user", "content": "latest"},
        ],
    });
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let response = timeout(
        Duration::from_secs(10),
        client.request(
            Request::post(format!("{}/v1/chat/completions", gateway.url()))
                .header(header::AUTHORIZATION, "Bearer client-key")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-llmux-context-overflow", "middle-out")
                .body(Full::new(Bytes::from(body.to_string())))
                .unwrap(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    drop(gateway);
    drop(server);

    let lines: Vec<Value> = {
        let mut found = Vec::new();
        for _ in 0..50 {
            if let Ok(text) = fs::read_to_string(&log_path) {
                found = text
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                if !found.is_empty() {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        found
    };
    assert_eq!(lines.len(), 1);
    // The middle-out retry drops the image message; the record must still
    // describe the inbound request (one image), not the truncated body.
    assert_eq!(lines[0]["image_count"], 1);
    assert_eq!(lines[0]["outcome"], "routed");
}
