use crate::{
    openai_compat::{bad_gateway, classify_error, collect_error},
    sse, OpenAiCompat,
};
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt;
use http::{header, HeaderMap, HeaderValue, Request, Response, StatusCode};
use hyper::body::Incoming;
use llmux_types::{
    AuthMaterial, BoxStream, BuildError, Deployment, GatewayRequest, GatewayResponse,
    InboundFormat, LimitObservation, ProviderAdapter, ProviderKind, SseEvent, Support,
    UpstreamError, Usage,
};
use serde_json::{json, Map, Value};
use std::time::SystemTime;

pub struct OpenAiResponses(ProviderKind);

impl Default for OpenAiResponses {
    fn default() -> Self {
        Self(ProviderKind("openai_responses".into()))
    }
}

impl OpenAiResponses {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ProviderAdapter for OpenAiResponses {
    fn kind(&self) -> &ProviderKind {
        &self.0
    }
    fn supports(&self, inbound: InboundFormat) -> Support {
        match inbound {
            InboundFormat::OpenAiResponses => Support::Passthrough,
            InboundFormat::OpenAiChat => Support::Convert,
            _ => Support::No,
        }
    }
    async fn build_request(
        &self,
        req: &GatewayRequest,
        dep: &Deployment,
        auth: &AuthMaterial,
    ) -> Result<Request<Bytes>, BuildError> {
        if self.supports(req.format) == Support::No {
            return Err(BuildError::UnsupportedFormat);
        }
        let body = normalize(&req.body, &dep.upstream_model, req.format)?;
        let mut parts = dep.base_url.clone().into_parts();
        parts.path_and_query =
            Some(format!("{}/responses", dep.base_url.path().trim_end_matches('/')).parse()?);
        let uri = http::Uri::from_parts(parts).map_err(|e| BuildError::InvalidRequest(e.into()))?;
        let mut request = Request::post(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "text/event-stream")
            .body(Bytes::from(body.to_string()))?;
        match auth {
            AuthMaterial::Bearer(token) => {
                let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
                    .map_err(|_| BuildError::InvalidAuth)?;
                value.set_sensitive(true);
                request.headers_mut().insert(header::AUTHORIZATION, value);
            }
            AuthMaterial::Header { name, value } => {
                let mut value = value.clone();
                value.set_sensitive(true);
                request.headers_mut().insert(name, value);
            }
            AuthMaterial::None => {}
        }
        Ok(request)
    }
    async fn parse_response(
        &self,
        req: &GatewayRequest,
        _: &Deployment,
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
        if !parts
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .eq_ignore_ascii_case("text/event-stream")
            })
        {
            return Err(bad_gateway());
        }
        let stream = validated(sse::decode(body));
        if req.stream {
            return Ok(GatewayResponse::Stream(
                if req.format == InboundFormat::OpenAiChat {
                    chat_stream(stream)
                } else {
                    stream
                },
            ));
        }
        let mut stream = stream;
        while let Some(event) = stream.next().await {
            let value: Value = serde_json::from_str(&event?.data).map_err(|_| bad_gateway())?;
            if value["type"] == "response.completed" {
                let response = &value["response"];
                let usage = usage(response);
                let response = if req.format == InboundFormat::OpenAiChat {
                    chat_complete(response)?
                } else {
                    response.clone()
                };
                return Ok(GatewayResponse::Complete {
                    status: StatusCode::OK,
                    headers: HeaderMap::new(),
                    body: Bytes::from(response.to_string()),
                    usage,
                });
            }
        }
        Err(bad_gateway())
    }
    fn observe_limits(&self, headers: &HeaderMap) -> Option<LimitObservation> {
        OpenAiCompat::new("openai_responses").observe_limits(headers)
    }
}

const OMIT: &[&str] = &[
    "background",
    "conversation",
    "max_output_tokens",
    "max_tool_calls",
    "metadata",
    "moderation",
    "multi_agent",
    "prompt",
    "prompt_cache_retention",
    "safety_identifier",
    "temperature",
    "top_logprobs",
    "top_p",
    "truncation",
    "user",
    "previous_response_id",
];

