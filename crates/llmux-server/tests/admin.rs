use axum::{body::Bytes, response::IntoResponse, routing::post, Json, Router};
use http::{header, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use llmux_config::RuntimeConfig;
use llmux_testkit::TestServer;
use serde_json::{json, Value};

fn config(upstream: &str) -> String {
    format!(
        r#"
[server]
client_keys = ["env:LLMUX_ADMIN_TEST_CLIENT"]
dashboard_key = "env:LLMUX_ADMIN_TEST_ADMIN"
[[credentials]]
id = "key"
kind = "api_key"
provider = "openai_compat"
key = "env:LLMUX_ADMIN_TEST_PROVIDER"
quota = {{ rpm = 10 }}
[[models]]
name = "shared"
[[models.deployments]]
id = "dep"
credential = "key"
base_url = "{upstream}/v1"
upstream_model = "test"
"#
    )
}

fn env() {
    std::env::set_var("LLMUX_ADMIN_TEST_CLIENT", "client-key");
    std::env::set_var("LLMUX_ADMIN_TEST_ADMIN", "admin-key");
    std::env::set_var("LLMUX_ADMIN_TEST_PROVIDER", "synthetic-key");
}

async fn upstream() -> TestServer {
    TestServer::start(
        Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                Json(json!({"id":"ok","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}]}))
                    .into_response()
            }),
        ),
    )
    .await
    .unwrap()
}

type HttpClient = hyper_util::client::legacy::Client<
    hyper_util::client::legacy::connect::HttpConnector,
    Full<Bytes>,
>;

async fn call(
    client: &HttpClient,
    method: &str,
    url: &str,
    key: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(url)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(key) = key {
        request = request.header(header::AUTHORIZATION, format!("Bearer {key}"));
    }
    let response = client
        .request(
            request
                .body(Full::new(Bytes::from(
                    serde_json::to_vec(&body.unwrap_or_else(|| json!({}))).unwrap(),
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(json!(null)))
}

async fn gateway() -> (TestServer, HttpClient) {
    env();
    let upstream = upstream().await;
    let router = llmux_server::app(RuntimeConfig::parse(&config(&upstream.url())).unwrap())
        .await
        .unwrap();
    let gateway = TestServer::start(router).await.unwrap();
    let client = Client::builder(TokioExecutor::new()).build_http();
    (gateway, client)
}

#[tokio::test]
async fn admin_requires_dashboard_key() {
    let (gateway, client) = gateway().await;
    let url = format!("{}/admin/status", gateway.url());
    let (status, _) = call(&client, "GET", &url, None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call(&client, "GET", &url, Some("client-key"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn admin_routes_absent_without_dashboard_key() {
    env();
    let upstream = upstream().await;
    let config =
        config(&upstream.url()).replace("dashboard_key = \"env:LLMUX_ADMIN_TEST_ADMIN\"\n", "");
    let gateway = TestServer::start(
        llmux_server::app(RuntimeConfig::parse(&config).unwrap())
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    let client = Client::builder(TokioExecutor::new()).build_http();
    let (status, _) = call(
        &client,
        "GET",
        &format!("{}/admin/status", gateway.url()),
        Some("client-key"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn status_deployments_models_and_credentials() {
    let (gateway, client) = gateway().await;
    let url = format!("{}/admin/status", gateway.url());
    let (status, body) = call(&client, "GET", &url, Some("admin-key"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["uptime_ms"].is_number());
    assert!(body["version"].is_string());
    assert_eq!(body["responses"]["ok"], 0);

    let url = format!("{}/admin/deployments", gateway.url());
    let (status, body) = call(&client, "GET", &url, Some("admin-key"), None).await;
    assert_eq!(status, StatusCode::OK);
    let dep = &body["deployments"][0];
    assert_eq!(dep["id"], "dep");
    assert_eq!(dep["model"], "shared");
    assert_eq!(dep["provider"], "openai_compat");
    assert_eq!(dep["disabled"], false);

    let url = format!("{}/admin/models", gateway.url());
    let (status, body) = call(&client, "GET", &url, Some("admin-key"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["models"][0]["name"], "shared");
    assert_eq!(body["models"][0]["deployments"], json!(["dep"]));

    let url = format!("{}/admin/credentials", gateway.url());
    let (status, body) = call(&client, "GET", &url, Some("admin-key"), None).await;
    assert_eq!(status, StatusCode::OK);
    let cred = &body["credentials"][0];
    assert_eq!(cred["id"], "key");
    assert_eq!(cred["kind"], "api_key");
    assert_eq!(cred["provider"], "openai_compat");
    assert_eq!(cred["disabled"], false);
    assert_eq!(cred["quota"]["rpm"], 10);
    assert!(cred.get("key").is_none(), "never return secret material");
}

#[tokio::test]
async fn disable_credential_stops_routing() {
    let (gateway, client) = gateway().await;
    let url = format!("{}/admin/credentials/key/disabled", gateway.url());
    let (status, _) = call(
        &client,
        "POST",
        &url,
        Some("admin-key"),
        Some(json!({"disabled": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let request = Request::post(format!("{}/v1/chat/completions", gateway.url()))
        .header(header::AUTHORIZATION, "Bearer client-key")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(
            serde_json::to_vec(
                &json!({"model":"shared","messages":[{"role":"user","content":"hi"}]}),
            )
            .unwrap(),
        )))
        .unwrap();
    let response = client.request(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);

    let (status, _) = call(
        &client,
        "POST",
        &url,
        Some("admin-key"),
        Some(json!({"disabled": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        &client,
        "POST",
        &format!("{}/admin/credentials/nope/disabled", gateway.url()),
        Some("admin-key"),
        Some(json!({"disabled": true})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let _ = body;
}

#[tokio::test]
async fn reload_swaps_config_and_rejects_bad_config() {
    env();
    let upstream = upstream().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("llmux.toml");
    std::fs::write(&path, config(&upstream.url())).unwrap();
    let router =
        llmux_server::app_with_path(RuntimeConfig::load(&path).unwrap(), Some(path.clone()))
            .await
            .unwrap();
    let gateway = TestServer::start(router).await.unwrap();
    let client = Client::builder(TokioExecutor::new()).build_http();

    std::fs::write(&path, config(&upstream.url()).replace("shared", "renamed")).unwrap();
    let url = format!("{}/admin/reload", gateway.url());
    let (status, body) = call(&client, "POST", &url, Some("admin-key"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["models"]
        .as_array()
        .unwrap()
        .contains(&json!("renamed")));

    let (status, body) = call(
        &client,
        "GET",
        &format!("{}/admin/models", gateway.url()),
        Some("admin-key"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["models"][0]["name"], "renamed");

    std::fs::write(&path, "not toml").unwrap();
    let (status, body) = call(&client, "POST", &url, Some("admin-key"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"]["message"].is_string());
    let (status, body) = call(
        &client,
        "GET",
        &format!("{}/admin/models", gateway.url()),
        Some("admin-key"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["models"][0]["name"], "renamed",
        "bad reload keeps old config"
    );
}

#[tokio::test]
async fn reload_without_config_path_is_rejected() {
    let (gateway, client) = gateway().await;
    let (status, body) = call(
        &client,
        "POST",
        &format!("{}/admin/reload", gateway.url()),
        Some("admin-key"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"]["message"].is_string());
}

#[tokio::test]
async fn reload_rotates_admin_key_and_warns_on_restart_only_fields() {
    env();
    let upstream = upstream().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("llmux.toml");
    let old_key = dir.path().join("old.key");
    let new_key = dir.path().join("new.key");
    std::fs::write(&old_key, "old-key").unwrap();
    std::fs::write(&new_key, "new-key").unwrap();
    std::fs::write(
        &path,
        config(&upstream.url()).replace(
            "env:LLMUX_ADMIN_TEST_ADMIN",
            &format!("file:{}", old_key.display()),
        ),
    )
    .unwrap();
    let router =
        llmux_server::app_with_path(RuntimeConfig::load(&path).unwrap(), Some(path.clone()))
            .await
            .unwrap();
    let gateway = TestServer::start(router).await.unwrap();
    let client = Client::builder(TokioExecutor::new()).build_http();
    let status_url = format!("{}/admin/status", gateway.url());

    std::fs::write(
        &path,
        config(&upstream.url())
            .replace(
                "env:LLMUX_ADMIN_TEST_ADMIN",
                &format!("file:{}", new_key.display()),
            )
            .replace("[server]", "[server]\nlisten = \"127.0.0.1:9\""),
    )
    .unwrap();
    let (status, body) = call(
        &client,
        "POST",
        &format!("{}/admin/reload", gateway.url()),
        Some("old-key"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| warning.as_str().unwrap().contains("listen")));

    let (status, _) = call(&client, "GET", &status_url, Some("old-key"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "rotated-out key denied");
    let (status, _) = call(&client, "GET", &status_url, Some("new-key"), None).await;
    assert_eq!(status, StatusCode::OK, "new key accepted");

    let stats_url = format!("{}/dashboard/stats", gateway.url());
    let (status, _) = call(&client, "GET", &stats_url, Some("old-key"), None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "dashboard revokes rotated-out key"
    );
    let (status, _) = call(&client, "GET", &stats_url, Some("new-key"), None).await;
    assert_eq!(status, StatusCode::OK, "dashboard accepts new key");
}

#[tokio::test]
async fn disable_credential_without_deployments() {
    env();
    let upstream = upstream().await;
    let mut content = config(&upstream.url());
    content.push_str(
        r#"
[[credentials]]
id = "spare"
kind = "api_key"
provider = "openai_compat"
key = "env:LLMUX_ADMIN_TEST_PROVIDER"
"#,
    );
    let gateway = TestServer::start(
        llmux_server::app(RuntimeConfig::parse(&content).unwrap())
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    let client = Client::builder(TokioExecutor::new()).build_http();

    let url = format!("{}/admin/credentials/spare/disabled", gateway.url());
    let (status, body) = call(
        &client,
        "POST",
        &url,
        Some("admin-key"),
        Some(json!({"disabled": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["disabled"], true);
}
