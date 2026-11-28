//! Predicate tests for the design v8 §6.7 terminal classes.
//!
//! Each test changes one fact from a healthy response so a class can only pass
//! for the predicate it names.

use llmux_probe::{classify, Observation, TerminalClass};
use serde_json::{json, Value};

fn usage() -> Value {
    json!({
        "prompt_tokens": 11,
        "completion_tokens": 3,
        "total_tokens": 14,
        "cost": 0,
        "opencode": {
            "billing_source": "free",
            "list_cost": 0,
            "tokens": {"input": 11, "output": 3}
        }
    })
}

fn healthy() -> Observation {
    Observation {
        http_status: Some(200),
        http_version: Some("HTTP/1.1".to_owned()),
        content_type: Some("text/event-stream".to_owned()),
        content_deltas: 2,
        mid_stream_error: false,
        usage: Some(usage()),
        done_seen: true,
        malformed_stream: false,
        response_model: Some("big-pickle".to_owned()),
        total_timeout: false,
        idle_timeout: false,
        transport_failure: false,
        local_harness_error: false,
        capture_truncated: false,
    }
}

#[test]
fn happy_path_is_free_zero_cost_streaming_success() {
    assert_eq!(
        classify(&healthy()),
        TerminalClass::FreeZeroCostStreamingSuccess
    );
    assert_eq!(
        TerminalClass::FreeZeroCostStreamingSuccess.name(),
        "free zero-cost streaming success"
    );
}

#[test]
fn class_names_are_the_design_names() {
    assert_eq!(
        TerminalClass::HttpSuccessMissingUsage.name(),
        "HTTP success with missing usage"
    );
    assert_eq!(TerminalClass::MalformedUsage.name(), "malformed usage");
    assert_eq!(TerminalClass::NonSseSuccess.name(), "non-SSE success");
    assert_eq!(
        TerminalClass::MalformedStreamSyntax.name(),
        "malformed stream syntax"
    );
    assert_eq!(TerminalClass::MissingDone.name(), "missing [DONE]");
    assert_eq!(
        TerminalClass::MidStreamErrorFrame.name(),
        "mid-stream error frame"
    );
    assert_eq!(TerminalClass::Forbidden.name(), "403");
    assert_eq!(TerminalClass::TooManyRequests.name(), "429");
    assert_eq!(TerminalClass::PaymentRequired.name(), "402");
    assert_eq!(TerminalClass::OtherClientError.name(), "other 4xx");
    assert_eq!(TerminalClass::ServerError.name(), "5xx");
    assert_eq!(TerminalClass::TransportFailure.name(), "transport failure");
    assert_eq!(TerminalClass::Timeout.name(), "timeout");
    assert_eq!(
        TerminalClass::LocalHarnessError.name(),
        "local harness error"
    );
    assert_eq!(TerminalClass::CostConcern.name(), "cost concern");
    assert_eq!(
        TerminalClass::AuthenticityConcern.name(),
        "authenticity concern"
    );
}

#[test]
fn missing_cost_is_malformed_usage() {
    let mut o = healthy();
    o.usage = Some(json!({"opencode": {"billing_source": "free"}}));
    assert_eq!(classify(&o), TerminalClass::MalformedUsage);
}

#[test]
fn cost_as_string_is_malformed_usage() {
    let mut o = healthy();
    o.usage = Some(json!({"cost": "0", "opencode": {"billing_source": "free"}}));
    assert_eq!(classify(&o), TerminalClass::MalformedUsage);
}

#[test]
fn non_zero_cost_is_cost_concern() {
    let mut o = healthy();
    o.usage = Some(json!({"cost": 0.0001, "opencode": {"billing_source": "free"}}));
    assert_eq!(classify(&o), TerminalClass::CostConcern);
}

#[test]
fn missing_billing_source_is_malformed_usage() {
    let mut o = healthy();
    o.usage = Some(json!({"cost": 0, "opencode": {}}));
    assert_eq!(classify(&o), TerminalClass::MalformedUsage);
}

#[test]
fn billing_source_as_non_string_is_malformed_usage() {
    let mut o = healthy();
    o.usage = Some(json!({"cost": 0, "opencode": {"billing_source": 1}}));
    assert_eq!(classify(&o), TerminalClass::MalformedUsage);
}

#[test]
fn wrong_billing_source_is_cost_concern() {
    let mut o = healthy();
    o.usage = Some(json!({"cost": 0, "opencode": {"billing_source": "billed"}}));
    assert_eq!(classify(&o), TerminalClass::CostConcern);
}

#[test]
fn missing_usage_is_http_success_with_missing_usage() {
    let mut o = healthy();
    o.usage = None;
    assert_eq!(classify(&o), TerminalClass::HttpSuccessMissingUsage);
}

#[test]
fn usage_not_an_object_is_malformed_usage() {
    let mut o = healthy();
    o.usage = Some(json!([]));
    assert_eq!(classify(&o), TerminalClass::MalformedUsage);
}

#[test]
fn usage_missing_opencode_object_is_malformed_usage() {
    let mut o = healthy();
    o.usage = Some(json!({"cost": 0}));
    assert_eq!(classify(&o), TerminalClass::MalformedUsage);
}

#[test]
fn no_content_delta_is_malformed_stream_syntax() {
    let mut o = healthy();
    o.content_deltas = 0;
    assert_eq!(classify(&o), TerminalClass::MalformedStreamSyntax);
}

