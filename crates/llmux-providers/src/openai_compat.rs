use crate::{responses, sse};
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use http::{
    header::{self, HeaderMap, HeaderValue},
    Method, Request, Response, StatusCode, Uri,
};
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use llmux_types::{
    AuthError, AuthMaterial, BuildError, CredentialSource, Deployment, GatewayRequest,
    GatewayResponse, InboundFormat, LimitObservation, LimitScope, ProviderAdapter, ProviderKind,
    Support, UpstreamError,
};
use serde::Deserialize;
use serde_json::value::RawValue;
use std::{
    borrow::Cow,
    ops::Range,
    sync::Arc,
    time::{Duration, SystemTime},
};

const MAX_COMPLETE_BYTES: usize = 16 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 64 * 1024;
const ERROR_BODY_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Deserialize)]
struct RawChatMetadata<'a> {
    #[serde(borrow)]
    model: &'a RawValue,
    #[serde(default)]
    stream: Option<bool>,
}

pub struct OpenAiChatMetadata<'a> {
    pub model: Cow<'a, str>,
    pub stream: bool,
}

/// Request fields the gateway reads before routing, parsed in one pass.
pub struct RequestMetadata<'a> {
    pub model: Cow<'a, str>,
    pub stream: bool,
    /// Bytes of strings and object keys in prompt fields, excluding `data:` URLs.
    pub prompt_bytes: u64,
    /// The output limit the inbound format forwards: Responses `max_output_tokens`,
    /// Chat `max_completion_tokens` else `max_tokens`, Anthropic `max_tokens`.
    pub max_output_tokens: Option<u32>,
}

#[derive(Deserialize)]
struct RawRequestMetadata<'a> {
    #[serde(borrow)]
    model: &'a RawValue,
    #[serde(default)]
    stream: Option<bool>,
    #[serde(default, borrow)]
    max_completion_tokens: Option<&'a RawValue>,
    #[serde(default, borrow)]
    max_tokens: Option<&'a RawValue>,
    #[serde(default, borrow)]
    max_output_tokens: Option<&'a RawValue>,
    #[serde(default)]
    messages: TextBytes,
    #[serde(default)]
    input: TextBytes,
    #[serde(default)]
    instructions: TextBytes,
    #[serde(default)]
    system: TextBytes,
    #[serde(default)]
    tools: TextBytes,
}

impl<'a> RequestMetadata<'a> {
    pub fn parse(body: &'a [u8], format: InboundFormat) -> Result<Self, BuildError> {
        if body.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
            return Err(BuildError::InvalidBody);
        }
        let raw: RawRequestMetadata<'_> =
            serde_json::from_slice(body).map_err(|_| BuildError::InvalidBody)?;
        let model = serde_json::from_str::<BorrowedModel<'_>>(raw.model.get())
            .map_err(|_| BuildError::InvalidBody)?
            .0;
        if model.is_empty() {
            return Err(BuildError::InvalidBody);
        }
        let tokens = |value: Option<&RawValue>| value.and_then(|v| v.get().parse::<u32>().ok());
        Ok(Self {
            model,
            stream: raw.stream.unwrap_or_default(),
            prompt_bytes: [
                raw.messages,
                raw.input,
                raw.instructions,
                raw.system,
                raw.tools,
            ]
            .iter()
            .map(|text| text.0)
            .sum(),
            max_output_tokens: match format {
                InboundFormat::OpenAiResponses => tokens(raw.max_output_tokens),
                InboundFormat::OpenAiChat => {
                    tokens(raw.max_completion_tokens).or_else(|| tokens(raw.max_tokens))
                }
                InboundFormat::AnthropicMessages => tokens(raw.max_tokens),
            },
        })
    }
}

fn is_base64_data_url(text: &str) -> bool {
    text.strip_prefix("data:")
        .and_then(|rest| rest.split_once(',').map(|(media, _)| media))
        .is_some_and(|media| media.len() < 256 && media.ends_with(";base64"))
}

/// Sums string and key lengths of any JSON value without allocating.
#[derive(Default)]
struct TextBytes(u64);

