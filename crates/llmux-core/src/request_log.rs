//! Durable per-request records: one JSON object per line, for nightly analysis.

use crate::GatewayError;
use futures_util::Stream;
use llmux_types::{BoxStream, ErrorKind, SseEvent, UpstreamError};
use serde::Serialize;
use std::{
    fs::OpenOptions,
    io::{self, Write},
    path::Path,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, SyncSender},
        Arc, Barrier,
    },
    task::{Context, Poll},
    thread::{self, JoinHandle},
    time::SystemTime,
};

/// Records waiting for the writer thread; a stalled filesystem drops rather
/// than blocks the runtime.
const QUEUE_CAPACITY: usize = 4096;

#[derive(Clone, Serialize)]
pub struct QuotaRemaining {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpm: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub concurrent: Option<u32>,
}

/// Provider-reported token accounting. Every field is optional: providers do
/// not all report the same counters, and a missing counter is not a zero.
#[derive(Clone, Default, Serialize)]
pub struct TokenUsage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
}

impl TokenUsage {
    fn merge(&mut self, newer: TokenUsage) {
        for (current, newer) in [
            (&mut self.input_tokens, newer.input_tokens),
            (&mut self.output_tokens, newer.output_tokens),
            (&mut self.cached_input_tokens, newer.cached_input_tokens),
            (
                &mut self.cache_creation_input_tokens,
                newer.cache_creation_input_tokens,
            ),
        ] {
            if newer.is_some() {
                *current = newer;
            }
        }
    }
}

fn number(value: &serde_json::Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| value.get(*key)?.as_u64())
}

fn parse_usage_object(usage: &serde_json::Value) -> Option<TokenUsage> {
    if !usage.is_object() {
        return None;
    }
    let parsed = TokenUsage {
        input_tokens: number(usage, &["input_tokens", "prompt_tokens"]),
        output_tokens: number(usage, &["output_tokens", "completion_tokens"]),
        cached_input_tokens: number(usage, &["cache_read_input_tokens"])
            .or_else(|| {
                usage
                    .get("prompt_tokens_details")
                    .and_then(|details| details.get("cached_tokens"))
                    .and_then(serde_json::Value::as_u64)
            })
            .or_else(|| {
                usage
                    .get("input_tokens_details")
                    .and_then(|details| details.get("cached_tokens"))
                    .and_then(serde_json::Value::as_u64)
            }),
        cache_creation_input_tokens: number(usage, &["cache_creation_input_tokens"]),
    };
    (parsed.input_tokens.is_some()
        || parsed.output_tokens.is_some()
        || parsed.cached_input_tokens.is_some()
        || parsed.cache_creation_input_tokens.is_some())
    .then_some(parsed)
}

/// Extracts token accounting without retaining prompts or response content.
/// Handles the OpenAI-compatible usage object directly, Anthropic's nested
/// `message.usage` form used by `message_start` stream events, and the
/// Responses API's nested `response.usage` completion form.
pub fn usage_from_json(value: &serde_json::Value) -> Option<TokenUsage> {
    value
        .get("usage")
        .and_then(parse_usage_object)
        .or_else(|| {
            value
                .get("message")
                .and_then(|message| message.get("usage"))
                .and_then(parse_usage_object)
        })
        .or_else(|| {
            value
                .get("response")
                .and_then(|response| response.get("usage"))
                .and_then(parse_usage_object)
        })
}

pub fn usage_from_body(body: &[u8]) -> Option<TokenUsage> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .as_ref()
        .and_then(usage_from_json)
}

/// Owned metadata needed to write a stream's request record after the stream
/// finishes. It intentionally contains only routing metadata and numeric usage.
pub struct PendingStreamRecord {
    pub log: Arc<RequestLog>,
    pub ts_ms: u64,
    pub request_id: Box<str>,
    pub model: Box<str>,
    pub deployment: Option<Box<str>>,
    pub provider: Option<Box<str>>,
    pub client_key: Option<Box<str>>,
    pub image_count: u32,
    pub upstream_request_id: Option<Box<str>>,
    pub attempts: usize,
    pub legs: Vec<AttemptLeg>,
    pub latency_ms: u64,
    pub quota_remaining: Option<QuotaRemaining>,
}

impl PendingStreamRecord {
    fn finish(self, usage: Option<&TokenUsage>) {
        self.log.record(&RequestRecord {
            ts_ms: self.ts_ms,
            request_id: &self.request_id,
            model: &self.model,
            deployment: self.deployment.as_deref(),
            provider: self.provider.as_deref(),
            client_key: self.client_key.as_deref(),
            image_count: self.image_count,
            upstream_request_id: self.upstream_request_id.as_deref(),
            attempts: self.attempts,
            legs: self.legs,
            latency_ms: self.latency_ms,
            outcome: "routed",
            error_kind: None,
            quota_remaining: self.quota_remaining,
            usage,
        });
    }
}

