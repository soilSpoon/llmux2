use axum::{
    body::Bytes,
    extract::State,
    response::{IntoResponse, Response},
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
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::Notify,
    time::{advance, pause, resume},
};

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

async fn chat_with_content(
    gateway: &str,
    key: &str,
    model: &str,
    content: &str,
) -> (StatusCode, Value) {
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let response = client
        .request(
            Request::post(format!("{gateway}/v1/chat/completions"))
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Full::new(Bytes::from(
                    json!({"model":model,"messages":[{"role":"user","content":content}]})
                        .to_string(),
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

async fn chat(gateway: &str, key: &str, model: &str) -> (StatusCode, Value) {
    chat_with_content(gateway, key, model, "hi").await
}

async fn empty_chat(gateway: &str, key: &str, model: &str) -> (StatusCode, Value) {
    chat_with_content(gateway, key, model, "").await
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

fn free_only_quota_config(url: &str, dir: &std::path::Path) -> String {
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
name = "quota-draining"
[[models.deployments]]
id = "free-quota"
auth = "none"
provider = "openai_compat"
base_url = "{url}/v1"
upstream_model = "up-free-quota"
billing = "free"
quota = {{ rpm = 1 }}
"#
    )
}

#[tokio::test]
async fn exhausted_free_leg_stops_counting_for_the_free_catalog() {
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
            RuntimeConfig::parse(&free_only_quota_config(&remote.url(), dir.path())).unwrap(),
        )
        .await
        .unwrap(),
    )
    .await
    .unwrap();
    let gateway = gateway.url();

    assert_eq!(listed(&gateway, "ci-key").await, ["quota-draining"]);
    let (status, _) = chat(&gateway, "ci-key", "quota-draining").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(seen_models(&seen), ["up-free-quota"]);

    let (status, _) = chat(&gateway, "ci-key", "quota-draining").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        seen_models(&seen),
        ["up-free-quota"],
        "no second upstream call"
    );

    let empty: Vec<String> = Vec::new();
    assert_eq!(listed(&gateway, "ci-key").await, empty);
    assert_eq!(listed(&gateway, "owner-key").await, ["quota-draining"]);
}

struct CoolingUpstream {
    seen: Mutex<Vec<Value>>,
}

async fn cooling_upstream(
    State(state): State<Arc<CoolingUpstream>>,
    Json(body): Json<Value>,
) -> Response {
    state.seen.lock().unwrap().push(body);
    if state.seen.lock().unwrap().len() == 1 {
        (StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, "5")]).into_response()
    } else {
        Json(json!({
            "id":"ok","object":"chat.completion",
            "choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
        }))
        .into_response()
    }
}

fn free_only_cooldown_config(url: &str, dir: &std::path::Path) -> String {
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
name = "cooling-free"
[[models.deployments]]
id = "cooling-free"
auth = "none"
provider = "openai_compat"
base_url = "{url}/v1"
upstream_model = "up-cooling-free"
billing = "free"
"#
    )
}

#[tokio::test]
async fn cooling_free_leg_leaves_and_returns_to_the_free_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let upstream_state = Arc::new(CoolingUpstream {
        seen: Mutex::new(Vec::new()),
    });
    let remote = TestServer::start(
        Router::new()
            .route("/v1/chat/completions", post(cooling_upstream))
            .with_state(upstream_state.clone()),
    )
    .await
    .unwrap();
    let gateway = TestServer::start(
        llmux_server::app(
            RuntimeConfig::parse(&free_only_cooldown_config(&remote.url(), dir.path())).unwrap(),
        )
        .await
        .unwrap(),
    )
    .await
    .unwrap();
    let gateway = gateway.url();

    assert_eq!(listed(&gateway, "ci-key").await, ["cooling-free"]);
    let (status, body) = chat(&gateway, "ci-key", "cooling-free").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(upstream_state.seen.lock().unwrap().len(), 1);

    let empty: Vec<String> = Vec::new();
    assert_eq!(
        listed(&gateway, "ci-key").await,
        empty,
        "cooling free leg still counted for the free catalog"
    );
    assert_eq!(listed(&gateway, "owner-key").await, ["cooling-free"]);

    pause();
    advance(Duration::from_secs(6)).await;
    resume();

    assert_eq!(listed(&gateway, "ci-key").await, ["cooling-free"]);
    let (status, body) = chat(&gateway, "ci-key", "cooling-free").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(upstream_state.seen.lock().unwrap().len(), 2);
}

