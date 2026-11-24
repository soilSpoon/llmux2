use crate::{
    openai_compat::{
        apply_auth, bad_gateway, collect, endpoint_uri, retry_after, rewrite_model,
        strip_request_fields,
    },
    sse,
};
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt;
use http::{header, HeaderMap, HeaderValue, Method, Request, Response, StatusCode};
use hyper::body::Incoming;
use llmux_types::{
    provider, AuthMaterial, BoxStream, BuildError, Deployment, GatewayRequest, GatewayResponse,
    InboundFormat, LimitObservation, ProviderAdapter, ProviderKind, SseEvent, Support,
    UpstreamError, Usage,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

const LIMIT_REMAINING_REQUESTS: &str = "anthropic-ratelimit-requests-remaining";
const LIMIT_REMAINING_TOKENS: &str = "anthropic-ratelimit-tokens-remaining";
const REQUEST_LIMIT: (&str, &str) = (
    LIMIT_REMAINING_REQUESTS,
    "anthropic-ratelimit-requests-reset",
);
const TOKEN_LIMITS: [(&str, &str); 3] = [
    (LIMIT_REMAINING_TOKENS, "anthropic-ratelimit-tokens-reset"),
    (
        "anthropic-ratelimit-input-tokens-remaining",
        "anthropic-ratelimit-input-tokens-reset",
    ),
    (
        "anthropic-ratelimit-output-tokens-remaining",
        "anthropic-ratelimit-output-tokens-reset",
    ),
];

fn limit_window(
    headers: &HeaderMap,
    (remaining, reset): (&str, &str),
    now: SystemTime,
) -> (Option<u32>, Option<Duration>) {
    let remaining = headers
        .get(remaining)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    let reset = headers
        .get(reset)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| OffsetDateTime::parse(v, &Rfc3339).ok())
        .and_then(|date| Duration::try_from(date - OffsetDateTime::from(now)).ok())
        .filter(|wait| !wait.is_zero());
    (remaining, reset)
}

fn anthropic_retry_after(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
    retry_after(headers, now)
        .filter(|wait| !wait.is_zero())
        .or_else(|| {
            [REQUEST_LIMIT]
                .into_iter()
                .chain(TOKEN_LIMITS)
                .map(|window| limit_window(headers, window, now))
                .filter(|(remaining, _)| remaining.is_none_or(|remaining| remaining == 0))
                .filter_map(|(_, reset)| reset)
                .max()
        })
}

const DEPRECATABLE_FIELDS: [&str; 3] = ["temperature", "top_p", "top_k"];

pub struct Anthropic(ProviderKind);

impl Anthropic {
    pub fn new(kind: impl Into<Arc<str>>) -> Self {
        Self(ProviderKind(kind.into()))
    }
}

fn set_metadata_user_id(body: Bytes, user_id: &str) -> Result<Bytes, BuildError> {
    let mut value: Value = serde_json::from_slice(&body).map_err(|_| BuildError::InvalidBody)?;
    let object = value.as_object_mut().ok_or(BuildError::InvalidBody)?;
    let metadata = object.entry("metadata").or_insert_with(|| json!({}));
    if !metadata.is_object() {
        *metadata = json!({});
    }
    metadata
        .as_object_mut()
        .expect("metadata object")
        .insert("user_id".into(), json!(user_id));
    Ok(Bytes::from(value.to_string()))
}

fn string(value: &Value) -> Result<&str, BuildError> {
    value.as_str().ok_or(BuildError::InvalidBody)
}

fn text(content: &Value) -> Result<String, BuildError> {
    if let Some(s) = content.as_str() {
        return Ok(s.to_owned());
    }
    let mut result = String::new();
    for part in content.as_array().ok_or(BuildError::InvalidBody)? {
        if part["type"] != "text" {
            return Err(BuildError::InvalidBody);
        }
        result.push_str(string(&part["text"])?);
    }
    Ok(result)
}

fn blocks(content: &Value) -> Result<Vec<Value>, BuildError> {
    if let Some(s) = content.as_str() {
        return Ok(vec![json!({"type":"text","text":s})]);
    }
    content
        .as_array()
        .ok_or(BuildError::InvalidBody)?
        .iter()
        .map(|part| match part["type"].as_str() {
            Some("text") => Ok(json!({"type":"text","text":string(&part["text"])?})),
            Some("image_url") => {
                let url = string(&part["image_url"]["url"])?;
                let source = if let Some(data) = url.strip_prefix("data:") {
                    let (mime, data) =
                        data.split_once(";base64,").ok_or(BuildError::InvalidBody)?;
                    if mime.is_empty() || data.is_empty() {
                        return Err(BuildError::InvalidBody);
                    }
                    json!({"type":"base64","media_type":mime,"data":data})
                } else if url.starts_with("https://") || url.starts_with("http://") {
                    json!({"type":"url","url":url})
                } else {
                    return Err(BuildError::InvalidBody);
                };
                Ok(json!({"type":"image","source":source}))
            }
            _ => Err(BuildError::InvalidBody),
        })
        .collect()
}

