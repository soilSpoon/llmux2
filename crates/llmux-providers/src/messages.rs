use bytes::Bytes;
use futures_util::StreamExt;
use llmux_types::{BoxStream, BuildError, SseEvent, UpstreamError, Usage};
use serde_json::{json, Value};
use std::collections::BTreeMap;

use crate::openai_compat::bad_gateway;

fn text(value: &Value) -> Result<&str, BuildError> {
    value.as_str().ok_or(BuildError::InvalidBody)
}

fn blocks(value: &Value) -> Result<&[Value], BuildError> {
    value
        .as_array()
        .map(Vec::as_slice)
        .ok_or(BuildError::InvalidBody)
}

fn text_content(value: &Value) -> Result<String, BuildError> {
    if let Some(text) = value.as_str() {
        return Ok(text.into());
    }
    let mut result = String::new();
    for block in blocks(value)? {
        if block["type"] != "text" {
            return Err(BuildError::InvalidBody);
        }
        result.push_str(text(&block["text"])?);
    }
    Ok(result)
}

fn messages(value: &Value) -> Result<Vec<Value>, BuildError> {
    let mut result = Vec::new();
    for message in blocks(value)? {
        let role = text(&message["role"])?;
        if !matches!(role, "user" | "assistant") {
            return Err(BuildError::InvalidBody);
        }
        let content = &message["content"];
        if content.is_string() {
            result.push(json!({"role":role,"content":content}));
            continue;
        }
        let mut parts = Vec::new();
        let mut calls = Vec::new();
        for block in blocks(content)? {
            match block["type"].as_str() {
                Some("text") => parts.push(json!({"type":"text","text":text(&block["text"])?})),
                Some("image") if role == "user" => {
                    let source = &block["source"];
                    let url = match source["type"].as_str() {
                        Some("url") => text(&source["url"])?.to_owned(),
                        Some("base64") => format!("data:{};base64,{}", text(&source["media_type"])?, text(&source["data"])?),
                        _ => return Err(BuildError::InvalidBody),
                    };
                    parts.push(json!({"type":"image_url","image_url":{"url":url}}));
                }
                Some("tool_use") if role == "assistant" && block["input"].is_object() => calls.push(json!({
                    "id":text(&block["id"])?, "type":"function",
                    "function":{"name":text(&block["name"])?,"arguments":block["input"].to_string()},
                })),
                Some("tool_result") if role == "user" => {
                    result.push(json!({"role":"tool","tool_call_id":text(&block["tool_use_id"])?,
                        "content":if block["content"].is_null() { String::new() } else { text_content(&block["content"])? }
                    }));
                }
                _ => return Err(BuildError::InvalidBody),
            }
        }
        if !parts.is_empty() || !calls.is_empty() {
            let mut output = json!({"role":role,"content":if parts.is_empty() { Value::Null } else { json!(parts) }});
            if !calls.is_empty() {
                output["tool_calls"] = json!(calls);
            }
            result.push(output);
        }
    }
    Ok(result)
}

pub(crate) fn request(body: &[u8], model: &str) -> Result<Bytes, BuildError> {
    Ok(Bytes::from(chat_request(body, model)?.to_string()))
}

