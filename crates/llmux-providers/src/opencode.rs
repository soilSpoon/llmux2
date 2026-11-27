//! OpenCode inference through the pinned CLI-shaped Chat Completions contract.
//!
//! The adapter deliberately reproduces the client shape recorded in
//! `protocol.v2.4.2.json`. That shape is a local protocol constraint, not proof
//! of the provider's free-tier gate; the gate inputs remain a runtime unknown.

use crate::OpenAiCompat;
use async_trait::async_trait;
use bytes::Bytes;
use http::{header, HeaderMap, HeaderValue, Request, Response};
use hyper::body::Incoming;
use llmux_types::{
    provider, AuthMaterial, BuildError, Deployment, GatewayRequest, GatewayResponse, InboundFormat,
    LimitObservation, ProviderAdapter, ProviderKind, Support, UpstreamError,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::LazyLock;

const PROTOCOL_JSON: &str = include_str!("opencode/protocol.v2.4.2.json");
const PUBLIC_AUTHORIZATION: &str = "Bearer public";
const USER_AGENT: &str = "opencode/unknown/2.4.2/opencode";
const TOOL_GUIDANCE_PLACEHOLDER: &str = "${OPENCODE_TOOL_GUIDANCE}";

#[derive(Deserialize)]
struct ProtocolArtifact {
    cli_version: String,
    prompt: String,
    tool_guidance: Vec<String>,
    headers: ArtifactHeaders,
    endpoint: ArtifactEndpoint,
}

#[derive(Deserialize)]
struct ArtifactHeaders {
    identity: Vec<String>,
    user_agent_pattern: String,
    anonymous_authorization: String,
}

#[derive(Deserialize)]
struct ArtifactEndpoint {
    base_url: String,
    chat_completions_path: String,
}

static ARTIFACT: LazyLock<ProtocolArtifact> = LazyLock::new(|| {
    serde_json::from_str(PROTOCOL_JSON).expect("embedded opencode protocol artifact must parse")
});

pub struct OpenCode(OpenAiCompat);

impl OpenCode {
    pub fn new() -> Self {
        debug_assert_eq!(ARTIFACT.cli_version, "v2.4.2");
        debug_assert_eq!(ARTIFACT.endpoint.base_url, provider::OPENCODE_BASE_URL);
        debug_assert_eq!(ARTIFACT.endpoint.chat_completions_path, "/chat/completions");
        debug_assert!(ARTIFACT
            .headers
            .identity
            .iter()
            .any(|name| name == "x-opencode-client"));
        debug_assert_eq!(
            ARTIFACT.headers.user_agent_pattern,
            "^opencode/[^/]+/[^/]+/[^/]+$"
        );
        debug_assert_eq!(
            ARTIFACT.headers.anonymous_authorization,
            PUBLIC_AUTHORIZATION
        );
        Self(OpenAiCompat::new(provider::OPENCODE))
    }
}

impl Default for OpenCode {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ProviderAdapter for OpenCode {
    fn kind(&self) -> &ProviderKind {
        self.0.kind()
    }

    fn supports(&self, inbound: InboundFormat) -> Support {
        match inbound {
            InboundFormat::OpenAiChat => Support::Passthrough,
            InboundFormat::OpenAiResponses | InboundFormat::AnthropicMessages => Support::No,
        }
    }

    async fn build_request(
        &self,
        req: &GatewayRequest,
        dep: &Deployment,
        _auth: &AuthMaterial,
    ) -> Result<Request<Bytes>, BuildError> {
        if req.format != InboundFormat::OpenAiChat {
            return Err(BuildError::UnsupportedFormat);
        }
        let mut request = self.0.build_request(req, dep, &AuthMaterial::None).await?;
        let (body, identity) = rewrite_body(request.body(), req)?;
        *request.body_mut() = body;
        apply_identity(request.headers_mut(), &identity);
        Ok(request)
    }

    async fn parse_response(
        &self,
        req: &GatewayRequest,
        dep: &Deployment,
        resp: Response<Incoming>,
    ) -> Result<GatewayResponse, UpstreamError> {
        let (parts, body) = resp.into_parts();
        if parts.status == http::StatusCode::FORBIDDEN {
            // The free-tier client gate is not a credential refresh failure.
            // Preserve the provider's body so the caller can see the refusal.
            let body = crate::openai_compat::collect_error(body).await;
            return Err(UpstreamError::BadRequest {
                status: parts.status,
                body,
            });
        }
        self.0
            .parse_response(req, dep, Response::from_parts(parts, body))
            .await
    }

    fn observe_limits(&self, headers: &HeaderMap) -> Option<LimitObservation> {
        self.0.observe_limits(headers)
    }
}

struct Identity {
    project: String,
    session: String,
}

fn apply_identity(headers: &mut HeaderMap, identity: &Identity) {
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static(PUBLIC_AUTHORIZATION),
    );
    headers.insert(header::USER_AGENT, HeaderValue::from_static(USER_AGENT));
    headers.insert("x-opencode-client", HeaderValue::from_static("opencode"));
    headers.insert(
        "x-opencode-project",
        HeaderValue::from_str(&identity.project).expect("derived project id is valid"),
    );
    for name in ["x-opencode-session", "x-session-affinity", "x-session-id"] {
        headers.insert(
            name,
            HeaderValue::from_str(&identity.session).expect("derived session id is valid"),
        );
    }
    headers.insert(
        "x-opencode-session-id",
        HeaderValue::from_str(&identity.session).expect("derived session id is valid"),
    );
}

fn rendered_prompt() -> String {
    ARTIFACT.prompt.replace(
        TOOL_GUIDANCE_PLACEHOLDER,
        &ARTIFACT.tool_guidance.join("\n"),
    )
}

fn rewrite_body(body: &Bytes, req: &GatewayRequest) -> Result<(Bytes, Identity), BuildError> {
    let mut value: Value = serde_json::from_slice(body).map_err(|_| BuildError::InvalidBody)?;
    let identity = identity_for(req, &value);
    let object = value.as_object_mut().ok_or(BuildError::InvalidBody)?;
    let messages = object
        .remove("messages")
        .and_then(|value| value.as_array().cloned())
        .ok_or(BuildError::InvalidBody)?;
    let mut system_parts = Vec::new();
    let mut lowered = Vec::new();
    for message in messages {
        if message.get("role").and_then(Value::as_str) == Some("system") {
            system_parts.push(content_text(message.get("content").unwrap_or(&Value::Null)));
        } else {
            lowered.push(message);
        }
    }
    let caller_system = system_parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let system = if caller_system.is_empty() {
        rendered_prompt()
    } else {
        format!("{}\n\n{caller_system}", rendered_prompt())
    };
    lowered.insert(0, json!({"role":"system","content":system}));
    let has_tool_history = lowered.iter().any(|message| {
        message.get("role").and_then(Value::as_str) == Some("tool")
            || message
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|calls| !calls.is_empty())
    });
    object.insert("messages".into(), Value::Array(lowered));

    if let Some(max_tokens) = object.remove("max_tokens") {
        object.entry("max_completion_tokens").or_insert(max_tokens);
    }
    object.remove("store");
    object.remove("prompt_cache_key");
    object.entry("stream_options").or_insert_with(|| json!({}));
    if let Some(options) = object
        .get_mut("stream_options")
        .and_then(Value::as_object_mut)
    {
        options.insert("include_usage".into(), Value::Bool(true));
    }
    if let Some(tools) = object.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools.iter_mut() {
            if let Some(function) = tool.get_mut("function").and_then(Value::as_object_mut) {
                function.insert("strict".into(), Value::Bool(false));
            }
        }
        if tools.is_empty() && !has_tool_history {
            object.remove("tools");
        }
    }

    let body = serde_json::to_vec(&value)
        .map(Bytes::from)
        .map_err(|_| BuildError::InvalidBody)?;
    Ok((body, identity))
}

