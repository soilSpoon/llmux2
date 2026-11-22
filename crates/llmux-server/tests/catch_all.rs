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
) -> (StatusCode, Json<Value>) {
    let failed = body["model"] == "failing-model";
    seen.lock().unwrap().push(body);
    if failed {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":{"message":"synthetic failure"}})),
        )
    } else {
        (
            StatusCode::OK,
            Json(json!({"id":"ok","object":"chat.completion","choices":[]})),
        )
    }
}

#[tokio::test]
async fn catch_all_routes_only_unknown_nonexcluded_ids() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let remote = TestServer::start(
        Router::new()
            .route("/v1/chat/completions", post(upstream))
            .with_state(seen.clone()),
    )
    .await
    .unwrap();
    let secrets = tempfile::tempdir().unwrap();
    let key = secrets.path().join("key");
    std::fs::write(&key, "synthetic-key").unwrap();
    let dashboard_key = secrets.path().join("dashboard-key");
    std::fs::write(&dashboard_key, "synthetic-dashboard-key").unwrap();
    let config = format!(
        r#"
[server]
client_keys = ["file:{}"]
dashboard_key = "file:{}"
[[models]]
name = "known"
aliases = ["alias"]
[[models.deployments]]
id = "known"
fallback = true
auth = "none"
provider = "openai_compat"
base_url = "{}/v1"
upstream_model = "fixed"
[[models]]
name = "native"
[[models.deployments]]
id = "native"
auth = "none"
provider = "openai_compat"
base_url = "{}/v1"
upstream_model = "native-fixed"
[[models]]
name = "*"
exclude = ["codex/*", "some-exact-id", "known"]
[[models.deployments]]
id = "catch-all"
auth = "none"
provider = "openai_compat"
base_url = "{}/v1"
upstream_model = "*"
"#,
        key.display(),
        dashboard_key.display(),
        remote.url(),
        remote.url(),
        remote.url()
    );
    let gateway = TestServer::start(
        llmux_server::app(RuntimeConfig::parse(&config).unwrap())
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    let client = Client::builder(TokioExecutor::new()).build_http();
    for (model, expected) in [
        ("vendor/New-ID", StatusCode::OK),
        ("CoDeX/private", StatusCode::BAD_REQUEST),
        ("SOME-EXACT-ID", StatusCode::BAD_REQUEST),
        ("*", StatusCode::BAD_REQUEST),
        ("known", StatusCode::OK),
        ("alias", StatusCode::OK),
        ("some-exact-id-extra", StatusCode::OK),
        ("native", StatusCode::OK),
        ("failing-model", StatusCode::BAD_REQUEST),
    ] {
        let request = Request::post(format!("{}/v1/chat/completions", gateway.url()))
            .header(header::AUTHORIZATION, "Bearer synthetic-key")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(
                json!({"model":model,"messages":[{"role":"user","content":"hello"}]}).to_string(),
            )))
            .unwrap();
        let response = client.request(request).await.unwrap();
        assert_eq!(response.status(), expected, "{model}");
        if expected != StatusCode::OK && model != "failing-model" {
            let body: Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            assert_eq!(body["error"]["code"], "bad_request");
            assert_eq!(body["error"]["message"], "Invalid model format");
        }
    }
    let models: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|body| body["model"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        models,
        [
            "vendor/New-ID",
            "fixed",
            "fixed",
            "some-exact-id-extra",
            "native-fixed",
            "failing-model"
        ]
    );
    let response = client
        .request(
            Request::get(format!("{}/v1/models", gateway.url()))
                .header(header::AUTHORIZATION, "Bearer synthetic-key")
                .body(Full::new(Bytes::new()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let listing: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let ids: Vec<_> = listing["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["alias", "known", "native"]);
    let response = client
        .request(
            Request::get(format!("{}/dashboard/stats", gateway.url()))
                .header(header::AUTHORIZATION, "Bearer synthetic-dashboard-key")
                .body(Full::new(Bytes::new()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let stats: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(stats["fallback"]["routed"], 4);
    assert_eq!(stats["fallback"]["total_routed"], 5);
    assert_eq!(stats["fallback"]["share"], 0.8);
    assert_eq!(stats["catch_all"]["models"]["vendor/New-ID"], 1);
    assert_eq!(stats["catch_all"]["models"]["some-exact-id-extra"], 1);
    assert!(stats["catch_all"]["models"].get("failing-model").is_none());
    assert_eq!(stats["catch_all"]["other"], 0);
    assert!(!stats.to_string().contains("synthetic-key"));
    assert!(!stats.to_string().contains("synthetic-dashboard-key"));
    for dep in stats["deployments"].as_array().unwrap() {
        assert_eq!(dep["fallback"], dep["id"] != "native");
    }
}
