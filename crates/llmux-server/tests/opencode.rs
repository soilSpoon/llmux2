use axum::{
    body::{Body, Bytes},
    extract::State,
    response::Response,
    routing::post,
    Router,
};
use futures_util::stream;
use http::{header, HeaderMap, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use llmux_config::RuntimeConfig;
use llmux_testkit::TestServer;
use serde_json::{json, Value};
use sha2::Digest;
use std::{
    convert::Infallible,
    fs,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::time::timeout;

const ARTIFACT_SHA256: &str = "6cf05f4c7aa25e01360d770785ff1880c9bbf0bf92bd97c67af64a2213d87c76";
const MARKER: &str = "ZZ-PROMPT-MARKER-ZZ";

#[derive(Clone, Copy)]
enum Mode {
    Stream,
    Error(u16),
    Hanging,
}

struct Capture {
    requests: Mutex<Vec<(HeaderMap, Value)>>,
    disconnected: Arc<AtomicBool>,
    mode: Mode,
}

struct DropProbe(Arc<AtomicBool>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn json_response(status: StatusCode, payload: Value) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(payload.to_string()))
        .unwrap()
}

fn sse_response(body: String) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from(body))
        .unwrap()
}

fn hanging_response(disconnected: Arc<AtomicBool>) -> Response {
    let events = stream::unfold(
        (DropProbe(disconnected), false),
        |(probe, sent)| async move {
            if sent {
                futures_util::future::pending::<()>().await;
                unreachable!("pending never completes")
            }
            Some((
                Ok::<_, Infallible>(Bytes::from_static(
                    br#"data: {"choices":[{"index":0,"delta":{"content":"first"},"finish_reason":null}]}

"#,
                )),
                (probe, true),
            ))
        },
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from_stream(events))
        .unwrap()
}

async fn capture_chat(
    State(capture): State<Arc<Capture>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let body: Value = serde_json::from_slice(&body).unwrap();
    capture
        .requests
        .lock()
        .unwrap()
        .push((headers.clone(), body));
    match capture.mode {
        Mode::Stream => sse_response(gv4_sse()),
        Mode::Error(402) => json_response(
            StatusCode::PAYMENT_REQUIRED,
            json!({"error":{"type":"FreeUsageLimitError","message":"opencode: [FreeUsageLimitError] free usage limit reached"}}),
        ),
        Mode::Error(403) => json_response(
            StatusCode::FORBIDDEN,
            json!({"error":{"message":"Free-tier models are only available from within OpenCode"}}),
        ),
        Mode::Error(429) => json_response(
            StatusCode::TOO_MANY_REQUESTS,
            json!({"error":{"type":"FreeUsageLimitError","message":"opencode: [FreeUsageLimitError] free usage limit reached"}}),
        ),
        Mode::Error(_) => unreachable!("unimplemented error mode"),
        Mode::Hanging => hanging_response(capture.disconnected.clone()),
    }
}

