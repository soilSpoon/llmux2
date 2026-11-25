use axum::{body::Bytes, routing::post, Json, Router};
use http::{header, HeaderMap, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use llmux_config::RuntimeConfig;
use llmux_testkit::TestServer;
use serde_json::{json, Value};

/// The DeepAI Code lane is `api-key` authenticated and requires the lane
/// version header the upstream uses to tell current clients apart; a Bearer
/// credential is refused with 401, so the gateway must never send one.
#[tokio::test]
async fn deepai_code_lane_sends_api_key_and_lane_version_and_forwards_openai_chat() {
    let upstream = TestServer::start(Router::new().route(
        "/deepai_code/chat/completions",
        post(|headers: HeaderMap, Json(body): Json<Value>| async move {
            assert_eq!(headers["api-key"], "synthetic-deepai-code");
            assert_eq!(headers["x-deepai-code-theos"], "7");
            assert!(!headers.contains_key(header::AUTHORIZATION));
            assert_eq!(body["model"], "claude-haiku-5-5");
            Json(json!({
                "id": "gen-1",
                "object": "chat.completion",
                "model": "anthropic/claude-haiku-5.5",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "Lane response"},
                    "finish_reason": "stop"
                }]
            }))
        }),
    ))
    .await
    .unwrap();
    let secrets = tempfile::tempdir().unwrap();
    let secret = secrets.path().join("key");
    std::fs::write(&secret, "synthetic-deepai-code").unwrap();
    let config = format!(
        r#"
[server]
client_keys = ["file:{secret}"]

[[credentials]]
id = "deepai-code"
kind = "api_key"
provider = "deepai_code"
key = "file:{secret}"

[[models]]
name = "claude-haiku-5-5"
[[models.deployments]]
id = "claude-haiku-5-5-deepai-code"
credential = "deepai-code"
base_url = "{base}/deepai_code"
upstream_model = "claude-haiku-5-5"
headers = {{ x-deepai-code-theos = "7" }}
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
    let request = Request::post(format!("{}/v1/chat/completions", gateway.url()))
        .header(header::AUTHORIZATION, "Bearer synthetic-deepai-code")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(
            json!({"model":"claude-haiku-5-5","messages":[{"role":"user","content":"hello"}]})
                .to_string(),
        )))
        .unwrap();
    let response = client.request(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["model"], "anthropic/claude-haiku-5.5");
    assert_eq!(body["choices"][0]["message"]["content"], "Lane response");
}
