//! Single-shot probe harness for the native `opencode` `big-pickle` lane.
//!
//! The harness builds its request through the merged [`OpenCode`] adapter and
//! its pinned protocol artifact. It sends at most one HTTP request, records
//! redacted evidence, and classifies the terminal outcome with the design v8
//! §6.7 predicates. Nothing here is wired into the gateway, config, keysync, or
//! a deployment path.

pub mod classify;
pub mod evidence;
pub mod redact;

use bytes::Bytes;
use http::{header, HeaderMap, Request, Response, Uri};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use llmux_providers::opencode::pinned_system_prompt;
use llmux_providers::OpenCode;
use llmux_types::{
    provider, AuthMaterial, Billing, Deployment, DeploymentIdx, GatewayRequest, InboundFormat,
    ProviderAdapter, ProviderKind, QuotaSpec, RequestId,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::Path,
    time::{Duration, Instant},
};

pub use classify::{classify, Observation, TerminalClass};
pub use evidence::{
    EndpointEvidence, Evidence, HttpEvidence, LimitEvidence, RequestEvidence, ResponseEvidence,
    TimingEvidence, EVIDENCE_SCHEMA,
};
pub use redact::{canonical_redacted_body, redact_text, sha256_hex};

/// Design v8 §6.2 item 5.
pub const CAPTURE_CAP_BYTES: usize = 65_536;
/// Design v8 §6.2 item 6.
pub const MAX_TOTAL_TIMEOUT: Duration = Duration::from_secs(20);
/// Design v8 §6.2 item 6.
pub const MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(10);
/// Design v8 §6.4 property 6.
pub const DESIGN_TOKEN_CAP: u32 = 16;
/// Design v8 §6.4.
pub const PROBE_MODEL: &str = "big-pickle";
/// Design v8 §6.4.
pub const PROBE_PROMPT: &str = "Reply with exactly: READY";

const SYSTEM_PROMPT_PLACEHOLDER: &str = "<redacted:system-prompt>";
const PROMPT_PLACEHOLDER: &str = "<redacted:prompt>";
const SYNTHETIC_CLIENT: &str = "synthetic-probe-client";
const MAX_RECORDED_EVENTS: usize = 64;
const SELECTED_HEADERS: &[&str] = &[
    "content-type",
    "content-length",
    "retry-after",
    "x-request-id",
    "request-id",
    "openai-request-id",
    "x-ratelimit-limit",
    "x-ratelimit-remaining",
    "x-ratelimit-reset",
    "server",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Build and hash the request but open no connection.
    DryRun,
    /// Send the single request.
    Execute,
}

/// Validated probe target row. Timeouts can only shorten the design values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeConfig {
    pub run_id: String,
    pub target_row: String,
    pub base_url: Uri,
    pub total_timeout: Duration,
    pub idle_timeout: Duration,
    pub local_token_cap: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetRow {
    target_row: String,
    model: String,
    stream: bool,
    #[serde(default)]
    base_url: Option<String>,
    total_timeout_ms: u64,
    idle_timeout_ms: u64,
    local_token_cap: u32,
}

