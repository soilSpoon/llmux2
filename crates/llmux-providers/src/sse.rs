use bytes::Bytes;
use futures_util::StreamExt;
#[cfg(test)]
use http::HeaderValue;
use http::{header, HeaderMap};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use llmux_types::{BoxStream, SseEvent, UpstreamError};
use serde::Deserialize;
use serde_json::Value;

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

pub(crate) fn requested_usage(body: &[u8]) -> Option<bool> {
    #[derive(Deserialize)]
    struct Chat {
        stream_options: Option<StreamOptions>,
    }
    #[derive(Deserialize)]
    struct StreamOptions {
        include_usage: Option<bool>,
    }
    serde_json::from_slice::<Chat>(body)
        .ok()?
        .stream_options?
        .include_usage
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
    mut body: Incoming,
    anthropic: bool,
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
