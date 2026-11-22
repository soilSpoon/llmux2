use axum::{body::Bytes, routing::post, Json, Router};
use http::{header, HeaderMap, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use llmux_config::RuntimeConfig;
use llmux_testkit::TestServer;
use serde_json::json;
use std::fs;

fn config(url: &str) -> RuntimeConfig {
    let secrets = tempfile::tempdir().unwrap();
    for (name, value) in [
        ("client", "gateway-key"),
        ("refresh", "refresh-secret"),
        ("secret", "client-secret"),
    ] {
        fs::write(secrets.path().join(name), value).unwrap();
    }
    let path = secrets.path().display();
    RuntimeConfig::parse(&format!(
        r#"
[server]
client_keys = ["file:{path}/client"]
[[credentials]]
id = "oauth"
kind = "oauth"
provider = "openai_compat"
token_url = "{url}/token"
client_id = "client-id"
client_secret = "file:{path}/secret"
refresh_token = "file:{path}/refresh"
scopes = ["read", "write"]
refresh_skew_ms = 1000
request_timeout_ms = 2000
[[models]]
name = "model"
[[models.deployments]]
id = "oauth-deployment"
credential = "oauth"
base_url = "{url}/v1"
upstream_model = "upstream"
"#
    ))
    .unwrap()
}

#[tokio::test]
async fn file_rotation_survives_restart_and_resource_is_forwarded() {
    let secrets = tempfile::tempdir().unwrap();
    let refresh = secrets.path().join("refresh");
    let client = secrets.path().join("client");
    fs::write(&refresh, "original").unwrap();
    fs::write(&client, "gateway-key").unwrap();
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let endpoint_hits = hits.clone();
    let upstream = TestServer::start(Router::new().route("/token", post(move |body: String| {
        let hits = endpoint_hits.clone();
        async move {
            let hit = hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let expected = if hit == 0 { "original" } else { "rotated" };
            assert_eq!(body, format!("grant_type=refresh_token&client_id=client-id&refresh_token={expected}&resource=https%3A%2F%2Fapi.openai.com%2Fv1"));
            Json(json!({"access_token":"access", "token_type":"Bearer", "expires_in":3600, "refresh_token":"rotated"}))
        }
    }))).await.unwrap();
    let text = format!(
        r#"
[server]
client_keys = ["file:{}"]
[[credentials]]
id = "oauth"
kind = "oauth"
provider = "openai_compat"
token_url = "{}/token"
client_id = "client-id"
refresh_token = "file:{}"
resource = "https://api.openai.com/v1"
[[models]]
name = "model"
[[models.deployments]]
id = "deployment"
credential = "oauth"
base_url = "{}/v1"
upstream_model = "upstream"
"#,
        client.display(),
        upstream.url(),
        refresh.display(),
        upstream.url()
    );
    for _ in 0..2 {
        let app = llmux_server::app(RuntimeConfig::parse(&text).unwrap())
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(&refresh).unwrap(), "rotated");
        drop(app);
    }
    assert_eq!(hits.load(std::sync::atomic::Ordering::Relaxed), 2);
}

#[tokio::test]
async fn oauth_deployment_uses_initial_access_token() {
    let upstream = TestServer::start(Router::new()
        .route("/token", post(|headers: HeaderMap, body: String| async move {
            assert_eq!(headers[header::CONTENT_TYPE], "application/x-www-form-urlencoded");
            assert_eq!(body, "grant_type=refresh_token&client_id=client-id&refresh_token=refresh-secret&client_secret=client-secret&scope=read+write");
            Json(json!({"access_token":"access-token", "token_type":"Bearer", "expires_in":3600}))
        }))
        .route("/v1/chat/completions", post(|headers: HeaderMap, Json(body): Json<serde_json::Value>| async move {
            assert_eq!(headers[header::AUTHORIZATION], "Bearer access-token");
            assert_eq!(body["model"], "upstream");
            Json(json!({"id":"oauth-response"}))
        }))).await.unwrap();
    let gateway = TestServer::start(llmux_server::app(config(&upstream.url())).await.unwrap())
        .await
        .unwrap();
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let response = client
        .request(
            Request::post(format!("{}/v1/chat/completions", gateway.url()))
                .header(header::AUTHORIZATION, "Bearer gateway-key")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Full::new(Bytes::from_static(
                    br#"{"model":"model","messages":[{"role":"user","content":"Hello"}]}"#,
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-llmux-deployment"], "oauth-deployment");
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        r#"{"id":"oauth-response"}"#
    );
}

#[tokio::test]
async fn oauth_startup_rejects_failed_or_empty_token_responses_without_leaking_secrets() {
    for (status, body) in [
        (
            StatusCode::BAD_REQUEST,
            r#"{"error_description":"refresh-secret client-secret"}"#,
        ),
        (
            StatusCode::OK,
            r#"{"access_token":"","token_type":"Bearer"}"#,
        ),
        (StatusCode::OK, "{}"),
    ] {
        let upstream = TestServer::start(
            Router::new().route("/token", post(move || async move { (status, body) })),
        )
        .await
        .unwrap();
        let error = llmux_server::app(config(&upstream.url()))
            .await
            .expect_err("startup must fail")
            .to_string();
        assert!(!error.contains("refresh-secret"));
        assert!(!error.contains("client-secret"));
    }
}

#[tokio::test]
async fn unopenable_request_log_fails_build_before_credential_refresh() {
    let secrets = tempfile::tempdir().unwrap();
    let refresh = secrets.path().join("refresh");
    let client = secrets.path().join("client");
    fs::write(&refresh, "original").unwrap();
    fs::write(&client, "gateway-key").unwrap();
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let endpoint_hits = hits.clone();
    let upstream = TestServer::start(Router::new().route("/token", post(move || {
        let hits = endpoint_hits.clone();
        async move {
            hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Json(json!({"access_token":"access","token_type":"Bearer","expires_in":3600,"refresh_token":"rotated"}))
        }
    }))).await.unwrap();
    // OpenOptions creates the file but not missing parents, so this open fails.
    let request_log = secrets.path().join("missing").join("requests.jsonl");
    let text = format!(
        r#"
[server]
client_keys = ["file:{}"]
request_log = "{}"
[[credentials]]
id = "oauth"
kind = "oauth"
provider = "openai_compat"
token_url = "{}/token"
client_id = "client-id"
refresh_token = "file:{}"
[[models]]
name = "model"
[[models.deployments]]
id = "deployment"
credential = "oauth"
base_url = "{}/v1"
upstream_model = "upstream"
"#,
        client.display(),
        request_log.display(),
        upstream.url(),
        refresh.display(),
        upstream.url()
    );
    llmux_server::app(RuntimeConfig::parse(&text).unwrap())
        .await
        .expect_err("build must fail on an unopenable request log");
    assert_eq!(hits.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert_eq!(fs::read_to_string(&refresh).unwrap(), "original");
}
