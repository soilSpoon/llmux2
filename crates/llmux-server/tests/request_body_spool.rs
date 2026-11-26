use axum::{
    body::Bytes,
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
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{
    sync::Semaphore,
    time::{sleep, timeout},
};

#[derive(Clone, Copy)]
enum Mode {
    Complete,
    RateLimited,
    BadRequest,
}

#[derive(Clone)]
struct Upstream {
    mode: Mode,
    calls: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<Value>>>,
    gate: Option<Arc<Semaphore>>,
}

impl Upstream {
    fn new(mode: Mode, gated: bool, gate: Arc<Semaphore>) -> Self {
        Self {
            mode,
            calls: Arc::new(AtomicUsize::new(0)),
            seen: Arc::new(Mutex::new(Vec::new())),
            gate: gated.then_some(gate),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    fn seen(&self) -> Vec<Value> {
        self.seen.lock().unwrap().clone()
    }
}

async fn upstream_chat(State(upstream): State<Upstream>, body: Bytes) -> Response {
    upstream.calls.fetch_add(1, Ordering::Relaxed);
    let request: Value = serde_json::from_slice(&body).unwrap();
    upstream.seen.lock().unwrap().push(request);
    if let Some(gate) = upstream.gate {
        gate.acquire().await.unwrap().forget();
    }
    match upstream.mode {
        Mode::Complete => Json(json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }]
        }))
        .into_response(),
        Mode::RateLimited => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error":{"message":"slow down","type":"rate_limit_error","code":"rate_limited"}})),
        )
            .into_response(),
        Mode::BadRequest => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":{"message":"bad input","type":"invalid_request_error","code":"bad_request"}})),
        )
            .into_response(),
    }
}

struct Fixture {
    primary: Upstream,
    secondary: Upstream,
    gateway: TestServer,
    gate: Arc<Semaphore>,
    spool_dir: PathBuf,
    _upstreams: [TestServer; 2],
    _dir: tempfile::TempDir,
}

impl Fixture {
    async fn start(
        primary_mode: Mode,
        secondary_mode: Mode,
        primary_gated: bool,
        secondary_gated: bool,
        memory_bytes: u64,
        budget_bytes: u64,
    ) -> Self {
        let gate = Arc::new(Semaphore::new(0));
        let primary = Upstream::new(primary_mode, primary_gated, gate.clone());
        let secondary = Upstream::new(secondary_mode, secondary_gated, gate.clone());
        let primary_server = TestServer::start(
            Router::new()
                .route("/v1/chat/completions", post(upstream_chat))
                .with_state(primary.clone()),
        )
        .await
        .unwrap();
        let secondary_server = TestServer::start(
            Router::new()
                .route("/v1/chat/completions", post(upstream_chat))
                .with_state(secondary.clone()),
        )
        .await
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let spool_dir = dir.path().join("spool");
        for key in ["client", "primary", "secondary"] {
            fs::write(dir.path().join(key), format!("{key}-key\n")).unwrap();
        }
        let base = dir.path().display();
        let spool = format!("{:?}", spool_dir.display().to_string());
        let config = format!(
            r#"
[server]
client_keys = ["file:{base}/client"]
max_request_bytes = 4096
request_body_memory_bytes = {memory_bytes}
request_body_spool_dir = {spool}
request_body_spool_budget_bytes = {budget_bytes}

[routing]
max_attempts = 2
fallback_on = ["rate_limited"]

[[credentials]]
id = "primary"
kind = "api_key"
provider = "openai_compat"
key = "file:{base}/primary"

[[credentials]]
id = "secondary"
kind = "api_key"
provider = "openai_compat"
key = "file:{base}/secondary"

[[models]]
name = "large-context"

[[models.deployments]]
id = "primary"
credential = "primary"
base_url = "{}/v1"
upstream_model = "vendor-primary"
priority = 0

[[models.deployments]]
id = "secondary"
credential = "secondary"
base_url = "{}/v1"
upstream_model = "vendor-secondary"
priority = 10
"#,
            primary_server.url(),
            secondary_server.url()
        );
        let gateway = TestServer::start(
            llmux_server::app(RuntimeConfig::parse(&config).unwrap())
                .await
                .unwrap(),
        )
        .await
        .unwrap();
        Self {
            primary,
            secondary,
            gateway,
            gate,
            spool_dir,
            _upstreams: [primary_server, secondary_server],
            _dir: dir,
        }
    }

