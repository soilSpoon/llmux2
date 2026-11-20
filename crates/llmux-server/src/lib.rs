//! Axum boundary for the OpenAI-compatible gateway.

mod dashboard;

use axum::body::Bytes;
use axum::{
    body::{self, Body},
    extract::{Request, State},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::post,
    Json, Router,
};
use futures_util::StreamExt;
use http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use http_body_util::LengthLimitError;
use llmux_config::{ApiKey, RuntimeConfig};
use llmux_core::{Executor, GatewayError};
use llmux_providers::oauth::{OAuthConfig, OAuthCredentialSource};
use llmux_providers::{truncate_middle, Anthropic, OpenAiCompat, RequestMetadata, StaticKey};
use llmux_types::{
    ContextOverflow, CredentialSource, GatewayRequest, GatewayResponse, InboundFormat,
    ProviderRegistry, RequestId, UpstreamError,
};
use serde_json::{json, value::RawValue};
use std::{convert::Infallible, error::Error, sync::Arc, time::Duration};
use subtle::ConstantTimeEq;

const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;
const DEPLOYMENT_HEADER: HeaderName = HeaderName::from_static("x-llmux-deployment");
const ATTEMPTS_HEADER: HeaderName = HeaderName::from_static("x-llmux-attempts");
const CONTEXT_OVERFLOW_HEADER: HeaderName = HeaderName::from_static("x-llmux-context-overflow");
const DROPPED_MESSAGES_HEADER: HeaderName = HeaderName::from_static("x-llmux-dropped-messages");
const ANTHROPIC: &str = "anthropic";
const ANTHROPIC_KEY_HEADER: HeaderName = HeaderName::from_static("x-api-key");

struct AppState {
    config: Arc<RuntimeConfig>,
    executor: Executor,
    dashboard: Option<dashboard::Dashboard>,
}

pub async fn app(mut config: RuntimeConfig) -> Result<Router, Box<dyn Error + Send + Sync>> {
    let mut registry = ProviderRegistry::default();
    for kind in ["openai", "openai_compat"] {
        registry.register(Arc::new(OpenAiCompat::new(kind)));
    }
    registry.register(Arc::new(Anthropic::new(ANTHROPIC)));
    let mut sources = std::mem::take(&mut config.api_keys)
        .into_iter()
        .map(|(id, ApiKey { provider, key })| {
            let source = if &*provider.0 == ANTHROPIC {
                StaticKey::header(ANTHROPIC_KEY_HEADER, &key)?
            } else {
                StaticKey::bearer(&key)?
            };
            Ok((id, Arc::new(source) as Arc<dyn CredentialSource>))
        })
        .collect::<Result<std::collections::HashMap<_, _>, llmux_types::BuildError>>()?;
    for (id, credential) in std::mem::take(&mut config.oauth) {
        let mut oauth = OAuthConfig::new(
            credential.token_url,
            credential.client_id,
            credential.refresh_token.to_string(),
        );
        oauth.client_secret = credential.client_secret.map(|secret| secret.to_string());
        oauth.scopes = credential.scopes;
        if let Some(resource) = credential.resource {
            oauth.extra_params.push(("resource".into(), resource));
        }
        oauth.refresh_token_path = credential.refresh_token_path;
        oauth.refresh_skew = credential.refresh_skew;
        oauth.request_timeout = credential.request_timeout;
        sources.insert(id, Arc::new(OAuthCredentialSource::start(oauth).await?));
    }

    let config = Arc::new(config);
    let executor = Executor::new(config.clone(), &registry, &sources)?;
    let dashboard = config
        .server
        .dashboard_key
        .clone()
        .map(dashboard::Dashboard::new);
    let state = Arc::new(AppState {
        config,
        executor,
        dashboard,
    });
    let mut router = Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/v1/responses", post(responses));
    if state.dashboard.is_some() {
        router = router.merge(dashboard::routes());
    }
    Ok(router.with_state(state))
}

async fn chat(State(state): State<Arc<AppState>>, request: Request) -> Response {
    execute(state, request, InboundFormat::OpenAiChat).await
}

