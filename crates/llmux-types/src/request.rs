use crate::{RequestId, UpstreamError};
use bytes::Bytes;
use futures_core::Stream;
use http::{HeaderMap, StatusCode};
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
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
    pub parsed: OnceCell<ChatRequest>,
    pub est_input_tokens: u32,
    /// Completion tokens the client asked for, reserved against the context window.
    pub max_output_tokens: Option<u32>,
    /// Per-request override of the model's `context_overflow`.
    pub context_overflow: Option<crate::ContextOverflow>,
    pub request_id: RequestId,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ChatRequest {
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: Option<MessageContent>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<Value>),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Tool {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

pub type BoxStream<'a, T> = Pin<Box<dyn Stream<Item = T> + Send + 'a>>;

pub enum GatewayResponse {
    Complete {
        status: StatusCode,
        headers: HeaderMap,
        body: Bytes,
        usage: Option<Usage>,
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