    async fn send(&self, payload: String) -> (StatusCode, HeaderMap, String) {
        let request = Request::post(format!("{}/v1/chat/completions", self.gateway.url()))
            .header(header::AUTHORIZATION, "Bearer client-key")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(payload)))
            .unwrap();
        let response = timeout(
            Duration::from_secs(5),
            Client::builder(TokioExecutor::new())
                .build_http()
                .request(request),
        )
        .await
        .unwrap()
        .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = timeout(Duration::from_secs(5), response.into_body().collect())
            .await
            .unwrap()
            .unwrap()
            .to_bytes();
        (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
    }
}

fn payload_with_pad(pad: String) -> String {
    json!({
        "model": "large-context",
        "messages": [{"role": "user", "content": "hi"}],
        "pad": pad,
    })
    .to_string()
}

fn spool_entries(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect()
}

async fn wait_for_calls(upstream: &Upstream, expected: usize) {
    timeout(Duration::from_secs(2), async {
        loop {
            if upstream.calls() == expected {
                return;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

async fn wait_for_entries(dir: &Path, expected: usize) -> Vec<PathBuf> {
    timeout(Duration::from_secs(2), async {
        loop {
            let entries = spool_entries(dir);
            if entries.len() == expected {
                return entries;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn large_body_spools_replays_after_failover_and_cleans_up() {
    let fixture =
        Arc::new(Fixture::start(Mode::RateLimited, Mode::Complete, false, true, 64, 4096).await);
    let pad = "x".repeat(600);
    let expected_pad = pad.len();
    let payload = payload_with_pad(pad);
    let sending = {
        let fixture = fixture.clone();
        tokio::spawn(async move { fixture.send(payload).await })
    };
    wait_for_calls(&fixture.secondary, 1).await;
    let entries = wait_for_entries(&fixture.spool_dir, 1).await;
    assert_eq!(
        entries[0].extension().and_then(|ext| ext.to_str()),
        Some("spool")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&entries[0]).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    fixture.gate.add_permits(1);
    let (status, headers, body) = sending.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["x-llmux-attempts"], "2");
    assert_eq!(fixture.primary.calls(), 1);
    assert_eq!(fixture.secondary.calls(), 1);
    let secondary = fixture.secondary.seen();
    assert_eq!(secondary[0]["model"], "vendor-secondary");
    assert_eq!(secondary[0]["pad"].as_str().unwrap().len(), expected_pad);
    wait_for_entries(&fixture.spool_dir, 0).await;
}

#[tokio::test]
async fn gateway_routing_fields_are_stripped_before_the_replay_spool() {
    let fixture =
        Arc::new(Fixture::start(Mode::RateLimited, Mode::Complete, false, true, 64, 4096).await);
    let payload = json!({
        "model": "large-context",
        "messages": [{"role": "user", "content": "hi"}],
        "pad": "x".repeat(600),
        "min_discount_percent": 50,
        "zdr": true,
        "ranking": {"prefer": "cheap"}
    })
    .to_string();
    let sending = {
        let fixture = fixture.clone();
        tokio::spawn(async move { fixture.send(payload).await })
    };
    wait_for_calls(&fixture.secondary, 1).await;
    assert_eq!(wait_for_entries(&fixture.spool_dir, 1).await.len(), 1);

    fixture.gate.add_permits(1);
    let (status, _, body) = sending.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    let secondary = fixture.secondary.seen();
    assert_eq!(secondary[0]["pad"].as_str().unwrap().len(), 600);
    assert!(secondary[0].get("min_discount_percent").is_none());
    assert!(secondary[0].get("zdr").is_none());
    assert!(secondary[0].get("ranking").is_none());
    wait_for_entries(&fixture.spool_dir, 0).await;
}

#[tokio::test]
async fn body_under_threshold_stays_in_memory() {
    let fixture =
        Arc::new(Fixture::start(Mode::Complete, Mode::Complete, true, false, 4096, 4096).await);
    let payload = payload_with_pad("x".repeat(128));
    let sending = {
        let fixture = fixture.clone();
        tokio::spawn(async move { fixture.send(payload).await })
    };
    wait_for_calls(&fixture.primary, 1).await;
    assert!(spool_entries(&fixture.spool_dir).is_empty());
    fixture.gate.add_permits(1);
    let (status, _, body) = sending.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(fixture.primary.calls(), 1);
    assert_eq!(fixture.secondary.calls(), 0);
    wait_for_entries(&fixture.spool_dir, 0).await;
}

#[tokio::test]
async fn in_flight_spool_budget_is_shared_across_requests() {
    let fixture =
        Arc::new(Fixture::start(Mode::Complete, Mode::Complete, true, false, 64, 768).await);
    let payload = payload_with_pad("x".repeat(600));
    let first = {
        let fixture = fixture.clone();
        let payload = payload.clone();
        tokio::spawn(async move { fixture.send(payload).await })
    };
    wait_for_calls(&fixture.primary, 1).await;
    assert_eq!(wait_for_entries(&fixture.spool_dir, 1).await.len(), 1);

    let (status, _, body) = fixture.send(payload).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(fixture.primary.calls(), 1);
    assert_eq!(fixture.secondary.calls(), 0);

    fixture.gate.add_permits(1);
    let (status, _, body) = first.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    wait_for_entries(&fixture.spool_dir, 0).await;
}

#[tokio::test]
async fn disk_budget_exhaustion_fails_closed_without_upstream_or_secret_leakage() {
    let fixture = Fixture::start(Mode::Complete, Mode::Complete, false, false, 64, 128).await;
    let payload = payload_with_pad("SECRET-DO-NOT-LEAK".repeat(40));
    let (status, _, body) = fixture.send(payload).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(body.contains("storage capacity"), "{body}");
    assert!(!body.contains("SECRET-DO-NOT-LEAK"), "{body}");
    assert_eq!(fixture.primary.calls(), 0);
    assert_eq!(fixture.secondary.calls(), 0);
    wait_for_entries(&fixture.spool_dir, 0).await;
}

#[tokio::test]
async fn upstream_error_cleans_spooled_body() {
    let fixture = Fixture::start(Mode::BadRequest, Mode::Complete, false, false, 64, 4096).await;
    let payload = payload_with_pad("x".repeat(600));
    let (status, _, body) = fixture.send(payload).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(fixture.primary.calls(), 1);
    assert_eq!(fixture.secondary.calls(), 0);
    wait_for_entries(&fixture.spool_dir, 0).await;
}

#[tokio::test]
async fn startup_sweeps_orphaned_spool_files() {
    let dir = tempfile::tempdir().unwrap();
    let spool_dir = dir.path().join("spool");
    fs::create_dir_all(&spool_dir).unwrap();
    let orphan = spool_dir.join(".llmux-body-orphan.spool");
    fs::write(&orphan, b"orphaned request body").unwrap();

    let fixture = Fixture::start_with_spool_dir(&spool_dir).await;
    assert!(!orphan.exists());
    drop(fixture);
}

impl Fixture {
    async fn start_with_spool_dir(spool_dir: &Path) -> Self {
        let fixture_dir = tempfile::tempdir().unwrap();
        let gate = Arc::new(Semaphore::new(0));
        let primary = Upstream::new(Mode::Complete, false, gate.clone());
        let secondary = Upstream::new(Mode::Complete, false, gate.clone());
        let primary_server = TestServer::start(
            Router::new()
                .route("/v1/chat/completions", post(upstream_chat))
                .with_state(primary.clone()),
        )
        .await
        .unwrap();
        let secondary_server = TestServer::start(
            Router::new()
                .route("/v1/chat/completions", post(upstream_chat))
                .with_state(secondary.clone()),
        )
        .await
        .unwrap();
        for key in ["client", "primary", "secondary"] {
            fs::write(fixture_dir.path().join(key), format!("{key}-key\n")).unwrap();
        }
        let base = fixture_dir.path().display();
        let spool = format!("{:?}", spool_dir.display().to_string());
        let config = format!(
            r#"
[server]
client_keys = ["file:{base}/client"]
max_request_bytes = 4096
request_body_memory_bytes = 64
request_body_spool_dir = {spool}
request_body_spool_budget_bytes = 4096

[[credentials]]
id = "primary"
kind = "api_key"
provider = "openai_compat"
key = "file:{base}/primary"

[[credentials]]
id = "secondary"
kind = "api_key"
provider = "openai_compat"
key = "file:{base}/secondary"

[[models]]
name = "large-context"
[[models.deployments]]
id = "primary"
credential = "primary"
base_url = "{}/v1"
upstream_model = "vendor-primary"
priority = 0
[[models.deployments]]
id = "secondary"
credential = "secondary"
base_url = "{}/v1"
upstream_model = "vendor-secondary"
priority = 10
"#,
            primary_server.url(),
            secondary_server.url()
        );
        let gateway = TestServer::start(
            llmux_server::app(RuntimeConfig::parse(&config).unwrap())
                .await
                .unwrap(),
        )
        .await
        .unwrap();
        Self {
            primary,
            secondary,
            gateway,
            gate,
            spool_dir: spool_dir.to_path_buf(),
            _upstreams: [primary_server, secondary_server],
            _dir: fixture_dir,
        }
    }
}
