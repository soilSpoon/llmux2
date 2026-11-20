use bytes::Bytes;
use futures_util::StreamExt;
use llmux_types::{BoxStream, BuildError, SseEvent, UpstreamError, Usage};
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn string(value: &Value) -> Result<&str, BuildError> {
    value.as_str().ok_or(BuildError::InvalidBody)
}

pub(crate) fn request(body: &[u8], model: &str) -> Result<Bytes, BuildError> {
    let body: Value = serde_json::from_slice(body).map_err(|_| BuildError::InvalidBody)?;
    let obj = body.as_object().ok_or(BuildError::InvalidBody)?;
    for key in ["previous_response_id", "conversation"] {
        if obj.get(key).is_some_and(|v| !v.is_null()) {
            return Err(BuildError::UnsupportedFormat);
        }
    }
    let mut messages = Vec::new();
    if let Some(instructions) = obj.get("instructions").filter(|v| !v.is_null()) {
        messages.push(json!({"role":"system", "content":string(instructions)?}));
    }
    match obj.get("input") {
        Some(Value::String(text)) => messages.push(json!({"role":"user", "content":text})),
        Some(Value::Array(items)) => {
            let mut previous_call = false;
            for item in items {
                let kind = item
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("message");
                match kind {
                    "message" => {
                        let role = string(&item["role"])?;
                        if !matches!(role, "user" | "assistant" | "system" | "developer") {
                            return Err(BuildError::UnsupportedFormat);
                        }
                        let mut refusal: Option<String> = None;
                        let content = match &item["content"] {
                            Value::String(s) => json!(s),
                            Value::Array(parts) => {
                                let mut content = Vec::new();
                                for part in parts {
                                    match part["type"].as_str() {
                                        Some("input_text" | "output_text") => content.push(json!({"type":"text", "text":string(&part["text"])?})),
                                        Some("refusal") => {
                                            let text = string(&part["refusal"])?;
                                            if role == "assistant" {
                                                refusal.get_or_insert_with(String::new).push_str(text);
                                            } else {
                                                content.push(json!({"type":"text", "text":text}));
                                            }
                                        }
                                        Some("input_image") => {
                                            let mut image = json!({"url":string(&part["image_url"])?});
                                            if let Some(detail) = part.get("detail") { image["detail"] = detail.clone(); }
                                            content.push(json!({"type":"image_url", "image_url":image}));
                                        }
                                        _ => return Err(BuildError::UnsupportedFormat),
                                    };
                                }
                                if content.is_empty() && refusal.is_some() { Value::Null } else { Value::Array(content) }
                            }
                            _ => return Err(BuildError::InvalidBody),
                        };
                        let mut message = json!({"role":role, "content":content});
                        if let Some(refusal) = refusal { message["refusal"] = json!(refusal); }
                        messages.push(message);
                    }
                    "function_call" => {
                        let call = json!({"id":string(&item["call_id"])?, "type":"function", "function":{"name":string(&item["name"])?, "arguments":string(&item["arguments"])?}});
                        if previous_call {
                            messages.last_mut().unwrap()["tool_calls"].as_array_mut().unwrap().push(call);
                        } else {
                            messages.push(json!({"role":"assistant", "tool_calls":[call]}));
                        }
                    }
                    "function_call_output" => messages.push(json!({"role":"tool", "tool_call_id":string(&item["call_id"])?, "content":string(&item["output"])?})),
                    _ => return Err(BuildError::UnsupportedFormat),
                }
                previous_call = kind == "function_call";
            }
        }
        _ => return Err(BuildError::InvalidBody),
    }
    let mut out = json!({"model":model, "messages":messages});
    for key in [
        "temperature",
        "top_p",
        "parallel_tool_calls",
        "user",
        "metadata",
        "stream",
    ] {
        if let Some(value) = obj.get(key) {
            out[key] = value.clone();
        }
    }
    if body["stream"] == true {
        out["stream_options"] = json!({"include_usage":true});
    }
    if let Some(value) = obj.get("max_output_tokens") {
        out["max_completion_tokens"] = value.clone();
    }
    let effort = body.pointer("/reasoning/effort").filter(|v| !v.is_null());
    let alias = obj.get("reasoning_effort").filter(|v| !v.is_null());
    if matches!((effort, alias), (Some(a), Some(b)) if a != b) {
        return Err(BuildError::InvalidBody);
    }
    if let Some(value) = effort.or(alias) {
        out["reasoning_effort"] = value.clone();
    }
    if let Some(format) = body.pointer("/text/format") {
        out["response_format"] = match format["type"].as_str() {
            Some("text" | "json_object") => json!({"type":format["type"]}),
            Some("json_schema") => {
                let mut schema = format.clone();
                schema
                    .as_object_mut()
                    .ok_or(BuildError::InvalidBody)?
                    .remove("type");
                json!({"type":"json_schema", "json_schema":schema})
            }
            _ => return Err(BuildError::UnsupportedFormat),
        };
    }
    if let Some(tools) = obj.get("tools") {
        out["tools"] = Value::Array(
            tools
                .as_array()
                .ok_or(BuildError::InvalidBody)?
                .iter()
                .map(|tool| {
                    if tool["type"] != "function" {
                        return Err(BuildError::UnsupportedFormat);
                    }
                    string(&tool["name"])?;
                    let mut function = tool.clone();
                    function.as_object_mut().unwrap().remove("type");
                    Ok(json!({"type":"function", "function":function}))
                })
                .collect::<Result<_, _>>()?,
        );
    }
    if let Some(choice) = obj.get("tool_choice") {
        out["tool_choice"] = if let Some(choice) = choice.as_str() {
            if !matches!(choice, "auto" | "none" | "required") {
                return Err(BuildError::UnsupportedFormat);
            }
            json!(choice)
        } else if choice["type"] == "function" {
            json!({"type":"function", "function":{"name":string(&choice["name"])?}})
        } else {
            return Err(BuildError::UnsupportedFormat);
        };
    }
    Ok(Bytes::from(
        serde_json::to_vec(&out).map_err(|_| BuildError::InvalidBody)?,
    ))
}

