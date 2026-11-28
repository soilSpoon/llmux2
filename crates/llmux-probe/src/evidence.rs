//! The machine-readable evidence record.

use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

pub const EVIDENCE_SCHEMA: &str = "llmux.probe.evidence.v1";

#[derive(Debug, Clone, Serialize)]
pub struct Evidence {
    pub schema: &'static str,
    pub run_id: String,
    pub target_row: String,
    /// `dry_run`, `execute`, or `refused`.
    pub mode: &'static str,
    /// Verbatim design v8 §6.7 class, absent when nothing was sent.
    pub terminal_class: Option<&'static str>,
    /// Exit status the binary uses: success or dry run only.
    pub exit_ok: bool,
    pub refusal: Option<String>,
    pub endpoint: EndpointEvidence,
    pub request: RequestEvidence,
    pub http: Option<HttpEvidence>,
    pub response: ResponseEvidence,
    pub timings: TimingEvidence,
    pub limits: LimitEvidence,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct EndpointEvidence {
    /// Scheme and authority only; the request path is not recorded.
    pub origin: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RequestEvidence {
    pub method: String,
    pub sent: bool,
    /// sha256 of the exact unredacted outbound body bytes, computed in memory
    /// before transmission.
    pub unredacted_body_sha256: String,
    /// sha256 of the canonical redacted copy in `redacted_body`.
    pub redacted_body_sha256: String,
    pub redacted_body: Value,
    /// Whether the adapter's identity shape set an Authorization header. The
    /// value itself is never recorded.
    pub authorization_present: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct HttpEvidence {
    pub status: u16,
    pub version: String,
    pub content_type: Option<String>,
    pub selected_headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ResponseEvidence {
    pub request_id: Option<String>,
    pub stream_id: Option<String>,
    pub event_order: Vec<String>,
    pub events_truncated: bool,
    pub content_deltas: usize,
    pub usage: Option<Value>,
    /// `usage.opencode.list_cost`, `usage.opencode.allowance`, and other
    /// optional corroborating metadata. Absence never blocks a class.
    pub optional_usage: BTreeMap<String, Value>,
    pub done_seen: bool,
    pub bytes_captured: usize,
    pub capture_truncated: bool,
    pub error_body: Option<String>,
    pub excerpt: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TimingEvidence {
    pub total_ms: u128,
    pub first_byte_ms: Option<u128>,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct LimitEvidence {
    pub total_timeout_ms: u64,
    pub idle_timeout_ms: u64,
    pub capture_cap_bytes: usize,
    pub local_token_cap: u32,
}

impl Evidence {
    pub fn refused(run_id: &str, message: &str) -> Self {
        let limits = LimitEvidence {
            total_timeout_ms: 20_000,
            idle_timeout_ms: 10_000,
            capture_cap_bytes: crate::CAPTURE_CAP_BYTES,
            local_token_cap: crate::DESIGN_TOKEN_CAP,
        };
        Self {
            schema: EVIDENCE_SCHEMA,
            run_id: run_id.to_owned(),
            target_row: String::new(),
            mode: "refused",
            terminal_class: Some("local harness error"),
            exit_ok: false,
            refusal: Some(message.to_owned()),
            endpoint: EndpointEvidence::default(),
            request: RequestEvidence::default(),
            http: None,
            response: ResponseEvidence::default(),
            timings: TimingEvidence::default(),
            limits,
        }
    }
}
