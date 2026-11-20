use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use llmux_types::{BoxStream, SseEvent, UpstreamError};

const MAX_EVENT_BYTES: usize = 1024 * 1024;

pub(crate) fn decode(body: Incoming) -> BoxStream<'static, Result<SseEvent, UpstreamError>> {
    decode_until(body, false)
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
                    let done = event.data == "[DONE]" || (anthropic && event.event.as_deref() == Some("message_stop"));
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
}