fn convert_request(body: &[u8], model: &str, end_user: Option<&str>) -> Result<Bytes, BuildError> {
    let input: Value = serde_json::from_slice(body).map_err(|_| BuildError::InvalidBody)?;
    let mut messages: Vec<Value> = Vec::new();
    let mut system = Vec::new();
    for message in input["messages"]
        .as_array()
        .ok_or(BuildError::InvalidBody)?
    {
        let role = string(&message["role"])?;
        if matches!(role, "system" | "developer") {
            system.push(text(&message["content"])?);
            continue;
        }
        let (role, mut content) = match role {
            "tool" => (
                "user",
                vec![
                    json!({"type":"tool_result","tool_use_id":string(&message["tool_call_id"])?,"content":text(&message["content"])?}),
                ],
            ),
            "user" => ("user", blocks(&message["content"])?),
            "assistant" => (
                "assistant",
                if message["content"].is_null() {
                    vec![]
                } else {
                    blocks(&message["content"])?
                },
            ),
            _ => return Err(BuildError::InvalidBody),
        };
        if role == "assistant" {
            if let Some(calls) = message.get("tool_calls").filter(|calls| !calls.is_null()) {
                for call in calls.as_array().ok_or(BuildError::InvalidBody)? {
                    let arguments: Value =
                        serde_json::from_str(string(&call["function"]["arguments"])?)
                            .map_err(|_| BuildError::InvalidBody)?;
                    content.push(json!({"type":"tool_use","id":string(&call["id"])?,"name":string(&call["function"]["name"])?,"input":arguments}));
                }
            }
        }
        content.retain(|block| block["type"] != "text" || block["text"] != "");
        if content.is_empty() {
            continue;
        }
        if let Some(last) = messages.last_mut().filter(|last| last["role"] == role) {
            last["content"].as_array_mut().unwrap().extend(content);
        } else {
            messages.push(json!({"role":role,"content":content}));
        }
    }
    let mut out = json!({"model":model,"messages":messages,"max_tokens":input.get("max_completion_tokens").filter(|v| !v.is_null()).or_else(|| input.get("max_tokens").filter(|v| !v.is_null())).cloned().unwrap_or(json!(4096))});
    if !system.is_empty() {
        out["system"] = json!(system.join("\n\n"));
    }
    for key in ["temperature", "top_p", "stream"] {
        if let Some(v) = input.get(key) {
            out[key] = v.clone();
        }
    }
    if let Some(stop) = input.get("stop").filter(|v| !v.is_null()) {
        out["stop_sequences"] = if stop.is_string() {
            json!([stop])
        } else if stop
            .as_array()
            .is_some_and(|a| a.iter().all(Value::is_string))
        {
            stop.clone()
        } else {
            return Err(BuildError::InvalidBody);
        };
    }
    if let Some(user_id) = end_user {
        out["metadata"] = json!({"user_id": user_id});
    } else if let Some(user) = input.get("user") {
        out["metadata"] = json!({"user_id":user});
    }
    if let Some(tools) = input.get("tools") {
        out["tools"] = Value::Array(tools.as_array().ok_or(BuildError::InvalidBody)?.iter().map(|tool| {
            if tool["type"] != "function" { return Err(BuildError::InvalidBody); }
            let f = &tool["function"];
            let mut t = json!({"name":string(&f["name"])?,"input_schema":f.get("parameters").cloned().unwrap_or(json!({"type":"object"}))});
            if let Some(d) = f.get("description") { t["description"] = d.clone(); }
            Ok(t)
        }).collect::<Result<_, BuildError>>()?);
    }
    if let Some(choice) = input.get("tool_choice") {
        out["tool_choice"] = match choice.as_str() {
            Some("auto") => json!({"type":"auto"}),
            Some("required") => json!({"type":"any"}),
            Some("none") => json!({"type":"none"}),
            None if choice["type"] == "function" => {
                json!({"type":"tool","name":string(&choice["function"]["name"])?})
            }
            _ => return Err(BuildError::InvalidBody),
        };
    }
    if input["parallel_tool_calls"] == false {
        if out.get("tool_choice").is_none() {
            out["tool_choice"] = json!({"type":"auto"});
        }
        out["tool_choice"]["disable_parallel_tool_use"] = json!(true);
    }
    Ok(Bytes::from(out.to_string()))
}

#[derive(Default)]
struct MessageUsage {
    tokens: Usage,
    cached_tokens: Option<u32>,
    cache_creation_input_tokens: Option<u32>,
}

impl MessageUsage {
    fn parse(value: &Value) -> Self {
        let count = |key: &str| value[key].as_u64().map(|n| n.min(u32::MAX as u64) as u32);
        let cached_tokens = count("cache_read_input_tokens");
        let cache_creation_input_tokens = count("cache_creation_input_tokens");
        Self {
            tokens: Usage {
                input_tokens: count("input_tokens")
                    .unwrap_or_default()
                    .saturating_add(cached_tokens.unwrap_or_default())
                    .saturating_add(cache_creation_input_tokens.unwrap_or_default()),
                output_tokens: count("output_tokens").unwrap_or_default(),
            },
            cached_tokens,
            cache_creation_input_tokens,
        }
    }

    fn chat_json(&self) -> Value {
        let Usage {
            input_tokens,
            output_tokens,
        } = self.tokens;
        let mut usage = json!({"prompt_tokens":input_tokens,"completion_tokens":output_tokens,"total_tokens":u64::from(input_tokens)+u64::from(output_tokens)});
        if let Some(cached) = self.cached_tokens {
            usage["prompt_tokens_details"] = json!({"cached_tokens":cached});
        }
        if let Some(created) = self.cache_creation_input_tokens {
            usage["cache_creation_input_tokens"] = json!(created);
        }
        usage
    }
}
fn finish(v: &Value) -> &'static str {
    match v.as_str() {
        Some("max_tokens" | "model_context_window_exceeded") => "length",
        Some("tool_use") => "tool_calls",
        _ => "stop",
    }
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn convert_response(v: &Value) -> Value {
    let mut text = String::new();
    let mut tools = Vec::new();
    if let Some(content) = v["content"].as_array() {
        for block in content {
            match block["type"].as_str() {
                Some("text") => text.push_str(block["text"].as_str().unwrap_or_default()),
                Some("tool_use") => tools.push(json!({"id":block["id"],"type":"function","function":{"name":block["name"],"arguments":block["input"].to_string()}})),
                _ => {}
            }
        }
    }
    let mut message = json!({"role":"assistant","content":if text.is_empty() { Value::Null } else { json!(text) }});
    if !tools.is_empty() {
        message["tool_calls"] = json!(tools);
    }
    json!({"id":v["id"],"object":"chat.completion","created":now(),"model":v["model"],"choices":[{"index":0,"message":message,"finish_reason":finish(&v["stop_reason"])}],"usage":MessageUsage::parse(&v["usage"]).chat_json()})
}

fn client_error(status: StatusCode, body: Bytes, convert: bool) -> UpstreamError {
    let value = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
    let details = &value["error"];
    if status == StatusCode::BAD_REQUEST
        && details["type"] == "invalid_request_error"
        && details["message"].as_str().is_some_and(|message| {
            message.starts_with("Your credit balance is too low to access the Anthropic API.")
        })
    {
        return UpstreamError::QuotaExhausted { reset_at: None };
    }
    let model_not_found = crate::openai_compat::is_model_not_found(status, details);
    if !model_not_found
        && matches!(status.as_u16(), 400 | 413)
        && details["message"]
            .as_str()
            .is_some_and(|s| s.contains("prompt is too long"))
    {
        return UpstreamError::ContextLength;
    }
    let body = if convert {
        let mut error = json!({
            "message": details["message"].as_str().unwrap_or("Upstream request failed"),
            "type": details["type"].as_str().unwrap_or("invalid_request_error")
        });
        if model_not_found {
            error["code"] = details["code"].clone();
        }
        if let Some(request_id) = value.get("request_id") {
            error["request_id"] = request_id.clone();
        }
        Bytes::from(json!({"error": error}).to_string())
    } else {
        body
    };
    if model_not_found {
        UpstreamError::ModelNotFound { status, body }
    } else {
        UpstreamError::BadRequest { status, body }
    }
}