fn usage(value: &Value) -> Option<Usage> {
    Some(Usage {
        input_tokens: u32::try_from(value["prompt_tokens"].as_u64()?).ok()?,
        output_tokens: u32::try_from(value["completion_tokens"].as_u64()?).ok()?,
    })
}

fn incomplete_reason(finish_reason: &Value) -> Option<&'static str> {
    match finish_reason.as_str() {
        Some("length") => Some("max_output_tokens"),
        Some("content_filter") => Some("content_filter"),
        _ => None,
    }
}

fn envelope(chat: &Value, output: Vec<Value>, status: &str, reason: Option<&str>) -> Value {
    let tokens = usage(&chat["usage"]);
    json!({"id":chat["id"], "object":"response", "created_at":chat["created"], "model":chat["model"],
        "status":status, "output":output, "error":null,
        "incomplete_details":reason.map(|reason| json!({"reason":reason})),
        "usage":tokens.map(|u| json!({"input_tokens":u.input_tokens,"output_tokens":u.output_tokens,"total_tokens":u64::from(u.input_tokens)+u64::from(u.output_tokens)}))})
}

fn text_part(text: &str) -> Value {
    json!({"type":"output_text", "text":text, "annotations":[]})
}

fn message(id: &str, text: &str, status: &str) -> Value {
    json!({"id":id,"type":"message","role":"assistant","status":status,"content":[text_part(text)]})
}

