use axum::{body::Bytes, routing::post, Json, Router};
use http::{header, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use llmux_config::RuntimeConfig;
use llmux_testkit::TestServer;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

async fn upstream(
    axum::extract::State(seen): axum::extract::State<Arc<Mutex<Vec<Value>>>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    seen.lock().unwrap().push(body);
    Json(json!({
        "id":"ok","object":"chat.completion",
        "choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
    }))
}

fn config(url: &str, dir: &std::path::Path) -> String {
    std::fs::write(dir.join("owner"), "owner-key").unwrap();
    let dir = dir.display();
    format!(
        r#"
[server]
client_keys = ["file:{dir}/owner"]
deny = ["omniroute/*", "billed-*"]

[[models]]
name = "claude-good"
[[models.deployments]]
id = "claude"
auth = "none"
provider = "openai_compat"
base_url = "{url}/v1"
upstream_model = "up-claude"
[[models]]
name = "billed-only"
[[models.deployments]]
id = "billed"
auth = "none"
provider = "openai_compat"
base_url = "{url}/v1"
upstream_model = "up-billed"
[[models]]
name = "*"
[[models.deployments]]
id = "catch-all"
auth = "none"
provider = "openai_compat"
base_url = "{url}/v1"
upstream_model = "*"
"#
    )
}

async fn fixture() -> (
    tempfile::TempDir,
    TestServer,
    TestServer,
    Arc<Mutex<Vec<Value>>>,
) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let remote = TestServer::start(
        Router::new()
            .route("/v1/chat/completions", post(upstream))
            .with_state(seen.clone()),
    )
    .await
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let app = llmux_server::app(RuntimeConfig::parse(&config(&remote.url(), dir.path())).unwrap())
        .await
        .unwrap();
    let gateway = TestServer::start(app).await.unwrap();
    (dir, remote, gateway, seen)
}

async fn chat(gateway: &str, model: &str) -> (StatusCode, Value) {
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let response = client
        .request(
            Request::post(format!("{gateway}/v1/chat/completions"))
                .header(header::AUTHORIZATION, "Bearer owner-key")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Full::new(Bytes::from(
                    json!({"model":model,"messages":[{"role":"user","content":"hi"}]}).to_string(),
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    (status, body)
}

fn seen_models(seen: &Arc<Mutex<Vec<Value>>>) -> Vec<String> {
    seen.lock()
        .unwrap()
        .iter()
        .map(|body| body["model"].as_str().unwrap().to_owned())
        .collect()
}

// The deny list closes the auto-resolve bypass: a `provider/model` id that no
// configured model names would otherwise ride the `*` catch-all to a billed
// upstream. Denied names are rejected on the requested text before alias,
// canonical, or catch-all resolution, for every key — including unrestricted
// ones with no key_policies entry.
#[tokio::test]
async fn denied_names_are_rejected_before_resolution_for_every_key() {
    let (_dir, _remote, gateway, seen) = fixture().await;
    for (model, expected) in [
        ("omniroute/gpt-4", StatusCode::FORBIDDEN), // unknown name, would hit the catch-all
        ("billed-only", StatusCode::FORBIDDEN),     // a configured name is denied too
        ("claude-good", StatusCode::OK),
        ("other/x", StatusCode::OK), // an unknown non-denied name still rides the catch-all
        ("omniroute-x/gpt", StatusCode::OK), // `omniroute/*` does not span the literal prefix
    ] {
        let (status, body) = chat(&gateway.url(), model).await;
        assert_eq!(status, expected, "{model}: {body}");
        if expected == StatusCode::FORBIDDEN {
            assert_eq!(body["error"]["code"], "model_not_allowed", "{model}");
        }
    }
    assert_eq!(
        seen_models(&seen),
        ["up-claude", "other/x", "omniroute-x/gpt"],
        "only non-denied requests may reach an upstream"
    );
}

async fn listed(gateway: &str) -> Vec<String> {
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let response = client
        .request(
            Request::get(format!("{gateway}/v1/models"))
                .header(header::AUTHORIZATION, "Bearer owner-key")
                .body(Full::new(Bytes::new()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let listing: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    listing["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn models_listing_hides_denied_names_for_every_key() {
    let (_dir, _remote, gateway, _seen) = fixture().await;
    assert_eq!(listed(&gateway.url()).await, ["claude-good"]);
}

#[test]
fn deny_rejects_empty_patterns() {
    let dir = tempfile::tempdir().unwrap();
    let text = config("http://127.0.0.1:1", dir.path()).replace("\"omniroute/*\"", "\"\"");
    let err = RuntimeConfig::parse(&text)
        .err()
        .expect("an empty deny pattern must be rejected");
    assert!(err.to_string().contains("deny"), "{err}");
}