fn content_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| {
                if part.get("type").and_then(Value::as_str) == Some("text") {
                    part.get("text").and_then(Value::as_str)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(""),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn identity_for(req: &GatewayRequest, body: &Value) -> Identity {
    let anchor = body
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .map(|message| content_text(message.get("content").unwrap_or(&Value::Null)))
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| req.model.to_string());
    let key = req.client_key.as_deref().unwrap_or("anonymous");
    let session = digest_hex(&[key, &req.model, &anchor, "session"], 64);
    let project = digest_hex(&[key, &req.model, "project"], 32);
    Identity {
        project: format!("prj_{project}"),
        session: format!("ses_{session}"),
    }
}

fn digest_hex(parts: &[&str], len: usize) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()[..len]
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_artifact_is_the_pinned_contract() {
        assert_eq!(ARTIFACT.cli_version, "v2.4.2");
        assert_eq!(ARTIFACT.endpoint.base_url, provider::OPENCODE_BASE_URL);
        assert_eq!(ARTIFACT.endpoint.chat_completions_path, "/chat/completions");
        assert_eq!(
            ARTIFACT.headers.identity,
            [
                "x-opencode-client",
                "x-opencode-project",
                "x-opencode-session",
                "x-opencode-session-id",
                "x-session-affinity",
                "x-session-id",
                "user-agent"
            ]
        );
        assert_eq!(
            ARTIFACT.headers.user_agent_pattern,
            "^opencode/[^/]+/[^/]+/[^/]+$"
        );
        assert_eq!(
            ARTIFACT.headers.anonymous_authorization,
            PUBLIC_AUTHORIZATION
        );
        let digest = Sha256::digest(PROTOCOL_JSON.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(
            digest,
            "6cf05f4c7aa25e01360d770785ff1880c9bbf0bf92bd97c67af64a2213d87c76"
        );
        assert!(rendered_prompt()
            .starts_with("You are an AI agent running in OpenCode, a coding agent harness."));
        assert!(!rendered_prompt().contains(TOOL_GUIDANCE_PLACEHOLDER));
    }
}