pub(crate) fn complete(body: &[u8]) -> Result<(Bytes, Option<Usage>), ()> {
    let chat: Value = serde_json::from_slice(body).map_err(|_| ())?;
    let choice = chat["choices"].get(0).ok_or(())?;
    let msg = choice.get("message").ok_or(())?;
    let id = chat["id"].as_str().ok_or(())?;
    let mut output = Vec::new();
    if let Some(text) = msg["content"].as_str() {
        output.push(message(&format!("msg_{id}"), text, "completed"));
    }
    if let Some(refusal) = msg["refusal"].as_str() {
        if output.is_empty() {
            let mut item = message(&format!("msg_{id}"), "", "completed");
            item["content"] = json!([]);
            output.push(item);
        }
        output[0]["content"]
            .as_array_mut()
            .ok_or(())?
            .push(json!({"type":"refusal","refusal":refusal}));
    }
    if let Some(calls) = msg["tool_calls"].as_array() {
        for call in calls {
            output.push(json!({"id":format!("fc_{}", call["id"].as_str().ok_or(())?), "type":"function_call", "status":"completed", "call_id":call["id"], "name":call["function"]["name"], "arguments":call["function"]["arguments"]}));
        }
    }
    let reason = incomplete_reason(&choice["finish_reason"]);
    let status = if reason.is_some() {
        "incomplete"
    } else {
        "completed"
    };
    for item in &mut output {
        item["status"] = json!(status);
    }
    let response = envelope(&chat, output, status, reason);
    Ok((
        Bytes::from(serde_json::to_vec(&response).map_err(|_| ())?),
        usage(&chat["usage"]),
    ))
}

#[derive(Default)]
struct Translator {
    sequence: u64,
    chat: Value,
    output: Vec<Value>,
    message: Option<usize>,
    text: Option<usize>,
    refusal: Option<usize>,
    calls: BTreeMap<u64, usize>,
    incomplete: Option<&'static str>,
    started: bool,
}

impl Translator {
    fn event(&mut self, kind: &str, mut data: Value) -> SseEvent {
        data["type"] = json!(kind);
        data["sequence_number"] = json!(self.sequence);
        self.sequence += 1;
        SseEvent {
            event: Some(kind.into()),
            data: data.to_string(),
        }
    }