async fn responses(State(state): State<Arc<AppState>>, request: Request) -> Response {
    execute(state, request, InboundFormat::OpenAiResponses).await
}

async fn execute(state: Arc<AppState>, request: Request, format: InboundFormat) -> Response {
    if !authorized(request.headers(), &state.config.server.client_keys) {
        let mut response = error(
            StatusCode::UNAUTHORIZED,
            "invalid_api_key",
            "Invalid gateway API key",
        );
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        return response;
    }
    let start = state.dashboard.as_ref().map(|_| std::time::Instant::now());
    let response = execute_authorized(&state, request, format).await;
    if let (Some(dashboard), Some(start)) = (&state.dashboard, start) {
        dashboard.record_response(response.status(), start.elapsed());
    }
    response
}

async fn execute_authorized(state: &AppState, request: Request, format: InboundFormat) -> Response {
    let context_overflow = match request.headers().get(CONTEXT_OVERFLOW_HEADER) {
        None => None,
        Some(value) => match value.to_str().ok().and_then(ContextOverflow::from_name) {
            Some(policy) => Some(policy),
            None => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "x-llmux-context-overflow must be `reject` or `middle-out`",
                )
            }
        },
    };
    let body = match body::to_bytes(request.into_body(), MAX_REQUEST_BYTES).await {
        Ok(body) => body,
        Err(err)
            if err
                .source()
                .is_some_and(|cause| cause.is::<LengthLimitError>()) =>
        {
            return error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "invalid_request_error",
                "Request body too large",
            );
        }
        Err(_) => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "Could not read request body",
            )
        }
    };
    let Ok(metadata) = RequestMetadata::parse(&body, format) else {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Expected a JSON object with a nonempty string model and optional boolean stream",
        );
    };

    let model: Arc<str> = Arc::from(metadata.model.as_ref());
    let request_id = RequestId(Arc::from(uuid::Uuid::new_v4().to_string()));
    let request = |body: Bytes, prompt_bytes: u64| GatewayRequest {
        format,
        model: model.clone(),
        stream: metadata.stream,
        est_input_tokens: u32::try_from(prompt_bytes.div_ceil(4)).unwrap_or(u32::MAX),
        max_output_tokens: metadata.max_output_tokens,
        context_overflow,
        body,
        parsed: Default::default(),
        request_id: request_id.clone(),
    };

    let mut dropped = 0;
    let mut result = state
        .executor
        .execute(request(body.clone(), metadata.prompt_bytes))
        .await;
    if let Err(GatewayError::ContextLengthExceeded {
        limit,
        middle_out: true,
        ..
    }) = result
    {
        if let Some(truncated) = truncate_middle(
            &body,
            metadata.prompt_bytes,
            metadata.max_output_tokens,
            limit,
        ) {
            dropped = truncated.dropped;
            result = state
                .executor
                .execute(request(truncated.body, truncated.prompt_bytes))
                .await;
        }
    }
    let routed = match result {
        Ok(routed) => routed,
        Err(err) => return gateway_error(err),
    };
    let mut response = match routed.response {
        GatewayResponse::Complete { status, body, .. } => json_body(status, Body::from(body)),
        GatewayResponse::Stream(events) => {
            let mut sequence_number = 0_u64;
            let stream = events.map(move |event| {
                let event = match event {
                    Ok(event) => {
                        if format == InboundFormat::OpenAiResponses {
                            sequence_number += 1;
                        }
                        let out = Event::default().data(event.data);
                        if let Some(name) = event.event {
                            out.event(name)
                        } else {
                            out
                        }
                    }
                    Err(_) if format == InboundFormat::OpenAiResponses => {
                        Event::default().event("error").data(
                            json!({
                                "type": "error",
                                "message": "Upstream stream ended unexpectedly",
                                "code": "stream_broken",
                                "param": null,
                                "sequence_number": sequence_number,
                            })
                            .to_string(),
                        )
                    }
                    Err(_) => Event::default().data(
                        r#"{"error":{"message":"Upstream stream ended unexpectedly","type":"upstream_error","code":"stream_broken"}}"#,
                    ),
                };
                Ok::<_, Infallible>(event)
            });
            Sse::new(stream)
                .keep_alive(KeepAlive::default())
                .into_response()
        }
    };
    if dropped > 0 {
        response
            .headers_mut()
            .insert(DROPPED_MESSAGES_HEADER, HeaderValue::from(dropped));
    }
    if state.config.server.expose_routing_headers {
        if let Ok(value) = HeaderValue::from_str(&routed.deployment) {
            response.headers_mut().insert(DEPLOYMENT_HEADER, value);
        }
        response
            .headers_mut()
            .insert(ATTEMPTS_HEADER, HeaderValue::from(routed.attempts));
    }
    response
}

