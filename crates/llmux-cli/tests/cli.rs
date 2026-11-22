use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use llmux_testkit::TestServer;
use serde_json::{json, Value};
use std::process::{Command, Output};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

async fn admin() -> (TestServer, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let server = TestServer::start(
        Router::new()
            .route(
                "/admin/status",
                get(|headers: HeaderMap| async move {
                    if headers["authorization"] != "Bearer admin-key" {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({"error":{"message":"denied"}})),
                        )
                            .into_response();
                    }
                    Json(json!({"uptime_ms": 1, "version": "test"})).into_response()
                }),
            )
            .route("/admin/models", get(|| async { "not json" }))
            .route(
                "/admin/reload",
                post(|| async { Json(json!({"reloaded": true})) }),
            )
            .route(
                "/admin/credentials/{id}/disabled",
                post(
                    |State(calls): State<Arc<AtomicUsize>>,
                     Path(id): Path<String>,
                     Json(body): Json<Value>| async move {
                        calls.fetch_add(1, Ordering::Relaxed);
                        Json(json!({"id": id, "disabled": body["disabled"]}))
                    },
                ),
            )
            .with_state(counted),
    )
    .await
    .unwrap();
    (server, calls)
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_llmux"))
        .args(args)
        .output()
        .unwrap()
}

async fn run_cli(args: Vec<String>) -> Output {
    tokio::task::spawn_blocking(move || run(&args.iter().map(String::as_str).collect::<Vec<_>>()))
        .await
        .unwrap()
}

#[tokio::test]
async fn status_prints_json() {
    let (server, _) = admin().await;
    let out = run_cli(vec![
        "--url".into(),
        server.url(),
        "--key".into(),
        "admin-key".into(),
        "status".into(),
    ])
    .await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(body["version"], "test");
}

#[tokio::test]
async fn enable_and_disable_post_flag() {
    let (server, calls) = admin().await;
    for (command, expected) in [("enable", false), ("disable", true)] {
        let out = run_cli(vec![
            "--url".into(),
            server.url(),
            "--key".into(),
            "admin-key".into(),
            command.into(),
            "key".into(),
        ])
        .await;
        assert!(
            out.status.success(),
            "{command}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let body: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(body["disabled"], expected);
    }
    assert_eq!(calls.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn env_vars_target_gateway() {
    let (server, _) = admin().await;
    let url = server.url();
    let out = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_llmux"))
            .arg("reload")
            .env("LLMUX_ADMIN_URL", url)
            .env("LLMUX_ADMIN_KEY", "admin-key")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(out.status.success());
    let body: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(body["reloaded"], true);
}

#[tokio::test]
async fn bad_usage_and_api_error_fail() {
    let (server, _) = admin().await;
    assert!(!run(&["frobnicate"]).status.success());
    assert!(
        !run(&["--url", &server.url(), "--key", "admin-key", "disable"])
            .status
            .success()
    );
    let out = run_cli(vec![
        "--url".into(),
        server.url(),
        "--key".into(),
        "wrong".into(),
        "status".into(),
    ])
    .await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("denied"));
}

#[tokio::test]
async fn escaped_credential_id_reaches_the_right_route() {
    let (server, _) = admin().await;
    let out = run_cli(vec![
        "--url".into(),
        server.url(),
        "--key".into(),
        "admin-key".into(),
        "disable".into(),
        "team/key%3Aone".into(),
    ])
    .await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(body["id"], "team/key%3Aone");
}

#[tokio::test]
async fn secrets_never_echo_and_plaintext_remote_is_refused() {
    let out = run(&["--key=supersecret", "status"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--key"), "{stderr}");
    assert!(
        !stderr.contains("supersecret"),
        "key value leaked: {stderr}"
    );

    let out = run(&[
        "--url",
        "http://example.com",
        "--key",
        "admin-key",
        "status",
    ]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("plaintext"));

    let (server, _) = admin().await;
    let out = run_cli(vec![
        "--url".into(),
        server.url(),
        "--key".into(),
        "admin-key".into(),
        "models".into(),
    ])
    .await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("invalid JSON"));

    // Every 127.0.0.0/8 address is loopback, not just 127.0.0.1: plaintext
    // http to another local gateway address must not be refused.
    let out = run(&[
        "--url",
        &server.url().replace("127.0.0.1", "127.0.0.2"),
        "--key",
        "admin-key",
        "status",
    ]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("plaintext"), "wrong refusal: {stderr}");
}
