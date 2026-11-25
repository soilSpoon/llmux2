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

#[tokio::test]
async fn export_upstream_quota_joins_config_log_and_runtime_state() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("llmux.toml");
    let log = dir.path().join("requests.jsonl");
    let runtime = dir.path().join("deployments.json");
    std::fs::write(
        &config,
        r#"[server]
client_keys = ["env:LLMUX_TEST_CLIENT_KEY"]
request_log = "/not-used-for-this-test/requests.jsonl"

[[credentials]]
id = "cred-free"
kind = "api_key"
provider = "openai_compat"
key = "env:LLMUX_TEST_UPSTREAM_KEY"
quota = { rpd = 100 }

[[credentials]]
id = "cred-disabled"
kind = "api_key"
provider = "openai_compat"
key = "env:LLMUX_TEST_DISABLED_KEY"
disabled = true

[[shared_quotas]]
name = "free-daily"
fixed = [ { unit = "requests", max = 1000, span_secs = 86400, anchor = "2026-11-25T00:00:00Z" } ]

[[models]]
name = "test-model"
aliases = ["test-alias"]

[[models.deployments]]
id = "free-leg"
credential = "cred-free"
base_url = "https://example.com/v1"
upstream_model = "upstream-test"
priority = 0
quota = { rpm = 10 }
shared_quota = "free-daily"
billing = "free"

[[models]]
name = "parked-model"

[[models.deployments]]
id = "parked-leg"
credential = "cred-disabled"
base_url = "https://example.com/v1"
upstream_model = "parked-upstream"
priority = 0
"#,
    )
    .unwrap();
    std::fs::write(
        &log,
        concat!(
            "{\"ts_ms\":1795694360000,\"request_id\":\"r1\",\"model\":\"test-model\",\"deployment\":\"free-leg\",\"provider\":\"openai_compat\",\"attempts\":1,\"legs\":[{\"deployment\":\"free-leg\",\"provider\":\"openai_compat\",\"outcome\":\"routed\"}],\"latency_ms\":100,\"outcome\":\"routed\"}\n",
            "{\"ts_ms\":1795694370000,\"request_id\":\"r2\",\"model\":\"test-alias\",\"deployment\":\"free-leg\",\"provider\":\"openai_compat\",\"attempts\":1,\"legs\":[{\"deployment\":\"free-leg\",\"provider\":\"openai_compat\",\"outcome\":\"rate_limited\"}],\"latency_ms\":20,\"outcome\":\"error\",\"error_kind\":\"rate_limited\"}\n",
            "{\"ts_ms\":1795694380000,\"request_id\":\"r3\",\"model\":\"test-model\",\"attempts\":0,\"legs\":[],\"latency_ms\":0,\"outcome\":\"error\",\"error_kind\":\"all_candidates_exhausted\"}\n",
            "{\"ts_ms\":1795611600000,\"request_id\":\"old\",\"model\":\"test-model\",\"deployment\":\"free-leg\",\"provider\":\"openai_compat\",\"attempts\":1,\"legs\":[{\"deployment\":\"free-leg\",\"provider\":\"openai_compat\",\"outcome\":\"routed\"}],\"latency_ms\":90,\"outcome\":\"routed\"}\n",
            "{\"ts_ms\":1795603200000,\"request_id\":\"ancient-coverage\",\"model\":\"unrelated\",\"attempts\":0,\"legs\":[],\"latency_ms\":0,\"outcome\":\"error\",\"error_kind\":\"all_candidates_exhausted\"}\n"
        ),
    )
    .unwrap();
    std::fs::write(
        &runtime,
        r#"{"deployments":[{"model":"test-model","id":"free-leg","provider":"openai_compat","priority":0,"disabled":false,"not_provisioned":false,"expired":false,"cooldown_ms":null,"cooldown_scope":null,"rpm_limit":10,"concurrency_limit":null,"in_flight":null,"routed":0,"failed_attempts":0,"stream_errors":0,"handoffs":0,"failure_classes":{},"skip_reasons":{}}]}"#,
    )
    .unwrap();

    let out = run(&[
        "export-upstream-quota",
        "--config",
        config.to_str().unwrap(),
        "--request-log",
        log.to_str().unwrap(),
        "--runtime-deployments",
        runtime.to_str().unwrap(),
        "--now",
        "2026-11-26T12:00:00Z",
    ]);
    assert!(
        out.status.success(),
        "export failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("LLMUX_TEST_UPSTREAM_KEY"),
        "secret reference leaked: {stdout}"
    );
    let body: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(body["source"], "llmux2");
    let rows = body["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let row = rows
        .iter()
        .find(|row| row["model"] == "test-model")
        .unwrap();
    assert_eq!(row["providerModelId"], "upstream-test");
    assert_eq!(row["legs"][0]["id"], "free-leg");
    assert_eq!(row["legs"][0]["provider"], "openai_compat");
    assert_eq!(row["legs"][0]["priority"], 0);
    assert_eq!(row["freeOrMembership"], "free");
    assert_eq!(row["remainingUnits"], 8);
    assert_eq!(row["resetAt"], "2026-11-27T00:00:00Z");
    assert_eq!(row["marginalCostPer1kReq"], Value::Null);
    assert!(row["availability"]["marginalCostPer1kReq"]
        .as_str()
        .unwrap()
        .contains("opportunity cost"));
    assert_eq!(row["parked"], false);
    assert_eq!(row["recent429"][0]["count"], 1);
    assert_eq!(row["recent429"][0]["kind"], "rate_limited");
    assert_eq!(row["attemptedUnits1h"], 3);
    assert_eq!(row["servedUnits1h"], 1);
    assert_eq!(row["zeroCandidateUnits1h"], 1);
    assert_eq!(row["attemptedUnits24h"], 4);
    assert_eq!(row["servedUnits24h"], 2);
    assert_eq!(row["capacityUnitsPerWindow"], 1000);
    assert_eq!(row["capacitySaturation"], 0.002);
    assert_eq!(row["lastErrorKind"], "all_candidates_exhausted");
    assert_eq!(row["medianFailureLatencyMs"], 10);
    let windows = row["quotaWindows"].as_array().unwrap();
    assert!(windows.iter().any(|window| {
        window["scope"] == "shared_quota"
            && window["unit"] == "requests"
            && window["max"] == 1000
            && window["remainingUnits"] == 998
            && window["resetAt"] == "2026-11-27T00:00:00Z"
    }));
    assert!(windows.iter().any(|window| {
        window["scope"] == "deployment" && window["max"] == 10 && window["remainingUnits"] == 8
    }));

    let parked = rows
        .iter()
        .find(|row| row["model"] == "parked-model")
        .unwrap();
    assert_eq!(parked["parked"], true);
    assert_eq!(parked["freeOrMembership"], "billed");
    assert_eq!(parked["legs"][0]["parked"], true);
    assert_eq!(
        parked["legs"][0]["parkReasons"],
        serde_json::json!(["credential_disabled_in_config"])
    );

    let offline = run(&[
        "export-upstream-quota",
        "--config",
        config.to_str().unwrap(),
        "--no-request-log",
        "--now",
        "2026-11-26T12:00:00Z",
    ]);
    assert!(
        offline.status.success(),
        "offline export failed: {}",
        String::from_utf8_lossy(&offline.stderr)
    );
    let offline_body: Value = serde_json::from_slice(&offline.stdout).unwrap();
    let offline_row = offline_body["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["model"] == "test-model")
        .unwrap();
    assert_eq!(offline_row["recent429"], Value::Null);
    assert_eq!(offline_row["attemptedUnits24h"], Value::Null);
    assert_eq!(offline_row["remainingUnits"], Value::Null);
}
