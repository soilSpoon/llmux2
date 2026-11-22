use axum::{body::Bytes, extract::State, routing::post, Json, Router};
use http::{header, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use llmux_config::RuntimeConfig;
use llmux_testkit::TestServer;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

// A session provider id attached as an alias must never replace the canonical
// upstream id on the shared fallback chain: a canonical request that meets a
// retryable failure falls back with the imported model name, and an alias
// request resolves to the same legs.
#[tokio::test]
async fn fallback_keeps_canonical_upstream_when_a_session_id_is_an_alias() {
    async fn record_and_limit(
        State(seen): State<Arc<Mutex<Vec<String>>>>,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        seen.lock()
            .unwrap()
            .push(body["model"].as_str().unwrap().to_owned());
        (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error":{"message":"synthetic rate limit"}})),
        )
    }
    async fn record_and_complete(
        State(seen): State<Arc<Mutex<Vec<String>>>>,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        seen.lock()
            .unwrap()
            .push(body["model"].as_str().unwrap().to_owned());
        (
            StatusCode::OK,
            Json(
                json!({"id":"ok","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"Hello!"},"finish_reason":"stop"}],"model":body["model"]}),
            ),
        )
    }
    let native_seen = Arc::new(Mutex::new(Vec::new()));
    let omniroute_seen = Arc::new(Mutex::new(Vec::new()));
    let native = TestServer::start(
        Router::new()
            .route("/v1/chat/completions", post(record_and_limit))
            .with_state(native_seen.clone()),
    )
    .await
    .unwrap();
    let omniroute = TestServer::start(
        Router::new()
            .route("/v1/chat/completions", post(record_and_complete))
            .with_state(omniroute_seen.clone()),
    )
    .await
    .unwrap();
    let secrets = tempfile::tempdir().unwrap();
    for (name, value) in [("client", "synthetic-client"), ("omni", "synthetic-omni")] {
        std::fs::write(secrets.path().join(name), value).unwrap();
    }
    let base = secrets.path().display();
    let config = format!(
        r#"
[server]
client_keys = ["file:{base}/client"]

[[credentials]]
id = "omniroute-fallback"
kind = "api_key"
provider = "openai_compat"
key = "file:{base}/omni"

[[models]]
name = "baseline"
aliases = ["ddgw/mistral-small-2603"]

[[models.deployments]]
id = "native"
auth = "none"
provider = "openai_compat"
base_url = "{}/v1"
upstream_model = "baseline"
priority = 0

[[models.deployments]]
id = "omniroute-fallback-baseline"
credential = "omniroute-fallback"
base_url = "{}/v1"
upstream_model = "baseline"
priority = 1
fallback = true
"#,
        native.url(),
        omniroute.url()
    );
    let gateway = TestServer::start(
        llmux_server::app(RuntimeConfig::parse(&config).unwrap())
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    let client = Client::builder(TokioExecutor::new()).build_http();
    for model in ["baseline", "ddgw/mistral-small-2603"] {
        let request = Request::post(format!("{}/v1/chat/completions", gateway.url()))
            .header(header::AUTHORIZATION, "Bearer synthetic-client")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(
                json!({"model":model,"messages":[{"role":"user","content":"hello"}]}).to_string(),
            )))
            .unwrap();
        let response = client.request(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{model}");
        response.into_body().collect().await.unwrap();
    }
    let native = native_seen.lock().unwrap();
    assert!(native.iter().all(|model| model == "baseline") && !native.is_empty());
    assert_eq!(*omniroute_seen.lock().unwrap(), ["baseline", "baseline"]);
}
