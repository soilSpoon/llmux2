use axum::{
    body::Bytes,
    extract::Path,
    routing::{delete, get, post},
    Json, Router,
};
use http::{header, HeaderMap, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use llmux_config::RuntimeConfig;
use llmux_testkit::TestServer;
use serde_json::json;
use std::{
    fs,
    sync::{Arc, Mutex},
};

#[derive(Clone, Default)]
struct Calls {
    entries: Arc<Mutex<Vec<String>>>,
}

impl Calls {
    fn push(&self, entry: impl Into<String>) {
        self.entries.lock().unwrap().push(entry.into());
    }

    fn entries(&self) -> Vec<String> {
        self.entries.lock().unwrap().clone()
    }
}

#[tokio::test]
async fn dahl_auth_failure_rotates_key_and_retries_same_deployment() {
    let calls = Calls::default();
    let account_calls = calls.clone();
    let inference_calls = calls.clone();
    let upstream = TestServer::start(
        Router::new()
            .route(
                "/v1/auth/signin",
                post({
                    let calls = account_calls.clone();
                    move |Json(body): Json<serde_json::Value>| {
                    let calls = calls.clone();
                    async move {
                        assert_eq!(body["fingerprint"], "synthetic-fingerprint-32chars!");
                        calls.push("signin".to_string());
                        (
                            StatusCode::OK,
                            [(header::SET_COOKIE, "dahl_session=synthetic; HttpOnly; Path=/")],
                            Json(json!({})),
                        )
                    }
                    }
                }),
            )
            .route(
                "/v1/account",
                get({
                    let calls = account_calls.clone();
                    move |headers: HeaderMap| {
                        let calls = calls.clone();
                        async move {
                            assert_eq!(headers[header::COOKIE], "dahl_session=synthetic");
                            calls.push("account".to_string());
                            Json(json!({"token_balance": 900_000}))
                        }
                    }
                }),
            )
            .route(
                "/v1/account/keys",
                post({
                    let calls = account_calls.clone();
                    move || {
                        let calls = calls.clone();
                        async move {
                            calls.push("create".to_string());
                            Json(json!({"key":{"id":42,"token":"new-dahl-key","status":"active","available_tokens":0,"total_tokens":0}}))
                        }
                    }
                })
                .get({
                    let calls = account_calls.clone();
                    move || {
                        let calls = calls.clone();
                        async move {
                            calls.push("keys".to_string());
                            Json(json!({"keys":[{"id":7,"token":"old-dahl-key","status":"active","available_tokens":0,"total_tokens":0}]}))
                        }
                    }
                }),
            )
            .route(
                "/v1/account/allocate",
                post({
                    let calls = account_calls.clone();
                    move |Json(body): Json<serde_json::Value>| {
                        let calls = calls.clone();
                        async move {
                            assert_eq!(body, json!({"key_id":42,"amount":500_000}));
                            calls.push("allocate".to_string());
                            Json(json!({}))
                        }
                    }
                }),
            )
            .route(
                "/v1/account/keys/{id}",
                delete({
                    let calls = account_calls.clone();
                    move |Path(id): Path<u64>| {
                        let calls = calls.clone();
                        async move {
                            assert_eq!(id, 7);
                            calls.push("revoke-old".to_string());
                            StatusCode::OK
                        }
                    }
                }),
            )
            .route(
                "/v1/chat/completions",
                post(move |headers: HeaderMap, Json(body): Json<serde_json::Value>| {
                    let calls = inference_calls.clone();
                    async move {
                        assert_eq!(body["model"], "upstream");
                        let auth = headers[header::AUTHORIZATION].to_str().unwrap().to_string();
                        calls.push(format!("inference:{auth}"));
                        if auth == "Bearer old-dahl-key" {
                            return (
                                StatusCode::UNAUTHORIZED,
                                Json(json!({"error":{"code":"invalid_api_key","message":"invalid API token","type":"authentication_error"}})),
                            );
                        }
                        assert_eq!(auth, "Bearer new-dahl-key");
                        (StatusCode::OK, Json(json!({"id":"rotated-response"})))
                    }
                }),
            ),
    )
    .await
    .unwrap();

    let secrets = tempfile::tempdir().unwrap();
    let fingerprint = secrets.path().join("fingerprint");
    let client = secrets.path().join("client");
    let state = secrets.path().join("dahl-state.json");
    fs::write(&fingerprint, "synthetic-fingerprint-32chars!").unwrap();
    fs::write(&client, "gateway-key").unwrap();
    fs::write(
        &state,
        r#"{"key_id":7,"token":"old-dahl-key","owned":true}"#,
    )
    .unwrap();
    let config = format!(
        r#"
[server]
client_keys = ["file:{}"]
[[credentials]]
id = "dahl"
kind = "dahl"
provider = "openai_compat"
fingerprint = "file:{}"
state_file = "{}"
base_url = "{}"
allocation_tokens = 500000
request_timeout_ms = 2000
[[models]]
name = "model"
[[models.deployments]]
id = "dahl-deployment"
credential = "dahl"
base_url = "{}/v1"
upstream_model = "upstream"
"#,
        client.display(),
        fingerprint.display(),
        state.display(),
        upstream.url(),
        upstream.url()
    );
    let gateway = TestServer::start(
        llmux_server::app(RuntimeConfig::parse(&config).unwrap())
            .await
            .unwrap(),
    )
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
    assert_eq!(response.headers()["x-llmux-deployment"], "dahl-deployment");
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        r#"{"id":"rotated-response"}"#
    );
    let entries = calls.entries();
    assert_eq!(
        entries,
        [
            "inference:Bearer old-dahl-key",
            "signin",
            "keys",
            "account",
            "create",
            "allocate",
            "revoke-old",
            "inference:Bearer new-dahl-key",
        ]
        .map(str::to_string)
    );
    let persisted = fs::read_to_string(&state).unwrap();
    assert!(persisted.contains("new-dahl-key"));
    assert!(!persisted.contains("old-dahl-key"));
}
