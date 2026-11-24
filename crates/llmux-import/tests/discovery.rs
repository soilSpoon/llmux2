use axum::{
    http::{HeaderMap, StatusCode},
    response::Json,
    routing::post,
    Router,
};
use llmux_import::discovery::{self, ProbeVerdict};
use llmux_testkit::TestServer;
use serde_json::{json, Value};

#[test]
fn experlabs_page_yields_free_promo_candidates() {
    let html = r#"{"model":"deepseek-v4.1-flash-uncensored-fast","promotion":"Free as deepseek-v4.1-flash-uncensored-fast:free"} another promo Free as gpt-7-omega:free&#x27;s end"#;
    let candidates = discovery::experlabs_promo_ids(html);
    assert_eq!(
        candidates.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        [
            "deepseek-v4.1-flash-uncensored-fast:free",
            "gpt-7-omega:free"
        ]
    );
    assert!(candidates.iter().all(|c| !c.evidence.is_empty()));
}

#[test]
fn experlabs_page_without_promos_yields_nothing() {
    assert!(
        discovery::experlabs_promo_ids("<html><body>No free rides here</body></html>").is_empty()
    );
}

#[tokio::test]
async fn probe_classifies_free_billed_and_gone() {
    let router = Router::new().route(
        "/v1/chat/completions",
        post(|headers: HeaderMap, body: Json<Value>| async move {
            assert_eq!(
                headers.get("authorization").unwrap(),
                "Bearer synthetic-key"
            );
            let model = body["model"].as_str().unwrap();
            match model {
                "free-model:free" => (
                    StatusCode::OK,
                    Json(json!({
                        "model": "free-model",
                        "choices": [{"message": {"content": "hi"}}],
                        "usage": {"prompt_tokens": 3, "completion_tokens": 1, "cost": 0.0}
                    })),
                ),
                "paid-model" => (
                    StatusCode::PAYMENT_REQUIRED,
                    Json(
                        json!({"error": {"message": "insufficient_credits", "code": "insufficient_credits"}}),
                    ),
                ),
                // A shared free tier's daily cap: 429 carrying the canonical
                // quota code. Must park, never classify as billed.
                "capped-model" => (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(
                        json!({"error": {"message": "insufficient_quota", "code": "insufficient_quota"}}),
                    ),
                ),
                _ => (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": {"message": "model_not_allowed"}})),
                ),
            }
        }),
    );
    let server = TestServer::start(router).await.unwrap();
    let client = discovery::client().unwrap();
    let base = format!("{}/v1", server.url());

    assert!(matches!(
        discovery::probe_model(&client, &base, "synthetic-key", "free-model:free").await,
        ProbeVerdict::Free
    ));
    assert!(matches!(
        discovery::probe_model(&client, &base, "synthetic-key", "paid-model").await,
        ProbeVerdict::Billed
    ));
    assert!(matches!(
        discovery::probe_model(&client, &base, "synthetic-key", "gone-model").await,
        ProbeVerdict::Unavailable(_)
    ));
    // Park-only: quota exhaustion is never a removal verdict.
    let verdict = discovery::probe_model(&client, &base, "synthetic-key", "capped-model").await;
    assert!(
        matches!(verdict, ProbeVerdict::Unknown(_)),
        "quota-exhausted probe must park, got {verdict:?}"
    );
}

#[tokio::test]
async fn probe_refuses_plaintext_http_to_non_loopback() {
    // The bearer key must never leave on a plaintext channel.
    let client = discovery::client().unwrap();
    let verdict = discovery::probe_model(
        &client,
        "http://203.0.113.10/v1",
        "synthetic-key",
        "any-model",
    )
    .await;
    assert!(
        matches!(verdict, ProbeVerdict::Unknown(ref why) if why.contains("https or loopback")),
        "plaintext http probe must be refused, got {verdict:?}"
    );
}

