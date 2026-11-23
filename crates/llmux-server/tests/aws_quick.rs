use axum::{body::Bytes, routing::post, Json, Router};
use http::{header, HeaderMap, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use llmux_config::RuntimeConfig;
use llmux_testkit::TestServer;
use serde_json::{json, Value};

#[tokio::test]
async fn aws_quick_deployment_initializes_and_serves() {
    let upstream = TestServer::start(Router::new().route(
        "/v1/chat/completions",
        post(|headers: HeaderMap, Json(body): Json<Value>| async move {
            assert!(!headers.contains_key(header::AUTHORIZATION));
            assert_eq!(headers["x-magpie-effort"], "high");
            assert_eq!(body["agent_mode"], "fast");
            assert_eq!(body["model"], "fast");
            Json(json!({
                "id":"quick-1","object":"chat.completion","model":"fast",
                "choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],
            }))
        }),
    ))
    .await
    .unwrap();

    let secrets = tempfile::tempdir().unwrap();
    let client_key = secrets.path().join("client");
    std::fs::write(&client_key, "synthetic-client").unwrap();
    let config = format!(
        r#"
[server]
client_keys = ["file:{}"]

[[models]]
name = "quick"

[[models.deployments]]
id = "quick-fast"
auth = "none"
provider = "aws_quick"
upstream_model = "fast"
headers = {{ x-magpie-effort = "high" }}
"#,
        client_key.display()
    );
    let mut runtime = RuntimeConfig::parse(&config).unwrap();
    assert_eq!(runtime.deployments[0].base_url, "http://127.0.0.1:3425");
    llmux_server::validate_startup(RuntimeConfig::parse(&config).unwrap())
        .expect("an aws_quick deployment must initialize");

    runtime.deployments[0].base_url = upstream.url().parse().unwrap();
    let gateway = TestServer::start(llmux_server::app(runtime).await.unwrap())
        .await
        .unwrap();
    let client = Client::builder(TokioExecutor::new()).build_http();
    let response = client
        .request(
            Request::post(format!("{}/v1/chat/completions", gateway.url()))
                .header(header::AUTHORIZATION, "Bearer synthetic-client")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Full::new(Bytes::from(
                    json!({"model":"quick","messages":[{"role":"user","content":"hello"}]})
                        .to_string(),
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-llmux-deployment"], "quick-fast");
    let body: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "hi");
}