impl<'de> Deserialize<'de> for TextBytes {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = TextBytes;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("any JSON value")
            }

            fn visit_str<E>(self, v: &str) -> Result<TextBytes, E> {
                Ok(TextBytes(if is_base64_data_url(v) {
                    0
                } else {
                    v.len() as u64
                }))
            }

            fn visit_bool<E>(self, _: bool) -> Result<TextBytes, E> {
                Ok(TextBytes(0))
            }

            fn visit_i64<E>(self, _: i64) -> Result<TextBytes, E> {
                Ok(TextBytes(0))
            }

            fn visit_u64<E>(self, _: u64) -> Result<TextBytes, E> {
                Ok(TextBytes(0))
            }

            fn visit_f64<E>(self, _: f64) -> Result<TextBytes, E> {
                Ok(TextBytes(0))
            }

            fn visit_unit<E>(self) -> Result<TextBytes, E> {
                Ok(TextBytes(0))
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<TextBytes, A::Error> {
                let mut total = 0;
                while let Some(TextBytes(n)) = seq.next_element()? {
                    total += n;
                }
                Ok(TextBytes(total))
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<TextBytes, A::Error> {
                let mut total = 0;
                while let Some((TextBytes(k), TextBytes(v))) = map.next_entry()? {
                    total += k + v;
                }
                Ok(TextBytes(total))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

impl<'a> OpenAiChatMetadata<'a> {
    pub fn parse(body: &'a [u8]) -> Result<Self, BuildError> {
        Self::parse_with_range(body).map(|(metadata, _)| metadata)
    }

    fn parse_with_range(body: &'a [u8]) -> Result<(Self, Range<usize>), BuildError> {
        if body.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
            return Err(BuildError::InvalidBody);
        }
        let raw: RawChatMetadata<'_> =
            serde_json::from_slice(body).map_err(|_| BuildError::InvalidBody)?;
        let value = raw.model.get();
        let start = value.as_ptr() as usize - body.as_ptr() as usize;
        let model = serde_json::from_str::<BorrowedModel<'_>>(value)
            .map_err(|_| BuildError::InvalidBody)?
            .0;
        if model.is_empty() {
            return Err(BuildError::InvalidBody);
        }
        Ok((
            Self {
                model,
                stream: raw.stream.unwrap_or_default(),
            },
            start..start + value.len(),
        ))
    }
}

#[derive(Deserialize)]
struct BorrowedModel<'a>(#[serde(borrow)] Cow<'a, str>);

pub(crate) fn rewrite_model(body: &Bytes, model: &str) -> Result<Bytes, BuildError> {
    let (metadata, range) = OpenAiChatMetadata::parse_with_range(body)?;
    if metadata.model == model {
        return Ok(body.clone());
    }
    let replacement = serde_json::to_string(model).map_err(|_| BuildError::InvalidBody)?;
    let mut rewritten = BytesMut::with_capacity(body.len() - range.len() + replacement.len());
    rewritten.extend_from_slice(&body[..range.start]);
    rewritten.extend_from_slice(replacement.as_bytes());
    rewritten.extend_from_slice(&body[range.end..]);
    Ok(rewritten.freeze())
}

pub struct StaticKey(AuthMaterial);

impl StaticKey {
    pub fn bearer(key: &str) -> Result<Self, BuildError> {
        let mut value =
            HeaderValue::from_str(&format!("Bearer {key}")).map_err(|_| BuildError::InvalidAuth)?;
        value.set_sensitive(true);
        Ok(Self(AuthMaterial::Header {
            name: header::AUTHORIZATION,
            value,
        }))
    }
}

#[async_trait]
impl CredentialSource for StaticKey {
    fn current(&self) -> Result<AuthMaterial, AuthError> {
        Ok(self.0.clone())
    }

    async fn refresh(&self) -> Result<(), AuthError> {
        Err(AuthError::RefreshFailed)
    }
}

pub struct OpenAiCompat(ProviderKind);

impl OpenAiCompat {
    pub fn new(kind: impl Into<Arc<str>>) -> Self {
        Self(ProviderKind(kind.into()))
    }
}

fn chat_uri(base: &Uri) -> Result<Uri, BuildError> {
    let mut parts = base.clone().into_parts();
    let path = format!("{}/chat/completions", base.path().trim_end_matches('/'));
    parts.path_and_query = Some(path.parse()?);
    Uri::from_parts(parts).map_err(|err| BuildError::InvalidRequest(err.into()))
}

#[async_trait]
impl ProviderAdapter for OpenAiCompat {
    fn kind(&self) -> &ProviderKind {
        &self.0
    }

    fn supports(&self, inbound: InboundFormat) -> Support {
        match inbound {
            InboundFormat::OpenAiChat => Support::Passthrough,
            InboundFormat::OpenAiResponses => Support::Convert,
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
        let mut request = Request::builder()
            .method(Method::POST)
            .uri(chat_uri(&dep.base_url)?)
            .header(header::CONTENT_TYPE, "application/json")
            .header(
                header::ACCEPT,
                if req.stream {
                    "text/event-stream"
                } else {
                    "application/json"
                },
            )
            .body(if req.format == InboundFormat::OpenAiResponses {
                responses::request(&req.body, &dep.upstream_model)?
            } else {
                rewrite_model(&req.body, &dep.upstream_model)?
            })?;

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
        _dep: &Deployment,
        resp: Response<Incoming>,
    ) -> Result<GatewayResponse, UpstreamError> {
        let (parts, body) = resp.into_parts();
        let status = parts.status;
        if !status.is_success() {
            let body = collect_error(body).await;
            return Err(classify_error(
                status,
                &parts.headers,
                body,
                SystemTime::now(),
            ));
        }

        if req.stream {
            let is_sse = parts
                .headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| {
                    v.split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .eq_ignore_ascii_case("text/event-stream")
                });
            if !is_sse {
                return Err(bad_gateway());
            }
            let stream = sse::decode(body);
            Ok(GatewayResponse::Stream(
                if req.format == InboundFormat::OpenAiResponses {
                    responses::stream(stream)
                } else {
                    stream
                },
            ))
        } else {
            let body = collect(body).await?;
            if let Some(error) = embedded_error(&body, &parts.headers, SystemTime::now()) {
                return Err(error);
            }
            let (body, usage) = if req.format == InboundFormat::OpenAiResponses {
                responses::complete(&body).map_err(|()| bad_gateway())?
            } else {
                (body, None)
            };
            let mut headers = parts.headers;
            if req.format == InboundFormat::OpenAiResponses {
                headers.remove(header::CONTENT_LENGTH);
            }
            Ok(GatewayResponse::Complete {
                status,
                headers,
                body,
                usage,
            })
        }
    }

    fn observe_limits(&self, headers: &HeaderMap) -> Option<LimitObservation> {
        let count = |name| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u32>().ok())
        };
        let remaining_requests = count("x-ratelimit-remaining-requests");
        let remaining_tokens = count("x-ratelimit-remaining-tokens");
        let reset_requests =
            header_text(headers, "x-ratelimit-reset-requests").and_then(parse_duration);
        let reset_tokens =
            header_text(headers, "x-ratelimit-reset-tokens").and_then(parse_duration);
        if remaining_requests.is_none()
            && remaining_tokens.is_none()
            && reset_requests.is_none()
            && reset_tokens.is_none()
        {
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

pub(crate) async fn collect(body: Incoming) -> Result<Bytes, UpstreamError> {
    Limited::new(body, MAX_COMPLETE_BYTES)
        .collect()
        .await
        .map(|buf| buf.to_bytes())
        .map_err(|_| bad_gateway())
}

async fn collect_error<B: hyper::body::Body<Data = Bytes> + Unpin>(mut body: B) -> Bytes {
    let mut bytes = BytesMut::new();
    let deadline = tokio::time::Instant::now() + ERROR_BODY_TIMEOUT;
    while bytes.len() < MAX_ERROR_BYTES {
        match tokio::time::timeout_at(deadline, body.frame()).await {
            Ok(Some(Ok(frame))) => {
                if let Ok(data) = frame.into_data() {
                    let len = data.len().min(MAX_ERROR_BYTES - bytes.len());
                    bytes.extend_from_slice(&data[..len]);
                }
            }
            _ => break,
        }
    }
    bytes.freeze()
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok().map(str::trim)
}

fn parse_duration(mut value: &str) -> Option<Duration> {
    let mut seconds = 0.0_f64;
    if value.is_empty() {
        return None;
    }
    while !value.is_empty() {
        let end = value.find(|c: char| !c.is_ascii_digit() && c != '.')?;
        let number = value[..end].parse::<f64>().ok()?;
        value = &value[end..];
        let (unit, scale) = if value.starts_with("ms") {
            (2, 0.001)
        } else {
            (
                1,
                match value.as_bytes().first()? {
                    b's' => 1.0,
                    b'm' => 60.0,
                    b'h' => 3600.0,
                    b'd' => 86400.0,
                    _ => return None,
                },
            )
        };
        seconds += number * scale;
        value = &value[unit..];
    }
    Some(Duration::from_secs_f64(
        seconds.min(MAX_RETRY.as_secs_f64()),
    ))
}

fn error_details(body: &[u8]) -> serde_json::Value {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|mut value| value.get_mut("error").map(serde_json::Value::take))
        .unwrap_or_default()
}

fn classify_error(
    status: StatusCode,
    headers: &HeaderMap,
    body: Bytes,
    now: SystemTime,
) -> UpstreamError {
    let details = error_details(&body);
    classify_error_details(status, headers, body, now, &details)
}

fn classify_error_details(
    status: StatusCode,
    headers: &HeaderMap,
    body: Bytes,
    now: SystemTime,
    details: &serde_json::Value,
) -> UpstreamError {
    let code = details
        .get("code")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let kind = details
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let message = details
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let scope = if header_text(headers, "x-ratelimit-remaining-requests") == Some("0") {
        LimitScope::Requests
    } else if header_text(headers, "x-ratelimit-remaining-tokens") == Some("0")
        || message.contains("tokens per")
        || kind.contains("tokens")
        || code.contains("tokens")
    {
        LimitScope::Tokens
    } else if message.contains("requests per")
        || kind.contains("requests")
        || code.contains("requests")
    {
        LimitScope::Requests
    } else {
        LimitScope::Unknown
    };
    let wait = retry_after(headers, now)
        .or_else(|| {
            header_text(headers, "retry-after-ms")?
                .parse::<f64>()
                .ok()
                .filter(|n| n.is_finite() && *n >= 0.0)
                .map(|n| Duration::from_secs_f64((n / 1000.0).min(MAX_RETRY.as_secs_f64())))
        })
        .or_else(|| match scope {
            LimitScope::Requests => {
                header_text(headers, "x-ratelimit-reset-requests").and_then(parse_duration)
            }
            LimitScope::Tokens => {
                header_text(headers, "x-ratelimit-reset-tokens").and_then(parse_duration)
            }
            LimitScope::Unknown => None,
        });
    let quota = [code, kind].iter().any(|marker| {
        matches!(
            *marker,
            "insufficient_quota"
                | "quota_exceeded"
                | "quota_exhausted"
                | "billing_hard_limit_reached"
                | "billing_not_active"
                | "insufficient_balance"
                | "credit_balance_too_low"
                | "credits_exhausted"
        )
    });
    match status.as_u16() {
        402 => UpstreamError::QuotaExhausted {
            reset_at: wait.and_then(|wait| now.checked_add(wait)),
        },
        429 if quota => UpstreamError::QuotaExhausted {
            reset_at: wait.and_then(|wait| now.checked_add(wait)),
        },
        429 => UpstreamError::RateLimited {
            retry_after: wait,
            scope,
        },
        401 => UpstreamError::Auth { refreshable: true },
        403 => UpstreamError::Auth { refreshable: false },
        408 => UpstreamError::Timeout,
        400 | 413 | 422
            if code == "context_length_exceeded"
                || kind == "context_length_exceeded"
                || message.contains("maximum context length")
                || message.contains("too many tokens")
                || message.contains("context window exceeded") =>
        {
            UpstreamError::ContextLength
        }
        500..=599 => UpstreamError::Server { status },
        400..=499 => UpstreamError::BadRequest { status, body },
        _ => bad_gateway(),
    }
}

#[derive(Deserialize)]
struct ResponseEnvelope<'a> {
    #[serde(borrow)]
    error: Option<&'a RawValue>,
    #[serde(borrow)]
    choices: Option<&'a RawValue>,
}