pub(crate) fn chat_request(body: &[u8], model: &str) -> Result<Value, BuildError> {
    let input: Value = serde_json::from_slice(body).map_err(|_| BuildError::InvalidBody)?;
    let limit = input["max_tokens"]
        .as_u64()
        .ok_or(BuildError::InvalidBody)?;
    let mut messages = messages(&input["messages"])?;
    if let Some(system) = input.get("system") {
        messages.insert(0, json!({"role":"system","content":text_content(system)?}));
    }
    let mut out = json!({"model":model,"messages":messages,"max_tokens":limit});
    for key in ["temperature", "top_p", "stream"] {
        if let Some(value) = input.get(key) {
            out[key] = value.clone();
        }
    }
    if let Some(stop) = input.get("stop_sequences") {
        out["stop"] = stop.clone();
    }
    if let Some(user) = input.pointer("/metadata/user_id") {
        out["user"] = user.clone();
    }
    if let Some(tools) = input.get("tools") {
        let tools = blocks(tools)?.iter().map(|tool| {
            let mut function = json!({"name":text(&tool["name"])?,"parameters":tool["input_schema"].as_object().ok_or(BuildError::InvalidBody)?});
            if let Some(description) = tool.get("description") {
                function["description"] = description.clone();
            }
            Ok(json!({"type":"function","function":function}))
        }).collect::<Result<Vec<_>, BuildError>>()?;
        out["tools"] = json!(tools);
    }
    if let Some(choice) = input.get("tool_choice") {
        out["tool_choice"] = match choice["type"].as_str() {
            Some("auto") => json!("auto"),
            Some("any") => json!("required"),
            Some("none") => json!("none"),
            Some("tool") => json!({"type":"function","function":{"name":text(&choice["name"]) ?}}),
            _ => return Err(BuildError::InvalidBody),
        };
        if choice["disable_parallel_tool_use"] == true {
            out["parallel_tool_calls"] = json!(false);
        }
    }
    if out["stream"] == true {
        out["stream_options"] = json!({"include_usage":true});
    }
    Ok(out)
}

fn usage(value: &Value) -> Option<Usage> {
    Some(Usage {
        input_tokens: u32::try_from(value["prompt_tokens"].as_u64()?).ok()?,
        output_tokens: u32::try_from(value["completion_tokens"].as_u64()?).ok()?,
    })
}

fn stop(reason: &Value) -> &'static str {
    match reason.as_str() {
        Some("tool_calls" | "function_call") => "tool_use",
        Some("length") => "max_tokens",
        Some("content_filter") => "refusal",
        _ => "end_turn",
    }
}

fn envelope(chat: &Value, content: Vec<Value>, reason: Value, usage: Usage) -> Value {
    json!({"id":chat["id"],"model":chat["model"],"type":"message","role":"assistant",
        "content":content,"stop_reason":reason,"stop_sequence":null,
        "usage":{"input_tokens":usage.input_tokens,"output_tokens":usage.output_tokens}
    })
}

pub(crate) fn complete(body: &[u8]) -> Result<Bytes, UpstreamError> {
    let chat: Value = serde_json::from_slice(body).map_err(|_| bad_gateway())?;
    Ok(Bytes::from(complete_chat(&chat)?.to_string()))
}

pub(crate) fn complete_chat(chat: &Value) -> Result<Value, UpstreamError> {
    let choice = chat["choices"].get(0).ok_or_else(bad_gateway)?;
    let finish = stop(&choice["finish_reason"]);
    let message = choice["message"].as_object().ok_or_else(bad_gateway)?;
    let mut content = Vec::new();
    if let Some(text) = message.get("content").and_then(Value::as_str) {
        content.push(json!({"type":"text","text":text}));
    }
    let refusal = message
        .get("refusal")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    if let Some(text) = refusal {
        content.push(json!({"type":"text","text":text}));
    }
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let parameters = serde_json::from_str::<Value>(
                call["function"]["arguments"]
                    .as_str()
                    .ok_or_else(bad_gateway)?,
            );
            let parameters = match parameters {
                Ok(input) if input.is_object() => input,
                Err(error) if finish == "max_tokens" && error.is_eof() => json!({}),
                _ => return Err(bad_gateway()),
            };
            content.push(json!({"type":"tool_use",
                "id":call["id"].as_str().ok_or_else(bad_gateway)?,
                "name":call["function"]["name"].as_str().ok_or_else(bad_gateway)?,
                "input":parameters
            }));
        }
    }
    Ok(envelope(
        chat,
        content,
        json!(if refusal.is_some() && finish != "max_tokens" {
            "refusal"
        } else {
            finish
        }),
        usage(&chat["usage"]).unwrap_or_default(),
    ))
}

