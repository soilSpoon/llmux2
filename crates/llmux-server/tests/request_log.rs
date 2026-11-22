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
    sync::{Arc, Mutex},
};
use tokio::time::timeout;

struct Upstream {
    bodies: Mutex<Vec<Value>>,
}

async fn messages(State(up): State<Arc<Upstream>>, body: Bytes) -> impl IntoResponse {
    let request: Value = serde_json::from_slice(&body).unwrap();
    up.bodies.lock().unwrap().push(request);
    Json(json!({
        "id": "msg_01XFDUD",
        "type": "message",
        "role": "assistant",
        "model": "claude-upstream",
        "content": [{"type": "text", "text": "ok"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    }))
}

#[tokio::test]
async fn request_log_attributes_key_images_and_upstream_id() {
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
                if !found.is_empty() {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        found
    };
    assert_eq!(lines.len(), 1);
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
}
