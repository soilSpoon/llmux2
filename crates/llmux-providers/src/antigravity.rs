//! Antigravity Cloud Code client. The upstream model id is the requested id;
//! this is not the reviewed Gemini API-key rewrite.

use crate::{
    messages,
    openai_compat::{apply_auth, bad_gateway, classify_error, collect_error, endpoint_uri},
    sse,
};
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt;
use http::{header, HeaderMap, Request, Response, StatusCode};
use hyper::body::Incoming;
use llmux_types::{
    header_request_id, provider, AuthMaterial, BoxStream, BuildError, Deployment, GatewayRequest,
    GatewayResponse, InboundFormat, ProviderAdapter, ProviderKind, SseEvent, Support,
    UpstreamError,
};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::time::SystemTime;

const PROJECT: &str = "x-llmux-project";
const PATH: &str = "/v1internal:streamGenerateContent?alt=sse";
// The upstream gates on the advertised client version: `antigravity/cli/1.0.0`
// gets a 200 whose only content is a "CLI no longer supported" notice. This is
// the UA the IDE-profile connections send (verified 200 on 2026-11-25); bump it
// when the real client line moves.
const USER_AGENT: &str = "antigravity/2.5.5 linux/x64 google-api-nodejs-client/10.3.0";
const SYSTEM: &str = "You are Antigravity, a powerful agentic AI coding assistant designed by the Google Deepmind team working on Advanced Agentic Coding.";

pub struct Antigravity(ProviderKind);

impl Antigravity {
    pub fn new() -> Self {
        Self(ProviderKind(provider::ANTIGRAVITY.into()))
    }
}

impl Default for Antigravity {
    fn default() -> Self {
        Self::new()
    }
}

struct Collected {
    text: String,
    tool_calls: Vec<Value>,
    finish: String,
    usage: Option<Value>,
}

impl Collected {
    fn new() -> Self {
        Self {
            text: String::new(),
            tool_calls: Vec::new(),
            finish: "stop".into(),
            usage: None,
        }
    }
}

fn text_of(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_owned();
    }
    value
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

fn chat_value(body: &[u8], format: InboundFormat, model: &str) -> Result<Value, BuildError> {
    match format {
        InboundFormat::OpenAiChat => {
            serde_json::from_slice(body).map_err(|_| BuildError::InvalidBody)
        }
        InboundFormat::AnthropicMessages => messages::chat_request(body, model),
        InboundFormat::OpenAiResponses => {
            let value: Value = serde_json::from_slice(body).map_err(|_| BuildError::InvalidBody)?;
            let mut messages = Vec::new();
            if let Some(instructions) = value["instructions"].as_str() {
                messages.push(json!({"role":"system","content":instructions}));
            }
            match &value["input"] {
                Value::String(text) => messages.push(json!({"role":"user","content":text})),
                Value::Array(items) => {
                    for item in items {
                        if item["role"].is_string() {
                            messages.push(item.clone());
                        }
                    }
                }
                _ => return Err(BuildError::InvalidBody),
            }
            Ok(json!({"messages":messages}))
        }
    }
}

