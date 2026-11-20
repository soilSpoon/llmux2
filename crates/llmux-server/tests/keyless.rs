use axum::{
    body::{Body, Bytes},
    extract::State,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use http::{header, HeaderMap, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use llmux_config::RuntimeConfig;
use llmux_testkit::TestServer;
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

#[derive(Default)]
struct Upstream {
    anonymous: AtomicUsize,
    keyed: AtomicUsize,
    limit_anonymous: bool,
}

async fn chat(State(state): State<Arc<Upstream>>, headers: HeaderMap) -> Response {
    if let Some(auth) = headers.get(header::AUTHORIZATION) {
        assert_eq!(auth, "Bearer synthetic-key");
        state.keyed.fetch_add(1, Ordering::SeqCst);
    } else {
        state.anonymous.fetch_add(1, Ordering::SeqCst);
        if state.limit_anonymous {
            return (StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, "60")]).into_response();
        }
    }
    Json(json!({"id":"ok","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}]})).into_response()
}

async fn exercise(mixed: bool) {
    let state = Arc::new(Upstream {
        limit_anonymous: mixed,
        ..Default::default()
    });
    let upstream = TestServer::start(
        Router::new()
            .route("/v1/chat/completions", post(chat))
            .with_state(state.clone()),
    )
    .await
    .unwrap();
    let secrets = tempfile::tempdir().unwrap();
    let secret = secrets.path().join("key");
    std::fs::write(&secret, "synthetic-key").unwrap();
    let keyed = if mixed {
        format!(
            r#"
[[credentials]]
id = "key"
kind = "api_key"
provider = "openai_compat"
key = "file:{}"
"#,
            secret.display()
        )
    } else {
        String::new()
    };
    let keyed_deployment = if mixed {
        format!(
            r#"
[[models.deployments]]
id = "keyed"
credential = "key"
base_url = "{}/v1"
upstream_model = "test"
priority = 10
quota = {{ rpm = 2 }}
"#,
            upstream.url()
        )
    } else {
        String::new()
    };
    let config = format!(
        r#"
[server]
client_keys = ["file:{}"]
{keyed}
[[models]]
name = "shared"
[[models.deployments]]
id = "anonymous"
auth = "none"
provider = "openai_compat"
base_url = "{}/v1"
upstream_model = "test"
{keyed_deployment}
"#,
        secret.display(),
        upstream.url()
    );
    let gateway = TestServer::start(
        llmux_server::app(RuntimeConfig::parse(&config).unwrap())
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    let client = Client::builder(TokioExecutor::new()).build_http();
    for index in 0..if mixed { 3 } else { 1 } {
        let request = Request::post(format!("{}/v1/chat/completions", gateway.url()))
            .header(header::AUTHORIZATION, "Bearer synthetic-key")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(
                serde_json::to_vec(
                    &json!({"model":"shared","messages":[{"role":"user","content":"hello"}]}),
                )
                .unwrap(),
            )))
            .unwrap();
        let response: http::Response<Body> = client.request(request).await.unwrap().map(Body::new);
        if mixed && index == 2 {
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        } else {
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers()["x-llmux-deployment"],
                if mixed { "keyed" } else { "anonymous" }
            );
            assert_eq!(
                response.headers()["x-llmux-attempts"],
                if mixed && index == 0 { "2" } else { "1" }
            );
        }
        response.into_body().collect().await.unwrap();
    }
    assert_eq!(state.anonymous.load(Ordering::SeqCst), 1);
    assert_eq!(
        state.keyed.load(Ordering::SeqCst),
        if mixed { 2 } else { 0 }
    );
}

#[tokio::test]
async fn anonymous_deployment_sends_no_authorization_header() {
    exercise(false).await;
}

#[tokio::test]
async fn mixed_auth_fails_over_and_keeps_cooldown_and_rpm_independent() {
    exercise(true).await;
}