fn normalize(body: &[u8], model: &str, format: InboundFormat) -> Result<Value, BuildError> {
    let mut value: Value = serde_json::from_slice(body).map_err(|_| BuildError::InvalidBody)?;
    if !value.is_object() {
        return Err(BuildError::InvalidBody);
    }
    if ["previous_response_id", "conversation"]
        .iter()
        .any(|key| value.get(*key).is_some_and(|v| !v.is_null()))
    {
        return Err(BuildError::InvalidBody);
    }
    if format == InboundFormat::OpenAiChat {
        value = chat_request(&value)?;
    }
    let object = value.as_object_mut().ok_or(BuildError::InvalidBody)?;
    for key in OMIT {
        object.remove(*key);
    }
    object.insert("model".into(), json!(model));
    object.insert("stream".into(), json!(true));
    object.insert("store".into(), json!(false));
    let input = object.entry("input").or_insert_with(|| json!([]));
    if let Some(text) = input.as_str() {
        *input = json!([{"role":"user","content":text}]);
    }
    for item in input.as_array().ok_or(BuildError::InvalidBody)? {
        if item["role"] == "system" {
            return Err(BuildError::InvalidBody);
        }
        if item["type"] == "additional_tools" {
            validate_tools(item.get("tools"))?;
        }
    }
    validate_tools(object.get("tools"))?;
    Ok(value)
}

fn validate_tools(tools: Option<&Value>) -> Result<(), BuildError> {
    if let Some(tools) = tools.filter(|v| !v.is_null()) {
        for tool in tools.as_array().ok_or(BuildError::InvalidBody)? {
            match tool["type"].as_str() {
                Some("namespace") => {
                    validate_tools(Some(tool.get("tools").ok_or(BuildError::InvalidBody)?))?
                }
                Some("function" | "custom" | "web_search" | "web_search_preview") => {}
                _ => return Err(BuildError::InvalidBody),
            }
        }
    }
    Ok(())
}

fn text(value: &Value) -> Result<String, BuildError> {
    if let Some(text) = value.as_str() {
        return Ok(text.into());
    }
    let mut out = Vec::new();
    for part in value.as_array().ok_or(BuildError::InvalidBody)? {
        out.push(part["text"].as_str().ok_or(BuildError::InvalidBody)?);
    }
    Ok(out.join("\n"))
}