/// Pass-through stream that captures numeric usage fields and writes the
/// request record on end-of-stream, the first upstream stream error, or drop.
pub struct UsageCaptureStream {
    inner: BoxStream<'static, Result<SseEvent, UpstreamError>>,
    pending: Option<PendingStreamRecord>,
    usage: Option<TokenUsage>,
}

impl UsageCaptureStream {
    pub fn new(
        inner: BoxStream<'static, Result<SseEvent, UpstreamError>>,
        pending: PendingStreamRecord,
    ) -> Self {
        Self {
            inner,
            pending: Some(pending),
            usage: None,
        }
    }

    fn finish(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.finish(self.usage.as_ref());
        }
    }
}

impl Stream for UsageCaptureStream {
    type Item = Result<SseEvent, UpstreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let poll = self.inner.as_mut().poll_next(cx);
        match &poll {
            Poll::Ready(Some(Ok(event))) => {
                if event.data.contains("\"usage\"") {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&event.data) {
                        if let Some(usage) = usage_from_json(&value) {
                            self.usage
                                .get_or_insert_with(TokenUsage::default)
                                .merge(usage);
                        }
                    }
                }
            }
            Poll::Ready(Some(Err(_))) | Poll::Ready(None) => self.finish(),
            Poll::Pending => {}
        }
        poll
    }
}

impl Drop for UsageCaptureStream {
    fn drop(&mut self) {
        self.finish();
    }
}

/// One upstream send: the deployment and provider it hit, and `routed` when it
/// served the response or the failure kind when it did not.
#[derive(Serialize)]
pub struct AttemptLeg {
    pub deployment: Box<str>,
    pub provider: Box<str>,
    pub outcome: &'static str,
}

#[derive(Serialize)]
pub struct RequestRecord<'a> {
    pub ts_ms: u64,
    pub request_id: &'a str,
    pub model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployment: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<&'a str>,
    /// Fingerprint of the gateway client key; never the key itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_key: Option<&'a str>,
    /// Image inputs in the inbound request body.
    pub image_count: u32,
    /// Provider-side request id (e.g. Anthropic `msg_`/`req_`, OpenAI `x-request-id`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_request_id: Option<&'a str>,
    pub attempts: usize,
    /// Every upstream send, in order — the legs a failover otherwise hides.
    pub legs: Vec<AttemptLeg>,
    pub latency_ms: u64,
    pub outcome: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota_remaining: Option<QuotaRemaining>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<&'a TokenUsage>,
}

pub struct RequestLog {
    /// `None` once the queue is disconnected for drain-on-drop.
    tx: Option<SyncSender<Vec<u8>>>,
    dropped: AtomicU64,
    writer: Option<JoinHandle<()>>,
}

impl RequestLog {
    /// Opens the file and starts its writer thread. The caller's runtime never
    /// touches the filesystem after this.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self::spawn_writer(file, None))
    }

    /// Test seam: the writer parks on `gate` before draining, so tests can fill
    /// the bounded queue deterministically.
    #[cfg(test)]
    fn open_gated(path: &Path, gate: Arc<Barrier>) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self::spawn_writer(file, Some(gate)))
    }

    fn spawn_writer(mut file: std::fs::File, gate: Option<Arc<Barrier>>) -> Self {
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(QUEUE_CAPACITY);
        let writer = thread::spawn(move || {
            if let Some(gate) = gate {
                gate.wait();
            }
            while let Ok(line) = rx.recv() {
                if let Err(err) = file.write_all(&line).and_then(|()| file.flush()) {
                    eprintln!("llmux: request log write failed: {err}");
                }
            }
        });
        Self {
            tx: Some(tx),
            dropped: AtomicU64::new(0),
            writer: Some(writer),
        }
    }

    /// Serializes the record and queues it for the writer thread; never blocks
    /// on filesystem I/O. A full queue drops the record — logging is
    /// best-effort and never worth stalling routing for — with a rate-limited
    /// warning so the operator can see the loss.
    pub fn record(&self, record: &RequestRecord<'_>) {
        let Ok(mut line) = serde_json::to_vec(record) else {
            return;
        };
        line.push(b'\n');
        let Some(tx) = &self.tx else {
            return;
        };
        if tx.try_send(line).is_err() {
            let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if dropped == 1 || dropped.is_multiple_of(256) {
                eprintln!("llmux: request log unavailable; dropped {dropped} records");
            }
        }
    }
}