fn embedded_error(body: &Bytes, headers: &HeaderMap, now: SystemTime) -> Option<UpstreamError> {
    // Normal completions only pay for a byte scan; suspect envelopes borrow raw fields.
    if memchr::memmem::find(body, b"\"error\"").is_none() && !body.contains(&b'\\') {
        return None;
    }
    let envelope: ResponseEnvelope<'_> = serde_json::from_slice(body).ok()?;
    if envelope.choices.is_some() || !envelope.error?.get().starts_with('{') {
        return None;
    }
    let bounded = body.slice(..body.len().min(MAX_ERROR_BYTES));
    let details = error_details(body);
    let status = ["status", "status_code", "code"]
        .iter()
        .find_map(|key| {
            let value = details.get(*key)?;
            let number = value
                .as_u64()
                .and_then(|n| u16::try_from(n).ok())
                .or_else(|| value.as_str()?.parse().ok())?;
            StatusCode::from_u16(number)
                .ok()
                .filter(|s| s.is_client_error() || s.is_server_error())
        })
        .unwrap_or_else(|| {
            let code = details
                .get("code")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let kind = details
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if [code, kind].iter().any(|v| {
                v.contains("quota")
                    || v.contains("rate_limit")
                    || v.contains("billing")
                    || v.contains("balance")
                    || v.contains("credits")
            }) {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::BAD_REQUEST
            }
        });
    Some(classify_error_details(
        status, headers, bounded, now, &details,
    ))
}

