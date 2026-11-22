use crate::{messages, responses, sse};
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use http::{
    header::{self, HeaderMap, HeaderName, HeaderValue},
    Method, Request, Response, StatusCode, Uri,
};
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use llmux_types::{
    provider, AuthError, AuthMaterial, BoxStream, BuildError, ChatRequestField, CredentialSource,
    Deployment, GatewayRequest, GatewayResponse, InboundFormat, LimitObservation, ProviderAdapter,
    ProviderKind, SseEvent, Support, UpstreamError,
};
use serde::Deserialize;
use serde_json::value::RawValue;
use std::{
    borrow::Cow,
    collections::BTreeMap,
    ops::Range,
    sync::Arc,
    time::{Duration, SystemTime},
};

const MAX_COMPLETE_BYTES: usize = 16 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 64 * 1024;
const ERROR_BODY_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Which rate-limit family a 429 belongs to, to pick the matching reset header.
enum Scope {
    Requests,
    Tokens,
    Unknown,
}

#[derive(Deserialize)]
struct RawChatMetadata<'a> {
    #[serde(borrow)]
    model: &'a RawValue,
    #[serde(default)]
    stream: Option<bool>,
}

struct OpenAiChatMetadata<'a> {
    model: Cow<'a, str>,
    #[allow(dead_code)] // read by tests; production code only needs the model
    stream: bool,
}

/// Request fields the gateway reads before routing, parsed in one pass.
pub struct RequestMetadata<'a> {
    pub model: Cow<'a, str>,
    pub stream: bool,
    pub temperature: Option<&'a RawValue>,
    /// Bytes of strings and object keys in prompt fields, excluding `data:` URLs.
    pub prompt_bytes: u64,
    /// The output limit the inbound format forwards: Responses `max_output_tokens`,
    /// Chat `max_completion_tokens` else `max_tokens`, Anthropic `max_tokens`.
    pub max_output_tokens: Option<u32>,
    pub has_gateway_routing_fields: bool,
}

#[derive(Deserialize)]
struct RawRequestMetadata<'a, M> {
    #[serde(borrow)]
    model: &'a RawValue,
    #[serde(default)]
    stream: Option<bool>,
    #[serde(default, borrow)]
    temperature: Option<&'a RawValue>,
    #[serde(default, borrow)]
    max_completion_tokens: Option<&'a RawValue>,
    #[serde(default, borrow)]
    max_tokens: Option<&'a RawValue>,
    #[serde(default, borrow)]
    max_output_tokens: Option<&'a RawValue>,
    #[serde(default)]
    messages: M,
    #[serde(default)]
    input: TextBytes,
    #[serde(default)]
    instructions: TextBytes,
    #[serde(default)]
    system: TextBytes,
    #[serde(default)]
    tools: TextBytes,
    #[serde(default, deserialize_with = "field_present")]
    min_discount_percent: bool,
    #[serde(default, deserialize_with = "field_present")]
    zdr: bool,
    #[serde(default, deserialize_with = "field_present")]
    ranking: bool,
}

fn field_present<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    serde::de::IgnoredAny::deserialize(deserializer).map(|_| true)
}

impl<'a> RequestMetadata<'a> {
    pub fn parse(body: &'a [u8], format: InboundFormat) -> Result<Self, BuildError> {
        if body.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
            return Err(BuildError::InvalidBody);
        }
        if format == InboundFormat::AnthropicMessages {
            Self::parse_with::<NativeMessagesText>(body, format)
        } else {
            Self::parse_with::<TextBytes>(body, format)
        }
    }

    fn parse_with<M: Deserialize<'a> + Default + Into<TextBytes>>(
        body: &'a [u8],
        format: InboundFormat,
    ) -> Result<Self, BuildError> {
        let raw: RawRequestMetadata<'_, M> =
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
            temperature: raw.temperature,
            prompt_bytes: [
                raw.messages.into(),
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
            has_gateway_routing_fields: raw.min_discount_percent || raw.zdr || raw.ranking,
        })
    }
}

#[derive(Default)]
struct NativeMessagesText(TextBytes);

impl From<NativeMessagesText> for TextBytes {
    fn from(value: NativeMessagesText) -> Self {
        value.0
    }
}

impl<'de> Deserialize<'de> for NativeMessagesText {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = <&RawValue>::deserialize(deserializer)?;
        let bytes = if let Ok(messages) = serde_json::from_str::<Vec<&RawValue>>(raw.get()) {
            messages.into_iter().try_fold(0, |sum, message| {
                native_message_bytes(message).map(|n| sum + n)
            })
        } else {
            text_bytes(raw)
        };
        bytes
            .map(|bytes| Self(TextBytes(bytes)))
            .map_err(serde::de::Error::custom)
    }
}

type RawFields<'a> = BTreeMap<String, &'a RawValue>;

fn text_bytes(raw: &RawValue) -> serde_json::Result<u64> {
    serde_json::from_str::<TextBytes>(raw.get()).map(|value| value.0)
}

