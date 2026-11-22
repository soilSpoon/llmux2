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

impl SseEvent {
    /// Whether forwarding this frame commits the downstream response.
    ///
    /// OpenAI-compat keepalive chunks (`choices: []` with no usage object) do
    /// not. `[DONE]` and every other complete event do, so a role-only or
    /// non-JSON frame still commits the way it does today.
    pub fn commits(&self) -> bool {
        if self.data == "[DONE]" {
            return true;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&self.data) else {
            return true;
        };
        let Some(choices) = value.get("choices").and_then(serde_json::Value::as_array) else {
            return true;
        };
        if !choices.is_empty() {
            return true;
        }
        value.get("usage").is_some_and(serde_json::Value::is_object)
    }
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
        assert!(!keepalive.commits());
        for data in [
            "[DONE]",
            r#"{"id":"chunk-1"}"#,
            "{}",
            r#"{"choices":[{"delta":{"content":"Hi"}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"id":"call_1"}]}}]}"#,
            r#"{"choices":[{"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":1}}"#,
        ] {
            assert!(
                SseEvent {
                    event: None,
                    data: data.into(),
                }
                .commits(),
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
