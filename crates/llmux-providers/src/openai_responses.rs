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
        let body = normalize(&req.body, crate::upstream_model(req, dep), req.format)?;
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
        if !sse::is_event_stream(&parts.headers) {
            return Err(bad_gateway());
        }
        let stream = validated(sse::decode(body));
        if req.stream {
            return Ok(GatewayResponse::Stream(
                if req.format == InboundFormat::OpenAiChat {
                    let include_usage = serde_json::from_slice::<Value>(&req.body)
                        .map_err(|_| bad_gateway())?["stream_options"]["include_usage"]
                        == true;
                    chat_stream(stream, include_usage)
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
                let response = if req.format == InboundFormat::OpenAiChat {
                    chat_complete(response)?
                } else {
                    response.clone()
                };
                return Ok(GatewayResponse::Complete {
                    status: StatusCode::OK,
                    headers: HeaderMap::new(),
                    body: Bytes::from(response.to_string()),
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
        let has_refusal = role == "assistant"
            && (!message["refusal"].is_null()
                || message["content"]
                    .as_array()
                    .is_some_and(|parts| parts.iter().any(|part| part["type"] == "refusal")));
        if has_refusal {
            input.push(refusal_message(message, input.len())?);
        } else if !message["content"].is_null() {
            let content = if let Some(parts) = message["content"].as_array() {
                let mut content = Vec::new();
                for part in parts {
                    content.push(match part["type"].as_str() {
                        Some("text") => json!({"type":"input_text","text":part["text"].as_str().ok_or(BuildError::InvalidBody)?}),
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
            function.entry("strict").or_insert(json!(false));
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

fn refusal_message(message: &Value, index: usize) -> Result<Value, BuildError> {
    let mut content = Vec::new();
    if let Some(text) = message["content"].as_str() {
        content.push(json!({"type":"output_text","text":text,"annotations":[]}));
    } else if !message["content"].is_null() {
        for part in message["content"]
            .as_array()
            .ok_or(BuildError::InvalidBody)?
        {
            content.push(match part["type"].as_str() {
                Some("text") => json!({"type":"output_text","text":part["text"].as_str().ok_or(BuildError::InvalidBody)?,"annotations":[]}),
                Some("refusal") => json!({"type":"refusal","refusal":part["refusal"].as_str().ok_or(BuildError::InvalidBody)?}),
                _ => return Err(BuildError::InvalidBody),
            });
        }
    }
    if !message["refusal"].is_null() {
        content.push(json!({"type":"refusal","refusal":message["refusal"].as_str().ok_or(BuildError::InvalidBody)?}));
    }
    Ok(
        json!({"type":"message","id":format!("msg_llmux_history_{index}"),"role":"assistant","status":"completed","content":content}),
    )
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

fn chat_event(mut value: Value, include_usage: bool) -> SseEvent {
    if include_usage && value.get("usage").is_none() {
        value["usage"] = Value::Null;
    }
    SseEvent {
        event: None,
        data: value.to_string(),
    }
}

fn chat_stream(
    mut upstream: BoxStream<'static, Result<SseEvent, UpstreamError>>,
    include_usage: bool,
) -> BoxStream<'static, Result<SseEvent, UpstreamError>> {
    Box::pin(async_stream::try_stream! {
        let mut response = json!({});
        let mut calls = std::collections::HashMap::new();
        while let Some(event) = upstream.next().await {
            let value: Value = serde_json::from_str(&event?.data).map_err(|_| bad_gateway())?;
            match value["type"].as_str().unwrap_or("") {
                "response.created" => { response = value["response"].clone(); yield chat_event(chunk(&response, json!({"role":"assistant","content":""}), Value::Null), include_usage); },
                "response.output_text.delta" => { yield chat_event(chunk(&response, json!({"content":value["delta"]}), Value::Null), include_usage); },
                "response.refusal.delta" => { yield chat_event(chunk(&response, json!({"refusal":value["delta"]}), Value::Null), include_usage); },
                "response.output_item.added" if value["item"]["type"] == "function_call" => {
                    let index = calls.len();
                    calls.insert(value["output_index"].as_u64().ok_or_else(bad_gateway)?, index);
                    yield chat_event(chunk(&response, json!({"tool_calls":[{"index":index,"id":value["item"]["call_id"],"type":"function","function":{"name":value["item"]["name"],"arguments":value["item"]["arguments"].as_str().unwrap_or("")}}]}), Value::Null), include_usage);
                },
                "response.function_call_arguments.delta" => {
                    let index = calls.get(&value["output_index"].as_u64().ok_or_else(bad_gateway)?).ok_or_else(bad_gateway)?;
                    yield chat_event(chunk(&response, json!({"tool_calls":[{"index":index,"function":{"arguments":value["delta"]}}]}), Value::Null), include_usage);
                },
                "response.completed" => {
                    response = value["response"].clone();
                    yield chat_event(chunk(&response, json!({}), json!(if calls.is_empty() {"stop"} else {"tool_calls"})), include_usage);
                    if include_usage {
                        let mut last = chunk(&response, json!({}), Value::Null);
                        last["choices"] = json!([]);
                        last["usage"] = chat_usage(&response);
                        yield chat_event(last, include_usage);
                    }
                    yield SseEvent {event:None,data:"[DONE]".into()};
                    return;
                },
                _ => {},
            }
        }
        Err(bad_gateway())?;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;
    use llmux_types::{DeploymentIdx, QuotaSpec, RequestId};
    use std::time::Duration;

    #[test]
    fn wildcard_deployment_forwards_requested_model() {
        let adapter = OpenAiResponses::new();
        let mut dep = Deployment {
            idx: DeploymentIdx(0),
            id: "r".into(),
            provider: adapter.kind().clone(),
            credential: "key".into(),
            base_url: "https://example.com/v1".parse().unwrap(),
            upstream_model: "fixed".into(),
            priority: 0,
            quota: QuotaSpec::default(),
            timeout: Duration::from_secs(10),
            max_context: None,
        };
        for (format, body) in [
            (
                InboundFormat::OpenAiResponses,
                json!({"model":"vendor/New-ID","input":"hi"}),
            ),
            (
                InboundFormat::OpenAiChat,
                json!({"model":"vendor/New-ID","messages":[{"role":"user","content":"hi"}]}),
            ),
        ] {
            let req = GatewayRequest {
                format,
                model: "vendor/New-ID".into(),
                stream: false,
                body: Bytes::from(body.to_string()),
                est_input_tokens: 0,
                max_output_tokens: None,
                context_overflow: None,
                request_id: RequestId("r".into()),
            };
            for (upstream, expected) in [("fixed", "fixed"), ("*", "vendor/New-ID")] {
                dep.upstream_model = upstream.into();
                let request = adapter
                    .build_request(&req, &dep, &AuthMaterial::None)
                    .now_or_never()
                    .unwrap()
                    .unwrap();
                let sent: Value = serde_json::from_slice(request.body()).unwrap();
                assert_eq!(sent["model"], expected, "{format:?} {upstream}");
            }
        }
    }

    #[test]
    fn chat_options_and_multipart_instructions_are_converted() {
        let schema = json!({"name":"answer","schema":{"type":"object"},"strict":true});
        let converted = chat_request(&json!({
            "messages":[
                {"role":"system","content":[{"type":"text","text":"First"},{"type":"text","text":"Second"}]},
                {"role":"system","content":"Third"},
                {"role":"developer","content":"Guide"},
                {"role":"tool","tool_call_id":"c1","content":[{"type":"text","text":"Result"}]}
            ],
            "tool_choice":{"type":"function","function":{"name":"lookup"}},
            "parallel_tool_calls":false,
            "reasoning_effort":"high",
            "response_format":{"type":"json_schema","json_schema":schema}
        })).unwrap();
        assert_eq!(converted["instructions"], "First\nSecond\n\nThird");
        assert_eq!(
            converted["input"],
            json!([
                {"role":"developer","content":"Guide"},
                {"type":"function_call_output","call_id":"c1","output":"Result"}
            ])
        );
        assert_eq!(
            converted["tool_choice"],
            json!({"type":"function","name":"lookup"})
        );
        assert_eq!(converted["parallel_tool_calls"], false);
        assert_eq!(converted["reasoning"], json!({"effort":"high"}));
        assert_eq!(
            converted["text"]["format"],
            json!({"type":"json_schema","name":"answer","schema":{"type":"object"},"strict":true})
        );
        for choice in ["auto", "none", "required"] {
            let converted = chat_request(&json!({"messages":[],"tool_choice":choice,"reasoning":{"effort":"low"},"response_format":{"type":"json_object"}})).unwrap();
            assert_eq!(converted["tool_choice"], choice);
            assert_eq!(converted["reasoning"], json!({"effort":"low"}));
            assert_eq!(converted["text"]["format"], json!({"type":"json_object"}));
            assert_eq!(converted["input"], json!([]));
        }
    }

    #[test]
    fn invalid_chat_history_and_options_are_client_errors() {
        for body in [
            json!({"messages":[{"role":"unknown","content":"text"}]}),
            json!({"messages":[{"role":"user","content":[{"type":"audio"}]}]}),
            json!({"messages":[{"role":"assistant","tool_calls":[{"type":"custom"}]}]}),
            json!({"messages":[{"role":"system","content":[{"text":5}]}]}),
            json!({"messages":[],"tools":[{"type":"web_search"}]}),
            json!({"messages":[],"tool_choice":{"type":"unsupported"}}),
            json!({"messages":[],"response_format":{"type":"json_schema","json_schema":null}}),
            json!({"messages":[{"role":"assistant","refusal":"No","content":[{"type":"image_url"}]}]}),
        ] {
            assert!(
                matches!(chat_request(&body), Err(BuildError::InvalidBody)),
                "{body}"
            );
        }
    }

    #[test]
    fn nested_responses_tools_are_validated() {
        let body = json!({"input":[{"type":"additional_tools","tools":[{"type":"namespace","tools":[{"type":"function","name":"lookup"}]}]}],"tools":[{"type":"custom","name":"custom"},{"type":"web_search"},{"type":"web_search_preview"}]});
        let normalized = normalize(
            &serde_json::to_vec(&body).unwrap(),
            "model",
            InboundFormat::OpenAiResponses,
        )
        .unwrap();
        assert_eq!(normalized["input"], body["input"]);
        assert_eq!(normalized["tools"], body["tools"]);
        assert_eq!(
            normalize(b"{}", "model", InboundFormat::OpenAiResponses).unwrap()["input"],
            json!([])
        );
        for body in [
            json!([]),
            json!({"input":42}),
            json!({"tools":[{"type":"namespace"}]}),
            json!({"input":[{"type":"additional_tools","tools":[{"type":"namespace","tools":[{"type":"file_search"}]}]}]}),
        ] {
            assert!(matches!(
                normalize(
                    &serde_json::to_vec(&body).unwrap(),
                    "model",
                    InboundFormat::OpenAiResponses
                ),
                Err(BuildError::InvalidBody)
            ));
        }
    }

    #[test]
    fn stream_errors_keep_quota_and_rate_limit_classification() {
        for code in [
            "subscription_sharing_usage_limit_exceeded",
            "subscription_sharing_usage_unavailable",
            "insufficient_quota",
        ] {
            assert!(matches!(
                stream_error(&json!({"response":{"error":{"code":code}}})),
                UpstreamError::QuotaExhausted { .. }
            ));
        }
        assert!(matches!(
            stream_error(&json!({"error":{"code":"rate_limit_exceeded"}})),
            UpstreamError::RateLimited { .. }
        ));
        assert!(matches!(
            stream_error(&json!({"code":"server_error"})),
            UpstreamError::Server {
                status: StatusCode::BAD_GATEWAY
            }
        ));
    }

    #[test]
    fn chat_completion_preserves_refusals_and_rejects_malformed_output() {
        let complete = chat_complete(&json!({"id":"r1","model":"model","created_at":1,"output":[{"type":"message","content":[{"type":"output_text","text":"Sorry"},{"type":"refusal","refusal":"No"}]}]})).unwrap();
        assert_eq!(
            complete["choices"][0]["message"],
            json!({"role":"assistant","content":"Sorry","refusal":"No"})
        );
        assert_eq!(complete["choices"][0]["finish_reason"], "stop");
        assert!(complete["usage"].is_null());
        assert!(
            chat_complete(&json!({"output":[]})).unwrap()["choices"][0]["message"]["content"]
                .is_null()
        );
        for response in [
            json!({}),
            json!({"output":[{"type":"message","content":null}]}),
            json!({"output":[{"type":"message","content":[{"type":"refusal","refusal":5}]}]}),
        ] {
            assert!(chat_complete(&response).is_err());
        }
    }

    #[test]
    fn chat_function_strictness_preserves_optional_arguments() {
        for strict in [None, Some(false), Some(true)] {
            let mut function = json!({"name":"test","parameters":{"type":"object","properties":{"optional":{"type":"string"}},"required":[]}});
            if let Some(strict) = strict {
                function["strict"] = json!(strict);
            }
            let value = chat_request(
                &json!({"messages":[],"tools":[{"type":"function","function":function.clone()}]}),
            )
            .unwrap();
            assert_eq!(value["tools"][0]["strict"], json!(strict.unwrap_or(false)));
            assert_eq!(value["tools"][0]["parameters"], function["parameters"]);
        }
        assert_eq!(
            chat_request(&json!({"messages":[],"tools":[]})).unwrap()["tools"],
            json!([])
        );
    }

    #[test]
    fn assistant_text_parts_use_valid_input_content() {
        let value = chat_request(&json!({"messages":[
            {"role":"assistant","content":"Hello"},
            {"role":"assistant","content":[{"type":"text","text":"Hello"}]},
            {"role":"assistant","content":[]},
        ]}))
        .unwrap();
        assert_eq!(
            value["input"][0],
            json!({"role":"assistant","content":"Hello"})
        );
        assert_eq!(
            value["input"][1],
            json!({"role":"assistant","content":[{"type":"input_text","text":"Hello"}]})
        );
        assert_eq!(value["input"][2], json!({"role":"assistant","content":[]}));
        assert!(chat_request(
            &json!({"messages":[{"role":"assistant","content":[{"type":"text","text":5}]}]})
        )
        .is_err());
    }

    #[test]
    fn assistant_refusals_survive_chat_history_conversion() {
        for message in [
            json!({"role":"assistant","content":null,"refusal":"No"}),
            json!({"role":"assistant","content":[{"type":"refusal","refusal":"No"}]}),
            json!({"role":"assistant","content":"Sorry","refusal":"No"}),
            json!({"role":"assistant","content":[{"type":"text","text":"Sorry"},{"type":"refusal","refusal":"No"}]}),
            json!({"role":"assistant","content":null,"refusal":""}),
        ] {
            let result = chat_request(&json!({"messages":[message.clone()]})).unwrap();
            assert_eq!(result["input"].as_array().unwrap().len(), 1);
            let item = &result["input"][0];
            assert_eq!(item["type"], "message");
            assert_eq!(item["role"], "assistant");
            assert_eq!(item["status"], "completed");
            assert!(item["id"].as_str().unwrap().starts_with("msg_"));
            let parts = item["content"].as_array().unwrap();
            let refusal = parts.last().unwrap();
            assert_eq!(refusal["type"], "refusal");
            assert_eq!(
                refusal["refusal"],
                if message["refusal"] == "" { "" } else { "No" }
            );
            if parts.len() == 2 {
                assert_eq!(
                    parts[0],
                    json!({"type":"output_text","text":"Sorry","annotations":[]})
                );
            }
        }
        assert!(chat_request(&json!({"messages":[{"role":"assistant","refusal":42}]})).is_err());
    }
}