fn authorized(headers: &HeaderMap, keys: &[Arc<str>]) -> bool {
    let mut auth = headers.get_all(header::AUTHORIZATION).iter();
    let Some((scheme, value)) = auth
        .next()
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.split_once(' '))
    else {
        return false;
    };
    if auth.next().is_some() || !scheme.eq_ignore_ascii_case("bearer") {
        return false;
    }
    let value = value.trim_start_matches(' ');
    keys.iter()
        .fold(subtle::Choice::from(0), |matched, key| {
            matched | key.as_bytes().ct_eq(value.as_bytes())
        })
        .into()
}

fn gateway_error(err: GatewayError) -> Response {
    match err {
        GatewayError::UnknownModel => {
            error(StatusCode::NOT_FOUND, "model_not_found", "Unknown model")
        }
        GatewayError::UnsupportedStream => error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Model does not support streaming",
        ),
        GatewayError::InvalidRequest => error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Invalid or unsupported request body",
        ),
        GatewayError::ContextLengthExceeded {
            limit,
            prompt_tokens,
            max_output_tokens,
            ..
        } => {
            let output = max_output_tokens.unwrap_or(0);
            let needed = u64::from(prompt_tokens) + u64::from(output);
            error(
                StatusCode::BAD_REQUEST,
                "context_length_exceeded",
                &format!(
                    "This model's maximum context length is {limit} tokens. However, the request needs about {needed} tokens ({prompt_tokens} estimated in the prompt, {output} requested for the output). Reduce the prompt or the maximum output tokens."
                ),
            )
        }
        GatewayError::AllCandidatesExhausted { retry_after, .. } => rate_limited(retry_after),
        GatewayError::Client(UpstreamError::RateLimited { retry_after, .. }) => {
            rate_limited(retry_after.unwrap_or_default())
        }
        GatewayError::Client(UpstreamError::BadRequest { status, body }) => {
            if serde_json::from_slice::<&RawValue>(&body).is_ok() {
                json_body(status, Body::from(body))
            } else {
                error(
                    status,
                    "upstream_client_error",
                    "Upstream rejected the request",
                )
            }
        }
        GatewayError::Client(UpstreamError::ContextLength) => error(
            StatusCode::BAD_REQUEST,
            "context_length_exceeded",
            "Request exceeds the upstream model's maximum context length",
        ),
        GatewayError::Client(UpstreamError::Timeout) => error(
            StatusCode::GATEWAY_TIMEOUT,
            "upstream_timeout",
            "Upstream timed out",
        ),
        GatewayError::Client(_) => error(
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            "Upstream request failed",
        ),
        GatewayError::Internal => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "Gateway could not prepare the request",
        ),
    }
}

fn rate_limited(wait: Duration) -> Response {
    let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    let mut response = error(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limit_exceeded",
        "No upstream currently available",
    );
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
    response
}

fn error(status: StatusCode, code: &str, message: &str) -> Response {
    let kind = match status {
        StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
        StatusCode::UNAUTHORIZED => "authentication_error",
        status if status.is_server_error() => "server_error",
        _ => "invalid_request_error",
    };
    (
        status,
        Json(json!({
            "error": {"message": message, "type": kind, "param": null, "code": code}
        })),
    )
        .into_response()
}

fn json_body(status: StatusCode, body: Body) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        body,
    )
        .into_response()
}
