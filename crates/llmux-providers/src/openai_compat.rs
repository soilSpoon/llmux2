use crate::sse;
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

#[derive(Deserialize)]
struct RawChatMetadata<'a> {
    #[serde(borrow)]
    model: &'a RawValue,
    #[serde(default)]
    stream: bool,
}

pub struct OpenAiChatMetadata<'a> {
    pub model: Cow<'a, str>,
    pub stream: bool,
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
                stream: raw.stream,
            },
            start..start + value.len(),
        ))
    }
}

#[derive(Deserialize)]
struct BorrowedModel<'a>(#[serde(borrow)] Cow<'a, str>);

fn rewrite_model(body: &Bytes, model: &str) -> Result<Bytes, BuildError> {
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
            .body(rewrite_model(&req.body, &dep.upstream_model)?)?;

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
        match status {
            StatusCode::TOO_MANY_REQUESTS => {
                return Err(UpstreamError::RateLimited {
                    retry_after: retry_after(&parts.headers, SystemTime::now()),
                    scope: LimitScope::Unknown,
                });
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                return Err(UpstreamError::Auth { refreshable: false });
            }
            _ if status.is_client_error() => {
                return Err(UpstreamError::BadRequest {
                    status,
                    body: collect(body).await?,
                });
            }
            _ if status.is_server_error() => return Err(UpstreamError::Server { status }),
            _ if !status.is_success() => return Err(bad_gateway()),
            _ => {}
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
            Ok(GatewayResponse::Stream(sse::decode(body)))
        } else {
            Ok(GatewayResponse::Complete {
                status,
                headers: parts.headers,
                body: collect(body).await?,
                usage: None,
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

async fn collect(body: Incoming) -> Result<Bytes, UpstreamError> {
    Limited::new(body, MAX_COMPLETE_BYTES)
        .collect()
        .await
        .map(|buf| buf.to_bytes())
        .map_err(|_| bad_gateway())
}

fn bad_gateway() -> UpstreamError {
    UpstreamError::Server {
        status: StatusCode::BAD_GATEWAY,
    }
}

fn retry_after(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
    let value = headers.get(header::RETRY_AFTER)?.to_str().ok()?.trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Some(Duration::from_secs(value.parse().unwrap_or(u64::MAX)));
    }
    let date = httpdate::parse_http_date(value).ok()?;
    Some(date.duration_since(now).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

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