/// Parse and validate a target row file for one run id.
///
/// Every missing or widened value refuses the run instead of taking a default,
/// because a default timeout or cap would silently relax the design bounds.
pub fn load_config(run_id: &str, path: &Path) -> Result<ProbeConfig, String> {
    if run_id.trim().is_empty() {
        return Err("run-id is unset; pass --run-id with a non-empty id".into());
    }
    if run_id.len() > 128 || run_id.chars().any(char::is_whitespace) {
        return Err("run-id must be at most 128 characters with no whitespace".into());
    }
    let text = std::fs::read_to_string(path)
        .map_err(|_| "target row file is unset or unreadable".to_owned())?;
    let row: TargetRow =
        serde_json::from_str(&text).map_err(|err| format!("target row is invalid: {err}"))?;

    if row.target_row.trim().is_empty() {
        return Err("target row id is unset".into());
    }
    if row.model != PROBE_MODEL {
        return Err(format!("target row model must be `{PROBE_MODEL}`"));
    }
    if !row.stream {
        return Err("target row must declare stream = true".into());
    }
    if row.local_token_cap != DESIGN_TOKEN_CAP {
        return Err(format!(
            "local token cap must be exactly {DESIGN_TOKEN_CAP}; a lower cap changes the exact request and a higher cap exceeds the design"
        ));
    }
    if row.total_timeout_ms == 0 || row.total_timeout_ms > MAX_TOTAL_TIMEOUT.as_millis() as u64 {
        return Err(format!(
            "total timeout must be set between 1 and {} ms",
            MAX_TOTAL_TIMEOUT.as_millis()
        ));
    }
    if row.idle_timeout_ms == 0 || row.idle_timeout_ms > MAX_IDLE_TIMEOUT.as_millis() as u64 {
        return Err(format!(
            "stream-idle timeout must be set between 1 and {} ms",
            MAX_IDLE_TIMEOUT.as_millis()
        ));
    }
    if row.idle_timeout_ms > row.total_timeout_ms {
        return Err("stream-idle timeout cannot exceed the total timeout".into());
    }

    let base_url = row
        .base_url
        .as_deref()
        .unwrap_or(provider::OPENCODE_BASE_URL);
    let base_url: Uri = base_url
        .parse()
        .map_err(|_| "target row base_url is not a valid URI".to_owned())?;
    if !matches!(base_url.scheme_str(), Some("http" | "https")) || base_url.authority().is_none() {
        return Err("target row base_url must be an absolute http(s) URI".into());
    }
    if base_url
        .authority()
        .is_some_and(|authority| authority.as_str().contains('@'))
    {
        return Err("target row base_url must not carry userinfo".into());
    }

    Ok(ProbeConfig {
        run_id: run_id.to_owned(),
        target_row: row.target_row,
        base_url,
        total_timeout: Duration::from_millis(row.total_timeout_ms),
        idle_timeout: Duration::from_millis(row.idle_timeout_ms),
        local_token_cap: row.local_token_cap,
    })
}

/// The exact design v8 §6.4 body, before the adapter rewrites it.
pub fn probe_body() -> Value {
    json!({
        "model": PROBE_MODEL,
        "stream": true,
        "messages": [
            { "role": "user", "content": PROBE_PROMPT }
        ],
        "max_completion_tokens": DESIGN_TOKEN_CAP,
        "temperature": 0
    })
}

fn deployment(config: &ProbeConfig) -> Deployment {
    Deployment {
        headers: HeaderMap::new(),
        fallback: false,
        idx: DeploymentIdx(0),
        id: config.target_row.clone().into(),
        provider: ProviderKind(provider::OPENCODE.into()),
        credential: "".into(),
        base_url: config.base_url.clone(),
        upstream_model: PROBE_MODEL.into(),
        strip_request_fields: Box::default(),
        priority: 0,
        quota: QuotaSpec::default(),
        shared_quota: None,
        timeout: config.total_timeout,
        max_context: None,
        expires_at: None,
        priority_boost: None,
        priority_boost_until: None,
        billing: Billing::Free,
    }
}

/// Build the probe request through the merged adapter and its pinned prompt.
pub async fn build_probe_request(config: &ProbeConfig) -> Result<Request<Bytes>, String> {
    let body = serde_json::to_vec(&probe_body())
        .map_err(|err| format!("probe body is not JSON: {err}"))?;
    let request = GatewayRequest {
        format: InboundFormat::OpenAiChat,
        model: PROBE_MODEL.into(),
        stream: true,
        body: Bytes::from(body),
        anthropic_headers: HeaderMap::new(),
        est_input_tokens: 0,
        max_output_tokens: Some(DESIGN_TOKEN_CAP),
        context_overflow: None,
        request_id: RequestId("probe".into()),
        end_user: None,
        client_key: Some(SYNTHETIC_CLIENT.into()),
        image_count: 0,
        free_only: true,
    };
    OpenCode::new()
        .build_request(&request, &deployment(config), &AuthMaterial::None)
        .await
        .map_err(|err| format!("adapter refused the probe request: {err}"))
}