fn field_is(fields: &RawFields<'_>, key: &str, expected: &str) -> bool {
    fields
        .get(key)
        .and_then(|value| serde_json::from_str::<Cow<'_, str>>(value.get()).ok())
        .is_some_and(|value| value == expected)
}

pub(crate) fn native_message_bytes(raw: &RawValue) -> serde_json::Result<u64> {
    let Ok(message) = serde_json::from_str::<RawFields<'_>>(raw.get()) else {
        return text_bytes(raw);
    };
    message.into_iter().try_fold(0, |sum, (key, value)| {
        let bytes = if key == "content" {
            native_content_bytes(value, true)?
        } else {
            text_bytes(value)?
        };
        Ok(sum + key.len() as u64 + bytes)
    })
}

fn native_content_bytes(raw: &RawValue, allow_tool_result: bool) -> serde_json::Result<u64> {
    let Ok(blocks) = serde_json::from_str::<Vec<&RawValue>>(raw.get()) else {
        return text_bytes(raw);
    };
    blocks.into_iter().try_fold(0, |sum, block| {
        native_block_bytes(block, allow_tool_result).map(|n| sum + n)
    })
}

fn native_block_bytes(raw: &RawValue, allow_tool_result: bool) -> serde_json::Result<u64> {
    let Ok(block) = serde_json::from_str::<RawFields<'_>>(raw.get()) else {
        return text_bytes(raw);
    };
    if allow_tool_result && field_is(&block, "type", "tool_result") {
        return block.into_iter().try_fold(0, |sum, (key, value)| {
            let bytes = if key == "content" {
                native_content_bytes(value, false)?
            } else {
                text_bytes(value)?
            };
            Ok(sum + key.len() as u64 + bytes)
        });
    }
    if !(field_is(&block, "type", "image") || field_is(&block, "type", "document")) {
        return text_bytes(raw);
    }
    let Some(source) = block
        .get("source")
        .and_then(|raw| serde_json::from_str::<RawFields<'_>>(raw.get()).ok())
        .filter(|source| field_is(source, "type", "base64"))
    else {
        return text_bytes(raw);
    };
    let source_bytes = source.into_iter().try_fold(0, |sum, (key, value)| {
        Ok::<_, serde_json::Error>(
            sum + key.len() as u64 + if key == "data" { 0 } else { text_bytes(value)? },
        )
    })?;
    block.into_iter().try_fold(0, |sum, (key, value)| {
        Ok(sum
            + key.len() as u64
            + if key == "source" {
                source_bytes
            } else {
                text_bytes(value)?
            })
    })
}

fn is_base64_data_url(text: &str) -> bool {
    text.strip_prefix("data:")
        .and_then(|rest| rest.split_once(',').map(|(media, _)| media))
        .is_some_and(|media| media.len() < 256 && media.ends_with(";base64"))
}

