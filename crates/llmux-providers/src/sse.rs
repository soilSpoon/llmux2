use bytes::Bytes;
use futures_util::StreamExt;
#[cfg(test)]
use http::HeaderValue;
use http::{header, HeaderMap};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use llmux_types::{BoxStream, SseEvent, UpstreamError};
use serde::{de, Deserialize};
use serde_json::Value;
use std::fmt;

const MAX_EVENT_BYTES: usize = 1024 * 1024;

/// Terminal SSE payload for OpenAI-compatible streams.
pub(crate) const DONE: &str = "[DONE]";

/// Whether a response content type is `text/event-stream`, parameters ignored.
pub(crate) fn is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("text/event-stream")
        })
}

pub(crate) fn decode(body: Incoming) -> BoxStream<'static, Result<SseEvent, UpstreamError>> {
    decode_until(body, false)
}

#[derive(Clone, Copy, PartialEq)]
enum UsageMode {
    Root,
    StreamOptions,
    IncludeUsage,
    Ignore,
}

struct UsageSeed(UsageMode);

impl<'de> de::DeserializeSeed<'de> for UsageSeed {
    type Value = Option<bool>;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(UsageVisitor(self.0))
    }
}

fn consume_seq<'de, A>(mut seq: A) -> Result<(), A::Error>
where
    A: de::SeqAccess<'de>,
{
    while seq
        .next_element_seed(UsageSeed(UsageMode::Ignore))?
        .is_some()
    {}
    Ok(())
}

fn consume_map<'de, A>(mut map: A) -> Result<(), A::Error>
where
    A: de::MapAccess<'de>,
{
    while map.next_key::<de::IgnoredAny>()?.is_some() {
        map.next_value_seed(UsageSeed(UsageMode::Ignore))?;
    }
    Ok(())
}

struct UsageVisitor(UsageMode);

impl<'de> de::Visitor<'de> for UsageVisitor {
    type Value = Option<bool>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("any JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok((self.0 == UsageMode::IncludeUsage).then_some(value))
    }

    fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(None)
    }

    fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(None)
    }

    fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(None)
    }

    fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(None)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(None)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(None)
    }

    fn visit_seq<A>(self, seq: A) -> Result<Self::Value, A::Error>
    where
        A: de::SeqAccess<'de>,
    {
        consume_seq(seq)?;
        Ok(None)
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: de::MapAccess<'de>,
    {
        if matches!(self.0, UsageMode::IncludeUsage | UsageMode::Ignore) {
            consume_map(map)?;
            return Ok(None);
        }

        let mut include_usage = None;
        while let Some(key) = map.next_key::<UsageKey>()? {
            let mode = match (self.0, key) {
                (UsageMode::Root, UsageKey::StreamOptions) => UsageMode::StreamOptions,
                (UsageMode::StreamOptions, UsageKey::IncludeUsage) => UsageMode::IncludeUsage,
                _ => UsageMode::Ignore,
            };
            let value = map.next_value_seed(UsageSeed(mode))?;
            if mode != UsageMode::Ignore {
                include_usage = value;
            }
        }
        Ok(include_usage)
    }
}

enum UsageKey {
    StreamOptions,
    IncludeUsage,
    Other,
}

impl<'de> Deserialize<'de> for UsageKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_identifier(UsageKeyVisitor)
    }
}

struct UsageKeyVisitor;

impl de::Visitor<'_> for UsageKeyVisitor {
    type Value = UsageKey;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a JSON object key")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(match value {
            "stream_options" => UsageKey::StreamOptions,
            "include_usage" => UsageKey::IncludeUsage,
            _ => UsageKey::Other,
        })
    }
}

struct RequestedUsage(Option<bool>);

impl<'de> Deserialize<'de> for RequestedUsage {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        de::DeserializeSeed::deserialize(UsageSeed(UsageMode::Root), deserializer).map(Self)
    }
}

pub(crate) fn requested_usage(body: &[u8]) -> Option<bool> {
    serde_json::from_slice::<RequestedUsage>(body).ok()?.0
}

