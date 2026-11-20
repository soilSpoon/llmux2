//! Opt-in middle-out truncation for requests that fit no deployment's context window.

use crate::openai_compat::{native_message_bytes, TextBytes};
use bytes::{BufMut, Bytes, BytesMut};
use llmux_types::InboundFormat;
use serde::Deserialize;
use serde_json::value::RawValue;

pub struct Truncated {
    pub body: Bytes,
    pub prompt_bytes: u64,
    /// Messages (or Responses input items) removed from the request.
    pub dropped: usize,
}

#[derive(Deserialize)]
struct Conversation<'a> {
    #[serde(default, borrow)]
    messages: Option<&'a RawValue>,
    #[serde(default, borrow)]
    input: Option<&'a RawValue>,
}

#[derive(Default, Deserialize)]
struct Item {
    #[serde(default)]
    role: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    tool_call_id: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<IdPart>>,
    #[serde(default)]
    content: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct IdPart {
    #[serde(default)]
    id: Option<String>,
}

impl Item {
    /// Call ids this item makes or answers: Chat `tool_calls`/`tool_call_id`,
    /// Responses `call_id`, Anthropic `tool_use`/`tool_result` blocks.
    fn call_ids(&self) -> Vec<&str> {
        let mut ids: Vec<&str> = self
            .call_id
            .iter()
            .chain(&self.tool_call_id)
            .map(String::as_str)
            .collect();
        ids.extend(
            self.tool_calls
                .iter()
                .flatten()
                .filter_map(|call| call.id.as_deref()),
        );
        if let Some(parts) = self.content.as_ref().and_then(|content| content.as_array()) {
            ids.extend(parts.iter().filter_map(|part| match part["type"].as_str() {
                Some("tool_use") => part["id"].as_str(),
                Some("tool_result") => part["tool_use_id"].as_str(),
                _ => None,
            }));
        }
        ids
    }