pub(crate) fn bad_gateway() -> UpstreamError {
    UpstreamError::Server {
        status: StatusCode::BAD_GATEWAY,
    }
}

pub(crate) fn retry_after(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
    let value = headers.get(header::RETRY_AFTER)?.to_str().ok()?.trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Some(Duration::from_secs(value.parse().unwrap_or(u64::MAX)).min(MAX_RETRY));
    }
    let date = httpdate::parse_http_date(value).ok()?;
    Some(date.duration_since(now).unwrap_or_default().min(MAX_RETRY))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(status: u16, body: &str) -> UpstreamError {
        classify_error(
            StatusCode::from_u16(status).unwrap(),
            &HeaderMap::new(),
            Bytes::copy_from_slice(body.as_bytes()),
            SystemTime::UNIX_EPOCH,
        )
    }

    #[test]
    fn classifies_status_even_without_a_valid_body() {
        for body in ["", "not json", r#"{"error":null}"#] {
            assert!(matches!(
                classify(401, body),
                UpstreamError::Auth { refreshable: true }
            ));
            assert!(matches!(
                classify(403, body),
                UpstreamError::Auth { refreshable: false }
            ));
            assert!(matches!(
                classify(402, body),
                UpstreamError::QuotaExhausted { reset_at: None }
            ));
            assert!(matches!(classify(408, body), UpstreamError::Timeout));
            assert!(matches!(
                classify(429, body),
                UpstreamError::RateLimited {
                    scope: LimitScope::Unknown,
                    retry_after: None
                }
            ));
            for status in [500, 503, 529, 599] {
                assert!(
                    matches!(classify(status, body), UpstreamError::Server { status: s } if s.as_u16() == status)
                );
            }
            for status in [400, 404, 413, 422] {
                assert!(
                    matches!(classify(status, body), UpstreamError::BadRequest { status: s, body: b } if s.as_u16() == status && b == body)
                );
            }
            assert!(matches!(
                classify(302, body),
                UpstreamError::Server {
                    status: StatusCode::BAD_GATEWAY
                }
            ));
        }
    }

    #[test]
    fn recognizes_quota_and_context_markers() {
        for field in ["code", "type"] {
            for marker in [
                "insufficient_quota",
                "quota_exceeded",
                "quota_exhausted",
                "billing_hard_limit_reached",
                "billing_not_active",
                "insufficient_balance",
                "credit_balance_too_low",
                "credits_exhausted",
            ] {
                let body = format!(r#"{{"error":{{"{field}":"{marker}"}}}}"#);
                assert!(
                    matches!(classify(429, &body), UpstreamError::QuotaExhausted { .. }),
                    "{body}"
                );
            }
        }
        for status in [400, 413, 422] {
            for details in [
                r#""code":"context_length_exceeded""#,
                r#""message":"Maximum context length is 4096""#,
                r#""message":"Too many tokens in prompt""#,
            ] {
                assert!(matches!(
                    classify(status, &format!(r#"{{"error":{{{details}}}}}"#)),
                    UpstreamError::ContextLength
                ));
            }
        }
        assert!(matches!(
            classify(400, r#"{"error":{"message":"invalid token"}}"#),
            UpstreamError::BadRequest { .. }
        ));
    }

    #[test]
    fn parses_bounded_durations_and_rejects_malformed_values() {
        for (value, millis) in [
            ("1s", 1000),
            ("6m0s", 360000),
            ("20ms", 20),
            ("1h2m3.5s", 3723500),
            ("0s", 0),
        ] {
            assert_eq!(
                parse_duration(value),
                Some(Duration::from_millis(millis)),
                "{value}"
            );
        }
        for value in ["", "-1s", "NaNs", "1", "1s garbage", "1.2.3s", "1xs"] {
            assert_eq!(parse_duration(value), None, "{value}");
        }
        assert_eq!(parse_duration("999999999999999999999999h"), Some(MAX_RETRY));
    }

    #[test]
    fn rate_limit_scope_and_retry_precedence() {
        let mut headers = HeaderMap::new();
        let now = SystemTime::UNIX_EPOCH;
        let body =
            Bytes::from_static(br#"{"error":{"message":"Rate limit on tokens per minute"}}"#);
        let check = |headers: &HeaderMap, scope, wait| {
            assert!(
                matches!(classify_error(StatusCode::TOO_MANY_REQUESTS, headers, body.clone(), now), UpstreamError::RateLimited { scope: s, retry_after: w } if s == scope && w == wait)
            );
        };
        check(&headers, LimitScope::Tokens, None);
        headers.insert("x-ratelimit-reset-tokens", HeaderValue::from_static("20ms"));
        check(
            &headers,
            LimitScope::Tokens,
            Some(Duration::from_millis(20)),
        );
        headers.insert(
            "x-ratelimit-remaining-requests",
            HeaderValue::from_static("0"),
        );
        headers.insert(
            "x-ratelimit-reset-requests",
            HeaderValue::from_static("6m0s"),
        );
        check(
            &headers,
            LimitScope::Requests,
            Some(Duration::from_secs(360)),
        );
        headers.insert("retry-after-ms", HeaderValue::from_static("1250"));
        check(
            &headers,
            LimitScope::Requests,
            Some(Duration::from_millis(1250)),
        );
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("2"));
        check(&headers, LimitScope::Requests, Some(Duration::from_secs(2)));
        assert!(
            matches!(classify_error(StatusCode::TOO_MANY_REQUESTS, &headers, Bytes::from_static(br#"{"error":{"code":"insufficient_quota"}}"#), now), UpstreamError::QuotaExhausted { reset_at: Some(reset) } if reset == now + Duration::from_secs(2))
        );
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("bad"));
        headers.insert("retry-after-ms", HeaderValue::from_static("NaN"));
        check(
            &headers,
            LimitScope::Requests,
            Some(Duration::from_secs(360)),
        );
        headers.insert(
            header::RETRY_AFTER,
            HeaderValue::from_static("999999999999999999999999"),
        );
        check(&headers, LimitScope::Requests, Some(MAX_RETRY));
    }

    #[test]
    fn observations_include_resets_without_counts_and_ignore_bad_headers() {
        let adapter = OpenAiCompat::new("test");
        let mut headers = HeaderMap::new();
        assert_eq!(adapter.observe_limits(&headers), None);
        headers.insert(
            "x-ratelimit-remaining-requests",
            HeaderValue::from_static("invalid"),
        );
        headers.insert(
            "x-ratelimit-reset-tokens",
            HeaderValue::from_static("invalid"),
        );
        assert_eq!(adapter.observe_limits(&headers), None);
        headers.insert(
            "x-ratelimit-reset-requests",
            HeaderValue::from_static("1h2m3.5s"),
        );
        assert_eq!(
            adapter.observe_limits(&headers).unwrap().reset_requests,
            Some(Duration::from_millis(3723500))
        );
        headers.insert(
            "x-ratelimit-remaining-requests",
            HeaderValue::from_static("0"),
        );
        headers.insert(
            "x-ratelimit-remaining-tokens",
            HeaderValue::from_static("42"),
        );
        headers.insert("x-ratelimit-reset-tokens", HeaderValue::from_static("20ms"));
        assert_eq!(
            adapter.observe_limits(&headers),
            Some(LimitObservation {
                remaining_requests: Some(0),
                remaining_tokens: Some(42),
                reset_requests: Some(Duration::from_millis(3723500)),
                reset_tokens: Some(Duration::from_millis(20))
            })
        );
    }

    #[test]
    fn large_embedded_errors_keep_details_but_bound_passthrough_bytes() {
        let headers = HeaderMap::new();
        let now = SystemTime::UNIX_EPOCH;
        for (details, expected) in [
            (
                r#""status":429,"code":"rate_limit_exceeded""#,
                llmux_types::ErrorKind::RateLimited,
            ),
            (
                r#""type":"insufficient_quota""#,
                llmux_types::ErrorKind::QuotaExhausted,
            ),
            (
                r#""code":"context_length_exceeded""#,
                llmux_types::ErrorKind::ContextLength,
            ),
            (r#""status":400"#, llmux_types::ErrorKind::BadRequest),
        ] {
            let body = Bytes::from(format!(
                r#"{{"error":{{"padding":"{}",{details}}}}}"#,
                "x".repeat(MAX_ERROR_BYTES)
            ));
            let error = embedded_error(&body, &headers, now).unwrap();
            assert_eq!(error.kind(), expected);
            if let UpstreamError::BadRequest { body: stored, .. } = error {
                assert_eq!(stored.len(), MAX_ERROR_BYTES);
                assert_eq!(stored, body.slice(..MAX_ERROR_BYTES));
            }
        }
    }

    #[test]
    fn detects_only_error_envelopes_without_choices() {
        let headers = HeaderMap::new();
        let now = SystemTime::UNIX_EPOCH;
        for raw in [
            "",
            "not json",
            r#"{"choices":[]}"#,
            r#"{"choices":[],"error":{"code":429}}"#,
            r#"{"error":"not an object"}"#,
            r#"{"message":"error"}"#,
        ] {
            assert!(
                embedded_error(&Bytes::copy_from_slice(raw.as_bytes()), &headers, now).is_none()
            );
        }
        for (raw, kind) in [
            (
                r#"{"error":{"code":"insufficient_quota"}}"#,
                llmux_types::ErrorKind::QuotaExhausted,
            ),
            (
                r#"{"error":{"code":429}}"#,
                llmux_types::ErrorKind::RateLimited,
            ),
            (r#"{"error":{"status":401}}"#, llmux_types::ErrorKind::Auth),
            (
                r#"{"error":{"status_code":"529"}}"#,
                llmux_types::ErrorKind::ServerError,
            ),
            (
                r#"{"error":{"code":"context_length_exceeded"}}"#,
                llmux_types::ErrorKind::ContextLength,
            ),
            (
                r#"{"err\u006fr":{"message":"invalid"}}"#,
                llmux_types::ErrorKind::BadRequest,
            ),
        ] {
            assert_eq!(
                embedded_error(&Bytes::copy_from_slice(raw.as_bytes()), &headers, now)
                    .unwrap()
                    .kind(),
                kind,
                "{raw}"
            );
        }
    }

    #[tokio::test]
    async fn error_collection_is_bounded_and_retains_partial_bytes_on_failure() {
        use http_body_util::{Full, StreamBody};
        use hyper::body::Frame;
        let body = Full::new(Bytes::from(vec![b'x'; MAX_ERROR_BYTES + 1]));
        assert_eq!(collect_error(body).await.len(), MAX_ERROR_BYTES);
        let frames = futures_util::stream::iter([
            Ok(Frame::data(Bytes::from_static(b"partial"))),
            Err(std::io::Error::other("read failed")),
        ]);
        assert_eq!(collect_error(StreamBody::new(frames)).await, "partial");
    }

    #[tokio::test]
    async fn stalled_error_body_preserves_status_headers_and_partial_details() {
        use futures_util::{stream, StreamExt};
        use http_body_util::StreamBody;
        use hyper::body::Frame;
        for partial in [
            Bytes::new(),
            Bytes::from_static(br#"{"error":{"code":"insufficient_quota"}}"#),
        ] {
            let frames = stream::once(std::future::ready(Ok::<_, std::io::Error>(Frame::data(
                partial.clone(),
            ))))
            .chain(stream::pending());
            let start = tokio::time::Instant::now();
            let body = tokio::time::timeout(
                ERROR_BODY_TIMEOUT * 3,
                collect_error(StreamBody::new(frames)),
            )
            .await
            .unwrap();
            assert!(start.elapsed() >= ERROR_BODY_TIMEOUT);
            assert_eq!(body, partial);
            let mut headers = HeaderMap::new();
            headers.insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
            let error = classify_error(
                StatusCode::TOO_MANY_REQUESTS,
                &headers,
                body,
                SystemTime::UNIX_EPOCH,
            );
            if partial.is_empty() {
                assert!(
                    matches!(error, UpstreamError::RateLimited { retry_after: Some(wait), .. } if wait == Duration::from_secs(5))
                );
            } else {
                assert!(matches!(
                    error,
                    UpstreamError::QuotaExhausted { reset_at: Some(_) }
                ));
            }
        }
    }

    #[test]
    fn replaces_only_the_top_level_model_and_preserves_raw_fields() {
        let body = Bytes::from_static(
            br#"{"future": {"model":"untouched"}, "m\u006fdel" : "al\u0069as", "stream":true}"#,
        );
        let meta = OpenAiChatMetadata::parse(&body).unwrap();
        assert_eq!(meta.model, "alias");
        assert!(meta.stream);
        assert_eq!(
            rewrite_model(&body, "upstream/\"model").unwrap(),
            br#"{"future": {"model":"untouched"}, "m\u006fdel" : "upstream/\"model", "stream":true}"#.as_slice(),
        );
        assert_eq!(
            rewrite_model(&body, "alias").unwrap().as_ptr(),
            body.as_ptr()
        );
    }

    #[test]
    fn request_metadata_counts_prompt_text_and_output_tokens() {
        let body = br#"{"model":"m","messages":[{"role":"user","content":[{"type":"text","text":"h\u00e9llo"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}]}],"tools":[{"type":"function"}],"metadata":{"ignored":"text"},"max_tokens":7,"temperature":0.5}"#;
        let meta = RequestMetadata::parse(body, InboundFormat::OpenAiChat).unwrap();
        assert_eq!(meta.model, "m");
        assert!(!meta.stream);
        let counted = [
            "role",
            "user",
            "content",
            "type",
            "text",
            "text",
            "h\u{e9}llo",
        ]
        .iter()
        .chain(&["type", "image_url", "image_url", "url", "type", "function"])
        .map(|text| text.len() as u64)
        .sum::<u64>();
        assert_eq!(meta.prompt_bytes, counted);
        assert_eq!(meta.max_output_tokens, Some(7));
        let chat = InboundFormat::OpenAiChat;
        let responses = InboundFormat::OpenAiResponses;
        for (format, fields, expected) in [
            (chat, r#""max_tokens":7,"max_completion_tokens":9"#, Some(9)),
            (
                chat,
                r#""max_tokens":7,"max_completion_tokens":null"#,
                Some(7),
            ),
            (chat, r#""max_output_tokens":5"#, None),
            (
                responses,
                r#""max_output_tokens":5,"max_tokens":7"#,
                Some(5),
            ),
            (responses, r#""max_completion_tokens":9"#, None),
            (
                InboundFormat::AnthropicMessages,
                r#""max_tokens":7"#,
                Some(7),
            ),
            (chat, r#""max_tokens":-1"#, None),
            (chat, r#""max_tokens":1.5"#, None),
            (chat, r#""max_tokens":"7""#, None),
            (chat, r#""max_tokens":99999999999"#, None),
        ] {
            let body = format!(r#"{{"model":"m",{fields}}}"#);
            assert_eq!(
                RequestMetadata::parse(body.as_bytes(), format)
                    .unwrap()
                    .max_output_tokens,
                expected,
                "{fields}"
            );
        }
        let meta = RequestMetadata::parse(
            br#"{"model":"m","input":"abcd","instructions":"ef","system":null}"#,
            responses,
        )
        .unwrap();
        assert_eq!(meta.prompt_bytes, 6);
        for (text, counted) in [
            ("data: 1,2,3", true),
            ("data:text/plain,hello", true),
            ("data:image/png;base64,AAAA", false),
            ("data:;base64,AAAA", false),
        ] {
            let body = serde_json::json!({"model": "m", "input": text}).to_string();
            let meta = RequestMetadata::parse(body.as_bytes(), responses).unwrap();
            let expected = if counted { text.len() as u64 } else { 0 };
            assert_eq!(meta.prompt_bytes, expected, "{text}");
        }
    }

    #[test]
    fn null_stream_setting_defaults_to_non_streaming() {
        let raw = br#"{"model":"model","stream":null}"#;
        assert!(!OpenAiChatMetadata::parse(raw).unwrap().stream);
    }

    #[test]
    fn rejects_missing_duplicate_or_invalid_model() {
        for invalid in [
            "{}",
            r#"["a", false]"#,
            r#"{"model":"a","model":"b"}"#,
            r#"{"model":9}"#,
            r#"{"model":""}"#,
            r#"{"model":"a","stream":"true"}"#,
            r#"{"model":"a","messages": [}"#,
        ] {
            assert!(
                OpenAiChatMetadata::parse(invalid.as_bytes()).is_err(),
                "{invalid}"
            );
            assert!(
                RequestMetadata::parse(invalid.as_bytes(), InboundFormat::OpenAiChat).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn accepts_retry_after_delta_or_http_date() {
        let mut headers = HeaderMap::new();
        let now = SystemTime::UNIX_EPOCH;
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
        assert_eq!(retry_after(&headers, now), Some(Duration::from_secs(5)));
        headers.insert(
            header::RETRY_AFTER,
            HeaderValue::from_static("Thu, 01 Jan 1970 00:00:09 GMT"),
        );
        assert_eq!(retry_after(&headers, now), Some(Duration::from_secs(9)));
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("invalid"));
        assert_eq!(retry_after(&headers, now), None);
    }
}