pub(crate) fn with_chat_usage(
    mut stream: BoxStream<'static, Result<SseEvent, UpstreamError>>,
    include: bool,
) -> BoxStream<'static, Result<SseEvent, UpstreamError>> {
    Box::pin(async_stream::try_stream! {
        let mut final_usage = None;
        while let Some(event) = stream.next().await {
            let mut event = event?;
            if event.data == "[DONE]" {
                if let Some(usage) = final_usage.take() {
                    yield usage;
                }
                yield event;
                return;
            }
            // Delta chunks carry no "usage" key; skip the DOM parse and forward them
            // untouched. Only usage-bearing chunks (or every chunk when include=true
            // rewrites usage:null) need the Value walk. A `\u0075`-escaped key still
            // decodes as `usage`, so those take the DOM path too.
            if !include
                && !event.data.contains("\"usage\"")
                && !event.data.contains("\\u0075")
            {
                yield event;
                continue;
            }
            if let Ok(Value::Object(mut chunk)) = serde_json::from_str(&event.data) {
                if let Some(choices) = chunk.get("choices").and_then(Value::as_array) {
                    let empty = choices.is_empty();
                    if chunk.get("usage").is_some_and(Value::is_object) {
                        if include {
                            final_usage = Some(if empty {
                                event.clone()
                            } else {
                                let mut usage = chunk.clone();
                                usage.insert("choices".into(), Value::Array(Vec::new()));
                                SseEvent {
                                    event: event.event.clone(),
                                    data: Value::Object(usage).to_string(),
                                }
                            });
                        }
                        if empty {
                            continue;
                        }
                    }
                    if include {
                        if !empty && chunk.get("usage") != Some(&Value::Null) {
                            chunk.insert("usage".into(), Value::Null);
                            event.data = Value::Object(chunk).to_string();
                        }
                    } else if chunk.remove("usage").is_some() {
                        event.data = Value::Object(chunk).to_string();
                    }
                }
            }
            yield event;
        }
    })
}

pub(crate) fn decode_until(
    body: Incoming,
    anthropic: bool,
) -> BoxStream<'static, Result<SseEvent, UpstreamError>> {
    decode_inner(body, anthropic, false)
}

/// Antigravity's `/v1internal` stream ends at EOF with no `data: [DONE]` frame —
/// a clean frame boundary is end-of-stream, pending bytes are truncation.
pub(crate) fn decode_eof(body: Incoming) -> BoxStream<'static, Result<SseEvent, UpstreamError>> {
    decode_inner(body, false, true)
}

fn decode_inner(
    mut body: Incoming,
    anthropic: bool,
    eof_ok: bool,
) -> BoxStream<'static, Result<SseEvent, UpstreamError>> {
    Box::pin(async_stream::try_stream! {
        let mut parser = Parser::default();
        let mut bytes_sent = 0;
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|_| UpstreamError::StreamBroken { bytes_sent })?;
            let Ok(mut chunk) = frame.into_data() else { continue };
            while !chunk.is_empty() {
                if let Some(event) = parser.next(&mut chunk)
                    .map_err(|()| UpstreamError::StreamBroken { bytes_sent })?
                {
                    let done = event.data == DONE || (anthropic && event.event.as_deref() == Some("message_stop"));
                    bytes_sent += event.data.len() as u64;
                    yield event;
                    if done {
                        return;
                    }
                }
            }
        }
        if eof_ok && parser.at_boundary() {
            return;
        }
        Err(UpstreamError::StreamBroken { bytes_sent })?;
    })
}

#[derive(Default)]
struct Parser {
    line: Vec<u8>,
    size: usize,
    data: Option<String>,
    event: Option<String>,
    skip_lf: bool,
    seen_first_line: bool,
}

impl Parser {
    /// No partial line or unterminated event is pending — the stream ended on
    /// an event boundary.
    fn at_boundary(&self) -> bool {
        self.line.is_empty() && self.data.is_none() && self.event.is_none()
    }

