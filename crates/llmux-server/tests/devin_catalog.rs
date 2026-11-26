//! Live `devin models` catalog: configured names stay the allowlist, a successful
//! load hides a Devin id the CLI no longer has, and a chat request is not sent
//! to that stale upstream.

use axum::body::{Body, Bytes};
use futures_util::StreamExt;
use http::{header, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use llmux_config::RuntimeConfig;
use llmux_providers::DevinAcpConfig;
use llmux_testkit::TestServer;
use serde_json::{json, Value};
use std::{
    convert::Infallible,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::time::{timeout, Instant};

enum ModelsMode {
    /// Prints a catalog that still has `swe-2-high` and not `swe-2-retired`.
    Live,
    /// Exits without a catalog. Must not hide configured names.
    Fail,
    /// Blocks until `release` exists, so a chat request can prove it does not wait.
    Block,
}

struct Gateway {
    server: TestServer,
    calls: PathBuf,
    acp_log: PathBuf,
    release: PathBuf,
    _home: tempfile::TempDir,
}

impl Gateway {
    async fn start(mode: ModelsMode) -> Self {
        Self::start_with_limit(mode, None).await
    }

    async fn start_with_limit(mode: ModelsMode, max_request_bytes: Option<u64>) -> Self {
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join("devin");
        let calls = home.path().join("calls");
        let acp_log = home.path().join("acp.log");
        let release = home.path().join("release");
        let key = home.path().join("client");
        let ci_key = home.path().join("ci");
        let dashboard_key = home.path().join("dashboard");
        fs::write(&key, "test-client\n").unwrap();
        fs::write(&ci_key, "test-ci\n").unwrap();
        fs::write(&dashboard_key, "test-dashboard\n").unwrap();
        fs::create_dir_all(home.path().join(".local/share")).unwrap();
        fs::write(home.path().join(".local/share/marker"), "").unwrap();
        write_cli(&bin, &calls, &acp_log, &release, mode);
        let limit = max_request_bytes
            .map(|value| format!("max_request_bytes = {value}\n"))
            .unwrap_or_default();

        let config = format!(
            r#"
[server]
client_keys = ["file:{}", "file:{}"]
dashboard_key = "file:{}"
request_timeout_ms = 4000
{limit}

[[server.key_policies]]
key = "file:{}"
free_only = true

[[models]]
name = "retired"
aliases = ["old-swe"]

[[models.deployments]]
id = "retired-devin"
auth = "none"
provider = "devin_acp"
billing = "free"
upstream_model = "swe-2-retired"
priority = 0

[[models]]
name = "swe"
aliases = ["swe-alias"]

[[models.deployments]]
id = "swe-stale"
auth = "none"
provider = "devin_acp"
billing = "free"
upstream_model = "swe-2-retired"
priority = 0

[[models.deployments]]
id = "swe-live"
auth = "none"
provider = "devin_acp"
billing = "free"
upstream_model = "swe-2-high"
priority = 10

[[models]]
name = "combo"

[[models.deployments]]
id = "combo-stale"
auth = "none"
provider = "devin_acp"
billing = "free"
upstream_model = "swe-2-retired"
priority = 0

[[models.deployments]]
id = "combo-openai"
auth = "none"
provider = "openai_compat"
base_url = "http://127.0.0.1:9/v1"
upstream_model = "plain-upstream"
priority = 10

[[models]]
name = "plain"

[[models.deployments]]
id = "plain-openai"
auth = "none"
provider = "openai_compat"
base_url = "http://127.0.0.1:9/v1"
upstream_model = "plain-upstream"
priority = 0

[[models]]
name = "gone"

[[models.deployments]]
id = "gone-devin"
auth = "none"
provider = "devin_acp"
billing = "free"
upstream_model = "swe-2-retired"
priority = 0
"#,
            key.display(),
            ci_key.display(),
            dashboard_key.display(),
            ci_key.display()
        );

        let router = llmux_server::app_with_devin(
            RuntimeConfig::parse(&config).unwrap(),
            DevinAcpConfig {
                binary: bin,
                home: home.path().to_owned(),
                turn_timeout: Duration::from_secs(10),
            },
        )
        .await
        .unwrap();
        let server = TestServer::start(router).await.unwrap();
        Self {
            server,
            calls,
            acp_log,
            release,
            _home: home,
        }
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(&self.calls)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn acp_log(&self) -> String {
        fs::read_to_string(&self.acp_log).unwrap_or_default()
    }

    async fn models(&self) -> Vec<String> {
        self.models_as("test-client").await
    }

    async fn models_as(&self, key: &str) -> Vec<String> {
        let response = request(
            &self.server,
            key,
            "GET",
            "/v1/models",
            None,
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let list: Value = serde_json::from_str(&body(response).await).unwrap();
        list["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["id"].as_str().unwrap().to_owned())
            .collect()
    }

    async fn chat(&self, model: &str, limit: Duration) -> http::Response<Body> {
        let payload = json!({
            "model": model,
            "messages": [{"role": "user", "content": "Hello"}]
        });
        request(
            &self.server,
            "test-client",
            "POST",
            "/v1/chat/completions",
            Some(payload.to_string()),
            limit,
        )
        .await
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        let _ = fs::write(&self.release, "go");
    }
}

fn write_cli(bin: &Path, calls: &Path, acp_log: &Path, release: &Path, mode: ModelsMode) {
    let models = match mode {
        ModelsMode::Fail => "exit 1".to_owned(),
        ModelsMode::Live => concat!(
            "printf '  swe-2-high   SWE-2 High  [262K context, Free]\\n'\n",
            "printf '  adaptive   Adaptive  [1K context]\\n'\n",
            "exit 0"
        )
        .to_owned(),
        ModelsMode::Block => format!(
            "while [ ! -f '{}' ]; do sleep 0.05; done\n{}",
            release.display(),
            concat!(
                "printf '  swe-2-high   SWE-2 High  [262K context, Free]\\n'\n",
                "exit 0"
            )
        ),
    };
    let script = format!(
        r#"#!/bin/sh
printf '%s\n' "$1" >> '{calls}'
if [ "$1" = models ]; then
{models}
fi
test -f "$XDG_DATA_HOME/marker" || exit 3
while read -r line; do
  printf '%s\n' "$line" >> '{acp}'
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*) printf '{{"jsonrpc":"2.0","id":%s,"result":{{}}}}\n' "$id";;
    *'"session/new"'*) printf '{{"jsonrpc":"2.0","id":%s,"result":{{"sessionId":"s1"}}}}\n' "$id";;
    *'"session/prompt"'*)
      printf '{{"jsonrpc":"2.0","method":"session/update","params":{{"update":{{"sessionUpdate":"agent_message_chunk","content":{{"text":"ok"}}}}}}}}\n'
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"stopReason":"end_turn"}}}}\n' "$id";;
  esac