#[derive(Default)]
struct StreamTool {
    index: usize,
    has_arguments: bool,
}

#[derive(Deserialize)]
struct ChatStreamOptions {
    stream_options: Option<StreamOptions>,
}

#[derive(Deserialize)]
struct StreamOptions {
    include_usage: Option<bool>,
}

#[derive(Default)]
struct StreamState {
    id: Value,
    model: Value,
    created: u64,
    tools: HashMap<u64, StreamTool>,
    usage: MessageUsage,
    include_usage: Option<bool>,
}
impl StreamState {
    fn chunk(&self, choices: Value) -> Value {
        json!({"id":self.id,"object":"chat.completion.chunk","created":self.created,"model":self.model,"choices":choices})
    }

    fn map(&mut self, event: SseEvent) -> Result<Option<SseEvent>, UpstreamError> {
        let v: Value = serde_json::from_str(&event.data).map_err(|_| bad_gateway())?;
        let kind = event.event.as_deref().or_else(|| v["type"].as_str());
        let (delta, reason) = match kind {
            Some("message_start") => {
                self.id = v["message"]["id"].clone();
                self.model = v["message"]["model"].clone();
                self.created = now();
                self.usage = MessageUsage::parse(&v["message"]["usage"]);
                (json!({"role":"assistant","content":""}), Value::Null)
            }
            Some("content_block_start") if v["content_block"]["type"] == "tool_use" => {
                let index = self.tools.len();
                self.tools.insert(
                    v["index"].as_u64().ok_or_else(bad_gateway)?,
                    StreamTool {
                        index,
                        has_arguments: false,
                    },
                );
                (
                    json!({"tool_calls":[{"index":index,"id":v["content_block"]["id"],"type":"function","function":{"name":v["content_block"]["name"],"arguments":""}}]}),
                    Value::Null,
                )
            }
            Some("content_block_delta") => match v["delta"]["type"].as_str() {
                Some("text_delta") => (json!({"content":v["delta"]["text"]}), Value::Null),
                Some("input_json_delta") => {
                    let tool = v["index"]
                        .as_u64()
                        .and_then(|i| self.tools.get_mut(&i))
                        .ok_or_else(bad_gateway)?;
                    let arguments = v["delta"]["partial_json"]
                        .as_str()
                        .ok_or_else(bad_gateway)?;
                    tool.has_arguments |= !arguments.is_empty();
                    let index = tool.index;
                    (
                        json!({"tool_calls":[{"index":index,"function":{"arguments":v["delta"]["partial_json"]}}]}),
                        Value::Null,
                    )
                }
                _ => return Ok(None),
            },
            Some("content_block_stop") => {
                let Some(tool) = v["index"].as_u64().and_then(|i| self.tools.get_mut(&i)) else {
                    return Ok(None);
                };
                if tool.has_arguments {
                    return Ok(None);
                }
                tool.has_arguments = true;
                (
                    json!({"tool_calls":[{"index":tool.index,"function":{"arguments":"{}"}}]}),
                    Value::Null,
                )
            }
            Some("message_delta") => {
                self.usage.tokens.output_tokens =
                    MessageUsage::parse(&v["usage"]).tokens.output_tokens;
                (json!({}), json!(finish(&v["delta"]["stop_reason"])))
            }
            Some("message_stop") => {
                return Ok(Some(SseEvent {
                    event: None,
                    data: "[DONE]".into(),
                }))
            }
            Some("error") => return Err(bad_gateway()),
            _ => return Ok(None),
        };
        let mut chunk = self.chunk(json!([{"index":0,"delta":delta,"finish_reason":reason}]));
        if self.include_usage == Some(true) {
            chunk["usage"] = Value::Null;
        } else if self.include_usage.is_none() && kind == Some("message_delta") {
            chunk["usage"] = self.usage.chat_json();
        }
        Ok(Some(SseEvent {
            event: None,
            data: chunk.to_string(),
        }))
    }
}

fn convert_stream(
    mut events: BoxStream<'static, Result<SseEvent, UpstreamError>>,
    include_usage: Option<bool>,
) -> BoxStream<'static, Result<SseEvent, UpstreamError>> {
    Box::pin(async_stream::try_stream! {
        let mut state = StreamState {
            include_usage,
            ..StreamState::default()
        };
        while let Some(event) = events.next().await {
            if let Some(event) = state.map(event?)? {
                let done = event.data == "[DONE]";
                if done && include_usage == Some(true) {
                    let mut chunk = state.chunk(json!([]));
                    chunk["usage"] = state.usage.chat_json();
                    yield SseEvent { event: None, data: chunk.to_string() };
                }
                yield event;
                if done { return; }
            }
        }
    })
}