struct DelayState {
    entered: Notify,
    release: Notify,
    seen: Mutex<Vec<Value>>,
}

async fn delayed_upstream(
    State(state): State<Arc<DelayState>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    state.entered.notify_one();
    state.release.notified().await;
    state.seen.lock().unwrap().push(body);
    Json(json!({
        "id":"ok","object":"chat.completion",
        "choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
    }))
}

fn free_only_concurrency_config(url: &str, dir: &std::path::Path) -> String {
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
name = "busy-free"
[[models.deployments]]
id = "busy-free"
auth = "none"
provider = "openai_compat"
base_url = "{url}/v1"
upstream_model = "up-busy-free"
billing = "free"
quota = {{ concurrent = 1 }}
"#
    )
}

#[tokio::test]
async fn full_concurrency_hides_then_releases_the_free_catalog_entry() {
    let dir = tempfile::tempdir().unwrap();
    let upstream_state = Arc::new(DelayState {
        entered: Notify::new(),
        release: Notify::new(),
        seen: Mutex::new(Vec::new()),
    });
    let remote = TestServer::start(
        Router::new()
            .route("/v1/chat/completions", post(delayed_upstream))
            .with_state(upstream_state.clone()),
    )
    .await
    .unwrap();
    let gateway = TestServer::start(
        llmux_server::app(
            RuntimeConfig::parse(&free_only_concurrency_config(&remote.url(), dir.path())).unwrap(),
        )
        .await
        .unwrap(),
    )
    .await
    .unwrap();
    let gateway = gateway.url();

    assert_eq!(listed(&gateway, "ci-key").await, ["busy-free"]);
    let first_gateway = gateway.clone();
    let first = tokio::spawn(async move { chat(&first_gateway, "ci-key", "busy-free").await });
    upstream_state.entered.notified().await;

    let empty: Vec<String> = Vec::new();
    assert_eq!(
        listed(&gateway, "ci-key").await,
        empty,
        "busy free leg stayed advertised while its only concurrency slot was full"
    );
    assert_eq!(listed(&gateway, "owner-key").await, ["busy-free"]);
    let (status, body) = chat(&gateway, "ci-key", "busy-free").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(upstream_state.seen.lock().unwrap().len(), 0);
    upstream_state.release.notify_one();

    let (first_status, first_body) = first.await.unwrap();
    assert_eq!(first_status, StatusCode::OK, "{first_body}");
    assert_eq!(listed(&gateway, "ci-key").await, ["busy-free"]);
}

async fn no_api_allowance_upstream(
    State(seen): State<Arc<Mutex<Vec<Value>>>>,
    Json(body): Json<Value>,
) -> Response {
    seen.lock().unwrap().push(body);
    (
        StatusCode::FORBIDDEN,
        Json(json!({"error":{"code":"no_api_allowance","message":"no api allowance"}})),
    )
        .into_response()
}

fn free_only_expiry_parking_config(url: &str, dir: &std::path::Path) -> String {
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
name = "expired-free"
[[models.deployments]]
id = "expired-free"
auth = "none"
provider = "openai_compat"
base_url = "{url}/v1"
upstream_model = "up-expired-free"
billing = "free"
expires_at = 2000-01-01T00:00:00Z

[[models]]
name = "parking-free"
[[models.deployments]]
id = "parking-free"
auth = "none"
provider = "openai_compat"
base_url = "{url}/v1"
upstream_model = "up-parking-free"
billing = "free"
"#
    )
}