    fn push(&mut self, data: &str) -> Result<Vec<SseEvent>, ()> {
        let mut events = Vec::new();
        if data == "[DONE]" {
            if !self.started {
                return Err(());
            }
            let status = if self.incomplete.is_some() {
                "incomplete"
            } else {
                "completed"
            };
            for index in 0..self.output.len() {
                self.output[index]["status"] = json!(status);
                let item = self.output[index].clone();
                if item["type"] == "message" {
                    for (content_index, part) in
                        item["content"].as_array().ok_or(())?.iter().enumerate()
                    {
                        let refusal = part["type"] == "refusal";
                        let mut data = json!({"item_id":item["id"],"output_index":index,"content_index":content_index});
                        let field = if refusal { "refusal" } else { "text" };
                        data[field] = part[field].clone();
                        events.push(self.event(
                            if refusal {
                                "response.refusal.done"
                            } else {
                                "response.output_text.done"
                            },
                            data,
                        ));
                        events.push(self.event("response.content_part.done", json!({"item_id":item["id"],"output_index":index,"content_index":content_index,"part":part})));
                    }
                } else {
                    events.push(self.event("response.function_call_arguments.done", json!({"item_id":item["id"],"output_index":index,"arguments":item["arguments"]})));
                }
                events.push(self.event(
                    "response.output_item.done",
                    json!({"output_index":index,"item":item}),
                ));
            }
            let response = envelope(
                &self.chat,
                self.output.clone(),
                if self.incomplete.is_some() {
                    "incomplete"
                } else {
                    "completed"
                },
                self.incomplete,
            );
            events.push(self.event(
                if self.incomplete.is_some() {
                    "response.incomplete"
                } else {
                    "response.completed"
                },
                json!({"response":response}),
            ));
            return Ok(events);
        }
        let chunk: Value = serde_json::from_str(data).map_err(|_| ())?;
        if !self.started {
            chunk["id"].as_str().ok_or(())?;
            self.chat = chunk.clone();
            self.started = true;
            let response = envelope(&self.chat, vec![], "in_progress", None);
            events.push(self.event("response.created", json!({"response":response})));
            events.push(self.event("response.in_progress", json!({"response":response})));
        }
        if !chunk["usage"].is_null() {
            self.chat["usage"] = chunk["usage"].clone();
        }
        let choices = chunk["choices"].as_array().ok_or(())?;
        let Some(choice) = choices.first() else {
            return Ok(events);
        };
        self.incomplete = incomplete_reason(&choice["finish_reason"]).or(self.incomplete);
        let delta = &choice["delta"];
        for (field, is_refusal) in [("content", false), ("refusal", true)] {
            let Some(text) = delta[field].as_str() else {
                continue;
            };
            let index = if let Some(index) = self.message {
                index
            } else {
                let index = self.output.len();
                let id = format!("msg_{}", self.chat["id"].as_str().ok_or(())?);
                let mut item = message(&id, "", "in_progress");
                item["content"] = json!([]);
                events.push(self.event(
                    "response.output_item.added",
                    json!({"output_index":index,"item":item}),
                ));
                self.output.push(item);
                self.message = Some(index);
                index
            };
            let content_index = if let Some(content_index) =
                if is_refusal { self.refusal } else { self.text }
            {
                content_index
            } else {
                let part = if is_refusal {
                    json!({"type":"refusal","refusal":""})
                } else {
                    text_part("")
                };
                let parts = self.output[index]["content"].as_array_mut().ok_or(())?;
                let content_index = parts.len();
                parts.push(part.clone());
                let id = self.output[index]["id"].clone();
                events.push(self.event("response.content_part.added", json!({"output_index":index,"item_id":id,"content_index":content_index,"part":part})));
                if is_refusal {
                    self.refusal = Some(content_index);
                } else {
                    self.text = Some(content_index);
                }
                content_index
            };
            let item = &mut self.output[index];
            let part_field = if is_refusal { "refusal" } else { "text" };
            let Value::String(accumulated) = &mut item["content"][content_index][part_field] else {
                return Err(());
            };
            accumulated.push_str(text);
            let id = item["id"].clone();
            events.push(self.event(
                if is_refusal { "response.refusal.delta" } else { "response.output_text.delta" },
                json!({"output_index":index,"item_id":id,"content_index":content_index,"delta":text}),
            ));
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for call in calls {
                let call_index = call["index"].as_u64().ok_or(())?;
                let index = if let Some(index) = self.calls.get(&call_index) {
                    *index
                } else {
                    let id = call["id"].as_str().ok_or(())?;
                    let name = call["function"]["name"].as_str().ok_or(())?;
                    let index = self.output.len();
                    let item = json!({"type":"function_call","id":format!("fc_{id}"),"call_id":id,"name":name,"arguments":"","status":"in_progress"});
                    events.push(self.event(
                        "response.output_item.added",
                        json!({"output_index":index,"item":item}),
                    ));
                    self.output.push(item);
                    self.calls.insert(call_index, index);
                    index
                };
                if let Some(arguments) = call["function"]["arguments"].as_str() {
                    let item = &mut self.output[index];
                    let Value::String(accumulated) = &mut item["arguments"] else {
                        return Err(());
                    };
                    accumulated.push_str(arguments);
                    let id = item["id"].clone();
                    events.push(self.event(
                        "response.function_call_arguments.delta",
                        json!({"output_index":index,"item_id":id,"delta":arguments}),
                    ));
                }
            }
        }
        Ok(events)
    }
}