impl Drop for RequestLog {
    fn drop(&mut self) {
        // Disconnect first so the writer drains queued records and exits, then
        // join it: a dropped log must leave everything queued on disk.
        drop(self.tx.take());
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

/// Stable snake_case names for nightly analysis: they double as the API's
/// error kind vocabulary.
pub fn error_kind(error: &GatewayError) -> &'static str {
    match error {
        GatewayError::UnknownModel => "unknown_model",
        GatewayError::UnsupportedStream => "unsupported_stream",
        GatewayError::InvalidRequest => "invalid_request",
        GatewayError::ContextLengthExceeded { .. } => "context_length_exceeded",
        GatewayError::AllCandidatesExhausted { .. } => "all_candidates_exhausted",
        GatewayError::Internal => "internal",
        GatewayError::Client(err) => upstream_error_kind(err),
    }
}

/// The same vocabulary for an upstream error seen mid-attempt.
pub fn upstream_error_kind(error: &llmux_types::UpstreamError) -> &'static str {
    match error.kind() {
        ErrorKind::RateLimited => "rate_limited",
        ErrorKind::QuotaExhausted => "quota_exhausted",
        ErrorKind::NotProvisioned => "not_provisioned",
        ErrorKind::Auth => "auth",
        ErrorKind::BadRequest => "bad_request",
        ErrorKind::ContextLength => "context_length",
        ErrorKind::ServerError => "server_error",
        ErrorKind::Timeout => "timeout",
        ErrorKind::ConnectError => "connect_error",
        ErrorKind::StreamBroken => "stream_broken",
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_append_one_json_line_each() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("requests.jsonl");
        let log = RequestLog::open(&path).unwrap();
        for i in 0..3 {
            log.record(&RequestRecord {
                ts_ms: now_ms(),
                request_id: "req-1",
                model: "shared",
                deployment: Some("dep"),
                provider: Some("openai_compat"),
                client_key: Some("key_ab12"),
                image_count: 2,
                upstream_request_id: Some("msg_01Xy"),
                attempts: i + 1,
                legs: vec![
                    AttemptLeg {
                        deployment: "dep-a".into(),
                        provider: "openai_compat".into(),
                        outcome: "rate_limited",
                    },
                    AttemptLeg {
                        deployment: "dep".into(),
                        provider: "openai_compat".into(),
                        outcome: "routed",
                    },
                ],
                latency_ms: 42,
                outcome: "routed",
                error_kind: None,
                usage: None,
                quota_remaining: Some(QuotaRemaining {
                    rpm: Some(9),
                    concurrent: None,
                }),
            });
        }
        drop(log);
        let lines: Vec<_> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(lines.len(), 3);
        for (i, line) in lines.iter().enumerate() {
            let parsed: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(parsed["attempts"], i + 1);
            assert_eq!(parsed["outcome"], "routed");
            assert_eq!(parsed["quota_remaining"]["rpm"], 9);
            assert!(parsed.get("error_kind").is_none());
            assert_eq!(
                parsed["legs"],
                serde_json::json!([
                    {"deployment": "dep-a", "provider": "openai_compat", "outcome": "rate_limited"},
                    {"deployment": "dep", "provider": "openai_compat", "outcome": "routed"},
                ])
            );
        }
    }

    #[test]
    fn failure_records_keep_nullable_fields_absent() {
        let dir = tempfile::tempdir().unwrap();
        let log = RequestLog::open(&dir.path().join("requests.jsonl")).unwrap();
        log.record(&RequestRecord {
            ts_ms: now_ms(),
            request_id: "req-2",
            model: "shared",
            deployment: None,
            provider: None,
            client_key: None,
            image_count: 0,
            upstream_request_id: None,
            attempts: 4,
            legs: Vec::new(),
            latency_ms: 1000,
            outcome: "error",
            error_kind: Some("rate_limited"),
            usage: None,
            quota_remaining: None,
        });
        drop(log);
        let parsed: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("requests.jsonl")).unwrap(),
        )
        .unwrap();
        assert_eq!(parsed["error_kind"], "rate_limited");
        assert!(parsed.get("deployment").is_none());
        assert!(parsed.get("quota_remaining").is_none());
        assert!(parsed.get("client_key").is_none());
        assert!(parsed.get("upstream_request_id").is_none());
        assert_eq!(parsed["image_count"], 0);
    }

    #[test]
    fn saturated_queue_drops_records_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("requests.jsonl");
        let gate = Arc::new(Barrier::new(2));
        let log = Arc::new(RequestLog::open_gated(&path, gate.clone()).unwrap());
        let record = |request_id: &'static str| RequestRecord {
            ts_ms: now_ms(),
            request_id,
            model: "shared",
            deployment: None,
            provider: None,
            client_key: None,
            image_count: 0,
            upstream_request_id: None,
            attempts: 1,
            legs: Vec::new(),
            latency_ms: 1,
            outcome: "routed",
            error_kind: None,
            usage: None,
            quota_remaining: None,
        };
        // The writer is parked on the barrier, so these fill the bounded queue.
        for _ in 0..QUEUE_CAPACITY {
            log.record(&record("accepted"));
        }
        assert_eq!(log.dropped.load(Ordering::Relaxed), 0);
        // One more submission must return while the writer is still parked.
        let (done_tx, done_rx) = mpsc::channel();
        let overflow = thread::spawn({
            let log = Arc::clone(&log);
            move || {
                log.record(&record("overflow"));
                let _ = done_tx.send(());
            }
        });
        done_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("record() blocked on a full queue");
        overflow.join().unwrap();
        assert_eq!(log.dropped.load(Ordering::Relaxed), 1);
        gate.wait();
        drop(log);
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body.lines().count(), QUEUE_CAPACITY);
        assert!(!body.contains("overflow"));
    }
}