fn envelope(
    chat: &Value,
    model: &str,
    project: &str,
    request_id: &str,
) -> Result<Value, BuildError> {
    let messages = chat["messages"].as_array().ok_or(BuildError::InvalidBody)?;
    let mut system = Vec::new();
    let mut contents = Vec::new();
    let mut names = Vec::new();
    for message in messages {
        let role = message["role"].as_str().ok_or(BuildError::InvalidBody)?;
        match role {
            "system" | "developer" => system.push(text_of(&message["content"])),
            "user" => contents
                .push(json!({"role":"user","parts":[{"text":text_of(&message["content"])}]})),
            "assistant" => {
                let mut parts = Vec::new();
                let text = text_of(&message["content"]);
                if !text.is_empty() {
                    parts.push(json!({"text":text}));
                }
                if let Some(calls) = message["tool_calls"].as_array() {
                    for call in calls {
                        let name = call["function"]["name"]
                            .as_str()
                            .ok_or(BuildError::InvalidBody)?;
                        names.push(name.to_owned());
                        let args = call["function"]["arguments"].as_str().unwrap_or("{}");
                        let args: Value =
                            serde_json::from_str(args).unwrap_or_else(|_| json!({"input":args}));
                        parts.push(json!({"functionCall":{"name":name,"args":args}}));
                    }
                }
                if !parts.is_empty() {
                    contents.push(json!({"role":"model","parts":parts}));
                }
            }
            "tool" => {
                let name = names.last().map(String::as_str).unwrap_or("tool");
                contents.push(json!({"role":"user","parts":[{"functionResponse":{"name":name,"response":{"result":text_of(&message["content"])}}}]}));
            }
            _ => return Err(BuildError::InvalidBody),
        }
    }
    if contents
        .last()
        .is_some_and(|content| content["role"] == "model")
        && contents.len() > 1
        && contents.last().is_some_and(|content| {
            !content["parts"]
                .as_array()
                .is_some_and(|parts| parts.iter().any(|part| part.get("functionCall").is_some()))
        })
    {
        contents.pop();
    }
    if !system.is_empty() {
        let prefix = system.join("\n\n");
        if let Some(first) = contents.first_mut() {
            if let Some(parts) = first["parts"].as_array_mut() {
                parts.insert(0, json!({"text":prefix}));
            }
        } else {
            contents.push(json!({"role":"user","parts":[{"text":prefix}]}));
        }
    }
    if contents.is_empty() {
        return Err(BuildError::InvalidBody);
    }
    let mut request = Map::new();
    request.insert("sessionId".into(), json!(format!("sess-{request_id}")));
    request.insert("contents".into(), json!(contents));
    request.insert(
        "systemInstruction".into(),
        json!({"role":"system","parts":[{"text":SYSTEM}]}),
    );
    let mut generation = Map::new();
    if let Some(max) = chat["max_completion_tokens"]
        .as_u64()
        .or_else(|| chat["max_tokens"].as_u64())
    {
        generation.insert("maxOutputTokens".into(), json!(max));
    }
    if chat.get("temperature").is_some() {
        generation.insert("temperature".into(), chat["temperature"].clone());
    }
    if !generation.is_empty() {
        request.insert("generationConfig".into(), Value::Object(generation));
    }
    if let Some(tools) = chat["tools"].as_array() {
        let mut declarations = Vec::new();
        for tool in tools {
            if tool["type"] != "function" {
                continue;
            }
            let function = &tool["function"];
            let mut declaration = json!({"name":function["name"]});
            if function.get("description").is_some() {
                declaration["description"] = function["description"].clone();
            }
            if function.get("parameters").is_some() {
                declaration["parameters"] = function["parameters"].clone();
            }
            declarations.push(declaration);
        }
        if !declarations.is_empty() {
            request.insert(
                "tools".into(),
                json!([{"functionDeclarations":declarations}]),
            );
            request.insert(
                "toolConfig".into(),
                json!({"functionCallingConfig":{"mode":"VALIDATED"}}),
            );
        }
    }
    Ok(json!({
        "project": project,
        "requestId": format!("agent-{request_id}"),
        "request": request,
        "model": model,
        "userAgent": "antigravity",
        "requestType": "agent",
    }))
}

fn apply_payload(collected: &mut Collected, payload: &str) {
    if payload.is_empty() || payload == "[DONE]" {
        return;
    }
    let Ok(parsed) = serde_json::from_str::<Value>(payload) else {
        return;
    };
    if let Some(markdown) = parsed["markdown"]
        .as_str()
        .or_else(|| parsed["response"]["markdown"].as_str())
    {
        collected.text.push_str(markdown);
    }
    let candidate = &parsed["response"]["candidates"][0];
    if let Some(parts) = candidate["content"]["parts"].as_array() {
        for part in parts {
            if let Some(name) = part["functionCall"]["name"].as_str() {
                let args = part["functionCall"]["args"].clone();
                collected.tool_calls.push(json!({
                    "id": format!("{name}-{}", collected.tool_calls.len()),
                    "type": "function",
                    "function": {"name": name, "arguments": args.to_string()},
                }));
                collected.finish = "tool_calls".into();
                continue;
            }
            if part["thought"] != true {
                if let Some(text) = part["text"].as_str() {
                    collected.text.push_str(text);
                }
            }
        }
    }
    if collected.tool_calls.is_empty() {
        if let Some(reason) = candidate["finishReason"].as_str() {
            collected.finish = match reason.to_ascii_lowercase().as_str() {
                "stop" | "max_tokens" | "length" => {
                    if reason.eq_ignore_ascii_case("length") {
                        "length"
                    } else {
                        "stop"
                    }
                }
                "tool_calls" | "tool_use" => "tool_calls",
                _ => "stop",
            }
            .into();
        }
    }
    if parsed["response"]["usageMetadata"].is_object() {
        let usage = &parsed["response"]["usageMetadata"];
        collected.usage = Some(json!({
            "prompt_tokens": usage["promptTokenCount"].as_u64().unwrap_or(0),
            "completion_tokens": usage["candidatesTokenCount"].as_u64().unwrap_or(0),
            "total_tokens": usage["totalTokenCount"].as_u64().unwrap_or(0),
        }));
    }
}