fn gv4_frames() -> Vec<String> {
    [
        r#"{"id":"chunk-role","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
        r#"{"id":"chunk-text-1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"hel"},"finish_reason":null}]}"#,
        r#"{"id":"chunk-text-2","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"lo"},"finish_reason":null}]}"#,
        r#"{"id":"chunk-tool-1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{\"a\":1"}}]},"finish_reason":null}]}"#,
        r#"{"id":"chunk-tool-2","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":",\"b\":2}"}}]},"finish_reason":null}]}"#,
        r#"{"id":"chunk-finish","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        r#"{"id":"chunk-usage","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18,"cost":0,"opencode":{"billing_source":"free","list_cost":0,"tokens":{"input":11,"output":7}}}}"#,
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn gv4_sse() -> String {
    let mut out = String::new();
    for frame in gv4_frames() {
        out.push_str("data: ");
        out.push_str(&frame);
        out.push_str("\n\n");
    }
    out.push_str("data: [DONE]\n\n");
    out
}

struct Fixture {
    capture: Arc<Capture>,
    billed_calls: Arc<AtomicUsize>,
    gateway: TestServer,
    log_path: std::path::PathBuf,
    _dir: tempfile::TempDir,
    _upstream: TestServer,
    _billed_upstream: TestServer,
}

async fn start_fixture(mode: Mode, free_only: bool, request_log: bool) -> Fixture {
    let capture = Arc::new(Capture {
        requests: Mutex::new(Vec::new()),
        disconnected: Arc::new(AtomicBool::new(false)),
        mode,
    });
    let upstream = TestServer::start(
        Router::new()
            .route("/inference/openai/v1/chat/completions", post(capture_chat))
            .with_state(capture.clone()),
    )
    .await
    .unwrap();
    let billed_calls = Arc::new(AtomicUsize::new(0));
    let billed_state = billed_calls.clone();
    let billed_upstream = TestServer::start(Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let billed_state = billed_state.clone();
            async move {
                billed_state.fetch_add(1, Ordering::SeqCst);
                json_response(
                    StatusCode::OK,
                    json!({"choices":[{"message":{"role":"assistant","content":"billed"}}]}),
                )
            }
        }),
    ))
    .await
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("client"), "synthetic-client-key\n").unwrap();
    fs::write(dir.path().join("billed"), "synthetic-upstream-key\n").unwrap();
    let base = dir.path().display();
    let policy = if free_only {
        format!("\n[[server.key_policies]]\nkey = \"file:{base}/client\"\nfree_only = true\n")
    } else {
        String::new()
    };
    let log_path = dir.path().join("requests.jsonl");
    let log = if request_log {
        format!("request_log = \"{}\"", log_path.display())
    } else {
        String::new()
    };
    let config = format!(
        r#"
[server]
client_keys = ["file:{base}/client"]
request_timeout_ms = 3000
{log}
{policy}

[routing]
fallback_on = ["quota_exhausted"]

[[credentials]]
id = "billed-key"
kind = "api_key"
provider = "openai_compat"
key = "file:{base}/billed"

[[models]]
name = "big-pickle"

[[models.deployments]]
id = "opencode-free"
auth = "none"
provider = "opencode"
base_url = "{}/inference/openai/v1"
upstream_model = "big-pickle"
priority = 0
billing = "free"

[[models.deployments]]
id = "billed-fallback"
credential = "billed-key"
base_url = "{}/v1"
upstream_model = "big-pickle"
priority = 1
billing = "billed"
"#,
        upstream.url(),
        billed_upstream.url()
    );
    let gateway = TestServer::start(
        llmux_server::app(RuntimeConfig::parse(&config).unwrap())
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    Fixture {
        capture,
        billed_calls,
        gateway,
        log_path,
        _dir: dir,
        _upstream: upstream,
        _billed_upstream: billed_upstream,
    }
}

fn payload(stream: bool) -> Value {
    json!({
        "model": "big-pickle",
        "stream": stream,
        "messages": [
            {"role":"system","content":"caller system instructions"},
            {"role":"user","content":MARKER}
        ],
        "tools": [{
            "type":"function",
            "function":{"name":"lookup","description":"look up a fact","parameters":{"type":"object","properties":{"q":{"type":"string"}}},"strict":false}
        }],
        "temperature": 0.2,
        "top_p": 0.7,
        "max_tokens": 64,
        "store": false
    })
}

async fn post_chat(fixture: &Fixture, body: Value) -> Response {
    let request = Request::post(format!("{}/v1/chat/completions", fixture.gateway.url()))
        .header(header::AUTHORIZATION, "Bearer synthetic-client-key")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap();
    timeout(
        Duration::from_secs(5),
        Client::builder(TokioExecutor::new())
            .build_http()
            .request(request),
    )
    .await
    .unwrap()
    .unwrap()
    .map(Body::new)
}