#[tokio::test]
async fn expired_and_parked_free_legs_leave_the_free_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let remote = TestServer::start(
        Router::new()
            .route("/v1/chat/completions", post(no_api_allowance_upstream))
            .with_state(seen.clone()),
    )
    .await
    .unwrap();
    let gateway = TestServer::start(
        llmux_server::app(
            RuntimeConfig::parse(&free_only_expiry_parking_config(&remote.url(), dir.path()))
                .unwrap(),
        )
        .await
        .unwrap(),
    )
    .await
    .unwrap();
    let gateway = gateway.url();

    assert_eq!(listed(&gateway, "ci-key").await, ["parking-free"]);
    assert_eq!(
        listed(&gateway, "owner-key").await,
        ["expired-free", "parking-free"]
    );

    let (_status, _body) = chat(&gateway, "ci-key", "parking-free").await;
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "parking probe missed upstream"
    );

    let empty: Vec<String> = Vec::new();
    assert_eq!(
        listed(&gateway, "ci-key").await,
        empty,
        "expired or parked free legs still counted for the free catalog"
    );
    assert_eq!(
        listed(&gateway, "owner-key").await,
        ["expired-free", "parking-free"]
    );
}

fn free_only_token_window_config(url: &str, dir: &std::path::Path) -> String {
    for (file, value) in [
        ("owner", "owner-key"),
        ("ci", "ci-key"),
        ("token-cred", "synthetic-token-cred"),
    ] {
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

[[shared_quotas]]
name = "token-family"
tpm = 6

[[credentials]]
id = "token-cred"
kind = "api_key"
provider = "openai_compat"
key = "file:{dir}/token-cred"
quota = {{ tpd = 6 }}

[[models]]
name = "deployment-token-starved"
[[models.deployments]]
id = "deployment-token"
auth = "none"
provider = "openai_compat"
base_url = "{url}/v1"
upstream_model = "up-deployment-token"
billing = "free"
quota = {{ tpm = 6 }}

[[models]]
name = "deployment-token-tight"
[[models.deployments]]
id = "deployment-token-tight"
auth = "none"
provider = "openai_compat"
base_url = "{url}/v1"
upstream_model = "up-deployment-token-tight"
billing = "free"
quota = {{ tpm = 9 }}

[[models]]
name = "deployment-token-exact"
[[models.deployments]]
id = "deployment-token-exact"
auth = "none"
provider = "openai_compat"
base_url = "{url}/v1"
upstream_model = "up-deployment-token-exact"
billing = "free"
quota = {{ tpm = 10 }}

[[models]]
name = "credential-token-starved"
[[models.deployments]]
id = "credential-token"
credential = "token-cred"
base_url = "{url}/v1"
upstream_model = "up-credential-token"
billing = "free"

[[models]]
name = "shared-token-starved"
[[models.deployments]]
id = "shared-token"
auth = "none"
provider = "openai_compat"
base_url = "{url}/v1"
upstream_model = "up-shared-token"
billing = "free"
shared_quota = "token-family"
"#
    )
}

