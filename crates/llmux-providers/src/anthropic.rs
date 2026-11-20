use crate::{
    openai_compat::{bad_gateway, collect, retry_after, rewrite_model},
    sse,
};
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt;
use http::{header, HeaderMap, HeaderValue, Method, Request, Response, StatusCode};
use hyper::body::Incoming;
use llmux_types::{
    AuthMaterial, BoxStream, BuildError, Deployment, GatewayRequest, GatewayResponse,
    InboundFormat, LimitObservation, LimitScope, ProviderAdapter, ProviderKind, SseEvent, Support,
    UpstreamError, Usage,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

pub struct Anthropic(ProviderKind);

impl Anthropic {
    pub fn new(kind: impl Into<Arc<str>>) -> Self {
        Self(ProviderKind(kind.into()))
    }
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

fn convert_request(body: &[u8], model: &str) -> Result<Bytes, BuildError> {
    let input: Value = serde_json::from_slice(body).map_err(|_| BuildError::InvalidBody)?;
    if input["n"].as_u64().is_some_and(|n| n > 1) {
        return Err(BuildError::InvalidBody);
    }
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
    if let Some(user) = input.get("user") {
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

fn usage(v: &Value) -> Usage {
    let count = |k: &str| v[k].as_u64().unwrap_or(0).min(u32::MAX as u64) as u32;
    Usage {
        input_tokens: count("input_tokens")
            .saturating_add(count("cache_creation_input_tokens"))
            .saturating_add(count("cache_read_input_tokens")),
        output_tokens: count("output_tokens"),
    }
}
fn usage_json(u: Usage) -> Value {
    json!({"prompt_tokens":u.input_tokens,"completion_tokens":u.output_tokens,"total_tokens":u64::from(u.input_tokens)+u64::from(u.output_tokens)})
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
    json!({"id":v["id"],"object":"chat.completion","created":now(),"model":v["model"],"choices":[{"index":0,"message":message,"finish_reason":finish(&v["stop_reason"])}],"usage":usage_json(usage(&v["usage"]))})
}

fn client_error(status: StatusCode, body: Bytes, convert: bool) -> UpstreamError {
    let value = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
    if matches!(status.as_u16(), 400 | 413)
        && value["error"]["message"]
            .as_str()
            .is_some_and(|s| s.contains("prompt is too long"))
    {
        return UpstreamError::ContextLength;
    }
    let body = if convert {
        Bytes::from(json!({"error":{"message":value["error"]["message"].as_str().unwrap_or("Upstream request failed"),"type":value["error"]["type"].as_str().unwrap_or("invalid_request_error")}}).to_string())
    } else {
        body
    };
    UpstreamError::BadRequest { status, body }
}

#[derive(Default)]
struct StreamTool {
    index: usize,
    has_arguments: bool,
}

#[derive(Default)]
struct StreamState {
    id: Value,
    model: Value,
    created: u64,
    tools: HashMap<u64, StreamTool>,
    usage: Usage,
}
impl StreamState {
    fn map(&mut self, event: SseEvent) -> Result<Option<SseEvent>, UpstreamError> {
        let v: Value = serde_json::from_str(&event.data).map_err(|_| bad_gateway())?;
        let kind = event.event.as_deref().or_else(|| v["type"].as_str());
        let (delta, reason) = match kind {
            Some("message_start") => {
                self.id = v["message"]["id"].clone();
                self.model = v["message"]["model"].clone();
                self.created = now();
                self.usage = usage(&v["message"]["usage"]);
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
                self.usage.output_tokens = usage(&v["usage"]).output_tokens;
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
        let mut chunk = json!({"id":self.id,"object":"chat.completion.chunk","created":self.created,"model":self.model,"choices":[{"index":0,"delta":delta,"finish_reason":reason}]});
        if kind == Some("message_delta") {
            chunk["usage"] = usage_json(self.usage);
        }
        Ok(Some(SseEvent {
            event: None,
            data: chunk.to_string(),
        }))
    }
}

fn convert_stream(
    mut events: BoxStream<'static, Result<SseEvent, UpstreamError>>,
) -> BoxStream<'static, Result<SseEvent, UpstreamError>> {
    Box::pin(async_stream::try_stream! {
        let mut state = StreamState::default();
        while let Some(event) = events.next().await {
            if let Some(event) = state.map(event?)? {
                let done = event.data == "[DONE]";
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
            Support::Passthrough => rewrite_model(&req.body, &dep.upstream_model)?,
            Support::Convert => convert_request(&req.body, &dep.upstream_model)?,
        };
        let mut parts = dep.base_url.clone().into_parts();
        parts.path_and_query =
            Some(format!("{}/messages", dep.base_url.path().trim_end_matches('/')).parse()?);
        let uri = http::Uri::from_parts(parts).map_err(|e| BuildError::InvalidRequest(e.into()))?;
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
        let auth = match auth {
            AuthMaterial::Bearer(key) => Some((
                http::HeaderName::from_static("x-api-key"),
                HeaderValue::from_str(key).map_err(|_| BuildError::InvalidAuth)?,
            )),
            AuthMaterial::Header { name, value } => Some((name.clone(), value.clone())),
            AuthMaterial::None => None,
        };
        if let Some((name, mut value)) = auth {
            value.set_sensitive(true);
            request.headers_mut().insert(name, value);
        }
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
                    retry_after: retry_after(&parts.headers, SystemTime::now()),
                    scope: LimitScope::Unknown,
                })
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                return Err(UpstreamError::Auth { refreshable: false })
            }
            s if s.is_client_error() => return Err(client_error(s, collect(body).await?, convert)),
            s if s.is_server_error() => return Err(UpstreamError::Server { status: s }),
            s if !s.is_success() => return Err(bad_gateway()),
            _ => {}
        }
        if req.stream {
            if !parts
                .headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|s| {
                    s.split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .eq_ignore_ascii_case("text/event-stream")
                })
            {
                return Err(bad_gateway());
            }
            let events = sse::decode_until(body, true);
            return Ok(GatewayResponse::Stream(if convert {
                convert_stream(events)
            } else {
                events
            }));
        }
        let mut body = collect(body).await?;
        let parsed = serde_json::from_slice::<Value>(&body);
        let usage = parsed.as_ref().ok().and_then(|v| v.get("usage")).map(usage);
        if convert {
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
            usage,
        })
    }
    fn observe_limits(&self, headers: &HeaderMap) -> Option<LimitObservation> {
        let count = |name| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u32>().ok())
        };
        let remaining_requests = count("anthropic-ratelimit-requests-remaining");
        let remaining_tokens = count("anthropic-ratelimit-tokens-remaining");
        if remaining_requests.is_none() && remaining_tokens.is_none() {
            return None;
        }
        Some(LimitObservation {
            remaining_requests,
            remaining_tokens,
            ..LimitObservation::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;
    use llmux_types::{Billing, DeploymentIdx, QuotaSpec, RequestId};
    use std::time::Duration;

    fn converted(v: Value) -> Value {
        serde_json::from_slice(&convert_request(v.to_string().as_bytes(), "claude").unwrap())
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
            "m"
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
            json!({"messages":[],"n":2}),
            json!({"messages":[{"role":"user","content":[{"type":"audio"}]}]}),
            json!({"messages":[{"role":"assistant","tool_calls":[{"id":"t","function":{"name":"f","arguments":"{"}}]}]}),
            json!({"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"file:///x"}}]}]}),
        ] {
            assert!(matches!(
                convert_request(v.to_string().as_bytes(), "m"),
                Err(BuildError::InvalidBody)
            ));
        }
        assert!(convert_request(b"not json", "m").is_err());
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
            json!({"prompt_tokens":15,"completion_tokens":4,"total_tokens":19})
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
        let result = convert_stream(Box::pin(futures_util::stream::iter(
            source.into_iter().map(|v| Ok(event(v))),
        )))
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
        let a = Anthropic::new("anthropic");
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
        let a = Anthropic::new("anthropic");
        let dep = Deployment {
            idx: DeploymentIdx(0),
            id: "a".into(),
            provider: a.kind().clone(),
            credential: "key".into(),
            base_url: "https://example.com/v1/".parse().unwrap(),
            upstream_model: "claude".into(),
            priority: 0,
            weight: 1,
            billing: Billing::default(),
            quota: QuotaSpec::default(),
            timeout: Duration::from_secs(10),
            max_context: None,
        };
        let mut req = GatewayRequest {
            format: InboundFormat::AnthropicMessages,
            model: "alias".into(),
            stream: true,
            body: Bytes::from_static(br#"{ "model":"alias", "future":{"model":"untouched"} }"#),
            parsed: Default::default(),
            est_input_tokens: 0,
            request_id: RequestId("r".into()),
        };
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
    }
}
