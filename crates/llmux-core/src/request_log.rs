//! Durable per-request records: one JSON object per line, for nightly analysis.

use crate::GatewayError;
use llmux_types::ErrorKind;
use serde::Serialize;
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::Path,
    sync::Mutex,
    time::SystemTime,
};

#[derive(Serialize)]
pub struct QuotaRemaining {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpm: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub concurrent: Option<u32>,
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
    pub attempts: usize,
    pub latency_ms: u64,
    pub outcome: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota_remaining: Option<QuotaRemaining>,
}

pub struct RequestLog {
    file: Mutex<File>,
}

impl RequestLog {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    pub fn record(&self, record: &RequestRecord<'_>) {
        let Ok(mut file) = self.file.lock() else {
            return;
        };
        // A full log line must be its own write so readers never see a torn record.
        if serde_json::to_writer(&mut *file, record).is_ok() {
            let _ = file.write_all(b"\n");
            let _ = file.flush();
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
        GatewayError::Client(err) => match err.kind() {
            ErrorKind::RateLimited => "rate_limited",
            ErrorKind::QuotaExhausted => "quota_exhausted",
            ErrorKind::Auth => "auth",
            ErrorKind::BadRequest => "bad_request",
            ErrorKind::ContextLength => "context_length",
            ErrorKind::ServerError => "server_error",
            ErrorKind::Timeout => "timeout",
            ErrorKind::ConnectError => "connect_error",
            ErrorKind::StreamBroken => "stream_broken",
        },
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
                attempts: i + 1,
                latency_ms: 42,
                outcome: "routed",
                error_kind: None,
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
            attempts: 4,
            latency_ms: 1000,
            outcome: "error",
            error_kind: Some("rate_limited"),
            quota_remaining: None,
        });
        let parsed: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("requests.jsonl")).unwrap(),
        )
        .unwrap();
        assert_eq!(parsed["error_kind"], "rate_limited");
        assert!(parsed.get("deployment").is_none());
        assert!(parsed.get("quota_remaining").is_none());
    }
}