#[tokio::test]
async fn token_starved_free_legs_stop_counting_for_the_free_catalog() {
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
            RuntimeConfig::parse(&free_only_token_window_config(&remote.url(), dir.path()))
                .unwrap(),
        )
        .await
        .unwrap(),
    )
    .await
    .unwrap();
    let gateway = gateway.url();

    for model in [
        "deployment-token-starved",
        "credential-token-starved",
        "shared-token-starved",
        "deployment-token-tight",
        "deployment-token-exact",
    ] {
        assert!(listed(&gateway, "ci-key").await.contains(&model.to_owned()));
    }

    // A lower boundary: after the first minimal chat the tight model has four
    // units left. A cost-1 probe would still admit, but another minimal chat
    // must not, so the model must leave the catalog.
    let tight = "deployment-token-tight";
    let before = seen.lock().unwrap().len();
    let (status, _) = chat(&gateway, "ci-key", tight).await;
    assert_eq!(status, StatusCode::OK, "{tight}: first minimal chat");
    assert_eq!(seen.lock().unwrap().len(), before + 1);
    let (status, _) = chat(&gateway, "ci-key", tight).await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "{tight}: four units cannot admit another minimal chat"
    );
    assert_eq!(seen.lock().unwrap().len(), before + 1);
    assert!(!listed(&gateway, "ci-key").await.contains(&tight.to_owned()));

    // A positive boundary: after the first minimal chat the exact model has
    // five units left, exactly the minimum OpenAI chat admission cost, so it
    // must remain advertised and admit the second minimal chat.
    let exact = "deployment-token-exact";
    let before = seen.lock().unwrap().len();
    let (status, _) = chat(&gateway, "ci-key", exact).await;
    assert_eq!(status, StatusCode::OK, "{exact}: first minimal chat");
    assert_eq!(seen.lock().unwrap().len(), before + 1);
    assert!(listed(&gateway, "ci-key").await.contains(&exact.to_owned()));
    let (status, _) = chat(&gateway, "ci-key", exact).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{exact}: exact remaining minimum cost"
    );
    assert_eq!(seen.lock().unwrap().len(), before + 2);

    let mut wrongly_advertised = Vec::new();
    for model in [
        "deployment-token-starved",
        "credential-token-starved",
        "shared-token-starved",
    ] {
        let before = seen.lock().unwrap().len();
        let (status, _) = chat(&gateway, "ci-key", model).await;
        assert_eq!(status, StatusCode::OK, "{model}: first minimal chat");
        assert_eq!(
            seen.lock().unwrap().len(),
            before + 1,
            "{model}: first chat reached upstream"
        );

        let (status, _) = chat(&gateway, "ci-key", model).await;
        assert_eq!(
            status,
            StatusCode::TOO_MANY_REQUESTS,
            "{model}: second minimal chat has only one token unit left"
        );
        assert_eq!(
            seen.lock().unwrap().len(),
            before + 1,
            "{model}: token-starved second chat made an upstream call"
        );
        if listed(&gateway, "ci-key").await.contains(&model.to_owned()) {
            wrongly_advertised.push(model);
        }
    }
    assert!(
        wrongly_advertised.is_empty(),
        "token-starved models still advertised: {wrongly_advertised:?}"
    );
}

fn free_only_tiny_context_config(url: &str, dir: &std::path::Path) -> String {
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
name = "tiny-context"
[[models.deployments]]
id = "tiny-context"
auth = "none"
provider = "openai_compat"
base_url = "{url}/v1"
upstream_model = "up-tiny-context"
billing = "free"
max_context = 4
"#
    )
}

#[tokio::test]
async fn context_too_small_for_minimal_chat_is_not_advertised() {
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
            RuntimeConfig::parse(&free_only_tiny_context_config(&remote.url(), dir.path()))
                .unwrap(),
        )
        .await
        .unwrap(),
    )
    .await
    .unwrap();
    let gateway = gateway.url();

    assert!(
        !listed(&gateway, "ci-key")
            .await
            .contains(&"tiny-context".to_owned()),
        "max_context=4 cannot fit the minimal OpenAI chat cost but was advertised"
    );
    let (status, body) = chat(&gateway, "ci-key", "tiny-context").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "max_context=4 minimal chat: {body}"
    );
    assert_eq!(
        seen.lock().unwrap().len(),
        0,
        "max_context=4 chat reached upstream"
    );
}

fn token_floor_config(url: &str, dir: &std::path::Path, cap: u32) -> String {
    for (file, value) in [
        ("owner", "owner-key"),
        ("ci", "ci-key"),
        ("token-cred", "synthetic-token-cred"),
    ] {
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

[[credentials]]
id = "token-cred"
kind = "api_key"
provider = "openai_compat"
key = "file:{dir}/token-cred"
quota = {{ tpm = {cap} }}

[[models]]
name = "token-floor"
[[models.deployments]]
id = "token-floor"
credential = "token-cred"
base_url = "{url}/v1"
upstream_model = "up-token-floor"
billing = "free"
"#
    )
}

