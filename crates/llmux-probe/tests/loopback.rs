//! Loopback-only end-to-end coverage for the single-shot probe.
//!
//! Every server binds `127.0.0.1:0`; nothing here can reach a real endpoint.

use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::Response,
    routing::post,
    Router,
};
use bytes::Bytes;
use futures_util::{stream, StreamExt};
use http::Uri;
use llmux_probe::{load_config, probe_body, run_probe, Mode, ProbeConfig};
use llmux_testkit::TestServer;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    convert::Infallible,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

const CAPTURE_CAP: usize = 65_536;
const PROMPT: &str = "Reply with exactly: READY";

#[derive(Clone, Copy)]
enum Scenario {
    Sse,
    SseNoDone,
    SseNoUsage,
    SseMalformed,
    SseMidStreamError,
    SseHuge,
    NonSse,
    Error(u16),
    ErrorEcho,
    HangingBeforeHeaders,
    HangingAfterFirstFrame,
}

struct Mock {
    scenario: Scenario,
    hits: AtomicUsize,
    requests: Mutex<Vec<(HeaderMap, Bytes)>>,
}

impl Mock {
    fn new(scenario: Scenario) -> Arc<Self> {
        Arc::new(Self {
            scenario,
            hits: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

async fn chat(State(mock): State<Arc<Mock>>, headers: HeaderMap, body: Bytes) -> Response {
    mock.hits.fetch_add(1, Ordering::SeqCst);
    mock.requests.lock().unwrap().push((headers, body.clone()));
    match mock.scenario {
        Scenario::Sse => sse(sse_body(true, true, "READY")),
        Scenario::SseNoDone => sse(sse_body(false, true, "READY")),
        Scenario::SseNoUsage => sse(sse_body(true, false, "READY")),
        Scenario::SseMalformed => sse(
            "data: {\"choices\":\n\ndata: [DONE]\n\n".to_owned(),
        ),
        Scenario::SseMidStreamError => sse(
            "data: {\"choices\":[{\"delta\":{\"content\":\"REA\"}}]}\n\ndata: {\"error\":{\"type\":\"server_error\",\"message\":\"boom\"}}\n\ndata: [DONE]\n\n"
                .to_owned(),
        ),
        Scenario::SseHuge => sse(huge_body()),
        Scenario::NonSse => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-request-id", "req-nonsse")
            .body(Body::from(
                json!({
                    "id": "chatcmpl-json",
                    "model": "big-pickle",
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": "READY"}}],
                    "usage": {"cost": 0, "opencode": {"billing_source": "free"}}
                })
                .to_string(),
            ))
            .unwrap(),
        Scenario::Error(status) => Response::builder()
            .status(StatusCode::from_u16(status).unwrap())
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({
                    "error": {
                        "type": "FreeUsageLimitError",
                        "code": "free_usage_limit",
                        "message": "opencode: [FreeUsageLimitError] free usage limit reached"
                    }
                })
                .to_string(),
            ))
            .unwrap(),
        Scenario::ErrorEcho => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({
                    "error": {
                        "type": "FreeUsageLimitError",
                        "message": format!("echo {PROMPT} Bearer public ses_abc123")
                    }
                })
                .to_string(),
            ))
            .unwrap(),
        Scenario::HangingBeforeHeaders => std::future::pending::<Response>().await,
        Scenario::HangingAfterFirstFrame => {
            let first = stream::once(async {
                Ok::<_, Infallible>(Bytes::from_static(
                    b"data: {\"choices\":[{\"delta\":{\"content\":\"REA\"}}]}\n\n",
                ))
            });
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .body(Body::from_stream(first.chain(stream::pending())))
                .unwrap()
        }
    }
}

fn sse(body: String) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header("x-request-id", "req-probe-1")
        .header("retry-after", "17")
        .body(Body::from(body))
        .unwrap()
}