    /// A result that follows its call; without a matching id it stays with the item before it.
    fn is_tool_result(&self) -> bool {
        matches!(self.role.as_deref(), Some("tool" | "function"))
            || self.kind.as_deref() == Some("function_call_output")
            || self
                .content
                .as_ref()
                .and_then(|content| content.as_array())
                .is_some_and(|parts| parts.iter().any(|part| part["type"] == "tool_result"))
    }
}

fn root(parent: &mut [usize], mut i: usize) -> usize {
    while parent[i] != i {
        parent[i] = parent[parent[i]];
        i = parent[i];
    }
    i
}

struct Unit {
    items: Vec<usize>,
    bytes: u64,
}

/// Drops messages from the middle of `messages` (or a Responses `input` array) until
/// `ceil(prompt_bytes / 4) + max_output_tokens <= limit`. System and developer messages
/// and the latest message are kept; a tool result is dropped together with the message
/// before it. Returns `None` when nothing can be dropped or the request still won't fit.
pub fn truncate_middle(
    body: &Bytes,
    prompt_bytes: u64,
    max_output_tokens: Option<u32>,
    limit: u32,
) -> Option<Truncated> {
    truncate_middle_for_format(
        body,
        prompt_bytes,
        max_output_tokens,
        limit,
        InboundFormat::OpenAiChat,
    )
}

pub fn truncate_middle_for_format(
    body: &Bytes,
    prompt_bytes: u64,
    max_output_tokens: Option<u32>,
    limit: u32,
    format: InboundFormat,
) -> Option<Truncated> {
    let conversation: Conversation<'_> = serde_json::from_slice(body).ok()?;
    let list = conversation.messages.or(conversation.input)?;
    let items: Vec<&RawValue> = serde_json::from_str(list.get()).ok()?;
    let fits = |bytes: u64| {
        bytes.div_ceil(4) + u64::from(max_output_tokens.unwrap_or(0)) <= u64::from(limit)
    };

    // Items sharing a call id (a call and its results) are dropped together.
    let mut parent: Vec<usize> = (0..items.len()).collect();
    let mut bytes_of = vec![0; items.len()];
    let mut conversation = Vec::new();
    let mut seen = std::collections::HashMap::new();
    for (i, raw) in items.iter().enumerate() {
        let item: Item = serde_json::from_str(raw.get()).unwrap_or_default();
        bytes_of[i] = if format == InboundFormat::AnthropicMessages {
            native_message_bytes(raw).ok()?
        } else {
            serde_json::from_str::<TextBytes>(raw.get()).ok()?.0
        };
        if matches!(item.role.as_deref(), Some("system" | "developer")) {
            continue;
        }
        let ids = item.call_ids();
        let mut matched = false;
        for id in ids {
            match seen.get(id) {
                Some(&j) => {
                    let (a, b) = (root(&mut parent, i), root(&mut parent, j));
                    parent[a.max(b)] = a.min(b);
                    matched = true;
                }
                None => {
                    seen.insert(id.to_owned(), i);
                }
            }
        }
        if !matched && item.is_tool_result() {
            if let Some(&previous) = conversation.last() {
                let (a, b) = (root(&mut parent, i), root(&mut parent, previous));
                parent[a.max(b)] = a.min(b);
            }
        }
        conversation.push(i);
    }
    let latest = root(&mut parent, *conversation.last()?);
    let mut units: Vec<Unit> = Vec::new();
    let mut unit_of = std::collections::HashMap::new();
    for &i in &conversation {
        let r = root(&mut parent, i);
        if r == latest {
            continue;
        }
        let unit = *unit_of.entry(r).or_insert_with(|| {
            units.push(Unit {
                items: Vec::new(),
                bytes: 0,
            });
            units.len() - 1
        });
        units[unit].items.push(i);
        units[unit].bytes += bytes_of[i];
    }
    let droppable = units.len();
    let center = droppable / 2;
    let (mut start, mut end) = (center, center);
    let mut bytes = prompt_bytes;
    while !fits(bytes) {
        let grow_right = end < droppable && (end - center <= center - start || start == 0);
        if grow_right {
            end += 1;
            bytes = bytes.saturating_sub(units[end - 1].bytes);
        } else if start > 0 {
            start -= 1;
            bytes = bytes.saturating_sub(units[start].bytes);
        } else {
            return None;
        }
    }
    if start == end {
        return None;
    }
    let mut drop = vec![false; items.len()];
    for unit in &units[start..end] {
        for &i in &unit.items {
            drop[i] = true;
        }
    }
    let dropped = units[start..end].iter().map(|unit| unit.items.len()).sum();

    let range_start = list.get().as_ptr() as usize - body.as_ptr() as usize;
    let range_end = range_start + list.get().len();
    let mut out = BytesMut::with_capacity(body.len());
    out.put_slice(&body[..range_start]);
    out.put_u8(b'[');
    let mut first = true;
    for (item, _) in items.iter().zip(&drop).filter(|(_, drop)| !**drop) {
        if !first {
            out.put_u8(b',');
        }
        first = false;
        out.put_slice(item.get().as_bytes());
    }
    out.put_u8(b']');
    out.put_slice(&body[range_end..]);
    Some(Truncated {
        body: out.freeze(),
        prompt_bytes: bytes,
        dropped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn prompt_bytes(body: &[u8]) -> u64 {
        crate::RequestMetadata::parse(body, llmux_types::InboundFormat::OpenAiChat)
            .unwrap()
            .prompt_bytes
    }

    fn contents(body: &[u8], key: &str) -> Vec<String> {
        let value: Value = serde_json::from_slice(body).unwrap();
        value[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                item.get("content")
                    .or_else(|| item.get("output"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn drops_from_the_middle_keeping_system_and_latest() {
        let filler = "x".repeat(400);
        let body = Bytes::from(
            json!({
                "model": "m",
                "max_tokens": 10,
                "messages": [
                    {"role": "system", "content": "sys"},
                    {"role": "user", "content": format!("u1{filler}")},
                    {"role": "assistant", "content": format!("a1{filler}"), "tool_calls": [{"id": "c"}]},
                    {"role": "tool", "content": format!("t1{filler}"), "tool_call_id": "c"},
                    {"role": "user", "content": format!("u2{filler}")},
                    {"role": "assistant", "content": format!("a2{filler}")},
                    {"role": "user", "content": "latest"},
                ],
                "vendor": {"messages": "untouched"},
            })
            .to_string(),
        );
        let bytes = prompt_bytes(&body);
        assert!(truncate_middle(&body, bytes, Some(10), u32::MAX).is_none());
        let limit = (bytes.div_ceil(4) - 200) as u32;
        let out = truncate_middle(&body, bytes, Some(10), limit).unwrap();
        assert_eq!(out.dropped, 3);
        assert_eq!(out.prompt_bytes, prompt_bytes(&out.body));
        assert!(out.prompt_bytes.div_ceil(4) + 10 <= u64::from(limit));
        let kept = contents(&out.body, "messages");
        assert_eq!(kept[0], "sys");
        assert!(kept[1].starts_with("u1") && kept[2].starts_with("a2"));
        assert_eq!(kept[3], "latest");
        let value: Value = serde_json::from_slice(&out.body).unwrap();
        assert_eq!(value["vendor"]["messages"], "untouched");
        assert_eq!(value["max_tokens"], 10);
        assert!(truncate_middle(&body, bytes, Some(10), 15).is_none());
    }

    #[test]
    fn keeps_anthropic_tool_results_with_their_calls() {
        let filler = "z".repeat(400);
        let body = Bytes::from(
            json!({
                "model": "m",
                "messages": [
                    {"role": "user", "content": format!("u1{filler}")},
                    {"role": "assistant", "content": [{"type": "tool_use", "id": "c", "input": {"q": filler}}]},
                    {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "c", "content": filler}]},
                    {"role": "user", "content": "latest"},
                ],
            })
            .to_string(),
        );
        let bytes = prompt_bytes(&body);
        let out = truncate_middle(&body, bytes, None, (bytes.div_ceil(4) - 150) as u32).unwrap();
        assert_eq!(out.dropped, 2);
        assert_eq!(
            contents(&out.body, "messages"),
            [format!("u1{filler}"), "latest".into()]
        );
    }

    #[test]
    fn keeps_parallel_calls_and_legacy_function_results_with_their_calls() {
        let filler = "w".repeat(400);
        let body = Bytes::from(
            json!({
                "model": "m",
                "input": [
                    {"role": "user", "content": format!("u1{filler}")},
                    {"type": "function_call", "call_id": "a", "arguments": filler},
                    {"type": "function_call", "call_id": "b", "arguments": filler},
                    {"type": "function_call_output", "call_id": "a", "output": "oa"},
                    {"type": "function_call_output", "call_id": "b", "output": "ob"},
                    {"role": "user", "content": "latest"},
                ],
            })
            .to_string(),
        );
        let bytes = prompt_bytes(&body);
        let out = truncate_middle(&body, bytes, None, (bytes.div_ceil(4) - 50) as u32).unwrap();
        let kept = contents(&out.body, "input");
        assert_eq!(out.dropped, 2, "{kept:?}");
        assert_eq!(
            kept,
            [
                format!("u1{filler}"),
                String::new(),
                "ob".into(),
                "latest".into()
            ]
        );

        let body = Bytes::from(
            json!({
                "model": "m",
                "messages": [
                    {"role": "user", "content": format!("u1{filler}")},
                    {"role": "assistant", "content": null, "function_call": {"name": "f", "arguments": filler}},
                    {"role": "function", "name": "f", "content": "result"},
                    {"role": "user", "content": "latest"},
                ],
            })
            .to_string(),
        );
        let bytes = prompt_bytes(&body);
        let out = truncate_middle(&body, bytes, None, (bytes.div_ceil(4) - 50) as u32).unwrap();
        assert_eq!(out.dropped, 2);
        assert_eq!(
            contents(&out.body, "messages"),
            [format!("u1{filler}"), "latest".into()]
        );

        let body = Bytes::from(
            json!({
                "model": "m",
                "messages": [
                    {"role": "user", "content": format!("u1{filler}")},
                    {"role": "assistant", "content": "", "tool_calls": [{"id": "c"}]},
                    {"role": "user", "content": format!("u2{filler}")},
                    {"role": "tool", "tool_call_id": "c", "content": "latest"},
                ],
            })
            .to_string(),
        );
        let bytes = prompt_bytes(&body);
        let out = truncate_middle(&body, bytes, None, (bytes.div_ceil(4) - 50) as u32).unwrap();
        assert_eq!(out.dropped, 1);
        assert_eq!(
            contents(&out.body, "messages"),
            [format!("u1{filler}"), String::new(), "latest".into()]
        );
    }

    #[test]
    fn keeps_responses_tool_outputs_with_their_calls() {
        let filler = "y".repeat(400);
        let body = Bytes::from(
            json!({
                "model": "m",
                "input": [
                    {"role": "developer", "content": "dev"},
                    {"role": "user", "content": format!("u1{filler}")},
                    {"type": "function_call", "call_id": "c", "arguments": filler},
                    {"type": "function_call_output", "call_id": "c", "output": format!("o1{filler}")},
                    {"role": "user", "content": "latest"},
                ],
            })
            .to_string(),
        );
        let bytes = prompt_bytes(&body);
        let out = truncate_middle(&body, bytes, None, (bytes.div_ceil(4) - 150) as u32).unwrap();
        assert_eq!(out.dropped, 2);
        assert_eq!(
            contents(&out.body, "input"),
            ["dev", &format!("u1{filler}"), "latest"]
        );
        let text = Bytes::from_static(br#"{"model":"m","input":"just text"}"#);
        assert!(truncate_middle(&text, 100, None, 1).is_none());
    }
}