done
"#,
        calls = calls.display(),
        acp = acp_log.display(),
        models = models,
    );
    fs::write(bin, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(bin, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn models_invocations(calls: &[String]) -> usize {
    calls
        .iter()
        .filter(|call| call.as_str() == "models")
        .count()
}

async fn request(
    server: &TestServer,
    key: &str,
    method: &str,
    path: &str,
    payload: Option<String>,
    limit: Duration,
) -> http::Response<Body> {
    let request = Request::builder()
        .method(method)
        .uri(format!("{}{path}", server.url()))
        .header(header::AUTHORIZATION, format!("Bearer {key}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(payload.unwrap_or_default())))
        .unwrap();
    timeout(
        limit,
        Client::builder(TokioExecutor::new())
            .build_http()
            .request(request),
    )
    .await
    .expect("request timed out")
    .unwrap()
    .map(Body::new)
}

async fn body(response: http::Response<Body>) -> String {
    let bytes = timeout(Duration::from_secs(5), response.into_body().collect())
        .await
        .unwrap()
        .unwrap()
        .to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn stale_devin_id_is_omitted_after_a_successful_catalog_load() {
    let gateway = Gateway::start(ModelsMode::Live).await;
    let ids = gateway.models().await;
    assert_eq!(
        ids,
        ["combo", "plain", "swe", "swe-alias"],
        "a configured Devin id missing from the loaded catalog must be omitted, and catalog-only ids must not be added"
    );
    let again = gateway.models().await;
    assert_eq!(again, ids);
    assert_eq!(
        models_invocations(&gateway.calls()),
        1,
        "a second list within the refresh interval must reuse the cached catalog"
    );
}

#[tokio::test]
async fn stale_free_devin_leg_does_not_count_for_the_free_catalog() {
    let gateway = Gateway::start(ModelsMode::Live).await;
    assert_eq!(
        gateway.models().await,
        ["combo", "plain", "swe", "swe-alias"],
        "the unrestricted key must still list the mixed-provider model"
    );
    assert_eq!(
        gateway.models_as("test-ci").await,
        ["swe", "swe-alias"],
        "stale free Devin legs and billed OpenAI-compatible legs must not count for the free-only catalog"
    );
}

#[tokio::test]
async fn failed_catalog_load_does_not_hide_configured_names() {
    let gateway = Gateway::start(ModelsMode::Fail).await;
    let ids = gateway.models().await;
    assert_eq!(
        ids,
        [
            "combo",
            "gone",
            "old-swe",
            "plain",
            "retired",
            "swe",
            "swe-alias"
        ],
        "a failed load must leave the configured allowlist unchanged"
    );
    assert!(!ids.iter().any(|id| id == "adaptive"));
    let again = gateway.models().await;
    assert_eq!(again, ids);
    assert_eq!(
        models_invocations(&gateway.calls()),
        1,
        "a failed refresh must be attempted, then cached as a failure for the interval"
    );
}

#[tokio::test]
async fn stale_devin_deployment_is_skipped_for_a_same_model_fallback() {
    let gateway = Gateway::start(ModelsMode::Live).await;
    // Listing waits for the first catalog attempt. Chat must then skip the stale leg.
    let _ = gateway.models().await;

    let response = gateway.chat("swe", Duration::from_secs(5)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-llmux-deployment"], "swe-live");
    assert_eq!(response.headers()["x-llmux-attempts"], "1");
    let payload: Value = serde_json::from_str(&body(response).await).unwrap();
    assert_eq!(payload["model"], "swe-2-high");
    let acp = gateway.acp_log();
    assert!(
        acp.contains("swe-2-high"),
        "the live same-model deployment should have been invoked: {acp}"
    );
    assert!(
        !acp.contains("swe-2-retired"),
        "a stale upstream must not be invoked: {acp}"
    );

    let before = gateway.acp_log();
    let gone = gateway.chat("gone", Duration::from_secs(5)).await;
    assert_eq!(gone.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(!gone.headers().contains_key(header::RETRY_AFTER));
    let error: Value = serde_json::from_str(&body(gone).await).unwrap();
    assert_eq!(error["error"]["code"], "rate_limit_exceeded");
    assert_eq!(error["error"]["message"], "No upstream currently available");
    assert_eq!(
        gateway.acp_log(),
        before,
        "an exhausted model must not be rewritten onto another model's deployment"
    );
    assert_eq!(models_invocations(&gateway.calls()), 1);
    let stats = request(
        &gateway.server,
        "test-dashboard",
        "GET",
        "/dashboard/stats",
        None,
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(stats.status(), StatusCode::OK);
    let stats: Value = serde_json::from_str(&body(stats).await).unwrap();
    let stale = stats["deployments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "swe-stale")
        .unwrap();
    assert_eq!(stale["skip_reasons"], json!({"upstream_unavailable":1}));
}

#[tokio::test]
async fn concurrent_lists_share_one_catalog_refresh() {
    let gateway = Gateway::start(ModelsMode::Block).await;
    let release = gateway.release.clone();
    let lists = async {
        let (left, right) = tokio::join!(gateway.models(), gateway.models());
        (left, right)
    };
    let releaser = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        fs::write(release, "go").unwrap();
    });
    let (left, right) = timeout(Duration::from_secs(5), lists)
        .await
        .expect("concurrent lists waited on two catalog refreshes");
    releaser.await.unwrap();
    assert_eq!(left, ["combo", "plain", "swe", "swe-alias"]);
    assert_eq!(right, left);
    assert_eq!(
        models_invocations(&gateway.calls()),
        1,
        "overlapping lists must share one in-flight refresh"
    );
}

#[tokio::test]
async fn chat_does_not_wait_for_a_devin_catalog_refresh() {
    let gateway = Gateway::start(ModelsMode::Block).await;
    let started = Instant::now();
    let response = gateway.chat("retired", Duration::from_secs(2)).await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "chat blocked on devin models for {:?}",
        started.elapsed()
    );
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-llmux-deployment"], "retired-devin");
    assert_eq!(
        models_invocations(&gateway.calls()),
        1,
        "chat should kick a background refresh without waiting for it"
    );
}

#[tokio::test]
async fn oversized_body_is_rejected_before_a_devin_catalog_refresh() {
    let gateway = Gateway::start_with_limit(ModelsMode::Live, Some(64)).await;
    let payload = json!({
        "model": "retired",
        "messages": [{"role": "user", "content": "Hello"}],
        "pad": "x".repeat(64)
    })
    .to_string();
    assert!(payload.len() > 64);
    let split = 32;
    let payload = Bytes::from(payload);
    let chunks = [payload.slice(..split), payload.slice(split..)];
    let stream = futures_util::stream::iter(chunks.into_iter().enumerate()).then(
        |(index, chunk)| async move {
            if index == 1 {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Ok::<Bytes, Infallible>(chunk)
        },
    );
    let request = Request::post(format!("{}/v1/chat/completions", gateway.server.url()))
        .header(header::AUTHORIZATION, "Bearer test-client")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from_stream(stream))
        .unwrap();

    let response = timeout(
        Duration::from_secs(5),
        Client::builder(TokioExecutor::new())
            .build_http()
            .request(request),
    )
    .await
    .expect("oversized request timed out")
    .unwrap()
    .map(Body::new);
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(
        body(response).await.contains("Request body too large"),
        "the oversized request should use the documented 413 contract"
    );
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        models_invocations(&gateway.calls()),
        0,
        "the size rejection must happen before devin models is spawned"
    );
}