fn chat_request(chat: &Value) -> Result<Value, BuildError> {
    let mut out = Map::new();
    let mut instructions = Vec::new();
    let mut input = Vec::new();
    for message in chat["messages"].as_array().ok_or(BuildError::InvalidBody)? {
        let role = message["role"].as_str().ok_or(BuildError::InvalidBody)?;
        if role == "system" {
            instructions.push(text(&message["content"])?);
            continue;
        }
        if role == "tool" {
            input.push(json!({"type":"function_call_output","call_id":message["tool_call_id"].as_str().ok_or(BuildError::InvalidBody)?,"output":text(&message["content"])?}));
            continue;
        }
        if !matches!(role, "user" | "assistant" | "developer") {
            return Err(BuildError::InvalidBody);
        }
        if !message["content"].is_null() {
            let content = if let Some(parts) = message["content"].as_array() {
                let mut content = Vec::new();
                for part in parts {
                    content.push(match part["type"].as_str() {
                        Some("text") => json!({"type":if role == "assistant" {"output_text"} else {"input_text"},"text":part["text"].as_str().ok_or(BuildError::InvalidBody)?}),
                        Some("image_url") => {
                            let mut image = json!({"type":"input_image","image_url":part["image_url"]["url"].as_str().ok_or(BuildError::InvalidBody)?});
                            if let Some(detail) = part["image_url"].get("detail") { image["detail"] = detail.clone(); }
                            image
                        },
                        _ => return Err(BuildError::InvalidBody),
                    });
                }
                json!(content)
            } else {
                json!(message["content"].as_str().ok_or(BuildError::InvalidBody)?)
            };
            input.push(json!({"role":role,"content":content}));
        }
        if let Some(calls) = message.get("tool_calls") {
            for call in calls.as_array().ok_or(BuildError::InvalidBody)? {
                if call["type"] != "function" {
                    return Err(BuildError::InvalidBody);
                }
                input.push(json!({"type":"function_call","call_id":call["id"].as_str().ok_or(BuildError::InvalidBody)?,"name":call["function"]["name"].as_str().ok_or(BuildError::InvalidBody)?,"arguments":call["function"]["arguments"].as_str().ok_or(BuildError::InvalidBody)?}));
            }
        }
    }
    out.insert("input".into(), json!(input));
    if !instructions.is_empty() {
        out.insert("instructions".into(), json!(instructions.join("\n\n")));
    }
    if let Some(tools) = chat.get("tools") {
        let mut converted = Vec::new();
        for tool in tools.as_array().ok_or(BuildError::InvalidBody)? {
            if tool["type"] != "function" {
                return Err(BuildError::InvalidBody);
            }
            let mut function = tool["function"]
                .as_object()
                .ok_or(BuildError::InvalidBody)?
                .clone();
            function.insert("type".into(), json!("function"));
            converted.push(Value::Object(function));
        }
        out.insert("tools".into(), json!(converted));
    }
    if let Some(choice) = chat.get("tool_choice") {
        out.insert(
            "tool_choice".into(),
            if choice.is_string() {
                choice.clone()
            } else if choice["type"] == "function" {
                json!({"type":"function","name":choice["function"]["name"]})
            } else {
                return Err(BuildError::InvalidBody);
            },
        );
    }
    for key in ["parallel_tool_calls", "reasoning"] {
        if let Some(v) = chat.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
    if let Some(effort) = chat.get("reasoning_effort") {
        out.insert("reasoning".into(), json!({"effort":effort}));
    }
    if let Some(format) = chat.get("response_format") {
        out.insert("text".into(), json!({"format":if format["type"] == "json_schema" { let mut schema = format["json_schema"].as_object().ok_or(BuildError::InvalidBody)?.clone(); schema.insert("type".into(), json!("json_schema")); Value::Object(schema) } else { format.clone() }}));
    }
    Ok(Value::Object(out))
}

fn usage(response: &Value) -> Option<Usage> {
    let u = response.get("usage")?;
    Some(Usage {
        input_tokens: u["input_tokens"].as_u64()?.try_into().ok()?,
        output_tokens: u["output_tokens"].as_u64()?.try_into().ok()?,
    })
}

fn stream_error(value: &Value) -> UpstreamError {
    let error = value
        .get("response")
        .and_then(|v| v.get("error"))
        .or_else(|| value.get("error"))
        .unwrap_or(value);
    let code = error["code"].as_str().unwrap_or("");
    if matches!(
        code,
        "subscription_sharing_usage_limit_exceeded" | "subscription_sharing_usage_unavailable"
    ) {
        return UpstreamError::QuotaExhausted { reset_at: None };
    }
    let status = if matches!(code, "rate_limit_exceeded" | "insufficient_quota") {
        StatusCode::TOO_MANY_REQUESTS
    } else {
        StatusCode::BAD_GATEWAY
    };
    classify_error(
        status,
        &HeaderMap::new(),
        Bytes::from(json!({"error":error}).to_string()),
        SystemTime::now(),
    )
}

fn validated(
    mut upstream: BoxStream<'static, Result<SseEvent, UpstreamError>>,
) -> BoxStream<'static, Result<SseEvent, UpstreamError>> {
    Box::pin(async_stream::try_stream! {
        while let Some(event) = upstream.next().await {
            let event = event?;
            let value: Value = serde_json::from_str(&event.data).map_err(|_| bad_gateway())?;
            let kind = value["type"].as_str().ok_or_else(bad_gateway)?;
            if matches!(kind, "response.failed" | "error") { Err(stream_error(&value))?; }
            if kind == "response.incomplete" { Err(bad_gateway())?; }
            let done = kind == "response.completed";
            if done && (!value["response"].is_object() || value["response"]["status"] != "completed") { Err(bad_gateway())?; }
            yield event;
            if done { return; }
        }
        Err(bad_gateway())?;
    })
}