/// Sums string and key lengths of any JSON value without allocating.
#[derive(Default)]
pub(crate) struct TextBytes(pub(crate) u64);

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
    #[cfg(test)]
    fn parse(body: &'a [u8]) -> Result<Self, BuildError> {
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

pub(crate) fn strip_request_fields(
    body: Bytes,
    strip: &[ChatRequestField],
) -> Result<Bytes, BuildError> {
    if strip.is_empty() {
        return Ok(body);
    }
    let mut fields: RawFields<'_> =
        serde_json::from_slice(&body).map_err(|_| BuildError::InvalidBody)?;
    let mut changed = false;
    let mut reasoning = None;
    for field in strip {
        let key = match field {
            ChatRequestField::Seed => "seed",
            ChatRequestField::User => "user",
            ChatRequestField::SafetyIdentifier => "safety_identifier",
            ChatRequestField::Logprobs => "logprobs",
            ChatRequestField::TopLogprobs => "top_logprobs",
            ChatRequestField::Temperature => "temperature",
            ChatRequestField::TopP => "top_p",
            ChatRequestField::TopK => "top_k",
            ChatRequestField::ReasoningEffort => {
                if let Some(mut object) = fields
                    .get("reasoning")
                    .and_then(|value| serde_json::from_str::<RawFields<'_>>(value.get()).ok())
                {
                    if object.remove("effort").is_some() {
                        fields.remove("reasoning");
                        changed = true;
                        if !object.is_empty() {
                            reasoning = Some(
                                serde_json::value::to_raw_value(&object)
                                    .map_err(|_| BuildError::InvalidBody)?,
                            );
                        }
                    }
                }
                continue;
            }
        };
        changed |= fields.remove(key).is_some();
    }
    if let Some(value) = &reasoning {
        fields.insert("reasoning".into(), value.as_ref());
    }
    if changed {
        serde_json::to_vec(&fields)
            .map(Bytes::from)
            .map_err(|_| BuildError::InvalidBody)
    } else {
        Ok(body)
    }
}

pub struct StaticKey(AuthMaterial);

impl StaticKey {
    pub fn bearer(key: &str) -> Result<Self, BuildError> {
        Self::header(header::AUTHORIZATION, &format!("Bearer {key}"))
    }

    /// Sends `value` verbatim in `name`, e.g. Anthropic's `x-api-key`.
    pub fn header(name: HeaderName, value: &str) -> Result<Self, BuildError> {
        let mut value = HeaderValue::from_str(value).map_err(|_| BuildError::InvalidAuth)?;
        value.set_sensitive(true);
        Ok(Self(AuthMaterial::Header { name, value }))
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

    fn forwards_end_user(&self) -> bool {
        matches!(
            self.0 .0.as_ref(),
            provider::OPENAI | provider::OPENAI_COMPAT
        )
    }
}

fn apply_user_field(body: Bytes, user: &str) -> Result<Bytes, BuildError> {
    let mut value: serde_json::Value =
        serde_json::from_slice(&body).map_err(|_| BuildError::InvalidBody)?;
    value
        .as_object_mut()
        .ok_or(BuildError::InvalidBody)?
        .insert("user".into(), serde_json::json!(user));
    serde_json::to_vec(&value)
        .map(Bytes::from)
        .map_err(|_| BuildError::InvalidBody)
}

/// `base` joined with an endpoint suffix like `/chat/completions`.
pub(crate) fn endpoint_uri(base: &Uri, suffix: &str) -> Result<Uri, BuildError> {
    let mut parts = base.clone().into_parts();
    let path = format!("{}{}", base.path().trim_end_matches('/'), suffix);
    parts.path_and_query = Some(path.parse()?);
    Uri::from_parts(parts).map_err(|err| BuildError::InvalidRequest(err.into()))
}

/// Insert the credential's headers: `bearer_name: <prefix><token>` for Bearer,
/// the stored name/value for Header, nothing for None.
pub(crate) fn apply_auth(
    headers: &mut HeaderMap,
    auth: &AuthMaterial,
    bearer_name: HeaderName,
    bearer_prefix: &str,
) -> Result<(), BuildError> {
    match auth {
        AuthMaterial::Bearer(token) => {
            let mut value = HeaderValue::from_str(&format!("{bearer_prefix}{token}"))
                .map_err(|_| BuildError::InvalidAuth)?;
            value.set_sensitive(true);
            headers.insert(bearer_name, value);
        }
        AuthMaterial::Header { name, value } => {
            let mut value = value.clone();
            value.set_sensitive(true);
            headers.insert(name.clone(), value);
        }
        AuthMaterial::None => {}
    }
    Ok(())
}

/// OpenAI-family rate-limit header names.
pub(crate) const LIMIT_REMAINING_REQUESTS: &str = "x-ratelimit-remaining-requests";
pub(crate) const LIMIT_REMAINING_TOKENS: &str = "x-ratelimit-remaining-tokens";
pub(crate) const LIMIT_RESET_REQUESTS: &str = "x-ratelimit-reset-requests";
pub(crate) const LIMIT_RESET_TOKENS: &str = "x-ratelimit-reset-tokens";

#[async_trait]
impl ProviderAdapter for OpenAiCompat {
    fn kind(&self) -> &ProviderKind {
        &self.0
    }

    fn supports(&self, inbound: InboundFormat) -> Support {
        match inbound {
            InboundFormat::OpenAiChat => Support::Passthrough,
            InboundFormat::OpenAiResponses | InboundFormat::AnthropicMessages => Support::Convert,
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
        let body = match req.format {
            InboundFormat::OpenAiResponses => {
                responses::request(&req.body, crate::upstream_model(req, dep))?
            }
            InboundFormat::AnthropicMessages => {
                messages::request(&req.body, crate::upstream_model(req, dep))?
            }
            InboundFormat::OpenAiChat => rewrite_model(&req.body, crate::upstream_model(req, dep))?,
        };
        let body = if self.forwards_end_user() {
            match req.end_user.as_deref() {
                Some(user) => apply_user_field(body, user)?,
                None => body,
            }
        } else {
            body
        };
        let mut request = Request::builder()
            .method(Method::POST)
            .uri(endpoint_uri(&dep.base_url, "/chat/completions")?)
            .header(header::CONTENT_TYPE, "application/json")
            .header(
                header::ACCEPT,
                if req.stream {
                    "text/event-stream"
                } else {
                    "application/json"
                },
            )
            .body(strip_request_fields(body, &dep.strip_request_fields)?)?;

        request.headers_mut().extend(dep.headers.clone());
        llmux_types::default_user_agent(request.headers_mut());
        apply_auth(
            request.headers_mut(),
            auth,
            header::AUTHORIZATION,
            "Bearer ",
        )?;
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
            let is_sse = sse::is_event_stream(&parts.headers);
            if !is_sse {
                return Err(bad_gateway());
            }
            let stream = without_stream_errors(parts.headers.clone(), sse::decode(body));
            if req.format == InboundFormat::AnthropicMessages {
                return Ok(GatewayResponse::Stream(messages::stream(stream)));
            }
            Ok(GatewayResponse::Stream(
                if req.format == InboundFormat::OpenAiResponses {
                    responses::stream(stream)
                } else if let Some(include_usage) = sse::requested_usage(&req.body) {
                    sse::with_chat_usage(stream, include_usage)
                } else {
                    stream
                },
            ))
        } else {
            let body = collect(body).await?;
            if let Some(error) = embedded_error(&body, &parts.headers, SystemTime::now()) {
                return Err(error);
            }
            let body = match req.format {
                InboundFormat::OpenAiResponses => {
                    responses::complete(&body).map_err(|()| bad_gateway())?
                }
                InboundFormat::AnthropicMessages => messages::complete(&body)?,
                InboundFormat::OpenAiChat => body,
            };
            let mut headers = parts.headers;
            if req.format != InboundFormat::OpenAiChat {
                headers.remove(header::CONTENT_LENGTH);
            }
            Ok(GatewayResponse::Complete {
                status,
                headers,
                body,
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
        let remaining_requests = count(LIMIT_REMAINING_REQUESTS);
        let remaining_tokens = count(LIMIT_REMAINING_TOKENS);
        let reset_requests = header_text(headers, LIMIT_RESET_REQUESTS).and_then(parse_duration);
        let reset_tokens = header_text(headers, LIMIT_RESET_TOKENS).and_then(parse_duration);
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

pub(crate) async fn collect_error<B: hyper::body::Body<Data = Bytes> + Unpin>(
    mut body: B,
) -> Bytes {
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
        .map(|value| match value {
            // Google's OpenAI-compatible endpoint wraps errors in a one-element array.
            serde_json::Value::Array(mut items) if !items.is_empty() => items.swap_remove(0),
            value => value,
        })
        .and_then(|mut value| value.get_mut("error").map(serde_json::Value::take))
        .unwrap_or_default()
}

pub(crate) fn is_model_not_found(status: StatusCode, details: &serde_json::Value) -> bool {
    matches!(status, StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND)
        && details["code"].as_str() == Some("model_not_found")
}

pub(crate) fn classify_error(
    status: StatusCode,
    headers: &HeaderMap,
    body: Bytes,
    now: SystemTime,
) -> UpstreamError {
    let details = error_details(&body);
    classify_error_details(status, headers, body, now, &details)
}

fn gemini_quota(details: &serde_json::Value) -> (bool, Option<Duration>) {
    let mut daily = false;
    let mut wait = None;
    for detail in details["details"].as_array().into_iter().flatten() {
        match detail["@type"].as_str() {
            Some("type.googleapis.com/google.rpc.QuotaFailure") => {
                daily |= detail["violations"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|violation| {
                        ["quotaId", "quotaMetric"].iter().any(|field| {
                            violation[field].as_str().is_some_and(|name| {
                                let name: String = name
                                    .chars()
                                    .filter(char::is_ascii_alphanumeric)
                                    .map(|c| c.to_ascii_lowercase())
                                    .collect();
                                name.contains("perday")
                            })
                        })
                    });
            }
            Some("type.googleapis.com/google.rpc.RetryInfo") => {
                wait = wait.max(detail["retryDelay"].as_str().and_then(gemini_retry_delay));
            }
            _ => {}
        }
    }
    (daily, wait)
}

fn gemini_retry_delay(value: &str) -> Option<Duration> {
    let value = value.strip_suffix('s')?;
    let (seconds, nanos) = match value.split_once('.') {
        Some((seconds, fraction)) => {
            if fraction.is_empty()
                || fraction.len() > 9
                || !fraction.bytes().all(|c| c.is_ascii_digit())
            {
                return None;
            }
            (
                seconds,
                fraction.parse::<u32>().ok()? * 10u32.pow(9 - fraction.len() as u32),
            )
        }
        None => (value, 0),
    };
    if seconds.is_empty() || !seconds.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let duration = Duration::new(seconds.parse().ok()?, nanos);
    (!duration.is_zero()).then_some(duration.min(MAX_RETRY))
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
    let scope = if header_text(headers, LIMIT_REMAINING_REQUESTS) == Some("0") {
        Scope::Requests
    } else if header_text(headers, LIMIT_REMAINING_TOKENS) == Some("0")
        || message.contains("tokens per")
        || kind.contains("tokens")
        || code.contains("tokens")
    {
        Scope::Tokens
    } else if message.contains("requests per")
        || kind.contains("requests")
        || code.contains("requests")
    {
        Scope::Requests
    } else {
        Scope::Unknown
    };
    let (daily, gemini_wait) = if status == StatusCode::TOO_MANY_REQUESTS {
        gemini_quota(details)
    } else {
        (false, None)
    };
    let wait = gemini_wait
        .or_else(|| retry_after(headers, now))
        .or_else(|| {
            header_text(headers, "retry-after-ms")?
                .parse::<f64>()
                .ok()
                .filter(|n| n.is_finite() && *n >= 0.0)
                .map(|n| Duration::from_secs_f64((n / 1000.0).min(MAX_RETRY.as_secs_f64())))
        })
        .or_else(|| match scope {
            Scope::Requests => header_text(headers, LIMIT_RESET_REQUESTS).and_then(parse_duration),
            Scope::Tokens => header_text(headers, LIMIT_RESET_TOKENS).and_then(parse_duration),
            Scope::Unknown => None,
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
        429 if quota || daily => UpstreamError::QuotaExhausted {
            reset_at: wait.and_then(|wait| now.checked_add(wait)),
        },
        429 => UpstreamError::RateLimited { retry_after: wait },
        401 => UpstreamError::Auth { refreshable: true },
        403 if body.trim_ascii_start().starts_with(b"<") => UpstreamError::Server { status },
        403 => UpstreamError::Auth { refreshable: false },
        408 => UpstreamError::Timeout,
        400 if message.contains("pass a valid api key")
            || message.contains("api key not valid")
            || details.to_string().contains("API_KEY_INVALID") =>
        {
            UpstreamError::Auth { refreshable: false }
        }
        400 | 413 | 422
            if code == "context_length_exceeded"
                || kind == "context_length_exceeded"
                || message.contains("maximum context length")
                || message.contains("too many tokens")
                || message.contains("context window exceeded") =>
        {
            UpstreamError::ContextLength
        }
        _ if is_model_not_found(status, details) => UpstreamError::ModelNotFound { status, body },
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

fn without_stream_errors(
    headers: HeaderMap,
    mut stream: BoxStream<'static, Result<SseEvent, UpstreamError>>,
) -> BoxStream<'static, Result<SseEvent, UpstreamError>> {
    Box::pin(async_stream::try_stream! {
        let now = SystemTime::now();
        while let Some(event) = stream.next().await {
            let event = event?;
            if let Some(error) = stream_frame_error(&event.data, &headers, now) {
                Err(error)?;
            }
            yield event;
        }
    })
}

/// An SSE data frame with an `error` object and no non-empty `choices`.
///
/// Classification matches [`embedded_error`]: a 429 payload is rate limited
/// (including a `Retry-After` hint from the response headers), a quota marker
/// is quota exhausted, and an explicit client status stays terminal. A frame
/// with no classifiable client status is a server error so the attempt can
/// fail over instead of committing an empty 200.
fn stream_frame_error(data: &str, headers: &HeaderMap, now: SystemTime) -> Option<UpstreamError> {
    if data == sse::DONE {
        return None;
    }
    let bytes = data.as_bytes();
    if memchr::memmem::find(bytes, b"error").is_none() && !bytes.contains(&b'\\') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(data).ok()?;
    if value
        .get("choices")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|choices| !choices.is_empty())
    {
        return None;
    }
    let error = value.get("error")?;
    if !error.is_object() {
        return None;
    }
    let body = Bytes::from(serde_json::to_vec(&serde_json::json!({"error": error})).ok()?);
    let classified = embedded_error(&body, headers, now).unwrap_or_else(bad_gateway);
    Some(match classified {
        UpstreamError::BadRequest { status, .. }
            if status == StatusCode::BAD_REQUEST && explicit_error_status(error).is_none() =>
        {
            bad_gateway()
        }
        other => other,
    })
}

fn explicit_error_status(error: &serde_json::Value) -> Option<StatusCode> {
    ["status", "status_code", "code"].iter().find_map(|key| {
        let value = error.get(*key)?;
        let number = value
            .as_u64()
            .and_then(|n| u16::try_from(n).ok())
            .or_else(|| value.as_str()?.parse().ok())?;
        StatusCode::from_u16(number)
            .ok()
            .filter(|status| status.is_client_error() || status.is_server_error())
    })
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

    #[tokio::test]
    async fn converts_anthropic_messages_json_and_stream_for_openai_chat() {
        use futures_util::StreamExt;
        use http_body_util::Full;
        use hyper_util::{client::legacy::Client, rt::TokioExecutor};
        use llmux_testkit::TestServer;
        use llmux_types::{DeploymentIdx, QuotaSpec, RequestId};
        use serde_json::{json, Value};

        let server = TestServer::start(axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(|axum::Json(body): axum::Json<Value>| async move {
                use axum::response::IntoResponse;
                assert_eq!(body["model"], "chat-model");
                assert_eq!(body["max_tokens"], 64);
                assert_eq!(body["stop"], json!(["end"]));
                assert_eq!(body["messages"][0], json!({"role":"system","content":"Be brief"}));
                assert_eq!(body["messages"][1], json!({"role":"user","content":"Hello"}));
                assert_eq!(body["tools"][0]["function"]["name"], "lookup");
                assert_eq!(body["tool_choice"]["function"]["name"], "lookup");
                if body["stream"] == true {
                    assert_eq!(body["stream_options"]["include_usage"], true);
                    (
                        [(header::CONTENT_TYPE,"text/event-stream")],
                        concat!(
                            "data: {\"id\":\"chat-1\",\"model\":\"chat-model\",\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
                            "data: {\"id\":\"chat-1\",\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n",
                            "data: {\"id\":\"chat-1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{\"}}]}}]}\n\n",
                            "data: {\"id\":\"chat-1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
                            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":4}}\n\n",
                            "data: [DONE]\n\n",
                        ),
                    ).into_response()
                } else {
                    axum::Json(json!({
                        "id":"chat-1","model":"chat-model",
                        "choices":[{"message":{"content":"Hi","tool_calls":[{"id":"call_1","function":{"name":"lookup","arguments":"{}"}}]},"finish_reason":"tool_calls"}],
                        "usage":{"prompt_tokens":3,"completion_tokens":4}
                    })).into_response()
                }
            }),
        )).await.unwrap();
        let adapter = OpenAiCompat::new("openai_compat");
        assert_eq!(
            adapter.supports(InboundFormat::AnthropicMessages),
            Support::Convert
        );
        let deployment = Deployment {
            headers: Default::default(),
            idx: DeploymentIdx(0),
            id: "d".into(),
            provider: adapter.kind().clone(),
            credential: "key".into(),
            base_url: format!("{}/v1", server.url()).parse().unwrap(),
            upstream_model: "chat-model".into(),
            strip_request_fields: Box::default(),
            priority: 0,
            quota: QuotaSpec::default(),
            timeout: Duration::from_secs(5),
            max_context: None,
            expires_at: None,
            fallback: false,
        };
        let client = Client::builder(TokioExecutor::new()).build_http();
        for stream in [false, true] {
            let req = GatewayRequest {
                format: InboundFormat::AnthropicMessages,
                model: "alias".into(),
                stream,
                body: Bytes::from(
                    json!({
                        "model":"alias","stream":stream,"max_tokens":64,"system":"Be brief",
                        "messages":[{"role":"user","content":"Hello"}],"stop_sequences":["end"],
                        "tools":[{"name":"lookup","input_schema":{"type":"object"}}],
                        "tool_choice":{"type":"tool","name":"lookup"}
                    })
                    .to_string(),
                ),
                est_input_tokens: 0,
                anthropic_headers: HeaderMap::new(),
                max_output_tokens: Some(64),
                context_overflow: None,
                request_id: RequestId("r".into()),
                end_user: None,
            };
            let upstream = adapter
                .build_request(&req, &deployment, &AuthMaterial::None)
                .await
                .unwrap();
            let upstream = client.request(upstream.map(Full::new)).await.unwrap();
            match adapter
                .parse_response(&req, &deployment, upstream)
                .await
                .unwrap()
            {
                GatewayResponse::Complete { body, .. } if !stream => {
                    let message: Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(
                        message["content"],
                        json!([
                            {"type":"text","text":"Hi"},
                            {"type":"tool_use","id":"call_1","name":"lookup","input":{}}
                        ])
                    );
                    assert_eq!(message["stop_reason"], "tool_use");
                    assert_eq!(
                        message["usage"],
                        json!({"input_tokens":3,"output_tokens":4})
                    );
                }
                GatewayResponse::Stream(events) if stream => {
                    let mut output = Vec::new();
                    let mut events = events;
                    while let Some(event) = events.next().await {
                        let event = event.unwrap();
                        let value: Value = serde_json::from_str(&event.data).unwrap();
                        assert_eq!(event.event.as_deref(), value["type"].as_str());
                        output.push(value);
                    }
                    let types: Vec<_> = output
                        .iter()
                        .map(|event| event["type"].as_str().unwrap())
                        .collect();
                    assert_eq!(
                        types,
                        [
                            "message_start",
                            "content_block_start",
                            "content_block_delta",
                            "content_block_stop",
                            "content_block_start",
                            "content_block_delta",
                            "content_block_delta",
                            "content_block_stop",
                            "message_delta",
                            "message_stop",
                        ]
                    );
                    assert_eq!(output[2]["delta"]["text"], "Hi");
                    assert_eq!(output[4]["content_block"]["name"], "lookup");
                    assert_eq!(output[5]["delta"]["partial_json"], "{");
                    assert_eq!(output[6]["delta"]["partial_json"], "}");
                    assert_eq!(output[8]["delta"]["stop_reason"], "tool_use");
                    assert_eq!(output[8]["usage"]["output_tokens"], 4);
                }
                _ => panic!("incorrect response format"),
            }
        }
    }

    #[test]
    fn limit_header_names_match_the_upstream_contract() {
        assert_eq!(LIMIT_REMAINING_REQUESTS, "x-ratelimit-remaining-requests");
        assert_eq!(LIMIT_REMAINING_TOKENS, "x-ratelimit-remaining-tokens");
        assert_eq!(LIMIT_RESET_REQUESTS, "x-ratelimit-reset-requests");
        assert_eq!(LIMIT_RESET_TOKENS, "x-ratelimit-reset-tokens");
    }

    fn classify(status: u16, body: &str) -> UpstreamError {
        classify_error(
            StatusCode::from_u16(status).unwrap(),
            &HeaderMap::new(),
            Bytes::copy_from_slice(body.as_bytes()),
            SystemTime::UNIX_EPOCH,
        )
    }

    #[test]
    fn gemini_quota_details_distinguish_daily_limits_and_prefer_retry_info() {
        let now = SystemTime::UNIX_EPOCH;
        for (quota_id, delay, daily, wait) in [
            (
                "GenerateRequestsPerMinutePerProjectPerModel-FreeTier",
                "3.125s",
                false,
                3125,
            ),
            (
                "GenerateRequestsPerDayPerProjectPerModel-FreeTier",
                "3600s",
                true,
                3_600_000,
            ),
            (
                "GenerateRequestsPerMinutePerProjectPerModel-FreeTier",
                "bad",
                false,
                8000,
            ),
            (
                "GenerateRequestsPerMinutePerProjectPerModel-FreeTier",
                "0s",
                false,
                8000,
            ),
        ] {
            let payload = serde_json::json!([{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","details":[
                {"@type":"type.googleapis.com/google.rpc.QuotaFailure","violations":[{"quotaId":quota_id}]},
                {"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":delay}
            ]}}]);
            let mut headers = HeaderMap::new();
            headers.insert(header::RETRY_AFTER, HeaderValue::from_static("8"));
            let error = classify_error(
                StatusCode::TOO_MANY_REQUESTS,
                &headers,
                Bytes::from(payload.to_string()),
                now,
            );
            let wait = Duration::from_millis(wait);
            match error {
                UpstreamError::QuotaExhausted { reset_at } if daily => {
                    assert_eq!(reset_at, Some(now + wait))
                }
                UpstreamError::RateLimited { retry_after } if !daily => {
                    assert_eq!(retry_after, Some(wait))
                }
                other => panic!("unexpected error for {quota_id}: {other:?}"),
            }
        }
    }

    #[test]
    fn gemini_invalid_api_key_400_is_a_rejected_credential() {
        for body in [
            r#"[{"error":{"code":400,"message":"API key not valid. Please pass a valid API key.","status":"INVALID_ARGUMENT","details":[{"reason":"API_KEY_INVALID"}]}}]"#,
            r#"{"error":{"code":400,"message":"Please pass a valid API key","status":"INVALID_ARGUMENT"}}"#,
        ] {
            assert!(matches!(
                classify(400, body),
                UpstreamError::Auth { refreshable: false }
            ));
        }
        assert!(matches!(
            classify(
                400,
                r#"{"error":{"code":400,"message":"Invalid value for temperature","status":"INVALID_ARGUMENT"}}"#
            ),
            UpstreamError::BadRequest { .. }
        ));
    }

    #[test]
    fn waf_html_403_is_transient_not_an_auth_failure() {
        for body in [
            "<!DOCTYPE html>\n<html><title>Attention Required! | Cloudflare</title></html>",
            "  <html>blocked</html>",
        ] {
            assert!(matches!(
                classify(403, body),
                UpstreamError::Server { status } if status == StatusCode::FORBIDDEN
            ));
        }
        for body in [r#"{"error":{"message":"forbidden"}}"#, "Forbidden", ""] {
            assert!(matches!(
                classify(403, body),
                UpstreamError::Auth { refreshable: false }
            ));
        }
        assert!(matches!(
            classify(401, "<html>login</html>"),
            UpstreamError::Auth { refreshable: true }
        ));
    }

    #[test]
    fn request_carries_a_default_user_agent_unless_the_deployment_sets_one() {
        use futures_util::FutureExt;
        use llmux_types::{DeploymentIdx, QuotaSpec, RequestId};
        let adapter = OpenAiCompat::new("openai_compat");
        let mut deployment = Deployment {
            headers: Default::default(),
            idx: DeploymentIdx(0),
            id: "d".into(),
            provider: adapter.kind().clone(),
            credential: "key".into(),
            base_url: "https://example.com/v1".parse().unwrap(),
            upstream_model: "m".into(),
            strip_request_fields: Box::default(),
            priority: 0,
            quota: QuotaSpec::default(),
            timeout: Duration::from_secs(5),
            max_context: None,
            expires_at: None,
            fallback: false,
        };
        let req = GatewayRequest {
            format: InboundFormat::OpenAiChat,
            model: "m".into(),
            stream: false,
            body: Bytes::from_static(br#"{"model":"m","messages":[]}"#),
            est_input_tokens: 0,
            anthropic_headers: HeaderMap::new(),
            max_output_tokens: None,
            context_overflow: None,
            request_id: RequestId("r".into()),
            end_user: None,
        };
        let build = |dep: &Deployment| {
            adapter
                .build_request(&req, dep, &AuthMaterial::None)
                .now_or_never()
                .unwrap()
                .unwrap()
        };
        assert_eq!(
            build(&deployment).headers()[header::USER_AGENT],
            concat!("llmux/", env!("CARGO_PKG_VERSION"))
        );
        deployment
            .headers
            .insert(header::USER_AGENT, HeaderValue::from_static("custom/1"));
        assert_eq!(build(&deployment).headers()[header::USER_AGENT], "custom/1");
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
                UpstreamError::RateLimited { retry_after: None }
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
        let check = |headers: &HeaderMap, wait| {
            assert!(
                matches!(classify_error(StatusCode::TOO_MANY_REQUESTS, headers, body.clone(), now), UpstreamError::RateLimited { retry_after: w, .. } if w == wait)
            );
        };
        check(&headers, None);
        headers.insert(LIMIT_RESET_TOKENS, HeaderValue::from_static("20ms"));
        check(&headers, Some(Duration::from_millis(20)));
        headers.insert(LIMIT_REMAINING_REQUESTS, HeaderValue::from_static("0"));
        headers.insert(LIMIT_RESET_REQUESTS, HeaderValue::from_static("6m0s"));
        check(&headers, Some(Duration::from_secs(360)));
        headers.insert("retry-after-ms", HeaderValue::from_static("1250"));
        check(&headers, Some(Duration::from_millis(1250)));
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("2"));
        check(&headers, Some(Duration::from_secs(2)));
        assert!(
            matches!(classify_error(StatusCode::TOO_MANY_REQUESTS, &headers, Bytes::from_static(br#"{"error":{"code":"insufficient_quota"}}"#), now), UpstreamError::QuotaExhausted { reset_at: Some(reset) } if reset == now + Duration::from_secs(2))
        );
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("bad"));
        headers.insert("retry-after-ms", HeaderValue::from_static("NaN"));
        check(&headers, Some(Duration::from_secs(360)));
        headers.insert(
            header::RETRY_AFTER,
            HeaderValue::from_static("999999999999999999999999"),
        );
        check(&headers, Some(MAX_RETRY));
    }

    #[test]
    fn observations_include_resets_without_counts_and_ignore_bad_headers() {
        let adapter = OpenAiCompat::new("test");
        let mut headers = HeaderMap::new();
        assert_eq!(adapter.observe_limits(&headers), None);
        headers.insert(
            LIMIT_REMAINING_REQUESTS,
            HeaderValue::from_static("invalid"),
        );
        headers.insert(LIMIT_RESET_TOKENS, HeaderValue::from_static("invalid"));
        assert_eq!(adapter.observe_limits(&headers), None);
        headers.insert(LIMIT_RESET_REQUESTS, HeaderValue::from_static("1h2m3.5s"));
        assert_eq!(
            adapter.observe_limits(&headers).unwrap().reset_requests,
            Some(Duration::from_millis(3723500))
        );
        headers.insert(LIMIT_REMAINING_REQUESTS, HeaderValue::from_static("0"));
        headers.insert(LIMIT_REMAINING_TOKENS, HeaderValue::from_static("42"));
        headers.insert(LIMIT_RESET_TOKENS, HeaderValue::from_static("20ms"));
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
    fn stream_error_frames_use_embedded_error_classification() {
        let now = SystemTime::UNIX_EPOCH;
        let mut headers = HeaderMap::new();
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("3"));
        let rate_limited = stream_frame_error(
            r#"{"id":"chatcmpl-keepalive","choices":[],"error":{"message":"rate limit - [429]: RESOURCE_EXHAUSTED","type":"rate_limit_error","code":429}}"#,
            &headers,
            now,
        )
        .unwrap();
        assert!(
            matches!(rate_limited, UpstreamError::RateLimited { retry_after: Some(wait) } if wait == Duration::from_secs(3))
        );
        assert!(matches!(
            stream_frame_error(
                r#"{"error":{"message":"quota exhausted","type":"insufficient_quota"}}"#,
                &HeaderMap::new(),
                now,
            )
            .unwrap(),
            UpstreamError::QuotaExhausted { .. }
        ));
        assert!(matches!(
            stream_frame_error(
                r#"{"error":{"message":"upstream exploded","type":"server_error"}}"#,
                &HeaderMap::new(),
                now,
            )
            .unwrap(),
            UpstreamError::Server {
                status: StatusCode::BAD_GATEWAY
            }
        ));
        assert!(matches!(
            stream_frame_error(
                r#"{"error":{"status":400,"message":"bad input","type":"invalid_request_error"}}"#,
                &HeaderMap::new(),
                now,
            )
            .unwrap(),
            UpstreamError::BadRequest {
                status: StatusCode::BAD_REQUEST,
                ..
            }
        ));
        for raw in [
            r#"{"id":"chatcmpl-keepalive","object":"chat.completion.chunk","choices":[]}"#,
            r#"{"choices":[{"delta":{"content":"Hi"}}],"error":{"code":429}}"#,
            "[DONE]",
            r#"{"id":"chunk-1"}"#,
        ] {
            assert!(
                stream_frame_error(raw, &HeaderMap::new(), now).is_none(),
                "{raw}"
            );
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

    #[test]
    fn end_user_hash_overrides_user_unless_stripped_or_unsupported() {
        use futures_util::FutureExt;
        use llmux_types::{DeploymentIdx, QuotaSpec, RequestId};
        let hash = "31ca399bfc61dac0b442a6bef7c00cd06cf9682c11b9192fb26a918b4a9130f8";
        let raw = "raw-client-SHOULD-NOT-LEAK";
        let body = Bytes::from(format!(
            r#"{{"model":"m","user":"{raw}","messages":[],"seed":1}}"#
        ));
        let req = GatewayRequest {
            format: InboundFormat::OpenAiChat,
            model: "m".into(),
            stream: false,
            body,
            est_input_tokens: 0,
            anthropic_headers: HeaderMap::new(),
            max_output_tokens: None,
            context_overflow: None,
            request_id: RequestId("r".into()),
            end_user: Some(hash.into()),
        };
        let deployment = |kind: &str, strip: &[ChatRequestField]| Deployment {
            headers: Default::default(),
            idx: DeploymentIdx(0),
            id: "d".into(),
            provider: ProviderKind(kind.into()),
            credential: "key".into(),
            base_url: "https://example.com/v1".parse().unwrap(),
            upstream_model: "upstream".into(),
            strip_request_fields: strip.to_vec().into_boxed_slice(),
            priority: 0,
            quota: QuotaSpec::default(),
            timeout: Duration::from_secs(5),
            max_context: None,
            expires_at: None,
            fallback: false,
        };
        let send = |adapter: &OpenAiCompat, dep: &Deployment| {
            let request = adapter
                .build_request(&req, dep, &AuthMaterial::None)
                .now_or_never()
                .unwrap()
                .unwrap();
            serde_json::from_slice::<serde_json::Value>(request.body()).unwrap()
        };
        let compat = OpenAiCompat::new(provider::OPENAI_COMPAT);
        let sent = send(&compat, &deployment(provider::OPENAI_COMPAT, &[]));
        assert_eq!(sent["user"], hash);
        assert!(!sent.to_string().contains(raw));
        assert_eq!(sent["model"], "upstream");
        let stripped = send(
            &compat,
            &deployment(provider::OPENAI_COMPAT, &[ChatRequestField::User]),
        );
        assert!(stripped.get("user").is_none());
        assert!(!stripped.to_string().contains(raw));
        assert!(!stripped.to_string().contains(hash));
        let xai = OpenAiCompat::new(provider::XAI);
        let untouched = send(&xai, &deployment(provider::XAI, &[]));
        assert_eq!(untouched["user"], raw);
    }
}
