//! Opt-in middle-out truncation for requests that fit no deployment's context window.

use crate::openai_compat::TextBytes;
use bytes::{BufMut, Bytes, BytesMut};
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
struct ItemKind {
    #[serde(default)]
    role: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    content: Option<serde_json::Value>,
}

impl ItemKind {
    /// Tool results must stay with the call before them: Chat `tool` messages,
    /// Responses `function_call_output` items, Anthropic `tool_result` blocks.
    fn is_tool_result(&self) -> bool {
        self.role.as_deref() == Some("tool")
            || self.kind.as_deref() == Some("function_call_output")
            || self
                .content
                .as_ref()
                .and_then(|c| c.as_array())
                .is_some_and(|parts| parts.iter().any(|part| part["type"] == "tool_result"))
    }
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
    let conversation: Conversation<'_> = serde_json::from_slice(body).ok()?;
    let list = conversation.messages.or(conversation.input)?;
    let items: Vec<&RawValue> = serde_json::from_str(list.get()).ok()?;
    let fits = |bytes: u64| {
        bytes.div_ceil(4) + u64::from(max_output_tokens.unwrap_or(0)) <= u64::from(limit)
    };

    let mut units: Vec<Unit> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let kind: ItemKind = serde_json::from_str(item.get()).unwrap_or_default();
        let bytes = serde_json::from_str::<TextBytes>(item.get()).ok()?.0;
        match kind.role.as_deref() {
            Some("system" | "developer") => {}
            _ if kind.is_tool_result() && !units.is_empty() => {
                let unit = units.last_mut()?;
                unit.items.push(i);
                unit.bytes += bytes;
            }
            _ => units.push(Unit {
                items: vec![i],
                bytes,
            }),
        }
    }
    let droppable = units.len().checked_sub(1)?;
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