    fn next(&mut self, chunk: &mut Bytes) -> Result<Option<SseEvent>, ()> {
        if self.skip_lf {
            self.skip_lf = false;
            if chunk.first() == Some(&b'\n') {
                let _ = chunk.split_to(1);
            }
        }
        if chunk.is_empty() {
            return Ok(None);
        }
        let end = memchr::memchr2(b'\r', b'\n', chunk);
        let count = end.unwrap_or(chunk.len());
        self.size += count + usize::from(end.is_some());
        if self.size > MAX_EVENT_BYTES {
            return Err(());
        }
        self.line.extend_from_slice(&chunk.split_to(count));
        let Some(_) = end else { return Ok(None) };
        self.skip_lf = chunk.split_to(1)[0] == b'\r';

        let mut line = std::str::from_utf8(&self.line).map_err(|_| ())?;
        if !self.seen_first_line {
            line = line.strip_prefix('\u{feff}').unwrap_or(line);
            self.seen_first_line = true;
        }
        let event = if line.is_empty() {
            self.size = 0;
            let name = self.event.take();
            self.data.take().map(|data| SseEvent { event: name, data })
        } else {
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "data" => match &mut self.data {
                    Some(data) => {
                        data.push('\n');
                        data.push_str(value);
                    }
                    None => self.data = Some(value.to_owned()),
                },
                "event" => self.event = (!value.is_empty()).then(|| value.to_owned()),
                _ => {}
            }
            None
        };
        self.line.clear();
        Ok(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_stream_accepts_parameters_and_rejects_json() {
        let mut headers = HeaderMap::new();
        assert!(!is_event_stream(&headers));
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        assert!(!is_event_stream(&headers));
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream; charset=utf-8"),
        );
        assert!(is_event_stream(&headers));
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("Text/Event-Stream"),
        );
        assert!(is_event_stream(&headers));
    }

    #[test]
    fn handles_split_crlf_bom_utf8_and_multiline_data() {
        let mut parser = Parser::default();
        let mut events = Vec::new();
        let wire =
            "\u{feff}: ping\r\n\r\nevent: example\rdata: café\r\ndata: next\n\rdata: [DONE]\n\n";
        for byte in wire.as_bytes() {
            let mut chunk = Bytes::copy_from_slice(&[*byte]);
            while !chunk.is_empty() {
                events.extend(parser.next(&mut chunk).unwrap());
            }
        }
        assert_eq!(
            events,
            vec![
                SseEvent {
                    event: Some("example".into()),
                    data: "café\nnext".into()
                },
                SseEvent {
                    event: None,
                    data: "[DONE]".into()
                }
            ]
        );
    }

    #[test]
    fn rejects_an_unbounded_partial_event() {
        let mut parser = Parser::default();
        let mut oversized = Bytes::from(vec![b'a'; MAX_EVENT_BYTES + 1]);
        assert!(parser.next(&mut oversized).is_err());
    }

    fn dom_requested_usage(body: &[u8]) -> Option<bool> {
        let value: Value = serde_json::from_slice(body).ok()?;
        value["stream_options"]["include_usage"].as_bool()
    }

    #[test]
    fn requested_usage_matches_dom_last_duplicate_wins() {
        let cases = [
            (
                "include_usage false then true",
                r#"{"stream_options":{"include_usage":false,"include_usage":true}}"#,
            ),
            (
                "include_usage true then false",
                r#"{"stream_options":{"include_usage":true,"include_usage":false}}"#,
            ),
            (
                "stream_options false then true",
                r#"{"stream_options":{"include_usage":false},"stream_options":{"include_usage":true}}"#,
            ),
            (
                "stream_options true then false",
                r#"{"stream_options":{"include_usage":true},"stream_options":{"include_usage":false}}"#,
            ),
            (
                "escaped duplicate include_usage",
                r#"{"stream_options":{"include_usage":false,"includ\u0065_usage":true}}"#,
            ),
            (
                "stream_options overwritten by null",
                r#"{"stream_options":{"include_usage":true},"stream_options":null}"#,
            ),
            (
                "null stream_options overwritten by an object",
                r#"{"stream_options":null,"stream_options":{"include_usage":true}}"#,
            ),
            (
                "array stream_options overwritten by an object",
                r#"{"stream_options":[],"stream_options":{"include_usage":true}}"#,
            ),
            (
                "null include_usage overwritten by true",
                r#"{"stream_options":{"include_usage":null,"include_usage":true}}"#,
            ),
            (
                "object include_usage overwritten by false",
                r#"{"stream_options":{"include_usage":{"nested":true},"include_usage":false}}"#,
            ),
            (
                "same names nested in ignored objects do not count",
                r#"{"noise":{"stream_options":{"include_usage":true}},"stream_options":{"include_usage":false}}"#,
            ),
            (
                "deeply nested ignored objects do not count",
                r#"{"noise":{"a":[{"b":{"stream_options":{"include_usage":true}}}]},"stream_options":{"include_usage":false}}"#,
            ),
        ];
        for (name, body) in cases {
            assert_eq!(
                requested_usage(body.as_bytes()),
                dom_requested_usage(body.as_bytes()),
                "{name}"
            );
        }
    }

    #[test]
    fn requested_usage_matches_dom_for_bounded_duplicate_combinations() {
        let include_values = ["null", "true", "false", "0", r#""text""#, "[]", "{}"];
        for first in include_values {
            for second in include_values {
                let body = format!(
                    r#"{{"stream_options":{{"include_usage":{first},"include_usage":{second}}}}}"#
                );
                assert_eq!(
                    requested_usage(body.as_bytes()),
                    dom_requested_usage(body.as_bytes()),
                    "include_usage {first} then {second}"
                );
            }
        }

        let stream_values = [
            "null",
            "true",
            "false",
            "0",
            r#""text""#,
            "[]",
            "{}",
            r#"{"include_usage":true}"#,
            r#"{"include_usage":false}"#,
            r#"{"include_usage":null}"#,
            r#"{"include_usage":true,"include_usage":false}"#,
        ];
        for first in stream_values {
            for second in stream_values {
                let body = format!(r#"{{"stream_options":{first},"stream_options":{second}}}"#);
                assert_eq!(
                    requested_usage(body.as_bytes()),
                    dom_requested_usage(body.as_bytes()),
                    "stream_options {first} then {second}"
                );
            }
        }
    }

    #[test]
    fn requested_usage_rejects_malformed_duplicate_value() {
        let body = r#"{"stream_options":{"include_usage":true},"stream_options":{"include_usage":"#;
        assert_eq!(requested_usage(body.as_bytes()), None);
    }

    #[test]
    fn requested_usage_matches_dom_for_deeply_nested_ignored_values() {
        for depth in [1, 64, 127, 128, 129] {
            let mut nested = "null".to_owned();
            for _ in 0..depth {
                nested = format!("[{nested}]");
            }
            let body = format!(r#"{{"noise":{nested},"stream_options":{{"include_usage":true}}}}"#);
            assert_eq!(
                requested_usage(body.as_bytes()),
                dom_requested_usage(body.as_bytes()),
                "depth {depth}"
            );
        }
    }

    #[tokio::test]
    async fn excluded_usage_forwards_plain_deltas_untouched() {
        let delta = r#"{"choices":[{"delta":{"content":"hi"}}]}"#;
        let events: BoxStream<'static, Result<SseEvent, UpstreamError>> =
            Box::pin(futures_util::stream::iter(vec![
                Ok(SseEvent {
                    event: None,
                    data: delta.into(),
                }),
                Ok(SseEvent {
                    event: None,
                    data: r#"{"choices":[{"delta":{"content":"x"}}],"usage":{"total":1}}"#.into(),
                }),
                Ok(SseEvent {
                    event: None,
                    data: r#"{"choices":[],"usage":{"total":10}}"#.into(),
                }),
                Ok(SseEvent {
                    event: None,
                    data: r#"{"choices":[],"\u0075sage":{"total":5}}"#.into(),
                }),
                Ok(SseEvent {
                    event: None,
                    data: "[DONE]".into(),
                }),
            ]));
        let out = with_chat_usage(events, false).collect::<Vec<_>>().await;
        assert_eq!(out.len(), 3);
        // No "usage" key: byte-identical, no DOM rewrite.
        assert_eq!(out[0].as_ref().unwrap().data, delta);
        // Real usage key: stripped from a non-empty chunk.
        assert_eq!(
            out[1].as_ref().unwrap().data,
            r#"{"choices":[{"delta":{"content":"x"}}]}"#
        );
        // Empty-choices usage chunk stays swallowed.
        assert_eq!(out[2].as_ref().unwrap().data, "[DONE]");
    }
}