#[test]
fn mid_stream_error_frame_is_its_own_class() {
    let mut o = healthy();
    o.mid_stream_error = true;
    assert_eq!(classify(&o), TerminalClass::MidStreamErrorFrame);
}

#[test]
fn missing_done_is_its_own_class() {
    let mut o = healthy();
    o.done_seen = false;
    assert_eq!(classify(&o), TerminalClass::MissingDone);
}

#[test]
fn non_sse_content_type_is_non_sse_success() {
    let mut o = healthy();
    o.content_type = Some("application/json".to_owned());
    assert_eq!(classify(&o), TerminalClass::NonSseSuccess);
}

#[test]
fn missing_content_type_is_non_sse_success() {
    let mut o = healthy();
    o.content_type = None;
    assert_eq!(classify(&o), TerminalClass::NonSseSuccess);
}

#[test]
fn malformed_stream_syntax_is_its_own_class() {
    let mut o = healthy();
    o.malformed_stream = true;
    assert_eq!(classify(&o), TerminalClass::MalformedStreamSyntax);
}

#[test]
fn http_402_is_payment_required() {
    let mut o = healthy();
    o.http_status = Some(402);
    assert_eq!(classify(&o), TerminalClass::PaymentRequired);
}

#[test]
fn http_403_is_forbidden() {
    let mut o = healthy();
    o.http_status = Some(403);
    assert_eq!(classify(&o), TerminalClass::Forbidden);
}

#[test]
fn http_429_is_too_many_requests() {
    let mut o = healthy();
    o.http_status = Some(429);
    assert_eq!(classify(&o), TerminalClass::TooManyRequests);
}

#[test]
fn http_400_is_other_4xx() {
    let mut o = healthy();
    o.http_status = Some(400);
    assert_eq!(classify(&o), TerminalClass::OtherClientError);
}

#[test]
fn http_418_is_other_4xx() {
    let mut o = healthy();
    o.http_status = Some(418);
    assert_eq!(classify(&o), TerminalClass::OtherClientError);
}

#[test]
fn http_500_is_5xx() {
    let mut o = healthy();
    o.http_status = Some(500);
    assert_eq!(classify(&o), TerminalClass::ServerError);
}

#[test]
fn http_503_is_5xx() {
    let mut o = healthy();
    o.http_status = Some(503);
    assert_eq!(classify(&o), TerminalClass::ServerError);
}

#[test]
fn total_timeout_is_timeout() {
    let mut o = healthy();
    o.total_timeout = true;
    assert_eq!(classify(&o), TerminalClass::Timeout);
}

#[test]
fn idle_timeout_is_timeout() {
    let mut o = healthy();
    o.idle_timeout = true;
    assert_eq!(classify(&o), TerminalClass::Timeout);
}

#[test]
fn transport_failure_is_its_own_class() {
    let mut o = healthy();
    o.transport_failure = true;
    assert_eq!(classify(&o), TerminalClass::TransportFailure);
}

#[test]
fn local_harness_error_is_its_own_class() {
    let mut o = healthy();
    o.local_harness_error = true;
    assert_eq!(classify(&o), TerminalClass::LocalHarnessError);
}

#[test]
fn capture_truncated_before_the_terminator_is_a_local_harness_error() {
    let mut o = healthy();
    o.done_seen = false;
    o.capture_truncated = true;
    assert_eq!(classify(&o), TerminalClass::LocalHarnessError);
}

#[test]
fn conflicting_response_model_is_an_authenticity_concern() {
    let mut o = healthy();
    o.response_model = Some("gpt-4o".to_owned());
    assert_eq!(classify(&o), TerminalClass::AuthenticityConcern);
}

#[test]
fn a_qualified_model_name_still_names_big_pickle() {
    let mut o = healthy();
    o.response_model = Some("opencode/big-pickle".to_owned());
    assert_eq!(classify(&o), TerminalClass::FreeZeroCostStreamingSuccess);
}

#[test]
fn absent_response_model_does_not_block_success() {
    let mut o = healthy();
    o.response_model = None;
    assert_eq!(classify(&o), TerminalClass::FreeZeroCostStreamingSuccess);
}

#[test]
fn optional_usage_metadata_is_not_required() {
    let mut o = healthy();
    o.usage = Some(json!({"cost": 0, "opencode": {"billing_source": "free"}}));
    assert_eq!(classify(&o), TerminalClass::FreeZeroCostStreamingSuccess);
}

#[test]
fn a_failed_precondition_never_reports_success() {
    let mut cases = Vec::new();

    let mut o = healthy();
    o.content_deltas = 0;
    cases.push(o.clone());
    let mut o = healthy();
    o.mid_stream_error = true;
    cases.push(o.clone());
    let mut o = healthy();
    o.done_seen = false;
    cases.push(o.clone());
    let mut o = healthy();
    o.usage = Some(json!({"cost": 0, "opencode": {"billing_source": "paid"}}));
    cases.push(o.clone());
    let mut o = healthy();
    o.usage = None;
    cases.push(o.clone());
    let mut o = healthy();
    o.content_type = Some("application/json".to_owned());
    cases.push(o.clone());

    for case in cases {
        assert_ne!(
            classify(&case),
            TerminalClass::FreeZeroCostStreamingSuccess,
            "a response with a failed precondition must not be a success"
        );
    }
}