fn sse_body(done: bool, usage: bool, content: &str) -> String {
    let mut out = String::new();
    out.push_str(
        "data: {\"id\":\"chunk-role\",\"model\":\"big-pickle\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n",
    );
    out.push_str(&format!(
        "data: {{\"id\":\"chunk-text\",\"model\":\"big-pickle\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{content}\"}},\"finish_reason\":null}}]}}\n\n"
    ));
    out.push_str(
        "data: {\"id\":\"chunk-finish\",\"model\":\"big-pickle\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    );
    if usage {
        out.push_str(
            "data: {\"id\":\"chunk-usage\",\"model\":\"big-pickle\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":3,\"total_tokens\":14,\"cost\":0,\"opencode\":{\"billing_source\":\"free\",\"list_cost\":0,\"allowance\":{\"remaining\":1}}}}\n\n",
        );
    }
    if done {
        out.push_str("data: [DONE]\n\n");
    }
    out
}

fn huge_body() -> String {
    let frame = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"padding-padding-padding\"},\"finish_reason\":null}]}\n\n";
    let mut out = String::new();
    while out.len() <= CAPTURE_CAP * 2 {
        out.push_str(frame);
    }
    out.push_str("data: [DONE]\n\n");
    out
}

async fn mock_server(scenario: Scenario) -> (TestServer, Arc<Mock>) {
    let mock = Mock::new(scenario);
    let router = Router::new()
        .route("/chat/completions", post(chat))
        .with_state(mock.clone());
    (TestServer::start(router).await.unwrap(), mock)
}

fn config(url: &str) -> ProbeConfig {
    ProbeConfig {
        run_id: "wo254-loopback".to_owned(),
        target_row: "opencode-big-pickle".to_owned(),
        base_url: url.parse::<Uri>().unwrap(),
        total_timeout: Duration::from_secs(2),
        idle_timeout: Duration::from_millis(500),
        local_token_cap: 16,
    }
}

fn evidence_json(evidence: &llmux_probe::Evidence) -> String {
    serde_json::to_string(evidence).unwrap()
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[tokio::test]
async fn happy_path_records_success_and_sends_exactly_one_request() {
    let (server, mock) = mock_server(Scenario::Sse).await;
    let evidence = run_probe(&config(&server.url()), Mode::Execute).await;

    assert_eq!(mock.hits(), 1);
    assert!(evidence.exit_ok);
    assert_eq!(
        evidence.terminal_class,
        Some("free zero-cost streaming success")
    );
    assert!(evidence.request.sent);
    assert_eq!(evidence.http.as_ref().unwrap().status, 200);
    assert_eq!(evidence.http.as_ref().unwrap().version, "HTTP/1.1");
    assert_eq!(
        evidence.http.as_ref().unwrap().content_type.as_deref(),
        Some("text/event-stream")
    );
    assert_eq!(
        evidence
            .http
            .as_ref()
            .unwrap()
            .selected_headers
            .get("retry-after")
            .map(String::as_str),
        Some("17")
    );
    assert_eq!(evidence.response.request_id.as_deref(), Some("req-probe-1"));
    assert_eq!(evidence.response.stream_id.as_deref(), Some("chunk-role"));
    assert_eq!(evidence.response.usage.as_ref().unwrap()["cost"], json!(0));
    assert_eq!(
        evidence.response.usage.as_ref().unwrap()["opencode"]["billing_source"],
        json!("free")
    );
    assert_eq!(
        evidence.response.optional_usage.get("list_cost"),
        Some(&json!(0))
    );
    assert_eq!(
        evidence.response.optional_usage.get("allowance"),
        Some(&json!({"remaining": 1}))
    );
    assert_eq!(evidence.response.content_deltas, 1);
    assert!(evidence.response.done_seen);
    assert!(evidence.response.event_order.contains(&"usage".to_owned()));
    assert!(evidence.response.event_order.contains(&"done".to_owned()));
    assert_eq!(evidence.limits.capture_cap_bytes, CAPTURE_CAP);
    assert_eq!(evidence.limits.total_timeout_ms, 2000);
    assert_eq!(evidence.limits.idle_timeout_ms, 500);
}

#[tokio::test]
async fn outbound_hash_is_the_sha256_of_the_bytes_the_mock_received() {
    let (server, mock) = mock_server(Scenario::Sse).await;
    let evidence = run_probe(&config(&server.url()), Mode::Execute).await;

    let (headers, body) = mock.requests.lock().unwrap().first().cloned().unwrap();
    assert_eq!(evidence.request.unredacted_body_sha256, sha256_hex(&body));
    assert_eq!(
        headers
            .get(header::AUTHORIZATION)
            .unwrap()
            .to_str()
            .unwrap(),
        "Bearer public"
    );
    assert_eq!(
        headers.get("x-opencode-client").unwrap().to_str().unwrap(),
        "opencode"
    );

    let sent: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(sent["model"], json!("big-pickle"));
    assert_eq!(sent["stream"], json!(true));
    assert_eq!(sent["max_completion_tokens"], json!(16));
    assert_eq!(sent["temperature"], json!(0));
    assert!(sent.get("tools").is_none());
    assert_eq!(sent["stream_options"]["include_usage"], json!(true));
    let messages = sent["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], json!("system"));
    assert!(messages[0]["content"]
        .as_str()
        .unwrap()
        .starts_with("You are an AI agent running in OpenCode"));
    assert_eq!(messages[1]["role"], json!("user"));
    assert_eq!(messages[1]["content"], json!(PROMPT));
}

#[tokio::test]
async fn redaction_never_writes_the_prompt_authorization_or_derived_ids() {
    let (server, _mock) = mock_server(Scenario::Sse).await;
    let evidence = run_probe(&config(&server.url()), Mode::Execute).await;
    let text = evidence_json(&evidence);

    assert!(!text.contains(PROMPT), "user prompt leaked: {text}");
    assert!(
        !text.contains("You are an AI agent running in OpenCode"),
        "pinned system prompt leaked"
    );
    assert!(!text.contains("Bearer public"), "authorization leaked");
    assert!(!text.contains("ses_"), "derived session id leaked");
    assert!(!text.contains("prj_"), "derived project id leaked");
    assert!(!text.contains("synthetic"), "client key leaked");

    assert_eq!(evidence.request.unredacted_body_sha256.len(), 64);
    assert_eq!(evidence.request.redacted_body_sha256.len(), 64);
    assert_ne!(
        evidence.request.unredacted_body_sha256,
        evidence.request.redacted_body_sha256
    );
    assert!(evidence.request.unredacted_body_sha256.is_ascii());
}

#[tokio::test]
async fn a_terminal_failure_stops_after_one_request() {
    let (server, mock) = mock_server(Scenario::Error(402)).await;
    let evidence = run_probe(&config(&server.url()), Mode::Execute).await;

    assert_eq!(mock.hits(), 1);
    assert!(!evidence.exit_ok);
    assert_eq!(evidence.terminal_class, Some("402"));
    let error_body = evidence.response.error_body.as_deref().unwrap_or_default();
    assert!(error_body.contains("FreeUsageLimitError"));
}

#[tokio::test]
async fn provider_error_text_echoing_the_prompt_or_authorization_is_redacted() {
    let (server, _mock) = mock_server(Scenario::ErrorEcho).await;
    let evidence = run_probe(&config(&server.url()), Mode::Execute).await;
    let text = evidence_json(&evidence);

    assert_eq!(evidence.terminal_class, Some("5xx"));
    assert!(!text.contains(PROMPT), "prompt echoed into evidence");
    assert!(!text.contains("Bearer public"), "authorization echoed");
    assert!(!text.contains("ses_abc123"), "session id echoed");
    assert!(text.contains("FreeUsageLimitError"));
}

#[tokio::test]
async fn five_hundred_is_classified_without_a_retry() {
    let (server, mock) = mock_server(Scenario::Error(503)).await;
    let evidence = run_probe(&config(&server.url()), Mode::Execute).await;

    assert_eq!(mock.hits(), 1);
    assert_eq!(evidence.terminal_class, Some("5xx"));
    assert!(!evidence.exit_ok);
}

#[tokio::test]
async fn forbidden_is_classified_without_a_retry() {
    let (server, mock) = mock_server(Scenario::Error(403)).await;
    let evidence = run_probe(&config(&server.url()), Mode::Execute).await;

    assert_eq!(mock.hits(), 1);
    assert_eq!(evidence.terminal_class, Some("403"));
}

#[tokio::test]
async fn too_many_requests_is_classified_without_a_retry() {
    let (server, mock) = mock_server(Scenario::Error(429)).await;
    let evidence = run_probe(&config(&server.url()), Mode::Execute).await;

    assert_eq!(mock.hits(), 1);
    assert_eq!(evidence.terminal_class, Some("429"));
}

#[tokio::test]
async fn other_client_errors_are_classified() {
    let (server, _mock) = mock_server(Scenario::Error(422)).await;
    let evidence = run_probe(&config(&server.url()), Mode::Execute).await;

    assert_eq!(evidence.terminal_class, Some("other 4xx"));
}

#[tokio::test]
async fn non_sse_success_is_classified() {
    let (server, mock) = mock_server(Scenario::NonSse).await;
    let evidence = run_probe(&config(&server.url()), Mode::Execute).await;

    assert_eq!(mock.hits(), 1);
    assert_eq!(evidence.terminal_class, Some("non-SSE success"));
    assert!(!evidence.exit_ok);
}

#[tokio::test]
async fn missing_usage_is_classified() {
    let (server, _mock) = mock_server(Scenario::SseNoUsage).await;
    let evidence = run_probe(&config(&server.url()), Mode::Execute).await;

    assert_eq!(
        evidence.terminal_class,
        Some("HTTP success with missing usage")
    );
}

#[tokio::test]
async fn missing_done_is_classified() {
    let (server, _mock) = mock_server(Scenario::SseNoDone).await;
    let evidence = run_probe(&config(&server.url()), Mode::Execute).await;

    assert_eq!(evidence.terminal_class, Some("missing [DONE]"));
}

#[tokio::test]
async fn malformed_stream_syntax_is_classified() {
    let (server, _mock) = mock_server(Scenario::SseMalformed).await;
    let evidence = run_probe(&config(&server.url()), Mode::Execute).await;

    assert_eq!(evidence.terminal_class, Some("malformed stream syntax"));
    assert!(!evidence.exit_ok);
}

#[tokio::test]
async fn mid_stream_error_frame_is_classified() {
    let (server, mock) = mock_server(Scenario::SseMidStreamError).await;
    let evidence = run_probe(&config(&server.url()), Mode::Execute).await;

    assert_eq!(mock.hits(), 1);
    assert_eq!(evidence.terminal_class, Some("mid-stream error frame"));
    assert!(!evidence.exit_ok);
}

#[tokio::test]
async fn capture_cap_truncates_and_is_recorded() {
    let (server, mock) = mock_server(Scenario::SseHuge).await;
    let evidence = run_probe(&config(&server.url()), Mode::Execute).await;

    assert_eq!(mock.hits(), 1);
    assert_eq!(evidence.response.bytes_captured, CAPTURE_CAP);
    assert!(evidence.response.capture_truncated);
    assert_eq!(evidence.terminal_class, Some("local harness error"));
    assert!(evidence.response.excerpt.len() <= 2048);
    assert!(!evidence.exit_ok);
}

#[tokio::test]
async fn idle_timeout_is_enforced() {
    let (server, mock) = mock_server(Scenario::HangingAfterFirstFrame).await;
    let mut cfg = config(&server.url());
    cfg.total_timeout = Duration::from_millis(1500);
    cfg.idle_timeout = Duration::from_millis(50);
    let evidence = run_probe(&cfg, Mode::Execute).await;

    assert_eq!(mock.hits(), 1);
    assert_eq!(evidence.terminal_class, Some("timeout"));
    assert!(evidence.timings.total_ms >= 50);
}

#[tokio::test]
async fn total_timeout_is_enforced() {
    let (server, mock) = mock_server(Scenario::HangingBeforeHeaders).await;
    let mut cfg = config(&server.url());
    cfg.total_timeout = Duration::from_millis(50);
    cfg.idle_timeout = Duration::from_millis(40);
    let evidence = run_probe(&cfg, Mode::Execute).await;

    assert_eq!(mock.hits(), 1);
    assert_eq!(evidence.terminal_class, Some("timeout"));
    assert!(!evidence.exit_ok);
}

#[tokio::test]
async fn a_closed_loopback_port_is_a_transport_failure() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let evidence = run_probe(&config(&format!("http://{address}")), Mode::Execute).await;

    assert_eq!(evidence.terminal_class, Some("transport failure"));
    assert!(!evidence.exit_ok);
}

#[tokio::test]
async fn dry_run_makes_zero_connections() {
    let (server, mock) = mock_server(Scenario::Sse).await;
    let evidence = run_probe(&config(&server.url()), Mode::DryRun).await;

    assert_eq!(mock.hits(), 0);
    assert!(!evidence.request.sent);
    assert!(evidence.exit_ok);
    assert!(evidence.terminal_class.is_none());
    assert_eq!(evidence.request.unredacted_body_sha256.len(), 64);
    assert!(!evidence_json(&evidence).contains(PROMPT));
}

fn row(overrides: &[(&str, Value)]) -> Value {
    let mut row = json!({
        "target_row": "opencode-big-pickle",
        "model": "big-pickle",
        "stream": true,
        "total_timeout_ms": 20000,
        "idle_timeout_ms": 10000,
        "local_token_cap": 16
    });
    for (key, value) in overrides {
        row[*key] = value.clone();
    }
    row
}

fn write_row(name: &str, row: &Value) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("llmux-probe-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("target.json");
    std::fs::write(&path, row.to_string()).unwrap();
    path
}

#[test]
fn config_loads_a_complete_row() {
    let path = write_row("complete", &row(&[]));
    let config = load_config("run-1", &path).unwrap();
    assert_eq!(config.run_id, "run-1");
    assert_eq!(config.target_row, "opencode-big-pickle");
    assert_eq!(
        config.base_url.to_string(),
        "https://opencode.ai/inference/openai/v1"
    );
    assert_eq!(config.total_timeout, Duration::from_secs(20));
    assert_eq!(config.idle_timeout, Duration::from_secs(10));
    assert_eq!(config.local_token_cap, 16);
}

#[test]
fn config_refuses_a_missing_or_empty_run_id() {
    let path = write_row("empty-run-id", &row(&[]));
    assert!(load_config("", &path).is_err());
    assert!(load_config("   ", &path).is_err());
}

#[test]
fn config_refuses_a_missing_target_row() {
    let path = write_row("missing-target", &row(&[("target_row", json!(""))]));
    assert!(load_config("run-1", &path).is_err());
}

#[test]
fn config_refuses_a_missing_target_file() {
    let path = std::env::temp_dir().join("llmux-probe-does-not-exist/target.json");
    assert!(load_config("run-1", &path).is_err());
}

#[test]
fn config_refuses_missing_timeouts() {
    let value = row(&[]);
    let mut no_total = value.clone();
    no_total.as_object_mut().unwrap().remove("total_timeout_ms");
    assert!(load_config("run-1", &write_row("no-total", &no_total)).is_err());

    let mut no_idle = value.clone();
    no_idle.as_object_mut().unwrap().remove("idle_timeout_ms");
    assert!(load_config("run-1", &write_row("no-idle", &no_idle)).is_err());

    let zero = row(&[("total_timeout_ms", json!(0))]);
    assert!(load_config("run-1", &write_row("zero-total", &zero)).is_err());
}

#[test]
fn config_refuses_timeouts_above_the_design_values() {
    let total = row(&[("total_timeout_ms", json!(20_001))]);
    assert!(load_config("run-1", &write_row("up-total", &total)).is_err());

    let idle = row(&[("idle_timeout_ms", json!(10_001))]);
    assert!(load_config("run-1", &write_row("up-idle", &idle)).is_err());

    let inverted = row(&[
        ("total_timeout_ms", json!(100)),
        ("idle_timeout_ms", json!(200)),
    ]);
    assert!(load_config("run-1", &write_row("inverted", &inverted)).is_err());
}

#[test]
fn config_refuses_a_missing_or_changed_local_token_cap() {
    let value = row(&[]);
    let mut missing = value.clone();
    missing.as_object_mut().unwrap().remove("local_token_cap");
    assert!(load_config("run-1", &write_row("no-cap", &missing)).is_err());

    let changed = row(&[("local_token_cap", json!(32))]);
    assert!(load_config("run-1", &write_row("big-cap", &changed)).is_err());
}

#[test]
fn probe_body_is_the_exact_design_request_before_rewriting() {
    assert_eq!(
        probe_body(),
        json!({
            "model": "big-pickle",
            "stream": true,
            "messages": [{"role": "user", "content": "Reply with exactly: READY"}],
            "max_completion_tokens": 16,
            "temperature": 0
        })
    );
}

#[test]
fn config_refuses_a_base_url_with_userinfo() {
    let with_userinfo = row(&[("base_url", json!("http://user:pass@127.0.0.1:9"))]);
    assert!(load_config("run-1", &write_row("userinfo", &with_userinfo)).is_err());
}

#[test]
fn config_refuses_a_row_that_is_not_the_exact_probe_request() {
    let model = row(&[("model", json!("gpt-4o"))]);
    assert!(load_config("run-1", &write_row("wrong-model", &model)).is_err());

    let stream = row(&[("stream", json!(false))]);
    assert!(load_config("run-1", &write_row("no-stream", &stream)).is_err());
}

fn binary() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_llmux-probe"))
}

#[test]
fn binary_defaults_to_a_dry_run_and_never_sends() {
    let path = write_row("binary-dry", &row(&[]));
    let output = binary()
        .args(["--run-id", "run-1", "--target"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(output.status.success());
    let evidence: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(evidence["mode"], json!("dry_run"));
    assert_eq!(evidence["request"]["sent"], json!(false));
    assert_eq!(evidence["terminal_class"], Value::Null);
}

#[test]
fn binary_refuses_a_missing_run_id() {
    let path = write_row("binary-no-run", &row(&[]));
    let output = binary().args(["--target"]).arg(&path).output().unwrap();
    assert!(!output.status.success());
    let evidence: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(evidence["terminal_class"], json!("local harness error"));
    assert!(evidence["refusal"].as_str().unwrap().contains("run-id"));
}

#[test]
fn binary_refuses_a_missing_target_row() {
    let path = write_row("binary-bad-row", &row(&[("total_timeout_ms", json!(null))]));
    let output = binary()
        .args(["--run-id", "run-1", "--target"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let evidence: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(evidence["terminal_class"], json!("local harness error"));
    assert!(!evidence["refusal"].as_str().unwrap().is_empty());
}

#[test]
fn binary_refuses_an_unknown_flag() {
    let output = binary().args(["--execute-me"]).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown flag"));
}