fn completion(model: &str, collected: &Collected) -> Value {
    let mut message = json!({"role":"assistant","content":collected.text});
    if !collected.tool_calls.is_empty() {
        message["tool_calls"] = json!(collected.tool_calls);
    }
    let mut body = json!({
        "id": "chatcmpl-antigravity",
        "object": "chat.completion",
        "created": 0,
        "model": model,
        "choices": [{"index":0,"message":message,"finish_reason":collected.finish}],
    });
    if let Some(usage) = &collected.usage {
        body["usage"] = usage.clone();
    }
    body
}

fn chunk(model: &str, delta: Value, finish: Option<&str>) -> SseEvent {
    SseEvent {
        event: None,
        data: json!({
            "id": "chatcmpl-antigravity",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": model,
            "choices": [{"index":0,"delta":delta,"finish_reason":finish}],
        })
        .to_string(),
    }
}

fn chat_stream(
    model: String,
    input: BoxStream<'static, Result<SseEvent, UpstreamError>>,
) -> BoxStream<'static, Result<SseEvent, UpstreamError>> {
    Box::pin(async_stream::try_stream! {
        let mut collected = Collected::new();
        let mut started = false;
        let mut stream = input;
        while let Some(event) = stream.next().await {
            let event = event?;
            let before = collected.text.len();
            let tools = collected.tool_calls.len();
            apply_payload(&mut collected, &event.data);
            if !started {
                yield chunk(&model, json!({"role":"assistant"}), None);
                started = true;
            }
            if collected.text.len() > before {
                let delta = &collected.text[before..];
                yield chunk(&model, json!({"content":delta}), None);
            }
            for call in &collected.tool_calls[tools..] {
                yield chunk(&model, json!({"tool_calls":[call]}), None);
            }
        }
        yield chunk(&model, json!({}), Some(collected.finish.as_str()));
        yield SseEvent { event: None, data: "[DONE]".into() };
    })
}

#[async_trait]
impl ProviderAdapter for Antigravity {
    fn kind(&self) -> &ProviderKind {
        &self.0
    }

    /// The complete body is synthesized with a fixed placeholder id; only
    /// upstream response headers carry a real request id.
    fn upstream_request_id(&self, headers: &HeaderMap, _body: Option<&Bytes>) -> Option<Arc<str>> {
        header_request_id(headers)
    }

    fn supports(&self, inbound: InboundFormat) -> Support {
        match inbound {
            InboundFormat::OpenAiChat
            | InboundFormat::OpenAiResponses
            | InboundFormat::AnthropicMessages => Support::Convert,
        }
    }