#[async_trait]
impl ProviderAdapter for Anthropic {
    fn kind(&self) -> &ProviderKind {
        &self.0
    }
    fn supports(&self, inbound: InboundFormat) -> Support {
        match inbound {
            InboundFormat::AnthropicMessages => Support::Passthrough,
            InboundFormat::OpenAiChat => Support::Convert,
            InboundFormat::OpenAiResponses => Support::No,
        }
    }
    async fn build_request(
        &self,
        req: &GatewayRequest,
        dep: &Deployment,
        auth: &AuthMaterial,
    ) -> Result<Request<Bytes>, BuildError> {
        let body = match self.supports(req.format) {
            Support::No => return Err(BuildError::UnsupportedFormat),
            Support::Passthrough => {
                let body = rewrite_model(&req.body, crate::upstream_model(req, dep))?;
                match req.end_user.as_deref() {
                    Some(user_id) => set_metadata_user_id(body, user_id)?,
                    None => body,
                }
            }
            Support::Convert => convert_request(
                &req.body,
                crate::upstream_model(req, dep),
                req.end_user.as_deref(),
            )?,
        };
        let body = strip_request_fields(body, &dep.strip_request_fields)?;
        let uri = endpoint_uri(&dep.base_url, "/messages")?;
        let mut request = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .header("anthropic-version", "2023-06-01")
            .header(
                header::ACCEPT,
                if req.stream {
                    "text/event-stream"
                } else {
                    "application/json"
                },
            )
            .body(body)?;
        if req.format == InboundFormat::AnthropicMessages {
            request.headers_mut().extend(req.anthropic_headers.clone());
        }
        request.headers_mut().extend(dep.headers.clone());
        llmux_types::default_user_agent(request.headers_mut());
        apply_auth(
            request.headers_mut(),
            auth,
            http::HeaderName::from_static(provider::ANTHROPIC_KEY_HEADER),
            "",
        )?;
        Ok(request)
    }
    async fn parse_response(
        &self,
        req: &GatewayRequest,
        _dep: &Deployment,
        resp: Response<Incoming>,
    ) -> Result<GatewayResponse, UpstreamError> {
        let (mut parts, body) = resp.into_parts();
        let convert = req.format == InboundFormat::OpenAiChat;
        match parts.status {
            StatusCode::TOO_MANY_REQUESTS => {
                return Err(UpstreamError::RateLimited {
                    retry_after: anthropic_retry_after(&parts.headers, SystemTime::now()),
                })
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                return Err(UpstreamError::Auth { refreshable: false })
            }
            StatusCode::REQUEST_TIMEOUT => return Err(UpstreamError::Timeout),
            s if s.is_client_error() => return Err(client_error(s, collect(body).await?, convert)),
            s if s.is_server_error() => return Err(UpstreamError::Server { status: s }),
            s if !s.is_success() => return Err(bad_gateway()),
            _ => {}
        }
        if req.stream {
            if !sse::is_event_stream(&parts.headers) {
                return Err(bad_gateway());
            }
            let events = sse::decode_until(body, true);
            return Ok(GatewayResponse::Stream(if convert {
                let usage = serde_json::from_slice::<ChatStreamOptions>(&req.body)
                    .ok()
                    .and_then(|chat| chat.stream_options)
                    .and_then(|options| options.include_usage);
                convert_stream(events, usage)
            } else {
                events
            }));
        }
        let mut body = collect(body).await?;
        if convert {
            let parsed = serde_json::from_slice::<Value>(&body);
            body = Bytes::from(convert_response(&parsed.map_err(|_| bad_gateway())?).to_string());
            parts.headers.remove(header::CONTENT_LENGTH);
            parts.headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
        }
        Ok(GatewayResponse::Complete {
            status: parts.status,
            headers: parts.headers,
            body,
        })
    }
    fn repair_rejected(&self, body: &Bytes, error: &UpstreamError) -> Option<Bytes> {
        let UpstreamError::BadRequest {
            body: rejection, ..
        } = error
        else {
            return None;
        };
        let rejection = serde_json::from_slice::<Value>(rejection).ok()?;
        let message = rejection["error"]["message"]
            .as_str()
            .or_else(|| rejection["message"].as_str())?;
        let field = DEPRECATABLE_FIELDS
            .into_iter()
            .find(|field| message.contains(&format!("`{field}` is deprecated for this model")))?;
        let mut payload = serde_json::from_slice::<Value>(body).ok()?;
        payload.as_object_mut()?.remove(field)?;
        eprintln!("llmux: anthropic rejected `{field}` as deprecated for this model; removed it from the repair payload");
        Some(Bytes::from(payload.to_string()))
    }
    fn observe_limits(&self, headers: &HeaderMap) -> Option<LimitObservation> {
        let now = SystemTime::now();
        let (remaining_requests, reset_requests) = limit_window(headers, REQUEST_LIMIT, now);
        let tokens = TOKEN_LIMITS.map(|window| limit_window(headers, window, now));
        let remaining_tokens = tokens.iter().filter_map(|(remaining, _)| *remaining).min();
        let reset_tokens = tokens
            .into_iter()
            .filter(|(remaining, _)| *remaining == Some(0))
            .filter_map(|(_, reset)| reset)
            .max();
        if remaining_requests.is_none() && remaining_tokens.is_none() {
            return None;
        }
        Some(LimitObservation {
            remaining_requests,
            remaining_tokens,
            reset_requests,
            reset_tokens,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;
    use llmux_types::{Billing, DeploymentIdx, QuotaSpec, RequestId};

    #[test]
    fn anthropic_retry_after_prefers_the_header_then_valid_reset_windows() {
        let now = UNIX_EPOCH + Duration::from_secs(30);
        let headers = HeaderMap::from_iter([
            (header::RETRY_AFTER, HeaderValue::from_static("7")),
            (
                http::HeaderName::from_static(REQUEST_LIMIT.1),
                HeaderValue::from_static("1970-01-01T00:00:25Z"),
            ),
            (
                http::HeaderName::from_static(TOKEN_LIMITS[0].1),
                HeaderValue::from_static("invalid"),
            ),
            (
                http::HeaderName::from_static(TOKEN_LIMITS[1].1),
                HeaderValue::from_static("1970-01-01T01:00:33.5+01:00"),
            ),
            (
                http::HeaderName::from_static(TOKEN_LIMITS[2].0),
                HeaderValue::from_static("5"),
            ),
            (
                http::HeaderName::from_static(TOKEN_LIMITS[2].1),
                HeaderValue::from_static("1970-01-01T00:01:00Z"),
            ),
        ]);
        assert_eq!(
            anthropic_retry_after(&headers, now),
            Some(Duration::from_secs(7))
        );
        let mut headers = headers;
        headers.insert(
            header::RETRY_AFTER,
            HeaderValue::from_static("Thu, 01 Jan 1970 00:00:20 GMT"),
        );
        assert_eq!(
            anthropic_retry_after(&headers, now),
            Some(Duration::from_millis(3500))
        );
        headers.remove(TOKEN_LIMITS[1].1);
        assert_eq!(anthropic_retry_after(&headers, now), None);
    }
    use std::time::Duration;

    #[test]
    fn exhausted_token_windows_observe_the_latest_applicable_reset() {
        let adapter = Anthropic::new(provider::ANTHROPIC);
        for (remaining, expected_seconds) in [
            (["9", "0", "9"], 30),
            (["9", "9", "0"], 20),
            (["0", "0", "0"], 30),
        ] {
            let now = OffsetDateTime::now_utc();
            let mut headers = HeaderMap::new();
            for (((count_header, reset_header), count), seconds) in
                TOKEN_LIMITS.into_iter().zip(remaining).zip([10, 30, 20])
            {
                headers.insert(count_header, HeaderValue::from_static(count));
                let reset = now + Duration::from_secs(seconds);
                headers.insert(
                    reset_header,
                    reset.format(&Rfc3339).unwrap().parse().unwrap(),
                );
            }
            let before = OffsetDateTime::now_utc();
            let observation = adapter.observe_limits(&headers).unwrap();
            let after = OffsetDateTime::now_utc();
            let selected = now + Duration::from_secs(expected_seconds);
            assert_eq!(observation.remaining_tokens, Some(0), "{remaining:?}");
            let actual = observation.reset_tokens.unwrap();
            assert!(
                before + actual <= selected && after + actual >= selected,
                "unexpected reset duration {actual:?} for {remaining:?}"
            );
        }
    }

    #[test]
    fn limit_header_names_match_the_upstream_contract() {
        assert_eq!(
            LIMIT_REMAINING_REQUESTS,
            "anthropic-ratelimit-requests-remaining"
        );
        assert_eq!(
            LIMIT_REMAINING_TOKENS,
            "anthropic-ratelimit-tokens-remaining"
        );
    }

    fn converted(v: Value) -> Value {
        serde_json::from_slice(&convert_request(v.to_string().as_bytes(), "claude", None).unwrap())
            .unwrap()
    }

    #[test]
    fn request_system_images_tools_and_merging() {
        let out = converted(json!({"messages":[
            {"role":"system","content":"one"},
            {"role":"developer","content":[{"type":"text","text":"two"},{"type":"text","text":"three"}]},
            {"role":"user","content":[{"type":"text","text":"look"},{"type":"image_url","image_url":{"url":"data:image/png;base64,YQ=="}},{"type":"image_url","image_url":{"url":"https://example.com/a.png"}}]},
            {"role":"assistant","content":null,"tool_calls":[{"id":"t1","function":{"name":"lookup","arguments":"{\"x\":1}"}}]},
            {"role":"tool","tool_call_id":"t1","content":"result"},
            {"role":"user","content":"next"}
        ],"tools":[{"type":"function","function":{"name":"lookup","description":"find"}}],"stop":"end","user":"u","n":1,"response_format":{"type":"json_object"}}));
        assert_eq!(out["system"], "one\n\ntwothree");
        assert_eq!(out["model"], "claude");
        assert_eq!(out["max_tokens"], 4096);
        assert_eq!(out["messages"].as_array().unwrap().len(), 3);
        assert_eq!(
            out["messages"][0]["content"][1]["source"],
            json!({"type":"base64","media_type":"image/png","data":"YQ=="})
        );
        assert_eq!(out["messages"][0]["content"][2]["source"]["type"], "url");
        assert_eq!(out["messages"][1]["content"][0]["input"], json!({"x":1}));
        assert_eq!(out["messages"][2]["content"].as_array().unwrap().len(), 2);
        assert_eq!(out["messages"][2]["content"][0]["tool_use_id"], "t1");
        assert_eq!(out["tools"][0]["input_schema"], json!({"type":"object"}));
        assert_eq!(out["stop_sequences"], json!(["end"]));
        assert_eq!(out["metadata"]["user_id"], "u");
        assert!(out.get("response_format").is_none());
    }

    #[test]
    fn drops_empty_text_blocks_without_dropping_tool_calls() {
        for content in [json!(""), json!([{"type":"text","text":""}])] {
            let out = converted(json!({"messages":[
                {"role":"user","content":""},
                {"role":"user","content":[{"type":"text","text":""},{"type":"text","text":"hello"}]},
                {"role":"assistant","content":content,"tool_calls":[{"id":"t1","function":{"name":"f","arguments":"{}"}}]}
            ]}));
            let messages = out["messages"].as_array().unwrap();
            assert_eq!(messages.len(), 2);
            assert_eq!(
                messages[0]["content"],
                json!([{"type":"text","text":"hello"}])
            );
            assert_eq!(
                messages[1]["content"],
                json!([{"type":"tool_use","id":"t1","name":"f","input":{}}])
            );
        }
    }

    #[test]
    fn null_and_empty_tool_calls_are_absent() {
        for calls in [Value::Null, json!([])] {
            let out = converted(
                json!({"messages":[{"role":"assistant","content":"hello","tool_calls":calls}]}),
            );
            assert_eq!(
                out["messages"],
                json!([{"role":"assistant","content":[{"type":"text","text":"hello"}]}])
            );
        }
        assert!(convert_request(
            br#"{"messages":[{"role":"assistant","content":"hello","tool_calls":{}}]}"#,
            "m",
            None
        )
        .is_err());
    }

    #[test]
    fn choices_parameters_and_empty_requests() {
        for (choice, expected) in [
            (json!("auto"), json!({"type":"auto"})),
            (json!("required"), json!({"type":"any"})),
            (json!("none"), json!({"type":"none"})),
            (
                json!({"type":"function","function":{"name":"f"}}),
                json!({"type":"tool","name":"f"}),
            ),
        ] {
            let out = converted(json!({"messages":[],"tool_choice":choice}));
            assert_eq!(out["tool_choice"], expected);
        }
        let out = converted(
            json!({"messages":[],"parallel_tool_calls":false,"max_tokens":10,"max_completion_tokens":20,"stop":["a","b"],"temperature":0.5,"top_p":0.9,"stream":true}),
        );
        assert_eq!(out["max_tokens"], 20);
        assert_eq!(
            out["tool_choice"],
            json!({"type":"auto","disable_parallel_tool_use":true})
        );
        assert_eq!(out["stop_sequences"], json!(["a", "b"]));
        assert_eq!(out["temperature"], 0.5);
        assert_eq!(out["stream"], true);
        assert_eq!(
            converted(json!({"messages":[],"max_tokens":10}))["max_tokens"],
            10
        );
    }

    #[test]
    fn invalid_requests_are_rejected() {
        for v in [
            json!({}),
            json!({"messages":[{"role":"user","content":[{"type":"audio"}]}]}),
            json!({"messages":[{"role":"assistant","tool_calls":[{"id":"t","function":{"name":"f","arguments":"{"}}]}]}),
            json!({"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"file:///x"}}]}]}),
        ] {
            assert!(matches!(
                convert_request(v.to_string().as_bytes(), "m", None),
                Err(BuildError::InvalidBody)
            ));
        }
        assert!(convert_request(b"not json", "m", None).is_err());
    }

    #[test]
    fn complete_responses_and_usage() {
        let mut v = json!({"id":"msg_1","model":"claude","content":[{"type":"text","text":"hi"},{"type":"text","text":" there"},{"type":"tool_use","id":"t1","name":"f","input":{"x":1}}],"stop_reason":"tool_use","usage":{"input_tokens":10,"cache_creation_input_tokens":3,"cache_read_input_tokens":2,"output_tokens":4}});
        let out = convert_response(&v);
        assert_eq!(out["choices"][0]["message"]["content"], "hi there");
        assert_eq!(
            out["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            "{\"x\":1}"
        );
        assert_eq!(
            out["usage"],
            json!({
                "prompt_tokens":15,"completion_tokens":4,"total_tokens":19,
                "prompt_tokens_details":{"cached_tokens":2},"cache_creation_input_tokens":3
            })
        );
        for (reason, expected) in [
            ("end_turn", "stop"),
            ("stop_sequence", "stop"),
            ("max_tokens", "length"),
            ("model_context_window_exceeded", "length"),
            ("tool_use", "tool_calls"),
            ("future", "stop"),
        ] {
            v["stop_reason"] = json!(reason);
            assert_eq!(
                convert_response(&v)["choices"][0]["finish_reason"],
                expected
            );
        }
        v["content"] = json!([]);
        assert_eq!(
            convert_response(&v)["choices"][0]["message"]["content"],
            Value::Null
        );
    }

    #[test]
    fn streaming_context_window_exceeded_finishes_with_length() {
        let chunk = StreamState::default()
            .map(event(json!({
                "type":"message_delta",
                "delta":{"stop_reason":"model_context_window_exceeded"},
                "usage":{"output_tokens":5}
            })))
            .unwrap()
            .unwrap();
        let chunk: Value = serde_json::from_str(&chunk.data).unwrap();
        assert_eq!(chunk["choices"][0]["finish_reason"], "length");
    }

    fn event(v: Value) -> SseEvent {
        SseEvent {
            event: v["type"].as_str().map(str::to_owned),
            data: v.to_string(),
        }
    }

    #[test]
    fn incremental_stream_with_tool_and_done() {
        let source = vec![
            json!({"type":"ping"}),
            json!({"type":"message_start","message":{"id":"msg_1","model":"claude","usage":{"input_tokens":10,"cache_read_input_tokens":2}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"t1","name":"f","input":{}}}),
            json!({"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{\"x\":"}}),
            json!({"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"1}"}}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}),
            json!({"type":"message_stop"}),
        ];
        let result = convert_stream(
            Box::pin(futures_util::stream::iter(
                source.into_iter().map(|v| Ok(event(v))),
            )),
            None,
        )
        .collect::<Vec<_>>()
        .now_or_never()
        .unwrap();
        let events: Vec<_> = result.into_iter().collect::<Result<_, _>>().unwrap();
        assert_eq!(events.last().unwrap().data, "[DONE]");
        let chunks: Vec<Value> = events[..events.len() - 1]
            .iter()
            .map(|e| {
                assert!(e.event.is_none());
                serde_json::from_str(&e.data).unwrap()
            })
            .collect();
        assert_eq!(chunks.len(), 6);
        for chunk in &chunks {
            assert_eq!(chunk["id"], "msg_1");
            assert_eq!(chunk["model"], "claude");
            assert_eq!(chunk["created"], chunks[0]["created"]);
        }
        assert_eq!(
            chunks[0]["choices"][0]["delta"],
            json!({"role":"assistant","content":""})
        );
        assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "hi");
        assert_eq!(
            chunks[2]["choices"][0]["delta"]["tool_calls"][0]["index"],
            0
        );
        assert_eq!(
            chunks[3]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "{\"x\":"
        );
        assert_eq!(chunks[5]["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(chunks[5]["usage"]["total_tokens"], 17);
        assert_eq!(
            chunks[5]["usage"]["prompt_tokens_details"]["cached_tokens"],
            2
        );
        let mut state = StreamState::default();
        assert!(matches!(
            state.map(event(
                json!({"type":"error","error":{"type":"overloaded_error"}})
            )),
            Err(UpstreamError::Server { .. })
        ));
        assert!(state
            .map(SseEvent {
                event: None,
                data: "invalid".into()
            })
            .is_err());
        assert!(state
            .map(event(json!({"type":"future"})))
            .unwrap()
            .is_none());
    }

    #[test]
    fn streaming_empty_tool_arguments_are_valid_json() {
        let mut state = StreamState::default();
        for (index, partials) in [(3, vec![]), (5, vec![""]), (8, vec!["", "{\"x\":", "1}"])] {
            let start = state.map(event(json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":format!("t{index}"),"name":"f","input":{}}}))).unwrap().unwrap();
            let start: Value = serde_json::from_str(&start.data).unwrap();
            let tool_index = start["choices"][0]["delta"]["tool_calls"][0]["index"].clone();
            let mut arguments = String::new();
            for partial in partials {
                let delta = state.map(event(json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":partial}}))).unwrap().unwrap();
                let delta: Value = serde_json::from_str(&delta.data).unwrap();
                arguments.push_str(
                    delta["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                        .as_str()
                        .unwrap(),
                );
            }
            let stop = state
                .map(event(json!({"type":"content_block_stop","index":index})))
                .unwrap();
            if index == 8 {
                assert!(stop.is_none());
            } else {
                let stop: Value = serde_json::from_str(&stop.unwrap().data).unwrap();
                assert_eq!(
                    stop["choices"][0]["delta"]["tool_calls"][0]["index"],
                    tool_index
                );
                arguments.push_str(
                    stop["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                        .as_str()
                        .unwrap(),
                );
            }
            let parsed: Value = serde_json::from_str(&arguments).unwrap();
            assert_eq!(
                parsed,
                if index == 8 {
                    json!({"x":1})
                } else {
                    json!({})
                }
            );
            assert!(state
                .map(event(json!({"type":"content_block_stop","index":index})))
                .unwrap()
                .is_none());
        }
        assert!(state
            .map(event(json!({"type":"content_block_stop","index":0})))
            .unwrap()
            .is_none());
    }

    #[test]
    fn anthropic_credit_error_requires_status_type_and_message_prefix() {
        let message = "Your credit balance is too low to access the Anthropic API.";
        let embedded = format!("Invalid input referenced this message: {message}");
        for (case, status, kind, text) in [
            (
                "non-400 status",
                StatusCode::NOT_FOUND,
                "invalid_request_error",
                message,
            ),
            (
                "different error type",
                StatusCode::BAD_REQUEST,
                "not_found_error",
                message,
            ),
            (
                "mid-message phrase",
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                embedded.as_str(),
            ),
        ] {
            let body = Bytes::from(json!({"error": {"type": kind, "message": text}}).to_string());
            let error = client_error(status, body, true);
            assert!(
                matches!(error, UpstreamError::BadRequest { status: actual, .. } if actual == status),
                "{case}: expected unchanged client error for {status}, got {error:?}"
            );
        }
    }

    #[test]
    fn errors_and_limits() {
        let body = Bytes::from_static(
            br#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#,
        );
        let UpstreamError::BadRequest { body, .. } =
            client_error(StatusCode::BAD_REQUEST, body.clone(), true)
        else {
            panic!()
        };
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({"error":{"message":"bad","type":"invalid_request_error"}})
        );
        for status in [StatusCode::BAD_REQUEST, StatusCode::PAYLOAD_TOO_LARGE] {
            assert!(matches!(
                client_error(
                    status,
                    Bytes::from_static(
                        br#"{"error":{"message":"prompt is too long: 200 tokens"}}"#
                    ),
                    true
                ),
                UpstreamError::ContextLength
            ));
        }
        let raw = Bytes::from_static(b"bad");
        let UpstreamError::BadRequest { body, .. } =
            client_error(StatusCode::BAD_REQUEST, raw.clone(), false)
        else {
            panic!()
        };
        assert_eq!(raw, body);
        let a = Anthropic::new(provider::ANTHROPIC);
        assert!(a.observe_limits(&HeaderMap::new()).is_none());
        let mut headers = HeaderMap::new();
        headers.insert(
            "anthropic-ratelimit-requests-remaining",
            HeaderValue::from_static("3"),
        );
        headers.insert(
            "anthropic-ratelimit-tokens-remaining",
            HeaderValue::from_static("100"),
        );
        let limits = a.observe_limits(&headers).unwrap();
        assert_eq!(limits.remaining_requests, Some(3));
        assert_eq!(limits.remaining_tokens, Some(100));
    }

    #[test]
    fn auth_passthrough_and_support() {
        let a = Anthropic::new(provider::ANTHROPIC);
        let mut dep = Deployment {
            headers: Default::default(),
            fallback: false,
            idx: DeploymentIdx(0),
            id: "a".into(),
            provider: a.kind().clone(),
            credential: "key".into(),
            base_url: "https://example.com/v1/".parse().unwrap(),
            upstream_model: "claude".into(),
            strip_request_fields: Box::default(),
            priority: 0,
            quota: QuotaSpec::default(),
            timeout: Duration::from_secs(10),
            max_context: None,
            expires_at: None,
            priority_boost: None,
            priority_boost_until: None,
            billing: Billing::Billed,
        };
        let mut req = GatewayRequest {
            format: InboundFormat::AnthropicMessages,
            model: "alias".into(),
            stream: true,
            body: Bytes::from_static(br#"{ "model":"alias", "future":{"model":"untouched"} }"#),
            anthropic_headers: HeaderMap::new(),
            est_input_tokens: 0,
            max_output_tokens: None,
            context_overflow: None,
            request_id: RequestId("r".into()),
            end_user: None,
            client_key: None,
            image_count: 0,
            free_only: false,
        };
        dep.headers
            .insert("x-deployment", HeaderValue::from_static("static-value"));
        for auth in [
            AuthMaterial::Bearer("secret".into()),
            AuthMaterial::Header {
                name: http::HeaderName::from_static("custom"),
                value: HeaderValue::from_static("secret"),
            },
            AuthMaterial::None,
        ] {
            let request = a
                .build_request(&req, &dep, &auth)
                .now_or_never()
                .unwrap()
                .unwrap();
            assert_eq!(request.uri(), "https://example.com/v1/messages");
            assert_eq!(request.headers()["x-deployment"], "static-value");
            assert_eq!(
                request.headers()[header::USER_AGENT],
                concat!("llmux/", env!("CARGO_PKG_VERSION"))
            );
            assert_eq!(request.method(), Method::POST);
            assert_eq!(request.headers()["anthropic-version"], "2023-06-01");
            assert_eq!(request.headers()[header::ACCEPT], "text/event-stream");
            assert_eq!(
                request.body().as_ref(),
                br#"{ "model":"claude", "future":{"model":"untouched"} }"#
            );
            match auth {
                AuthMaterial::Bearer(_) => {
                    assert_eq!(request.headers()["x-api-key"], "secret");
                    assert!(request.headers()["x-api-key"].is_sensitive());
                }
                AuthMaterial::Header { .. } => assert!(request.headers()["custom"].is_sensitive()),
                AuthMaterial::None => assert!(!request.headers().contains_key("x-api-key")),
            }
        }
        assert_eq!(a.supports(InboundFormat::OpenAiChat), Support::Convert);
        assert_eq!(
            a.supports(InboundFormat::AnthropicMessages),
            Support::Passthrough
        );
        req.format = InboundFormat::OpenAiResponses;
        assert!(matches!(
            a.build_request(&req, &dep, &AuthMaterial::None)
                .now_or_never()
                .unwrap(),
            Err(BuildError::UnsupportedFormat)
        ));
        dep.upstream_model = "*".into();
        req.format = InboundFormat::AnthropicMessages;
        let request = a
            .build_request(&req, &dep, &AuthMaterial::None)
            .now_or_never()
            .unwrap()
            .unwrap();
        assert_eq!(
            request.body().as_ref(),
            br#"{ "model":"alias", "future":{"model":"untouched"} }"#
        );
        req.format = InboundFormat::OpenAiChat;
        req.body =
            Bytes::from_static(br#"{"model":"alias","messages":[{"role":"user","content":"hi"}]}"#);
        let request = a
            .build_request(&req, &dep, &AuthMaterial::None)
            .now_or_never()
            .unwrap()
            .unwrap();
        let body: Value = serde_json::from_slice(request.body()).unwrap();
        assert_eq!(body["model"], "alias");
    }
    fn rejection(message: &str) -> UpstreamError {
        UpstreamError::BadRequest {
            status: StatusCode::BAD_REQUEST,
            body: Bytes::from(
                json!({"type":"error","error":{"type":"invalid_request_error","message":message}})
                    .to_string(),
            ),
        }
    }

    #[test]
    fn strip_request_fields_applies_to_converted_and_passthrough_bodies() {
        let a = Anthropic::new(provider::ANTHROPIC);
        let dep = Deployment {
            headers: Default::default(),
            fallback: false,
            idx: DeploymentIdx(0),
            id: "a".into(),
            provider: a.kind().clone(),
            credential: "key".into(),
            base_url: "https://example.com/v1/".parse().unwrap(),
            upstream_model: "claude".into(),
            strip_request_fields: Box::new([
                llmux_types::ChatRequestField::Temperature,
                llmux_types::ChatRequestField::TopP,
                llmux_types::ChatRequestField::TopK,
            ]),
            priority: 0,
            quota: QuotaSpec::default(),
            timeout: Duration::from_secs(10),
            max_context: None,
            expires_at: None,
            priority_boost: None,
            priority_boost_until: None,
            billing: Billing::Billed,
        };
        for (format, body) in [
            (
                InboundFormat::OpenAiChat,
                r#"{"model":"m","temperature":0.7,"top_p":0.9,"messages":[{"role":"user","content":"hi"}]}"#,
            ),
            (
                InboundFormat::AnthropicMessages,
                r#"{"model":"m","max_tokens":5,"temperature":0.7,"top_p":0.9,"top_k":4,"messages":[]}"#,
            ),
        ] {
            let req = GatewayRequest {
                format,
                model: "m".into(),
                stream: false,
                body: Bytes::from_static(body.as_bytes()),
                anthropic_headers: HeaderMap::new(),
                est_input_tokens: 0,
                max_output_tokens: None,
                context_overflow: None,
                request_id: RequestId("r".into()),
                end_user: None,
                client_key: None,
                image_count: 0,
                free_only: false,
            };
            let request = a
                .build_request(&req, &dep, &AuthMaterial::None)
                .now_or_never()
                .unwrap()
                .unwrap();
            let sent: Value = serde_json::from_slice(request.body()).unwrap();
            for field in ["temperature", "top_p", "top_k"] {
                assert!(sent.get(field).is_none(), "{format:?} kept {field}");
            }
            assert_eq!(sent["model"], "claude");
        }
    }

    #[test]
    fn deprecated_parameter_rejections_drop_only_the_named_field() {
        let a = Anthropic::new(provider::ANTHROPIC);
        let body = Bytes::from_static(br#"{"model":"m","temperature":0.7,"top_p":0.9,"top_k":4}"#);
        for (field, kept) in [
            ("temperature", ["top_p", "top_k"]),
            ("top_p", ["temperature", "top_k"]),
            ("top_k", ["temperature", "top_p"]),
        ] {
            let message = format!("`{field}` is deprecated for this model.");
            let repaired = a.repair_rejected(&body, &rejection(&message)).unwrap();
            let repaired: Value = serde_json::from_slice(&repaired).unwrap();
            assert!(repaired.get(field).is_none());
            assert!(kept.iter().all(|key| repaired.get(key).is_some()));
            assert_eq!(repaired["model"], "m");
        }
    }

    #[test]
    fn unrelated_or_unactionable_rejections_are_not_repaired() {
        let a = Anthropic::new(provider::ANTHROPIC);
        let body = Bytes::from_static(br#"{"model":"m","temperature":0.7}"#);
        assert!(a
            .repair_rejected(&body, &rejection("max_tokens: field required"))
            .is_none());
        assert!(a.repair_rejected(&body, &UpstreamError::Timeout).is_none());
        let without = Bytes::from_static(br#"{"model":"m"}"#);
        assert!(a
            .repair_rejected(
                &without,
                &rejection("`temperature` is deprecated for this model.")
            )
            .is_none());
    }

    #[test]
    fn end_user_hash_overrides_client_user_id_on_convert_and_passthrough() {
        let a = Anthropic::new(provider::ANTHROPIC);
        let dep = Deployment {
            headers: Default::default(),
            fallback: false,
            idx: DeploymentIdx(0),
            id: "a".into(),
            provider: a.kind().clone(),
            credential: "key".into(),
            base_url: "https://example.com/v1/".parse().unwrap(),
            upstream_model: "claude".into(),
            strip_request_fields: Box::new([llmux_types::ChatRequestField::Temperature]),
            priority: 0,
            quota: QuotaSpec::default(),
            timeout: Duration::from_secs(10),
            max_context: None,
            expires_at: None,
            priority_boost: None,
            priority_boost_until: None,
            billing: Billing::Billed,
        };
        let hash = "31ca399bfc61dac0b442a6bef7c00cd06cf9682c11b9192fb26a918b4a9130f8";
        let raw = "raw-client-SHOULD-NOT-LEAK";
        let build = |format, body: &str, end_user: Option<&str>| {
            let req = GatewayRequest {
                format,
                model: "m".into(),
                stream: false,
                body: Bytes::from(body.to_owned()),
                anthropic_headers: HeaderMap::new(),
                est_input_tokens: 0,
                max_output_tokens: None,
                context_overflow: None,
                request_id: RequestId("r".into()),
                end_user: end_user.map(Arc::from),
                client_key: None,
                image_count: 0,
                free_only: false,
            };
            let request = a
                .build_request(&req, &dep, &AuthMaterial::None)
                .now_or_never()
                .unwrap()
                .unwrap();
            serde_json::from_slice::<Value>(request.body()).unwrap()
        };
        let converted = build(
            InboundFormat::OpenAiChat,
            &format!(
                r#"{{"model":"m","temperature":0.2,"user":"{raw}","messages":[{{"role":"user","content":"hi"}}]}}"#
            ),
            Some(hash),
        );
        assert_eq!(converted["metadata"]["user_id"], hash);
        assert!(!converted.to_string().contains(raw));
        assert!(converted.get("temperature").is_none());
        assert!(converted.get("user").is_none());

        let passed = build(
            InboundFormat::AnthropicMessages,
            &format!(
                r#"{{"model":"alias","metadata":{{"user_id":"{raw}","account":"acme"}},"messages":[]}}"#
            ),
            Some(hash),
        );
        assert_eq!(passed["metadata"]["user_id"], hash);
        assert!(!passed.to_string().contains(raw));
        assert_eq!(passed["metadata"]["account"], "acme");
        assert_eq!(passed["model"], "claude");

        let kept = build(
            InboundFormat::OpenAiChat,
            &format!(
                r#"{{"model":"m","user":"{raw}","messages":[{{"role":"user","content":"hi"}}]}}"#
            ),
            None,
        );
        assert_eq!(kept["metadata"]["user_id"], raw);
    }
}