async fn body_string(response: Response) -> String {
    let bytes = timeout(Duration::from_secs(5), response.into_body().collect())
        .await
        .unwrap()
        .unwrap()
        .to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn first_request(fixture: &Fixture) -> (HeaderMap, Value) {
    fixture.capture.requests.lock().unwrap()[0].clone()
}

fn data_events(body: &str) -> Vec<String> {
    body.lines()
        .filter_map(|line| {
            line.strip_prefix("data:")
                .map(|data| data.trim().to_owned())
        })
        .collect()
}

#[tokio::test]
async fn gv1_headers_and_gv2_body_shape_match_the_opencode_cli_contract() {
    let fixture = start_fixture(Mode::Stream, false, false).await;
    let response = post_chat(&fixture, payload(true)).await;
    assert_eq!(response.status(), StatusCode::OK);
    drop(body_string(response).await);
    let (headers, body) = first_request(&fixture);
    assert_eq!(headers[header::AUTHORIZATION], "Bearer public");
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    for name in [
        "x-opencode-client",
        "x-opencode-project",
        "x-opencode-session",
        "x-opencode-session-id",
        "x-session-affinity",
        "x-session-id",
    ] {
        assert!(headers.contains_key(name), "missing {name}");
    }
    assert!(!headers.contains_key("x-opencode-request"));
    let ua = headers[header::USER_AGENT].to_str().unwrap();
    let parts: Vec<&str> = ua.split('/').collect();
    assert_eq!(parts.len(), 4, "user agent {ua}");
    assert_eq!(parts[0], "opencode");
    assert!(parts.iter().skip(1).all(|part| !part.is_empty()));
    assert_eq!(headers["x-opencode-session"], headers["x-session-affinity"]);
    assert_eq!(headers["x-opencode-session"], headers["x-session-id"]);

    assert_eq!(body["model"], "big-pickle");
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert!(body["max_completion_tokens"].as_u64().unwrap() > 0);
    assert!(body.get("max_tokens").is_none());
    assert!(body.get("store").is_none());
    assert!(body.get("prompt_cache_key").is_none());
    assert_eq!(body["messages"][0]["role"], "system");
    assert!(body["messages"][0]["content"]
        .as_str()
        .unwrap()
        .starts_with("You are an AI agent running in OpenCode, a coding agent harness."));
    let tool = &body["tools"][0];
    assert_eq!(tool["type"], "function");
    assert_eq!(tool["function"]["name"], "lookup");
    assert_eq!(tool["function"]["strict"], false);
}

#[tokio::test]
async fn gv2_tools_are_omitted_when_the_caller_sent_none() {
    let fixture = start_fixture(Mode::Stream, false, false).await;
    let mut body = payload(true);
    body.as_object_mut().unwrap().remove("tools");
    let response = post_chat(&fixture, body).await;
    assert_eq!(response.status(), StatusCode::OK);
    drop(body_string(response).await);
    let (_, upstream) = first_request(&fixture);
    assert!(upstream.get("tools").is_none());
}

#[tokio::test]
async fn gv3_caller_system_content_and_sampling_survive() {
    let fixture = start_fixture(Mode::Stream, false, false).await;
    let response = post_chat(&fixture, payload(true)).await;
    assert_eq!(response.status(), StatusCode::OK);
    drop(body_string(response).await);
    let (_, upstream) = first_request(&fixture);
    let system = upstream["messages"][0]["content"].as_str().unwrap();
    let official = system
        .find("You are an AI agent running in OpenCode")
        .unwrap();
    let caller = system.find("caller system instructions").unwrap();
    assert!(official < caller);
    assert_eq!(system.matches("caller system instructions").count(), 1);
    assert_eq!(upstream["temperature"], 0.2);
    assert_eq!(upstream["top_p"], 0.7);
}

#[tokio::test]
async fn gv4_stream_frames_and_opencode_usage_pass_through_in_order() {
    let fixture = start_fixture(Mode::Stream, false, false).await;
    let response = post_chat(&fixture, payload(true)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let events = data_events(&body_string(response).await);
    let mut expected = gv4_frames();
    expected.push("[DONE]".into());
    assert_eq!(events, expected);
    let usage: Value = serde_json::from_str(&events[6]).unwrap();
    assert_eq!(usage["usage"]["cost"], 0);
    assert_eq!(usage["usage"]["opencode"]["billing_source"], "free");
    assert_eq!(usage["usage"]["total_tokens"], 18);
}

#[tokio::test]
async fn gv5_free_tier_errors_cool_the_leg_without_billed_fallback() {
    for status in [402, 403, 429] {
        let fixture = start_fixture(Mode::Error(status), true, true).await;
        let response = post_chat(&fixture, payload(false)).await;
        assert_eq!(
            response.status(),
            if status == 403 {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::TOO_MANY_REQUESTS
            },
            "status fixture {status}"
        );
        let text = body_string(response).await;
        if status == 403 {
            assert!(
                text.contains("only available from within OpenCode"),
                "{text}"
            );
        }
        assert!(!text.contains(MARKER));
        assert!(!text.contains("synthetic-client-key"));
        assert!(!text.contains("synthetic-upstream-key"));
        assert_eq!(fixture.capture.requests.lock().unwrap().len(), 1);
        assert_eq!(fixture.billed_calls.load(Ordering::SeqCst), 0);
        if status != 403 {
            let second = post_chat(&fixture, payload(false)).await;
            assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
            drop(body_string(second).await);
            assert_eq!(
                fixture.capture.requests.lock().unwrap().len(),
                1,
                "the opencode leg must cool down after status {status}"
            );
        }
    }
}

#[tokio::test]
async fn gv6_client_disconnect_closes_upstream_without_retry_or_dead_mark() {
    let fixture = start_fixture(Mode::Hanging, false, false).await;
    let request = Request::post(format!("{}/v1/chat/completions", fixture.gateway.url()))
        .header(header::AUTHORIZATION, "Bearer synthetic-client-key")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(payload(true).to_string())))
        .unwrap();
    let response = Client::builder(TokioExecutor::new())
        .build_http()
        .request(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let first = timeout(Duration::from_secs(3), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(first.into_data().is_ok());
    drop(body);
    for _ in 0..40 {
        if fixture.capture.disconnected.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(fixture.capture.disconnected.load(Ordering::SeqCst));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fixture.capture.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn gv7_embedded_artifact_headers_and_base_url_are_pinned() {
    let artifact: Value = serde_json::from_str(include_str!(
        "../../llmux-providers/src/opencode/protocol.v2.4.2.json"
    ))
    .unwrap();
    let bytes = include_bytes!("../../llmux-providers/src/opencode/protocol.v2.4.2.json");
    let digest = sha2::Sha256::digest(bytes);
    let digest = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert_eq!(digest, ARTIFACT_SHA256);
    assert_eq!(artifact["cli_version"], "v2.4.2");
    assert_eq!(
        artifact["endpoint"]["base_url"],
        "https://opencode.ai/inference/openai/v1"
    );
    let names: Vec<&str> = artifact["headers"]["identity"]
        .as_array()
        .unwrap()
        .iter()
        .map(Value::as_str)
        .collect::<Option<_>>()
        .unwrap();
    assert_eq!(
        names,
        [
            "x-opencode-client",
            "x-opencode-project",
            "x-opencode-session",
            "x-opencode-session-id",
            "x-session-affinity",
            "x-session-id",
            "user-agent"
        ]
    );
}

#[tokio::test]
async fn gv8_request_log_and_errors_hide_secrets_and_prompt_marker() {
    let fixture = start_fixture(Mode::Error(429), true, true).await;
    let response = post_chat(&fixture, payload(false)).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let error = body_string(response).await;
    assert!(!error.contains(MARKER));
    assert!(!error.contains("synthetic-client-key"));
    assert!(!error.contains("synthetic-upstream-key"));
    let mut text = String::new();
    for _ in 0..40 {
        text = fs::read_to_string(&fixture.log_path).unwrap_or_default();
        if !text.trim().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(!text.trim().is_empty(), "request log did not flush");
    assert!(!text.contains(MARKER), "{text}");
    assert!(!text.contains("synthetic-client-key"), "{text}");
    assert!(!text.contains("synthetic-upstream-key"), "{text}");
}