    async fn build_request(
        &self,
        req: &GatewayRequest,
        dep: &Deployment,
        auth: &AuthMaterial,
    ) -> Result<Request<Bytes>, BuildError> {
        let project = dep
            .headers
            .get(PROJECT)
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
            .ok_or(BuildError::UnsupportedConversion)?;
        let model = crate::upstream_model(req, dep);
        let chat = chat_value(&req.body, req.format, model)?;
        let body = envelope(&chat, model, project, &req.request_id.0)?;
        let mut request = Request::post(endpoint_uri(&dep.base_url, PATH)?)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "text/event-stream")
            .header(header::USER_AGENT, USER_AGENT)
            .body(Bytes::from(body.to_string()))?;
        for (name, value) in &dep.headers {
            if name == PROJECT {
                continue;
            }
            request.headers_mut().insert(name, value.clone());
        }
        apply_auth(
            request.headers_mut(),
            auth,
            header::AUTHORIZATION,
            "Bearer ",
        )?;
        Ok(request)
    }

    async fn parse_response(
        &self,
        req: &GatewayRequest,
        dep: &Deployment,
        resp: Response<Incoming>,
    ) -> Result<GatewayResponse, UpstreamError> {
        let (parts, body) = resp.into_parts();
        if !parts.status.is_success() {
            return Err(classify_error(
                parts.status,
                &parts.headers,
                collect_error(body).await,
                SystemTime::now(),
            ));
        }
        if !sse::is_event_stream(&parts.headers) {
            return Err(bad_gateway());
        }
        let model = crate::upstream_model(req, dep).to_owned();
        let stream = sse::decode_eof(body);
        if req.stream {
            let chat = chat_stream(model, stream);
            return Ok(GatewayResponse::Stream(
                if req.format == InboundFormat::AnthropicMessages {
                    messages::stream(chat)
                } else {
                    chat
                },
            ));
        }
        let mut collected = Collected::new();
        let mut stream = stream;
        while let Some(event) = stream.next().await {
            apply_payload(&mut collected, &event?.data);
        }
        if collected.text.is_empty() && collected.tool_calls.is_empty() {
            return Err(bad_gateway());
        }
        let body = completion(&model, &collected);
        let body = if req.format == InboundFormat::AnthropicMessages {
            messages::complete_chat(&body)?
        } else {
            body
        };
        Ok(GatewayResponse::Complete {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Bytes::from(body.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;
    use llmux_types::{Billing, DeploymentIdx, QuotaSpec, RequestId};
    use std::time::Duration;

    fn dep() -> Deployment {
        let mut headers = HeaderMap::new();
        headers.insert(PROJECT, HeaderValue::from_static("synthetic-project"));
        Deployment {
            headers,
            fallback: false,
            idx: DeploymentIdx(0),
            id: "ag".into(),
            provider: ProviderKind(provider::ANTIGRAVITY.into()),
            credential: "oauth".into(),
            base_url: "https://daily-cloudcode-pa.googleapis.com".parse().unwrap(),
            upstream_model: "gemini-3-flash".into(),
            strip_request_fields: Box::default(),
            priority: 0,
            quota: QuotaSpec::default(),
            timeout: Duration::from_secs(30),
            max_context: None,
            expires_at: None,
            priority_boost: None,
            priority_boost_until: None,
            billing: Billing::Billed,
        }
    }

    #[tokio::test]
    async fn request_keeps_the_antigravity_model_id() {
        let adapter = Antigravity::new();
        let req = GatewayRequest {
            format: InboundFormat::OpenAiChat,
            model: "antigravity/gemini-3-flash".into(),
            stream: true,
            body: Bytes::from(json!({"messages":[{"role":"user","content":"hello"}]}).to_string()),
            anthropic_headers: HeaderMap::new(),
            est_input_tokens: 1,
            max_output_tokens: None,
            context_overflow: None,
            request_id: RequestId("req".into()),
            end_user: None,
            client_key: None,
            image_count: 0,
            free_only: false,
        };
        let built = adapter
            .build_request(
                &req,
                &dep(),
                &AuthMaterial::Bearer("synthetic-token".into()),
            )
            .await
            .unwrap();
        assert!(built
            .uri()
            .to_string()
            .ends_with("/v1internal:streamGenerateContent?alt=sse"));
        assert!(built.headers().get(PROJECT).is_none());
        assert_eq!(built.headers()["authorization"], "Bearer synthetic-token");
        let body: Value = serde_json::from_slice(built.body()).unwrap();
        assert_eq!(body["model"], "gemini-3-flash");
        assert_eq!(body["project"], "synthetic-project");
        assert_eq!(body["userAgent"], "antigravity");
        assert_eq!(body["request"]["contents"][0]["parts"][0]["text"], "hello");
    }

    #[test]
    fn sse_payload_keeps_text_and_does_not_rename_the_model() {
        let mut collected = Collected::new();
        apply_payload(
            &mut collected,
            r#"{"response":{"candidates":[{"content":{"parts":[{"text":"same-model"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":2,"totalTokenCount":3}}}"#,
        );
        let body = completion("gemini-3-flash", &collected);
        assert_eq!(body["model"], "gemini-3-flash");
        assert_eq!(body["choices"][0]["message"]["content"], "same-model");
        assert_eq!(body["choices"][0]["finish_reason"], "stop");
    }

    #[tokio::test]
    async fn stream_emits_every_tool_call_added_by_one_event() {
        let payload = r#"{"response":{"candidates":[{"content":{"parts":[{"functionCall":{"name":"first","args":{"a":1}}},{"functionCall":{"name":"second","args":{"b":2}}}]},"finishReason":"STOP"}]}}"#;
        let input: BoxStream<'static, Result<SseEvent, UpstreamError>> =
            Box::pin(futures_util::stream::iter(vec![Ok(SseEvent {
                event: None,
                data: payload.into(),
            })]));
        let events: Vec<SseEvent> = chat_stream("gemini-3-flash".into(), input)
            .map(|event| event.unwrap())
            .collect()
            .await;
        let calls: Vec<Value> = events
            .iter()
            .filter_map(|event| serde_json::from_str::<Value>(&event.data).ok())
            .flat_map(|chunk| {
                chunk["choices"][0]["delta"]["tool_calls"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .collect();
        let names: Vec<&str> = calls
            .iter()
            .map(|call| call["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["first", "second"]);
        assert_eq!(calls[0]["id"], "first-0");
        assert_eq!(calls[1]["id"], "second-1");
        let last: Value = serde_json::from_str(&events[events.len() - 2].data).unwrap();
        assert_eq!(last["choices"][0]["finish_reason"], "tool_calls");
    }
}
