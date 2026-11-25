use axum::{http::StatusCode, response::Json, routing::post, Router};
use llmux_testkit::TestServer;
use serde_json::{json, Value};
use std::{fs, path::Path, process::Command};

fn write_config(dir: &Path, deployments: &str) -> std::path::PathBuf {
    let client_key = dir.join("client.key");
    let upstream_key = dir.join("upstream.key");
    fs::write(&client_key, "synthetic-client-key").unwrap();
    fs::write(&upstream_key, "synthetic-upstream-key").unwrap();
    let config = dir.join("llmux.toml");
    fs::write(
        &config,
        format!(
            r#"[server]
client_keys = ["file:{}"]

[[credentials]]
id = "upstream"
kind = "api_key"
provider = "openai_compat"
key = "file:{}"

[[models]]
name = "canonical"

{}
"#,
            client_key.display(),
            upstream_key.display(),
            deployments
        ),
    )
    .unwrap();
    config
}

fn verify(config: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_llmux-import"))
        .arg("--verify-free")
        .arg(config)
        .output()
        .unwrap()
}

fn row<'a>(report: &'a Value, leg: &str) -> &'a Value {
    report["legs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["leg_id"] == leg)
        .unwrap_or_else(|| panic!("missing row for {leg}: {report}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verify_free_classifies_cost_quota_and_extension() {
    let probed = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = probed.clone();
    let router = Router::new().route(
        "/v1/chat/completions",
        post(move |body: Json<Value>| {
            let seen = seen.clone();
            async move {
                let model = body["model"].as_str().unwrap().to_string();
                seen.lock().unwrap().push(model.clone());
                match model.as_str() {
                    "cost-positive" => (
                        StatusCode::OK,
                        Json(json!({"usage": {"cost": 0.01}})),
                    ),
                    "capped" => (
                        StatusCode::TOO_MANY_REQUESTS,
                        Json(json!({"error": {"type": "insufficient_quota", "message": "quota exhausted"}})),
                    ),
                    "extended" | "still-free" => (
                        StatusCode::OK,
                        Json(json!({"usage": {"cost": 0.0}})),
                    ),
                    _ => (
                        StatusCode::NOT_FOUND,
                        Json(json!({"error": {"message": "model_not_allowed"}})),
                    ),
                }
            }
        }),
    );
    let server = TestServer::start(router).await.unwrap();
    let base = format!("{}/v1", server.url());
    let deployments = format!(
        r#"[[models.deployments]]
id = "cost-positive"
credential = "upstream"
base_url = "{base}"
upstream_model = "cost-positive"
billing = "free"

[[models.deployments]]
id = "capped"
credential = "upstream"
base_url = "{base}"
upstream_model = "capped"
billing = "free"

[[models.deployments]]
id = "extended"
credential = "upstream"
base_url = "{base}"
upstream_model = "extended"
billing = "free"
expires_at = 2020-01-01T00:00:00Z

[[models.deployments]]
id = "still-free"
credential = "upstream"
base_url = "{base}"
upstream_model = "still-free"
billing = "free"
"#
    );
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), &deployments);

    let out = verify(&config);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        !stdout.contains("synthetic-upstream-key"),
        "report leaked the upstream credential: {stdout}"
    );
    let report: Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(
        row(&report, "cost-positive")["classification"],
        "promo_ended"
    );
    assert_eq!(row(&report, "cost-positive")["removal_candidate"], true);
    assert_eq!(row(&report, "cost-positive")["cost"].as_f64(), Some(0.01));
    assert_eq!(row(&report, "capped")["classification"], "quota_drained");
    assert_eq!(row(&report, "capped")["removal_candidate"], false);
    assert_eq!(row(&report, "extended")["classification"], "promo_extended");
    assert_eq!(row(&report, "extended")["removal_candidate"], false);
    assert_eq!(row(&report, "still-free")["classification"], "still_free");
    for leg in ["cost-positive", "capped", "extended", "still-free"] {
        assert_eq!(row(&report, leg)["probed"], true, "{leg} must be probed");
        assert!(
            row(&report, leg)["probed_at"].is_string(),
            "{leg} must carry probe time"
        );
    }
    let mut seen = probed.lock().unwrap().clone();
    seen.sort();
    assert_eq!(
        seen,
        ["capped", "cost-positive", "extended", "still-free"].map(str::to_string)
    );
}

#[test]
fn verify_free_never_probes_a_provider_with_unknown_billing_policy() {
    let deployments = r#"[[models.deployments]]
id = "freemodel-claude"
credential = "upstream"
base_url = "https://freemodel.in/v1"
upstream_model = "claude-haiku-5.5"
billing = "free"
"#;
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), deployments);

    let out = verify(&config);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(!stdout.contains("synthetic-upstream-key"));
    let report: Value = serde_json::from_str(&stdout).unwrap();
    let row = row(&report, "freemodel-claude");
    assert_eq!(row["classification"], "unknown");
    assert_eq!(row["probed"], false);
    assert_eq!(row["removal_candidate"], false);
    assert!(
        row["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("probe policy unknown")),
        "unknown-policy row must say why no probe ran: {row}"
    );
}
