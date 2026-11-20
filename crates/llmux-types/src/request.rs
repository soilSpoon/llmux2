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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}