fn chat_complete(response: &Value) -> Result<Value, UpstreamError> {
    let mut content = String::new();
    let mut refusal = String::new();
    let mut calls = Vec::new();
    for item in response["output"].as_array().ok_or_else(bad_gateway)? {
        if item["type"] == "function_call" {
            calls.push(json!({"id":item["call_id"],"type":"function","function":{"name":item["name"],"arguments":item["arguments"]}}));
        }
        if item["type"] == "message" {
            for part in item["content"].as_array().ok_or_else(bad_gateway)? {
                if part["type"] == "output_text" {
                    content.push_str(part["text"].as_str().ok_or_else(bad_gateway)?);
                }
                if part["type"] == "refusal" {
                    refusal.push_str(part["refusal"].as_str().ok_or_else(bad_gateway)?);
                }
            }
        }
    }
    let mut message = json!({"role":"assistant","content":if content.is_empty() {Value::Null} else {json!(content)}});
    let finish = if calls.is_empty() {
        "stop"
    } else {
        message["tool_calls"] = json!(calls);
        "tool_calls"
    };
    if !refusal.is_empty() {
        message["refusal"] = json!(refusal);
    }
    let mut out = json!({"id":response["id"],"object":"chat.completion","created":response["created_at"].as_u64().unwrap_or(0),"model":response["model"],"choices":[{"index":0,"message":message,"finish_reason":finish}]});
    out["usage"] = chat_usage(response);
    Ok(out)
}

fn chat_usage(response: &Value) -> Value {
    usage(response).map(|u| json!({"prompt_tokens":u.input_tokens,"completion_tokens":u.output_tokens,"total_tokens":u64::from(u.input_tokens)+u64::from(u.output_tokens)})).unwrap_or(Value::Null)
}

fn chunk(response: &Value, delta: Value, finish: Value) -> Value {
    json!({"id":response["id"].as_str().unwrap_or("chatcmpl-responses"),"object":"chat.completion.chunk","created":response["created_at"].as_u64().unwrap_or(0),"model":response["model"],"choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
}

fn chat_event(value: Value) -> SseEvent {
    SseEvent {
        event: None,
        data: value.to_string(),
    }
}

fn chat_stream(
    mut upstream: BoxStream<'static, Result<SseEvent, UpstreamError>>,
) -> BoxStream<'static, Result<SseEvent, UpstreamError>> {
    Box::pin(async_stream::try_stream! {
        let mut response = json!({});
        let mut calls = std::collections::HashMap::new();
        while let Some(event) = upstream.next().await {
            let value: Value = serde_json::from_str(&event?.data).map_err(|_| bad_gateway())?;
            match value["type"].as_str().unwrap_or("") {
                "response.created" => { response = value["response"].clone(); yield chat_event(chunk(&response, json!({"role":"assistant","content":""}), Value::Null)); },
                "response.output_text.delta" => { yield chat_event(chunk(&response, json!({"content":value["delta"]}), Value::Null)); },
                "response.refusal.delta" => { yield chat_event(chunk(&response, json!({"refusal":value["delta"]}), Value::Null)); },
                "response.output_item.added" if value["item"]["type"] == "function_call" => {
                    let index = calls.len();
                    calls.insert(value["output_index"].as_u64().ok_or_else(bad_gateway)?, index);
                    yield chat_event(chunk(&response, json!({"tool_calls":[{"index":index,"id":value["item"]["call_id"],"type":"function","function":{"name":value["item"]["name"],"arguments":value["item"]["arguments"].as_str().unwrap_or("")}}]}), Value::Null));
                },
                "response.function_call_arguments.delta" => {
                    let index = calls.get(&value["output_index"].as_u64().ok_or_else(bad_gateway)?).ok_or_else(bad_gateway)?;
                    yield chat_event(chunk(&response, json!({"tool_calls":[{"index":index,"function":{"arguments":value["delta"]}}]}), Value::Null));
                },
                "response.completed" => {
                    response = value["response"].clone();
                    let mut last = chunk(&response, json!({}), json!(if calls.is_empty() {"stop"} else {"tool_calls"}));
                    last["usage"] = chat_usage(&response);
                    yield chat_event(last);
                    yield SseEvent {event:None,data:"[DONE]".into()};
                    return;
                },
                _ => {},
            }
        }
        Err(bad_gateway())?;
    })
}