pub(crate) fn stream(
    mut upstream: BoxStream<'static, Result<SseEvent, UpstreamError>>,
) -> BoxStream<'static, Result<SseEvent, UpstreamError>> {
    Box::pin(async_stream::try_stream! {
        let mut translator = Translator::default();
        let mut bytes_sent = 0;
        while let Some(event) = upstream.next().await {
            let event = event?;
            let done = event.data == "[DONE]";
            for translated in translator.push(&event.data).map_err(|()| UpstreamError::StreamBroken { bytes_sent })? {
                bytes_sent += translated.data.len() as u64;
                yield translated;
            }
            if done { return; }
        }
        Err(UpstreamError::StreamBroken { bytes_sent })?;
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert(value: Value) -> Value {
        serde_json::from_slice(&request(&serde_json::to_vec(&value).unwrap(), "upstream").unwrap())
            .unwrap()
    }

    #[test]
    fn string_input_and_options() {
        let result = convert(
            json!({"input":"hello","instructions":"be nice","store":true,"stream":true,"max_output_tokens":10,"reasoning":{"effort":"low"},"text":{"format":{"type":"json_schema","name":"result","schema":{"type":"object"},"strict":true}},"temperature":0.5,"metadata":{"a":"b"}}),
        );
        assert_eq!(
            result["messages"],
            json!([{"role":"system","content":"be nice"},{"role":"user","content":"hello"}])
        );
        assert_eq!(result["model"], "upstream");
        assert_eq!(result["stream_options"], json!({"include_usage":true}));
        assert_eq!(result["max_completion_tokens"], 10);
        assert_eq!(result["reasoning_effort"], "low");
        assert_eq!(result["response_format"]["json_schema"]["name"], "result");
        assert!(result.get("store").is_none());
        assert_eq!(convert(json!({"input":[]}))["messages"], json!([]));
        assert_eq!(convert(json!({"input":""}))["messages"][0]["content"], "");
    }

    #[test]
    fn top_level_reasoning_effort_alias() {
        assert_eq!(
            convert(json!({"input":"hello","reasoning_effort":"high"}))["reasoning_effort"],
            "high"
        );
        assert_eq!(
            convert(
                json!({"input":"hello","reasoning_effort":"high","reasoning":{"effort":"high"}})
            )["reasoning_effort"],
            "high"
        );
        let conflict =
            json!({"input":"hello","reasoning_effort":"high","reasoning":{"effort":"low"}});
        assert!(matches!(
            request(conflict.to_string().as_bytes(), "upstream"),
            Err(BuildError::InvalidBody)
        ));
    }

    #[test]
    fn items_and_tools() {
        let result = convert(json!({"input":[
            {"type":"message","role":"developer","content":[{"type":"input_text","text":"look"},{"type":"input_image","image_url":"https://example.com/a.png","detail":"low"}]},
            {"type":"function_call","call_id":"a","name":"lookup","arguments":"{}"},
            {"type":"function_call","call_id":"b","name":"lookup","arguments":"{}"},
            {"type":"function_call_output","call_id":"a","output":"ok"},
            {"role":"assistant","content":[{"type":"output_text","text":"done"}]}
        ],"tools":[{"type":"function","name":"lookup","description":"find","parameters":{"type":"object"},"strict":true}],"tool_choice":{"type":"function","name":"lookup"}}));
        assert_eq!(result["messages"].as_array().unwrap().len(), 4);
        assert_eq!(
            result["messages"][1]["tool_calls"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            result["messages"][2],
            json!({"role":"tool","tool_call_id":"a","content":"ok"})
        );
        assert_eq!(
            result["messages"][0]["content"][1],
            json!({"type":"image_url","image_url":{"url":"https://example.com/a.png","detail":"low"}})
        );
        assert_eq!(result["tools"][0]["function"]["strict"], true);
        assert_eq!(
            result["tool_choice"],
            json!({"type":"function","function":{"name":"lookup"}})
        );
    }

    #[test]
    fn rejects_unsupported_and_invalid() {
        for body in [
            json!({"input":"a","previous_response_id":"x"}),
            json!({"input":"a","conversation":"x"}),
            json!({"input":"a","tools":[{"type":"web_search"}]}),
            json!({"input":[{"type":"unknown"}]}),
        ] {
            assert!(matches!(
                request(&serde_json::to_vec(&body).unwrap(), "m"),
                Err(BuildError::UnsupportedFormat)
            ));
        }
        for body in [b"null".as_slice(), b"{}", b"{", br#"{"input":5}"#] {
            assert!(matches!(request(body, "m"), Err(BuildError::InvalidBody)));
        }
        assert!(complete(b"{}").is_err());
    }

    #[test]
    fn complete_text_tools_and_length() {
        let fixture = br#"{"id":"chat1","created":42,"model":"m","choices":[{"message":{"content":"hello","tool_calls":[{"id":"a","type":"function","function":{"name":"lookup","arguments":"{}"}}]},"finish_reason":"length"}],"usage":{"prompt_tokens":2,"completion_tokens":3,"total_tokens":5}}"#;
        let (body, tokens) = complete(fixture).unwrap();
        let response: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            tokens,
            Some(Usage {
                input_tokens: 2,
                output_tokens: 3
            })
        );
        assert_eq!(response["status"], "incomplete");
        assert_eq!(
            response["incomplete_details"]["reason"],
            "max_output_tokens"
        );
        assert_eq!(response["output"][0]["content"][0], text_part("hello"));
        assert_eq!(response["output"][1]["call_id"], "a");
        assert_eq!(response["output"][0]["status"], "incomplete");
        assert_eq!(response["output"][1]["status"], "incomplete");
        assert_eq!(response["usage"]["total_tokens"], 5);
        let (body, tokens) = complete(
            br#"{"id":"x","choices":[{"message":{"content":null},"finish_reason":"stop"}]}"#,
        )
        .unwrap();
        assert!(tokens.is_none());
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["output"],
            json!([])
        );
    }

    fn chunk(delta: Value) -> String {
        json!({"id":"chat1","created":42,"model":"m","choices":[{"index":0,"delta":delta,"finish_reason":null}]}).to_string()
    }

    #[test]
    fn preserves_refusals() {
        for content in [Value::Null, json!("preface")] {
            let (body, _) = complete(&serde_json::to_vec(&json!({"id":"r","choices":[{"message":{"content":content,"refusal":"cannot help"},"finish_reason":"stop"}]})).unwrap()).unwrap();
            let response: Value = serde_json::from_slice(&body).unwrap();
            let parts = response["output"][0]["content"].as_array().unwrap();
            assert_eq!(
                parts.last().unwrap(),
                &json!({"type":"refusal","refusal":"cannot help"})
            );
            assert_eq!(parts.len(), if content.is_null() { 1 } else { 2 });
        }
        for prefix in [None, Some("preface")] {
            let mut translator = Translator::default();
            if let Some(text) = prefix {
                translator.push(&chunk(json!({"content":text}))).unwrap();
            }
            let mut events = translator
                .push(&chunk(json!({"refusal":"cannot "})))
                .unwrap();
            events.extend(translator.push(&chunk(json!({"refusal":"help"}))).unwrap());
            events.extend(translator.push("[DONE]").unwrap());
            let data: Vec<Value> = events
                .iter()
                .map(|event| serde_json::from_str(&event.data).unwrap())
                .collect();
            let index = usize::from(prefix.is_some());
            let added = data
                .iter()
                .find(|e| e["type"] == "response.content_part.added")
                .unwrap();
            assert_eq!(added["content_index"], index);
            assert_eq!(added["part"], json!({"type":"refusal","refusal":""}));
            let deltas: Vec<_> = data
                .iter()
                .filter(|e| e["type"] == "response.refusal.delta")
                .map(|e| e["delta"].clone())
                .collect();
            assert_eq!(deltas, [json!("cannot "), json!("help")]);
            let done = data
                .iter()
                .find(|e| e["type"] == "response.refusal.done")
                .unwrap();
            assert_eq!(done["refusal"], "cannot help");
            assert_eq!(done["content_index"], index);
            assert_eq!(
                data.last().unwrap()["response"]["output"][0]["content"][index],
                json!({"type":"refusal","refusal":"cannot help"})
            );
        }
    }

    #[test]
    fn replays_converted_refusal_response() {
        for content in [Value::Null, json!("preface")] {
            let (body, _) = complete(&serde_json::to_vec(&json!({"id":"r","choices":[{"message":{"content":content,"refusal":"cannot help"},"finish_reason":"stop"}]})).unwrap()).unwrap();
            let response: Value = serde_json::from_slice(&body).unwrap();
            let chat = convert(json!({"input":response["output"]}));
            assert_eq!(chat["messages"][0]["refusal"], "cannot help");
            assert_eq!(
                chat["messages"][0]["content"],
                if content.is_null() {
                    Value::Null
                } else {
                    json!([{"type":"text","text":"preface"}])
                }
            );
        }
        for role in ["user", "system", "developer"] {
            let chat = convert(
                json!({"input":[{"role":role,"content":[{"type":"refusal","refusal":"cannot help"}]}]}),
            );
            assert_eq!(
                chat["messages"][0]["content"],
                json!([{"type":"text","text":"cannot help"}])
            );
            assert!(chat["messages"][0].get("refusal").is_none());
        }
        let chat = convert(
            json!({"input":[{"role":"assistant","content":[{"type":"refusal","refusal":""},{"type":"refusal","refusal":"no"},{"type":"refusal","refusal":"pe"}]}]}),
        );
        assert_eq!(chat["messages"][0]["refusal"], "nope");
        assert!(matches!(
            request(
                br#"{"input":[{"role":"assistant","content":[{"type":"refusal"}]}]}"#,
                "m"
            ),
            Err(BuildError::InvalidBody)
        ));
    }

    #[test]
    fn streaming_text_sequence() {
        let mut translator = Translator::default();
        let mut events = translator
            .push(&chunk(json!({"role":"assistant"})))
            .unwrap();
        events.extend(translator.push(&chunk(json!({"content":"hel"}))).unwrap());
        events.extend(translator.push(&chunk(json!({"content":"lo"}))).unwrap());
        events.extend(
            translator
                .push(r#"{"choices":[],"usage":{"prompt_tokens":2,"completion_tokens":3}}"#)
                .unwrap(),
        );
        events.extend(translator.push("[DONE]").unwrap());
        let names: Vec<_> = events.iter().map(|e| e.event.as_deref().unwrap()).collect();
        assert_eq!(
            names,
            [
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.completed"
            ]
        );
        for (index, event) in events.iter().enumerate() {
            let data: Value = serde_json::from_str(&event.data).unwrap();
            assert_eq!(data["sequence_number"], index);
            assert_eq!(data["type"], event.event.as_deref().unwrap());
        }
        let last: Value = serde_json::from_str(&events.last().unwrap().data).unwrap();
        assert_eq!(last["response"]["output"][0]["content"][0]["text"], "hello");
        assert_eq!(last["response"]["usage"]["input_tokens"], 2);
    }

    #[test]
    fn streaming_length_is_incomplete() {
        let mut translator = Translator::default();
        translator.push(&chunk(json!({"content":"partial","tool_calls":[{"index":0,"id":"a","function":{"name":"lookup","arguments":"{"}}]}))).unwrap();
        translator
            .push(r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#)
            .unwrap();
        translator
            .push(r#"{"choices":[],"usage":{"prompt_tokens":2,"completion_tokens":3}}"#)
            .unwrap();
        let events = translator.push("[DONE]").unwrap();
        assert_eq!(
            events.last().unwrap().event.as_deref(),
            Some("response.incomplete")
        );
        let data: Vec<Value> = events
            .iter()
            .map(|event| serde_json::from_str(&event.data).unwrap())
            .collect();
        for event in data
            .iter()
            .filter(|event| event["type"] == "response.output_item.done")
        {
            assert_eq!(event["item"]["status"], "incomplete");
        }
        let response = &data.last().unwrap()["response"];
        assert_eq!(response["status"], "incomplete");
        assert_eq!(
            response["incomplete_details"]["reason"],
            "max_output_tokens"
        );
        assert_eq!(response["usage"]["input_tokens"], 2);
        assert_eq!(response["output"].as_array().unwrap().len(), 2);
        for item in response["output"].as_array().unwrap() {
            assert_eq!(item["status"], "incomplete");
        }
    }

    #[test]
    fn content_filter_is_incomplete() {
        for msg in [
            json!({"content":null}),
            json!({"content":"partial","refusal":"blocked","tool_calls":[{"id":"a","function":{"name":"lookup","arguments":"{"}}]}),
        ] {
            let (body, _) = complete(
                &serde_json::to_vec(
                    &json!({"id":"r","choices":[{"message":msg,"finish_reason":"content_filter"}]}),
                )
                .unwrap(),
            )
            .unwrap();
            let response: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(response["status"], "incomplete");
            assert_eq!(
                response["incomplete_details"],
                json!({"reason":"content_filter"})
            );
            for item in response["output"].as_array().unwrap() {
                assert_eq!(item["status"], "incomplete");
            }
        }
        for delta in [
            json!({}),
            json!({"content":"partial","refusal":"blocked","tool_calls":[{"index":0,"id":"a","function":{"name":"lookup","arguments":"{"}}]}),
        ] {
            let mut translator = Translator::default();
            translator.push(&chunk(delta)).unwrap();
            translator
                .push(r#"{"choices":[{"delta":{},"finish_reason":"content_filter"}]}"#)
                .unwrap();
            translator
                .push(r#"{"choices":[],"usage":{"prompt_tokens":2,"completion_tokens":3}}"#)
                .unwrap();
            let events = translator.push("[DONE]").unwrap();
            assert_eq!(
                events.last().unwrap().event.as_deref(),
                Some("response.incomplete")
            );
            let data: Vec<Value> = events
                .iter()
                .map(|event| serde_json::from_str(&event.data).unwrap())
                .collect();
            for event in data
                .iter()
                .filter(|event| event["type"] == "response.output_item.done")
            {
                assert_eq!(event["item"]["status"], "incomplete");
            }
            let response = &data.last().unwrap()["response"];
            assert_eq!(response["status"], "incomplete");
            assert_eq!(
                response["incomplete_details"],
                json!({"reason":"content_filter"})
            );
            assert_eq!(response["usage"]["output_tokens"], 3);
            for item in response["output"].as_array().unwrap() {
                assert_eq!(item["status"], "incomplete");
            }
        }
    }

    #[test]
    fn streaming_tool_sequence() {
        let mut translator = Translator::default();
        let mut events = translator.push(&chunk(json!({"tool_calls":[{"index":1,"id":"b","function":{"name":"lookup","arguments":"{"}},{"index":0,"id":"a","function":{"name":"other","arguments":""}}]}))).unwrap();
        events.extend(translator.push(&chunk(json!({"tool_calls":[{"index":1,"function":{"arguments":"}"}},{"index":0,"function":{"arguments":"{}"}}]}))).unwrap());
        events.extend(translator.push("[DONE]").unwrap());
        let names: Vec<_> = events.iter().map(|e| e.event.as_deref().unwrap()).collect();
        assert_eq!(
            names,
            [
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.function_call_arguments.delta",
                "response.output_item.added",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.done",
                "response.output_item.done",
                "response.function_call_arguments.done",
                "response.output_item.done",
                "response.completed"
            ]
        );
        let last: Value = serde_json::from_str(&events.last().unwrap().data).unwrap();
        assert_eq!(last["response"]["output"][0]["call_id"], "b");
        assert_eq!(last["response"]["output"][0]["arguments"], "{}");
        assert!(Translator::default().push("[DONE]").is_err());
        assert!(translator.push("invalid json").is_err());
    }

    #[test]
    fn missing_done_and_upstream_errors_remain_broken() {
        use futures_util::FutureExt;
        let events = futures_util::stream::iter(vec![Ok(SseEvent {
            event: None,
            data: chunk(json!({"content":"x"})),
        })]);
        let results: Vec<_> = stream(Box::pin(events))
            .collect::<Vec<_>>()
            .now_or_never()
            .unwrap();
        assert!(matches!(
            results.last().unwrap(),
            Err(UpstreamError::StreamBroken { .. })
        ));
        let events =
            futures_util::stream::iter(vec![Err(UpstreamError::StreamBroken { bytes_sent: 123 })]);
        let results = stream(Box::pin(events))
            .collect::<Vec<_>>()
            .now_or_never()
            .unwrap();
        assert!(matches!(
            results.as_slice(),
            [Err(UpstreamError::StreamBroken { bytes_sent: 123 })]
        ));
        let events = futures_util::stream::iter(vec![
            Ok(SseEvent {
                event: None,
                data: chunk(json!({})),
            }),
            Ok(SseEvent {
                event: None,
                data: "[DONE]".into(),
            }),
        ]);
        let results = stream(Box::pin(events))
            .collect::<Vec<_>>()
            .now_or_never()
            .unwrap();
        assert!(results.iter().all(Result::is_ok));
        assert_eq!(
            results.last().unwrap().as_ref().unwrap().event.as_deref(),
            Some("response.completed")
        );
    }
}
