use axum::{body::Bytes, routing::post, Json, Router};
use http::{header, HeaderMap, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use llmux_config::RuntimeConfig;
use llmux_testkit::TestServer;
use serde_json::{json, Value};

#[tokio::test]
async fn deepai_text_and_code_use_their_respective_auth_and_response_formats() {
    let upstream = TestServer::start(
        Router::new()
            .route(
                "/api/text-generator",
                post(|headers: HeaderMap| async move {
                    assert_eq!(headers["api-key"], "synthetic-deepai");
                    assert!(!headers.contains_key(header::AUTHORIZATION));
                    Json(json!({"id":"job-1", "output":"Text response"}))
                }),
            )
            .route(
                "/deepai/v1/chat/completions",
                post(|headers: HeaderMap, Json(body): Json<Value>| async move {
                    assert_eq!(headers[header::AUTHORIZATION], "Bearer synthetic-deepai");
                    assert!(!headers.contains_key("api-key"));
                    assert_eq!(body["model"], "example-code");
                    Json(json!({"id":"code-1","object":"chat.completion","model":"example-code",
                        "choices":[{"index":0,"message":{"role":"assistant","content":"Code response"},"finish_reason":"stop"}]}))
                }),
            ),
    )
    .await
    .unwrap();
    let secrets = tempfile::tempdir().unwrap();
    let secret = secrets.path().join("key");
    std::fs::write(&secret, "synthetic-deepai").unwrap();
    let config = format!(
        r#"
[server]
client_keys = ["file:{secret}"]

[[credentials]]
id = "text"
kind = "api_key"
provider = "deepai"
key = "file:{secret}"

[[credentials]]
id = "code"
kind = "api_key"
provider = "openai_compat"
key = "file:{secret}"

[[models]]
name = "text"
[[models.deployments]]
id = "text"
credential = "text"
base_url = "{base}/api"
upstream_model = "text-generator"

[[models]]
name = "code"
[[models.deployments]]
id = "code"
credential = "code"
base_url = "{base}/deepai/v1"
upstream_model = "example-code"
"#,
        secret = secret.display(),
        base = upstream.url()
    );
    let gateway = TestServer::start(
        llmux_server::app(RuntimeConfig::parse(&config).unwrap())
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    let client = Client::builder(TokioExecutor::new()).build_http();
    for (name, model, content) in [
        ("text", "text-generator", "Text response"),
        ("code", "example-code", "Code response"),
    ] {
        let request = Request::post(format!("{}/v1/chat/completions", gateway.url()))
            .header(header::AUTHORIZATION, "Bearer synthetic-deepai")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(
                json!({"model":name,"messages":[{"role":"user","content":"hello"}]}).to_string(),
            )))
            .unwrap();
        let response = client.request(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["model"], model);
        assert_eq!(body["choices"][0]["message"]["content"], content);
    }
}
