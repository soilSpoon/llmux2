use axum::{body::Bytes, extract::State, routing::post, Json, Router};
use http::{header, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use llmux_config::RuntimeConfig;
use llmux_testkit::TestServer;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

async fn upstream(
    State(seen): State<Arc<Mutex<Vec<Value>>>>,
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
    for (file, value) in [("owner", "owner-key"), ("ci", "ci-key")] {
        std::fs::write(dir.join(file), value).unwrap();
    }
    let dir = dir.display();
    format!(
        r#"
[server]
client_keys = ["file:{dir}/owner", "file:{dir}/ci"]

[[server.key_policies]]
key = "file:{dir}/ci"
models = ["claude-*", "vendor/*", "exact?y"]
blocked = ["vendor/blocked-*"]

[[models]]
name = "claude-good"
aliases = ["claude-alias"]
[[models.deployments]]
id = "claude"
auth = "none"
provider = "openai_compat"
base_url = "{url}/v1"
upstream_model = "up-claude"
[[models]]
name = "other-model"
[[models.deployments]]
id = "other"
auth = "none"
provider = "openai_compat"
base_url = "{url}/v1"
upstream_model = "up-other"
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

async fn chat(gateway: &str, key: &str, model: &str) -> (StatusCode, Value) {
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let response = client
        .request(
            Request::post(format!("{gateway}/v1/chat/completions"))
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
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

#[tokio::test]
async fn restricted_key_may_only_request_names_matching_its_policy() {
    let (_dir, _remote, gateway, seen) = fixture().await;
    for (model, expected) in [
        ("claude-good", StatusCode::OK), // configured name matching `claude-*`
        ("claude-alias", StatusCode::OK), // the requested alias text still matches `claude-*`
        ("vendor/x", StatusCode::OK), // an unknown name allowed by `vendor/*`, routed by the catch-all
        ("exactly", StatusCode::OK),  // `?` matches the single character `l`
        ("exactx", StatusCode::FORBIDDEN), // `?` cannot match nothing
        ("other-model", StatusCode::FORBIDDEN), // a configured name outside the allowlist
        ("gpt-4", StatusCode::FORBIDDEN),
        ("vendor/blocked-x", StatusCode::FORBIDDEN), // `blocked` wins over `vendor/*`
    ] {
        let (status, body) = chat(&gateway.url(), "ci-key", model).await;
        assert_eq!(status, expected, "{model}: {body}");
        if expected == StatusCode::FORBIDDEN {
            assert_eq!(body["error"]["code"], "model_not_allowed", "{model}");
        }
    }
    assert_eq!(
        seen_models(&seen),
        ["up-claude", "up-claude", "vendor/x", "exactly"],
        "only allowed requests may reach an upstream"
    );

    // /v1/responses shares the same enforcement and identity.
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    for (model, expected) in [
        ("gpt-4", StatusCode::FORBIDDEN),
        ("claude-good", StatusCode::OK),
    ] {
        let response = client
            .request(
                Request::post(format!("{}/v1/responses", gateway.url()))
                    .header(header::AUTHORIZATION, "Bearer ci-key")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Full::new(Bytes::from(
                        json!({"model":model,"input":"hi"}).to_string(),
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{model}");
        if expected == StatusCode::FORBIDDEN {
            let body: Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            assert_eq!(body["error"]["code"], "model_not_allowed");
        }
    }
    assert_eq!(seen_models(&seen).last().unwrap(), "up-claude");
}

#[tokio::test]
async fn unrestricted_key_keeps_current_behavior_including_catch_all() {
    let (_dir, _remote, gateway, seen) = fixture().await;
    for (model, expected) in [
        ("claude-good", StatusCode::OK),
        ("other-model", StatusCode::OK),
        ("vendor/blocked-x", StatusCode::OK), // an unknown name passes through the catch-all
        ("gpt-4", StatusCode::OK),
        ("*", StatusCode::BAD_REQUEST), // a literal `*` stays an invalid model id
    ] {
        let (status, body) = chat(&gateway.url(), "owner-key", model).await;
        assert_eq!(status, expected, "{model}: {body}");
    }
    assert_eq!(
        seen_models(&seen),
        ["up-claude", "up-other", "vendor/blocked-x", "gpt-4"]
    );
}

#[tokio::test]
async fn restricted_key_applies_to_anthropic_messages_identity() {
    let (_dir, _remote, gateway, seen) = fixture().await;
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let messages = |model: &str| {
        Request::post(format!("{}/v1/messages", gateway.url()))
            .header("x-api-key", "ci-key")
            .header("anthropic-version", "2023-06-01")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(
                json!({"model":model,"max_tokens":1,"messages":[]}).to_string(),
            )))
            .unwrap()
    };
    let denied = client.request(messages("other-model")).await.unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    let body: Value =
        serde_json::from_slice(&denied.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["error"]["type"], "permission_error");
    assert!(seen.lock().unwrap().is_empty(), "no upstream call");

    let allowed = client.request(messages("claude-good")).await.unwrap();
    assert_eq!(allowed.status(), StatusCode::OK);
    assert_eq!(seen_models(&seen), ["up-claude"]);
}

async fn listed(gateway: &str, key: &str) -> Vec<String> {
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let response = client
        .request(
            Request::get(format!("{gateway}/v1/models"))
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
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
async fn models_listing_is_filtered_by_the_requesting_key_policy() {
    let (_dir, _remote, gateway, _seen) = fixture().await;
    assert_eq!(
        listed(&gateway.url(), "ci-key").await,
        ["claude-alias", "claude-good"]
    );
    assert_eq!(
        listed(&gateway.url(), "owner-key").await,
        ["claude-alias", "claude-good", "other-model"]
    );
}

fn free_only_config(url: &str, dir: &std::path::Path) -> String {
    for (file, value) in [("owner", "owner-key"), ("ci", "ci-key")] {
        std::fs::write(dir.join(file), value).unwrap();
    }
    let dir = dir.display();
    format!(
        r#"
[server]
client_keys = ["file:{dir}/owner", "file:{dir}/ci"]

[[server.key_policies]]
key = "file:{dir}/ci"
free_only = true

[[models]]
name = "billed-only"
[[models.deployments]]
id = "billed"
auth = "none"
provider = "openai_compat"
base_url = "{url}/v1"
upstream_model = "up-billed"
billing = "billed"
[[models]]
name = "free-servable"
[[models.deployments]]
id = "free"
auth = "none"
provider = "openai_compat"
base_url = "{url}/v1"
upstream_model = "up-free"
billing = "free"
[[models]]
name = "mixed"
[[models.deployments]]
id = "mixed-billed"
auth = "none"
provider = "openai_compat"
base_url = "{url}/v1"
upstream_model = "up-mixed-billed"
billing = "billed"
[[models.deployments]]
id = "mixed-free"
auth = "none"
provider = "openai_compat"
base_url = "{url}/v1"
upstream_model = "up-mixed-free"
billing = "free"
"#
    )
}

#[tokio::test]
async fn free_only_key_sees_no_billed_only_models() {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let remote = TestServer::start(
        Router::new()
            .route("/v1/chat/completions", post(upstream))
            .with_state(seen.clone()),
    )
    .await
    .unwrap();
    let gateway = TestServer::start(
        llmux_server::app(
            RuntimeConfig::parse(&free_only_config(&remote.url(), dir.path())).unwrap(),
        )
        .await
        .unwrap(),
    )
    .await
    .unwrap();
    let gateway = gateway.url();

    // ci key: billed-only models are treated as if they do not exist —
    // absent from the list and unroutable — while any model with a usable
    // free leg (including shared billed/free models) stays advertised.
    assert_eq!(listed(&gateway, "ci-key").await, ["free-servable", "mixed"]);
    assert_eq!(
        listed(&gateway, "owner-key").await,
        ["billed-only", "free-servable", "mixed"]
    );

    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let messages = |model: &str, key: &str| {
        Request::post(format!("{gateway}/v1/chat/completions"))
            .header(header::AUTHORIZATION, format!("Bearer {key}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(
                json!({"model":model,"messages":[{"role":"user","content":"hi"}]}).to_string(),
            )))
            .unwrap()
    };

    // The free-only key never spends on a billed leg — billed-only models
    // fail without an upstream call, and mixed models hit only the free leg.
    let response = client
        .request(messages("billed-only", "ci-key"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(seen.lock().unwrap().is_empty(), "no upstream call");

    let response = client.request(messages("mixed", "ci-key")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(seen_models(&seen), ["up-mixed-free"]);
}

#[test]
fn key_policies_must_reference_a_client_key() {
    let dir = tempfile::tempdir().unwrap();
    // `other` resolves to a real secret but is not in `client_keys`.
    std::fs::write(dir.path().join("other"), "other-key").unwrap();
    let text = config("http://127.0.0.1:1", dir.path()).replace(
        &format!("key = \"file:{}/ci\"", dir.path().display()),
        &format!("key = \"file:{}/other\"", dir.path().display()),
    );
    let err = RuntimeConfig::parse(&text)
        .err()
        .expect("a policy for a key outside client_keys must be rejected");
    assert!(err.to_string().contains("client_keys"), "{err}");
}