/// Run one probe. Exactly one request is attempted, and only in [`Mode::Execute`].
pub async fn run_probe(config: &ProbeConfig, mode: Mode) -> Evidence {
    let started = Instant::now();
    let request = match build_probe_request(config).await {
        Ok(request) => request,
        Err(err) => return refused(config, &err),
    };
    let (parts, body) = request.into_parts();
    let unredacted_body_sha256 = sha256_hex(&body);
    let (redacted_body, redacted_body_sha256) = match canonical_redacted_body(&body) {
        Ok(redacted) => redacted,
        Err(err) => return refused(config, &err),
    };
    let request_evidence = RequestEvidence {
        method: parts.method.to_string(),
        sent: mode == Mode::Execute,
        unredacted_body_sha256,
        redacted_body_sha256,
        redacted_body,
        authorization_present: parts.headers.contains_key(header::AUTHORIZATION),
    };
    let limits = LimitEvidence {
        total_timeout_ms: config.total_timeout.as_millis() as u64,
        idle_timeout_ms: config.idle_timeout.as_millis() as u64,
        capture_cap_bytes: CAPTURE_CAP_BYTES,
        local_token_cap: config.local_token_cap,
    };
    let endpoint = EndpointEvidence {
        origin: origin(&parts.uri),
    };

    if mode == Mode::DryRun {
        return Evidence {
            schema: EVIDENCE_SCHEMA,
            run_id: config.run_id.clone(),
            target_row: config.target_row.clone(),
            mode: "dry_run",
            terminal_class: None,
            exit_ok: true,
            refusal: None,
            endpoint,
            request: request_evidence,
            http: None,
            response: ResponseEvidence::default(),
            timings: TimingEvidence {
                total_ms: started.elapsed().as_millis(),
                first_byte_ms: None,
            },
            limits,
        };
    }

    let connector = HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .build();
    let client = Client::builder(TokioExecutor::new()).build(connector);
    let upstream = Request::from_parts(parts, Full::new(body));

    let exchange = async move {
        let response = match client.request(upstream).await {
            Ok(response) => response,
            Err(_) => return Capture::transport(),
        };
        read_response(config, response, started).await
    };
    let capture = match tokio::time::timeout(config.total_timeout, exchange).await {
        Ok(capture) => capture,
        Err(_) => Capture::total_timeout(),
    };

    let class = classify(&capture.observation);
    Evidence {
        schema: EVIDENCE_SCHEMA,
        run_id: config.run_id.clone(),
        target_row: config.target_row.clone(),
        mode: "execute",
        terminal_class: Some(class.name()),
        exit_ok: class == TerminalClass::FreeZeroCostStreamingSuccess,
        refusal: None,
        endpoint,
        request: request_evidence,
        http: capture.http,
        response: ResponseEvidence {
            request_id: capture.request_id,
            stream_id: capture.stream_id,
            event_order: capture.event_order,
            events_truncated: capture.events_truncated,
            content_deltas: capture.observation.content_deltas,
            usage: capture.observation.usage,
            optional_usage: capture.optional_usage,
            done_seen: capture.observation.done_seen,
            bytes_captured: capture.bytes_captured,
            capture_truncated: capture.capture_truncated,
            error_body: capture.error_body,
            excerpt: capture.excerpt,
        },
        timings: TimingEvidence {
            total_ms: started.elapsed().as_millis(),
            first_byte_ms: capture.first_byte_ms,
        },
        limits,
    }
}

fn refused(config: &ProbeConfig, message: &str) -> Evidence {
    let mut evidence = Evidence::refused(&config.run_id, message);
    evidence.target_row = config.target_row.clone();
    evidence
}

#[derive(Default)]
struct Capture {
    observation: Observation,
    http: Option<HttpEvidence>,
    request_id: Option<String>,
    stream_id: Option<String>,
    event_order: Vec<String>,
    events_truncated: bool,
    optional_usage: BTreeMap<String, Value>,
    error_body: Option<String>,
    excerpt: String,
    bytes_captured: usize,
    capture_truncated: bool,
    first_byte_ms: Option<u128>,
}

