use axum::{body::Bytes, extract::State, routing::post, Json, Router};
use http::{header, HeaderMap, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use llmux_config::RuntimeConfig;
use llmux_testkit::TestServer;
use llmux_types::NousState;
use serde_json::json;
use std::{
    fs,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

#[derive(Default)]
struct Fake {
    refreshes: AtomicUsize,
    presented: Mutex<Vec<String>>,
    inference: Mutex<Vec<String>>,
}

async fn token(State(fake): State<Arc<Fake>>, headers: HeaderMap) -> Json<serde_json::Value> {
    assert_eq!(
        headers["user-agent"],
        concat!("llmux/", env!("CARGO_PKG_VERSION"))
    );
    fake.presented
        .lock()
        .unwrap()
        .push(headers["x-nous-refresh-token"].to_str().unwrap().into());
    let n = fake.refreshes.fetch_add(1, Ordering::Relaxed) + 1;
    Json(json!({
        "access_token": format!("access-{n}"),
        "refresh_token": format!("refresh-{n}"),
        "token_type": "Bearer",
        "expires_in": 3600
    }))
}

async fn chat(
    State(fake): State<Arc<Fake>>,
    headers: HeaderMap,
) -> (StatusCode, Json<serde_json::Value>) {
    assert_eq!(
        headers[header::USER_AGENT],
        concat!("llmux/", env!("CARGO_PKG_VERSION"))
    );
    let bearer = headers[header::AUTHORIZATION].to_str().unwrap().to_owned();
    fake.inference.lock().unwrap().push(bearer.clone());
    if bearer == "Bearer access-1" {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":{"message":"token expired"}})),
        );
    }
    (StatusCode::OK, Json(json!({"id":"served"})))
}

fn config(dir: &std::path::Path, url: &str, state_file: &std::path::Path) -> String {
    fs::write(dir.join("client"), "gateway-key").unwrap();
    format!(
        r#"
[server]
client_keys = ["file:{}/client"]
[[credentials]]
id = "nous"
kind = "nous_portal"
provider = "openai_compat"
state_file = "{}"
[[models]]
name = "nous/vendor/free:free"
[[models.deployments]]
id = "nous-free"
credential = "nous"
base_url = "{url}/v1"
upstream_model = "vendor/free:free"
"#,
        dir.display(),
        state_file.display()
    )
}

#[tokio::test]
async fn unauthorized_triggers_one_refresh_and_one_retry_with_rotation_persisted() {
    let fake = Arc::new(Fake::default());
    let upstream = TestServer::start(
        Router::new()
            .route("/api/oauth/token", post(token))
            .route("/v1/chat/completions", post(chat))
            .with_state(fake.clone()),
    )
    .await
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let state_file = dir.path().join("state.json");
    NousState {
        portal_base_url: upstream.url(),
        client_id: "hermes-cli".into(),
        scope: "inference:invoke".into(),
        access_token: "stale".into(),
        refresh_token: "refresh-0".into(),
        expires_at: 0,
    }
    .save(&state_file)
    .unwrap();
    let gateway = TestServer::start(
        llmux_server::app(
            RuntimeConfig::parse(&config(dir.path(), &upstream.url(), &state_file)).unwrap(),
        )
        .await
        .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        fake.refreshes.load(Ordering::Relaxed),
        1,
        "start refreshes the expired token"
    );
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let response = client
        .request(
            Request::post(format!("{}/v1/chat/completions", gateway.url()))
                .header(header::AUTHORIZATION, "Bearer gateway-key")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Full::new(Bytes::from_static(
                    br#"{"model":"nous/vendor/free:free","messages":[{"role":"user","content":"hi"}]}"#,
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        r#"{"id":"served"}"#
    );
    assert_eq!(
        *fake.inference.lock().unwrap(),
        ["Bearer access-1", "Bearer access-2"]
    );
    assert_eq!(*fake.presented.lock().unwrap(), ["refresh-0", "refresh-1"]);
    let saved = NousState::load(&state_file).unwrap();
    assert_eq!(saved.refresh_token, "refresh-2");
}

#[tokio::test]
async fn startup_rejects_missing_or_unreadable_state_and_names_the_login_command() {
    let dir = tempfile::tempdir().unwrap();
    for state in [dir.path().join("absent.json"), dir.path().to_owned()] {
        let error = RuntimeConfig::parse(&config(dir.path(), "http://127.0.0.1:1", &state))
            .err()
            .expect("must be rejected")
            .to_string();
        assert!(error.contains("llmux-login nous"), "{error}");
    }
    let garbage = dir.path().join("garbage.json");
    fs::write(&garbage, "not json").unwrap();
    assert!(RuntimeConfig::parse(&config(dir.path(), "http://127.0.0.1:1", &garbage)).is_err());
}