#[tokio::test]
async fn subfive_openai_chat_estimates_use_the_five_token_floor() {
    for cap in [4_u32, 5] {
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
                RuntimeConfig::parse(&token_floor_config(&remote.url(), dir.path(), cap)).unwrap(),
            )
            .await
            .unwrap(),
        )
        .await
        .unwrap();
        let gateway = gateway.url();

        let advertised = listed(&gateway, "ci-key")
            .await
            .contains(&"token-floor".to_owned());
        let (status, body) = empty_chat(&gateway, "ci-key", "token-floor").await;
        let upstream_calls = seen.lock().unwrap().len();
        if cap == 4 {
            assert!(!advertised, "cap 4 advertised a sub-five-token chat");
            assert_eq!(
                status,
                StatusCode::TOO_MANY_REQUESTS,
                "cap 4 empty chat: {body}"
            );
            assert_eq!(upstream_calls, 0, "cap 4 empty chat reached upstream");
        } else {
            assert!(advertised, "cap 5 did not advertise the exact floor cost");
            assert_eq!(status, StatusCode::OK, "cap 5 empty chat: {body}");
            assert_eq!(upstream_calls, 1, "cap 5 empty chat missed upstream");
        }
    }
}

fn free_only_cred_config(url: &str, dir: &std::path::Path) -> String {
    for (file, value) in [
        ("owner", "owner-key"),
        ("ci", "ci-key"),
        ("admin", "admin-key"),
        ("free-cred", "synthetic-free"),
    ] {
        std::fs::write(dir.join(file), value).unwrap();
    }
    let dir = dir.display();
    format!(
        r#"
[server]
client_keys = ["file:{dir}/owner", "file:{dir}/ci"]
dashboard_key = "file:{dir}/admin"

[[server.key_policies]]
key = "file:{dir}/ci"
free_only = true

[[credentials]]
id = "free-cred"
kind = "api_key"
provider = "openai_compat"
key = "file:{dir}/free-cred"

[[models]]
name = "free-cred-model"
[[models.deployments]]
id = "free-cred-dep"
credential = "free-cred"
base_url = "{url}/v1"
upstream_model = "up-free-cred"
billing = "free"
"#
    )
}

async fn set_disabled(gateway: &str, id: &str, disabled: bool) {
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let response = client
        .request(
            Request::post(format!("{gateway}/admin/credentials/{id}/disabled"))
                .header(header::AUTHORIZATION, "Bearer admin-key")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Full::new(Bytes::from(
                    json!({"disabled": disabled}).to_string(),
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn disabling_a_free_legs_credential_removes_it_from_the_free_catalog() {
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
            RuntimeConfig::parse(&free_only_cred_config(&remote.url(), dir.path())).unwrap(),
        )
        .await
        .unwrap(),
    )
    .await
    .unwrap();
    let gateway = gateway.url();

    // Baseline: the credential-backed free leg makes the model free for ci.
    assert_eq!(listed(&gateway, "ci-key").await, ["free-cred-model"]);
    assert_eq!(listed(&gateway, "owner-key").await, ["free-cred-model"]);

    // Disabling the credential behind its only free leg takes the model out
    // of the free-only key's catalog — an expired plan stops counting.
    set_disabled(&gateway, "free-cred", true).await;
    let empty: Vec<String> = Vec::new();
    assert_eq!(listed(&gateway, "ci-key").await, empty);
    // The unrestricted key still sees the model; disable is a routing state,
    // not a config removal.
    assert_eq!(listed(&gateway, "owner-key").await, ["free-cred-model"]);

    // Re-enabling restores the leg and the model returns for the free key.
    set_disabled(&gateway, "free-cred", false).await;
    assert_eq!(listed(&gateway, "ci-key").await, ["free-cred-model"]);
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