fn event(kind: &str, mut value: Value) -> SseEvent {
    value["type"] = json!(kind);
    SseEvent {
        event: Some(kind.into()),
        data: value.to_string(),
    }
}

fn block_stop(index: usize) -> SseEvent {
    event("content_block_stop", json!({"index":index}))
}

pub(crate) fn stream(
    mut input: BoxStream<'static, Result<SseEvent, UpstreamError>>,
) -> BoxStream<'static, Result<SseEvent, UpstreamError>> {
    Box::pin(async_stream::try_stream! {
        let mut started = false;
        let mut text = None;
        let mut tools = BTreeMap::new();
        let mut next = 0;
        let mut tokens = Usage::default();
        let mut reason = None;
        let mut refusal = false;
        while let Some(chunk) = input.next().await {
            let chunk = chunk?;
            if chunk.data == "[DONE]" {
                let finish = reason.ok_or_else(bad_gateway)?;
                let finish = if refusal && finish != "max_tokens" { "refusal" } else { finish };
                if let Some((index, _)) = text {
                    yield block_stop(index);
                }
                for index in tools.into_values() {
                    yield block_stop(index);
                }
                yield event("message_delta", json!({"delta":{"stop_reason":finish,"stop_sequence":null},
                    "usage":{"input_tokens":tokens.input_tokens,"output_tokens":tokens.output_tokens}}));
                yield event("message_stop", json!({}));
                return;
            }
            let chat: Value = serde_json::from_str(&chunk.data).map_err(|_| bad_gateway())?;
            if chat.get("error").is_some() {
                Err(bad_gateway())?;
            }
            if let Some(value) = usage(&chat["usage"]) {
                tokens = value;
            }
            let Some(choice) = chat["choices"].get(0) else { continue };
            if !started {
                yield event("message_start", json!({"message":envelope(&chat, Vec::new(), Value::Null, tokens)}));
                started = true;
            }
            let delta = &choice["delta"];
            for (field, is_refusal) in [("content", false), ("refusal", true)] {
                let Some(fragment) = delta[field].as_str().filter(|s| !s.is_empty()) else { continue };
                refusal |= is_refusal;
                if let Some((index, was_refusal)) = text {
                    if is_refusal != was_refusal {
                        yield block_stop(index);
                        text = None;
                    }
                }
                let index = if let Some((index, _)) = text { index } else {
                    let index = next;
                    next += 1;
                    text = Some((index, is_refusal));
                    yield event("content_block_start", json!({"index":index,"content_block":{"type":"text","text":""}}));
                    index
                };
                yield event("content_block_delta", json!({"index":index,"delta":{"type":"text_delta","text":fragment}}));
            }
            if let Some(calls) = delta["tool_calls"].as_array() {
                for call in calls {
                    if let Some((index, _)) = text.take() {
                        yield block_stop(index);
                    }
                    let upstream = call["index"].as_u64().ok_or_else(bad_gateway)?;
                    let index = if let Some(index) = tools.get(&upstream) { *index } else {
                        let index = next;
                        next += 1;
                        let id = call["id"].as_str().ok_or_else(bad_gateway)?;
                        let name = call["function"]["name"].as_str().ok_or_else(bad_gateway)?;
                        tools.insert(upstream, index);
                        yield event("content_block_start", json!({"index":index,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}));
                        index
                    };
                    if let Some(fragment) = call["function"]["arguments"].as_str().filter(|s| !s.is_empty()) {
                        yield event("content_block_delta", json!({"index":index,"delta":{"type":"input_json_delta","partial_json":fragment}}));
                    }
                }
            }
            if !choice["finish_reason"].is_null() {
                reason = Some(stop(&choice["finish_reason"]));
            }
        }
        Err(bad_gateway())?;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;

    #[tokio::test]
    async fn messages_conversion_preserves_refusals() {
        let chat = json!({
            "id":"chat","model":"model","choices":[{"message":{"content":null,"refusal":"Cannot help."},"finish_reason":"stop"}]
        });
        let complete = complete_chat(&chat).unwrap();
        let input = [
            json!({"id":"chat","model":"model","choices":[{"delta":{"refusal":"Cannot "}}]})
                .to_string(),
            json!({"choices":[{"delta":{"refusal":"help."},"finish_reason":"stop"}]}).to_string(),
            "[DONE]".into(),
        ];
        let events: Vec<_> = super::stream(Box::pin(stream::iter(
            input
                .into_iter()
                .map(|data| Ok(SseEvent { event: None, data })),
        )))
        .map(|event| serde_json::from_str::<Value>(&event.unwrap().data).unwrap())
        .collect()
        .await;
        let text: String = events
            .iter()
            .filter_map(|event| event["delta"]["text"].as_str())
            .collect();
        assert_eq!(
            complete["content"],
            json!([{"type":"text","text":"Cannot help."}])
        );
        assert_eq!(complete["stop_reason"], "refusal");
        assert_eq!(text, "Cannot help.");
        assert_eq!(events[events.len() - 2]["delta"]["stop_reason"], "refusal");
    }

    #[tokio::test]
    async fn messages_conversion_stream_separates_text_and_refusal() {
        let chat = json!({"id":"chat","model":"model","choices":[{
            "message":{"content":"Sorry. ","refusal":"Cannot help."},"finish_reason":"stop"
        }]});
        let complete = complete_chat(&chat).unwrap();
        let input = [
            json!({"id":"chat","model":"model","choices":[{"delta":{"content":"Sorry. ","refusal":"Cannot "}}]}).to_string(),
            json!({"choices":[{"delta":{"refusal":"help."},"finish_reason":"stop"}]}).to_string(),
            "[DONE]".into(),
        ];
        let events: Vec<_> = super::stream(Box::pin(stream::iter(
            input
                .into_iter()
                .map(|data| Ok(SseEvent { event: None, data })),
        )))
        .map(|event| serde_json::from_str::<Value>(&event.unwrap().data).unwrap())
        .collect()
        .await;
        let block_events: Vec<_> = events
            .iter()
            .filter(|event| {
                event["type"]
                    .as_str()
                    .unwrap()
                    .starts_with("content_block_")
            })
            .map(|event| {
                (
                    event["type"].as_str().unwrap(),
                    event["index"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            block_events,
            [
                ("content_block_start", 0),
                ("content_block_delta", 0),
                ("content_block_stop", 0),
                ("content_block_start", 1),
                ("content_block_delta", 1),
                ("content_block_delta", 1),
                ("content_block_stop", 1),
            ]
        );
        for index in [0, 1] {
            let text: String = events
                .iter()
                .filter(|event| event["index"] == index)
                .filter_map(|event| event["delta"]["text"].as_str())
                .collect();
            assert_eq!(
                text,
                complete["content"][index as usize]["text"]
                    .as_str()
                    .unwrap()
            );
        }
    }

    #[test]
    fn messages_conversion_keeps_max_tokens_for_partial_tool_json() {
        let mut chat = json!({
            "id":"chat","model":"model","choices":[{"message":{"tool_calls":[
                {"id":"call","function":{"name":"lookup","arguments":"{\"incomplete\":"}}
            ]},"finish_reason":"length"}]
        });
        let complete = complete_chat(&chat).unwrap();
        assert_eq!(complete["stop_reason"], "max_tokens");
        assert_eq!(
            complete["content"][0],
            json!({"type":"tool_use","id":"call","name":"lookup","input":{}})
        );
        chat["choices"][0]["finish_reason"] = json!("tool_calls");
        assert!(complete_chat(&chat).is_err());
    }
}
