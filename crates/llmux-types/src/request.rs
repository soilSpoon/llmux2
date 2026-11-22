use crate::{RequestId, UpstreamError};
use bytes::Bytes;
use futures_core::Stream;
use http::{HeaderMap, StatusCode};
use std::{pin::Pin, sync::Arc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InboundFormat {
    OpenAiChat,
    OpenAiResponses,
    AnthropicMessages,
}

pub struct GatewayRequest {
    pub format: InboundFormat,
    pub model: Arc<str>,
    pub stream: bool,
    pub body: Bytes,
    /// Allowlisted Anthropic protocol headers from a native Messages request.
    pub anthropic_headers: HeaderMap,
    pub est_input_tokens: u32,
    /// Completion tokens the client asked for, reserved against the context window.
    pub max_output_tokens: Option<u32>,
    /// Per-request override of the model's `context_overflow`.
    pub context_overflow: Option<crate::ContextOverflow>,
    pub request_id: RequestId,
    /// Hashed end-user id for upstream abuse attribution. `None` keeps today's behavior.
    pub end_user: Option<Arc<str>>,
    /// Fingerprint of the gateway client key used; never the key itself.
    pub client_key: Option<Arc<str>>,
    /// Number of image inputs found in the inbound body.
    pub image_count: u32,
}

pub type BoxStream<'a, T> = Pin<Box<dyn Stream<Item = T> + Send + 'a>>;

pub enum GatewayResponse {
    Complete {
        status: StatusCode,
        headers: HeaderMap,
        body: Bytes,
    },
    Stream(BoxStream<'static, Result<SseEvent, UpstreamError>>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SseCommitment {
    Keepalive,
    RolePreamble,
    Commit,
}

impl SseEvent {
    pub fn commitment(&self) -> SseCommitment {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&self.data) else {
            return SseCommitment::Commit;
        };
        let Some(choices) = value.get("choices").and_then(serde_json::Value::as_array) else {
            return SseCommitment::Commit;
        };
        if value.get("usage").is_some_and(serde_json::Value::is_object) {
            return SseCommitment::Commit;
        }
        if choices.is_empty() {
            return SseCommitment::Keepalive;
        }
        if choices.iter().all(is_role_only) {
            return SseCommitment::RolePreamble;
        }
        SseCommitment::Commit
    }
}

fn is_role_only(choice: &serde_json::Value) -> bool {
    let Some(choice) = choice.as_object() else {
        return false;
    };
    let Some(delta) = choice.get("delta").and_then(serde_json::Value::as_object) else {
        return false;
    };
    delta.get("role").and_then(serde_json::Value::as_str) == Some("assistant")
        && choice.iter().all(|(key, value)| match key.as_str() {
            "index" | "delta" => true,
            "finish_reason" | "logprobs" => value.is_null(),
            _ => false,
        })
        && delta.iter().all(|(key, value)| match key.as_str() {
            "role" => true,
            "content" => value.is_null() || value.as_str() == Some(""),
            _ => value.is_null(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keepalives_do_not_commit_and_real_chunks_do() {
        let keepalive = SseEvent {
            event: None,
            data: r#"{"id":"chatcmpl-keepalive","choices":[]}"#.into(),
        };
        assert_eq!(keepalive.commitment(), SseCommitment::Keepalive);
        for data in [
            "[DONE]",
            r#"{"id":"chunk-1"}"#,
            "{}",
            r#"{"choices":[{"delta":{"content":"Hi"}}]}"#,
            r#"{"choices":[{"delta":{"role":"assistant","content":"Hi"}}]}"#,
            r#"{"choices":[{"delta":{"role":"assistant","reasoning_content":"hi"}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"id":"call_1"}]}}]}"#,
            r#"{"choices":[{"finish_reason":"stop"}]}"#,
            r#"{"choices":[{"delta":{"role":"assistant"},"finish_reason":"stop"}]}"#,
            r#"{"choices":[{"delta":{"role":"assistant"},"logprobs":{"content":[{"token":"Hi","logprob":-0.1}]}}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":1}}"#,
        ] {
            assert_eq!(
                SseEvent {
                    event: None,
                    data: data.into(),
                }
                .commitment(),
                SseCommitment::Commit,
                "{data}"
            );
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}
