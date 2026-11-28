//! Design v8 §6.7 terminal classes.
//!
//! Classification is a pure function of mechanical facts captured from the
//! single response. Precedence is fixed so one malformed fact cannot mask
//! another: an affirmative failure stops before any success predicate runs.

use serde_json::Value;

/// The exact terminal class names from design v8 §6.7.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalClass {
    FreeZeroCostStreamingSuccess,
    HttpSuccessMissingUsage,
    MalformedUsage,
    NonSseSuccess,
    MalformedStreamSyntax,
    MissingDone,
    MidStreamErrorFrame,
    Forbidden,
    TooManyRequests,
    PaymentRequired,
    OtherClientError,
    ServerError,
    TransportFailure,
    Timeout,
    LocalHarnessError,
    CostConcern,
    AuthenticityConcern,
}

impl TerminalClass {
    /// The verbatim class name recorded in the evidence record.
    pub fn name(self) -> &'static str {
        match self {
            Self::FreeZeroCostStreamingSuccess => "free zero-cost streaming success",
            Self::HttpSuccessMissingUsage => "HTTP success with missing usage",
            Self::MalformedUsage => "malformed usage",
            Self::NonSseSuccess => "non-SSE success",
            Self::MalformedStreamSyntax => "malformed stream syntax",
            Self::MissingDone => "missing [DONE]",
            Self::MidStreamErrorFrame => "mid-stream error frame",
            Self::Forbidden => "403",
            Self::TooManyRequests => "429",
            Self::PaymentRequired => "402",
            Self::OtherClientError => "other 4xx",
            Self::ServerError => "5xx",
            Self::TransportFailure => "transport failure",
            Self::Timeout => "timeout",
            Self::LocalHarnessError => "local harness error",
            Self::CostConcern => "cost concern",
            Self::AuthenticityConcern => "authenticity concern",
        }
    }
}

/// Facts the reader observed before any llmux classification transform.
#[derive(Clone, Debug, Default)]
pub struct Observation {
    pub http_status: Option<u16>,
    pub http_version: Option<String>,
    pub content_type: Option<String>,
    pub content_deltas: usize,
    pub mid_stream_error: bool,
    pub usage: Option<Value>,
    pub done_seen: bool,
    pub malformed_stream: bool,
    pub response_model: Option<String>,
    pub total_timeout: bool,
    pub idle_timeout: bool,
    pub transport_failure: bool,
    pub local_harness_error: bool,
    pub capture_truncated: bool,
}

pub fn classify(observation: &Observation) -> TerminalClass {
    if observation.local_harness_error {
        return TerminalClass::LocalHarnessError;
    }
    if observation.total_timeout || observation.idle_timeout {
        return TerminalClass::Timeout;
    }
    if observation.transport_failure {
        return TerminalClass::TransportFailure;
    }
    let Some(status) = observation.http_status else {
        return TerminalClass::LocalHarnessError;
    };

    if !(200..300).contains(&status) {
        return match status {
            402 => TerminalClass::PaymentRequired,
            403 => TerminalClass::Forbidden,
            429 => TerminalClass::TooManyRequests,
            400..=499 => TerminalClass::OtherClientError,
            500..=599 => TerminalClass::ServerError,
            // 1xx/3xx were never a probe outcome; do not invent a success.
            _ => TerminalClass::LocalHarnessError,
        };
    }

    if observation.mid_stream_error {
        return TerminalClass::MidStreamErrorFrame;
    }
    if !is_event_stream(observation.content_type.as_deref()) {
        return TerminalClass::NonSseSuccess;
    }
    // A capture that stopped at the cap without the terminator has incomplete
    // evidence; it is a local stop, not a provider verdict.
    if observation.capture_truncated && !observation.done_seen {
        return TerminalClass::LocalHarnessError;
    }
    if observation.malformed_stream {
        return TerminalClass::MalformedStreamSyntax;
    }
    let Some(usage) = observation.usage.as_ref() else {
        return TerminalClass::HttpSuccessMissingUsage;
    };
    match usage_facts(usage) {
        UsageFacts::Malformed => return TerminalClass::MalformedUsage,
        UsageFacts::CostConcern => return TerminalClass::CostConcern,
        UsageFacts::Free => {}
    }
    if let Some(model) = observation.response_model.as_deref() {
        if !names_the_probe_model(model) {
            return TerminalClass::AuthenticityConcern;
        }
    }
    if !observation.done_seen {
        return TerminalClass::MissingDone;
    }
    // §6.7 item 1 requires at least one well-formed content delta. An SSE
    // success without one matches no other named class, so it is a stream
    // shape contract violation rather than a success or a usage failure.
    if observation.content_deltas == 0 {
        return TerminalClass::MalformedStreamSyntax;
    }
    TerminalClass::FreeZeroCostStreamingSuccess
}

/// A provider may qualify the model as `<vendor>/big-pickle`; that still names
/// the requested model. Only a different terminal name is a contradiction.
pub fn names_the_probe_model(model: &str) -> bool {
    model == "big-pickle" || model.rsplit('/').next() == Some("big-pickle")
}

fn is_event_stream(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|value| {
        value
            .split(';')
            .next()
            .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("text/event-stream"))
    })
}

enum UsageFacts {
    Free,
    Malformed,
    CostConcern,
}

fn usage_facts(usage: &Value) -> UsageFacts {
    let Some(object) = usage.as_object() else {
        return UsageFacts::Malformed;
    };
    let Some(cost) = object.get("cost").and_then(Value::as_f64) else {
        return UsageFacts::Malformed;
    };
    let Some(billing) = object
        .get("opencode")
        .and_then(Value::as_object)
        .and_then(|opencode| opencode.get("billing_source"))
        .and_then(Value::as_str)
    else {
        return UsageFacts::Malformed;
    };
    if cost != 0.0 || billing != "free" {
        return UsageFacts::CostConcern;
    }
    UsageFacts::Free
}
