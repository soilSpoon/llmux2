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