impl Capture {
    fn transport() -> Self {
        let mut capture = Self::default();
        capture.observation.transport_failure = true;
        capture
    }

    fn total_timeout() -> Self {
        let mut capture = Self::default();
        capture.observation.total_timeout = true;
        capture
    }
}

async fn read_response(
    config: &ProbeConfig,
    response: Response<Incoming>,
    started: Instant,
) -> Capture {
    let status = response.status();
    let version = format!("{:?}", response.version());
    let headers = response.headers().clone();
    let content_type = header_string(&headers, header::CONTENT_TYPE);
    let system_prompt = pinned_system_prompt();
    let secrets = [
        (system_prompt.as_str(), SYSTEM_PROMPT_PLACEHOLDER),
        (PROBE_PROMPT, PROMPT_PLACEHOLDER),
    ];

    let mut capture = Capture {
        first_byte_ms: Some(started.elapsed().as_millis()),
        ..Capture::default()
    };
    capture.observation.http_status = Some(status.as_u16());
    capture.observation.http_version = Some(version.clone());
    capture.observation.content_type = content_type.clone();
    capture.request_id = llmux_types::header_request_id(&headers).map(|id| id.to_string());
    capture.http = Some(HttpEvidence {
        status: status.as_u16(),
        version,
        content_type: content_type.clone(),
        selected_headers: selected_headers(&headers, &secrets),
    });

    let raw = read_body(response.into_body(), config.idle_timeout, &mut capture).await;
    let text = String::from_utf8_lossy(&raw);
    capture.excerpt = redact_text(&text, &secrets);

    if !status.is_success() {
        capture.error_body = Some(redact_text(&text, &secrets));
        return capture;
    }
    if !is_event_stream(content_type.as_deref()) {
        return capture;
    }
    parse_sse(&text, &mut capture, &secrets);
    capture
}

async fn read_body(mut body: Incoming, idle: Duration, capture: &mut Capture) -> Vec<u8> {
    let mut raw = Vec::new();
    loop {
        if raw.len() >= CAPTURE_CAP_BYTES {
            capture.capture_truncated = true;
            capture.observation.capture_truncated = true;
            break;
        }
        match tokio::time::timeout(idle, body.frame()).await {
            Err(_) => {
                capture.observation.idle_timeout = true;
                break;
            }
            Ok(None) => break,
            Ok(Some(Err(_))) => {
                capture.observation.transport_failure = true;
                break;
            }
            Ok(Some(Ok(frame))) => {
                let Ok(data) = frame.into_data() else {
                    continue;
                };
                let remaining = CAPTURE_CAP_BYTES - raw.len();
                if data.len() > remaining {
                    raw.extend_from_slice(&data[..remaining]);
                    capture.capture_truncated = true;
                    capture.observation.capture_truncated = true;
                    break;
                }
                raw.extend_from_slice(&data);
                if String::from_utf8_lossy(&raw).contains("data: [DONE]") {
                    break;
                }
            }
        }
    }
    capture.bytes_captured = raw.len();
    raw
}

fn parse_sse(text: &str, capture: &mut Capture, secrets: &[(&str, &str)]) {
    let mut rest = text;
    while let Some((frame, consumed)) = next_frame(rest) {
        rest = &rest[consumed..];
        if process_frame(frame, capture, secrets) {
            return;
        }
    }
    let tail = rest.trim();
    if !tail.is_empty() {
        process_frame(tail, capture, secrets);
    }
}

/// Return the next complete SSE frame and how many bytes it consumes.
fn next_frame(text: &str) -> Option<(&str, usize)> {
    let lf = text.find("\n\n").map(|index| (index, 2));
    let crlf = text.find("\r\n\r\n").map(|index| (index, 4));
    let (index, width) = match (lf, crlf) {
        (Some(left), Some(right)) => {
            if left.0 <= right.0 {
                left
            } else {
                right
            }
        }
        (Some(left), None) => left,
        (None, Some(right)) => right,
        (None, None) => return None,
    };
    Some((&text[..index], index + width))
}