#[tokio::test]
async fn probe_times_out_instead_of_wedging() {
    let router = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            Json(json!({}))
        }),
    );
    let server = TestServer::start(router).await.unwrap();
    let client = discovery::client().unwrap();
    let base = format!("{}/v1", server.url());
    let verdict = discovery::probe_model_with_deadline(
        &client,
        &base,
        "synthetic-key",
        "any-model",
        std::time::Duration::from_millis(50),
    )
    .await;
    assert!(
        matches!(verdict, ProbeVerdict::Unknown(ref why) if why.contains("timed out")),
        "stalled probe must time out, got {verdict:?}"
    );
}

#[tokio::test]
async fn quota_in_error_type_field_still_parks() {
    // Providers put the machine code in `type`, not `code` — a numeric
    // code must not hide the quota classification.
    let router = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            (
                StatusCode::FORBIDDEN,
                Json(json!({"error": {"code": 403, "type": "insufficient_quota", "message": "payment required"}})),
            )
        }),
    );
    let server = TestServer::start(router).await.unwrap();
    let client = discovery::client().unwrap();
    let base = format!("{}/v1", server.url());
    let verdict = discovery::probe_model(&client, &base, "synthetic-key", "any-model").await;
    assert!(
        matches!(verdict, ProbeVerdict::Unknown(_)),
        "quota code in `type` must still park, got {verdict:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_free_cli_lists_candidates_and_verdicts() {
    // The observable --scan-free contract: promo ids become candidates,
    // reviewed ids are marked listed, and probes verify with usage.cost.
    let probed = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = probed.clone();
    let router = Router::new()
        .route(
            "/models",
            axum::routing::get(|| async {
                r#"{"promotion":"Free as gpt-test-alpha:free"} Free as deepseek-v4.1-flash"#
            }),
        )
        .route(
            "/v1/chat/completions",
            post(move |body: Json<Value>| {
                let seen = seen.clone();
                async move {
                    seen.lock()
                        .unwrap()
                        .push(body["model"].as_str().unwrap().to_string());
                    (StatusCode::OK, Json(json!({"usage": {"cost": 0.0}})))
                }
            }),
        );
    let server = TestServer::start(router).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("key");
    std::fs::write(&key, "synthetic-key").unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_llmux-import"))
        .args(["--scan-free", "experlabs"])
        .arg("--scan-page-url")
        .arg(format!("{}/models", server.url()))
        .arg("--scan-probe-base")
        .arg(format!("{}/v1", server.url()))
        .arg("--scan-key-file")
        .arg(&key)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("candidate gpt-test-alpha:free\tlisted=false\tverdict=free"));
    // A promo id already in the reviewed FREE map is marked listed.
    assert!(stdout.contains("candidate deepseek-v4.1-flash\tlisted=true\tverdict=free"));
    assert_eq!(
        probed.lock().unwrap().len(),
        2,
        "each candidate probed once"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_free_cli_without_key_probes_nothing() {
    let probed = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = probed.clone();
    let router = Router::new()
        .route(
            "/models",
            axum::routing::get(|| async { r#"{"promotion":"Free as gpt-test-alpha:free"}"# }),
        )
        .route(
            "/v1/chat/completions",
            post(move |body: Json<Value>| {
                let seen = seen.clone();
                async move {
                    seen.lock()
                        .unwrap()
                        .push(body["model"].as_str().unwrap().to_string());
                    (StatusCode::OK, Json(json!({})))
                }
            }),
        );
    let server = TestServer::start(router).await.unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_llmux-import"))
        .args(["--scan-free", "experlabs"])
        .arg("--scan-page-url")
        .arg(format!("{}/models", server.url()))
        .arg("--scan-probe-base")
        .arg(format!("{}/v1", server.url()))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("candidate gpt-test-alpha:free\tlisted=false\tverdict=unverified"));
    assert!(probed.lock().unwrap().is_empty(), "no key, no probe");
}