/// Process one frame. Returns true when the stream reached a terminal frame.
fn process_frame(frame: &str, capture: &mut Capture, secrets: &[(&str, &str)]) -> bool {
    let mut data_lines = Vec::new();
    for line in frame.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.strip_prefix(' ').unwrap_or(rest));
        } else if let Some(event) = line.strip_prefix("event:") {
            if event.trim() == "error" {
                capture.observation.mid_stream_error = true;
            }
        }
    }
    if data_lines.is_empty() {
        return false;
    }
    let data = data_lines.join("\n");
    if data.trim() == "[DONE]" {
        capture.observation.done_seen = true;
        push_event(capture, "done");
        return true;
    }
    let value: Value = match serde_json::from_str(&data) {
        Ok(value) => value,
        Err(_) => {
            capture.observation.malformed_stream = true;
            return false;
        }
    };
    let Some(object) = value.as_object() else {
        capture.observation.malformed_stream = true;
        return false;
    };
    if object.get("error").is_some_and(|error| !error.is_null()) {
        capture.observation.mid_stream_error = true;
        capture.error_body = Some(redact_text(&value.to_string(), secrets));
        push_event(capture, "error");
        return true;
    }
    if let Some(id) = object.get("id").and_then(Value::as_str) {
        if capture.stream_id.is_none() {
            capture.stream_id = Some(id.to_owned());
        }
    }
    if let Some(model) = object.get("model").and_then(Value::as_str) {
        capture.observation.response_model = Some(model.to_owned());
    }

    let mut recognised = false;
    if let Some(choices) = object.get("choices").and_then(Value::as_array) {
        for choice in choices {
            if let Some(delta) = choice.get("delta").and_then(Value::as_object) {
                if delta.get("role").and_then(Value::as_str).is_some() {
                    push_event(capture, "role");
                    recognised = true;
                }
                if delta
                    .get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|content| !content.is_empty())
                {
                    capture.observation.content_deltas += 1;
                    push_event(capture, "content");
                    recognised = true;
                }
                if delta
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .is_some_and(|calls| !calls.is_empty())
                {
                    push_event(capture, "tool_call");
                    recognised = true;
                }
            }
            if choice
                .get("finish_reason")
                .is_some_and(|reason| !reason.is_null())
            {
                push_event(capture, "finish");
                recognised = true;
            }
        }
    }
    if let Some(usage) = object.get("usage").filter(|usage| !usage.is_null()) {
        if let Some(opencode) = usage.get("opencode").and_then(Value::as_object) {
            for key in ["list_cost", "allowance"] {
                if let Some(value) = opencode.get(key) {
                    capture.optional_usage.insert(key.to_owned(), value.clone());
                }
            }
        }
        capture.observation.usage = Some(usage.clone());
        push_event(capture, "usage");
        recognised = true;
    }
    if !recognised {
        push_event(capture, "other");
    }
    false
}

fn push_event(capture: &mut Capture, event: &str) {
    if capture.event_order.len() < MAX_RECORDED_EVENTS {
        capture.event_order.push(event.to_owned());
    } else {
        capture.events_truncated = true;
    }
}

fn is_event_stream(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|value| {
        value
            .split(';')
            .next()
            .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("text/event-stream"))
    })
}

fn header_string(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn selected_headers(headers: &HeaderMap, secrets: &[(&str, &str)]) -> BTreeMap<String, String> {
    let mut selected = BTreeMap::new();
    for name in SELECTED_HEADERS {
        if let Some(value) = headers.get(*name).and_then(|value| value.to_str().ok()) {
            selected.insert((*name).to_owned(), redact_text(value, secrets));
        }
    }
    selected
}

fn origin(uri: &Uri) -> String {
    match (uri.scheme_str(), uri.host(), uri.port_u16()) {
        (Some(scheme), Some(host), Some(port)) => format!("{scheme}://{host}:{port}"),
        (Some(scheme), Some(host), None) => format!("{scheme}://{host}"),
        _ => String::new(),
    }
}
