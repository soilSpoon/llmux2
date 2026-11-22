//! Axum boundary for the OpenAI-compatible gateway.

mod admin;
mod catalog;
mod dashboard;
mod lifecycle;
#[cfg(unix)]
mod notify;
mod policy;
pub use lifecycle::{bind, serve};

use axum::body::Bytes;
use axum::{
    body::{self, Body},
    extract::{Request, State},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use futures_util::StreamExt;
use http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use http_body_util::LengthLimitError;
use llmux_config::{ApiKey, RuntimeConfig};
use llmux_core::{Executor, GatewayError, LiveModels, RoutedError};
use llmux_providers::nous_portal::{NousPortalConfig, NousPortalSource};
use llmux_providers::oauth::{OAuthConfig, OAuthCredentialSource};
use llmux_providers::{
    truncate_middle_for_format, Anthropic, Antigravity, DeepAi, DevinAcp, DevinAcpConfig,
    OpenAiCompat, OpenAiResponses, RequestMetadata, StaticKey, Xai,
};
use llmux_types::{
    provider, ContextOverflow, CredentialSource, GatewayRequest, GatewayResponse, InboundFormat,
    ProviderRegistry, RequestId, UpstreamError,
};
use serde_json::{json, value::RawValue, Value};
use std::{collections::BTreeMap, convert::Infallible, error::Error, sync::Arc, time::Duration};
use subtle::ConstantTimeEq;

const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;
const DEPLOYMENT_HEADER: HeaderName = HeaderName::from_static("x-llmux-deployment");
const ATTEMPTS_HEADER: HeaderName = HeaderName::from_static("x-llmux-attempts");
const CONTEXT_OVERFLOW_HEADER: HeaderName = HeaderName::from_static("x-llmux-context-overflow");
const DROPPED_MESSAGES_HEADER: HeaderName = HeaderName::from_static("x-llmux-dropped-messages");
const ANTHROPIC_KEY_HEADER: HeaderName = HeaderName::from_static(provider::ANTHROPIC_KEY_HEADER);
const DEEPAI_KEY_HEADER: HeaderName = HeaderName::from_static(provider::DEEPAI_KEY_HEADER);

struct Active {
    config: Arc<RuntimeConfig>,
    executor: Arc<Executor>,
}

struct AppState {
    active: arc_swap::ArcSwap<Active>,
    dashboard: Option<dashboard::Dashboard>,
    config_path: Option<std::path::PathBuf>,
    reload_lock: tokio::sync::Mutex<()>,
    catalog: Arc<catalog::Catalog>,
}

fn provider_registry(devin: DevinAcpConfig) -> ProviderRegistry {
    let mut registry = ProviderRegistry::default();
    for kind in [provider::OPENAI, provider::OPENAI_COMPAT] {
        registry.register(Arc::new(OpenAiCompat::new(kind)));
    }
    registry.register(Arc::new(Anthropic::new(provider::ANTHROPIC)));
    registry.register(Arc::new(OpenAiResponses::new()));
    registry.register(Arc::new(DeepAi::new()));
    registry.register(Arc::new(Xai::new()));
    registry.register(Arc::new(Antigravity::new()));
    registry.register(Arc::new(DevinAcp::new(devin)));
    registry
}

fn static_sources(
    config: &mut RuntimeConfig,
) -> Result<std::collections::HashMap<Arc<str>, Arc<dyn CredentialSource>>, llmux_types::BuildError>
{
    std::mem::take(&mut config.api_keys)
        .into_iter()
        .map(|(id, ApiKey { provider, key })| {
            let source = match provider.0.as_ref() {
                provider::ANTHROPIC => StaticKey::header(ANTHROPIC_KEY_HEADER, &key)?,
                provider::DEEPAI => StaticKey::header(DEEPAI_KEY_HEADER, &key)?,
                _ => StaticKey::bearer(&key)?,
            };
            Ok((id, Arc::new(source) as Arc<dyn CredentialSource>))
        })
        .collect()
}

pub fn validate_startup(mut config: RuntimeConfig) -> Result<(), Box<dyn Error + Send + Sync>> {
    let registry = provider_registry(DevinAcpConfig::from_env());
    let sources = static_sources(&mut config)?;
    let credentials = sources
        .keys()
        .chain(config.oauth.keys())
        .chain(config.nous.keys())
        .cloned()
        .collect();
    Executor::validate(&config, &registry, &credentials)?;
    Ok(())
}

pub async fn app(config: RuntimeConfig) -> Result<Router, Box<dyn Error + Send + Sync>> {
    app_with_path(config, None).await
}

pub async fn app_with_path(
    config: RuntimeConfig,
    config_path: Option<std::path::PathBuf>,
) -> Result<Router, Box<dyn Error + Send + Sync>> {
    app_with(config, config_path, DevinAcpConfig::from_env()).await
}

/// Same as [`app`], with an explicit Devin CLI config. Tests use this so they
/// never read process environment or a real `devin` binary.
pub async fn app_with_devin(
    config: RuntimeConfig,
    devin: DevinAcpConfig,
) -> Result<Router, Box<dyn Error + Send + Sync>> {
    app_with(config, None, devin).await
}

async fn app_with(
    config: RuntimeConfig,
    config_path: Option<std::path::PathBuf>,
    devin: DevinAcpConfig,
) -> Result<Router, Box<dyn Error + Send + Sync>> {
    let catalog = Arc::new(catalog::Catalog::new(devin.clone()));
    let (config, executor) = build(config, devin, catalog.live()).await?;
    let dashboard = config
        .server
        .dashboard_key
        .is_some()
        .then(dashboard::Dashboard::new);
    let state = Arc::new(AppState {
        active: arc_swap::ArcSwap::from_pointee(Active {
            config,
            executor: Arc::new(executor),
        }),
        dashboard,
        config_path,
        reload_lock: tokio::sync::Mutex::new(()),
        catalog,
    });
    let mut router = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .route("/v1/messages", post(messages))
        .route("/v1/responses", post(responses));
    if state.dashboard.is_some() {
        router = router.merge(dashboard::routes()).merge(admin::routes());
    }
    Ok(router.with_state(state))
}

async fn build(
    mut config: RuntimeConfig,
    devin: DevinAcpConfig,
    live: Arc<LiveModels>,
) -> Result<(Arc<RuntimeConfig>, Executor), Box<dyn Error + Send + Sync>> {
    let registry = provider_registry(devin);
    let mut sources = static_sources(&mut config)?;
    // Validate before starting OAuth sources: a rejected build must not
    // rotate refresh tokens the running gateway still relies on.
    Executor::validate(
        &config,
        &registry,
        &sources
            .keys()
            .chain(config.oauth.keys())
            .chain(config.nous.keys())
            .cloned()
            .collect(),
    )?;
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
    for (id, credential) in std::mem::take(&mut config.nous) {
        let mut nous = NousPortalConfig::new(credential.state_path);
        nous.refresh_skew = credential.refresh_skew;
        nous.request_timeout = credential.request_timeout;
        sources.insert(id, Arc::new(NousPortalSource::start(nous).await?));
    }

    let config = Arc::new(config);
    let mut executor = Executor::new(config.clone(), &registry, &sources)?;
    executor.set_devin_catalog(live);
    Ok((config, executor))
}

async fn models(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let active = state.active.load();
    if !authorized(&headers, &active.config.server.client_keys) {
        return unauthorized();
    }
    let live = if catalog::has_devin(&active.config) {
        let kick = state.catalog.kick();
        if kick.inflight && state.catalog.loaded().is_none() {
            state.catalog.wait_for(kick.target).await;
        }
        state.catalog.loaded()
    } else {
        None
    };
    let ids = catalog::listed_ids(&active.config, live.as_deref());
    let data: Vec<_> = ids
        .into_iter()
        .map(|id| json!({"id": id, "object": "model", "created": 0, "owned_by": "llmux2"}))
        .collect();
    Json(json!({"object": "list", "data": data})).into_response()
}

async fn chat(State(state): State<Arc<AppState>>, request: Request) -> Response {
    execute(state, request, InboundFormat::OpenAiChat).await
}

async fn responses(State(state): State<Arc<AppState>>, request: Request) -> Response {
    execute(state, request, InboundFormat::OpenAiResponses).await
}

async fn messages(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let response = execute(state, request, InboundFormat::AnthropicMessages).await;
    if response.status().is_success() {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let mut payload: Value = body::to_bytes(body, MAX_REQUEST_BYTES)
        .await
        .ok()
        .and_then(|body| serde_json::from_slice(&body).ok())
        .unwrap_or_default();
    if !payload["error"].is_object() {
        payload = json!({"error":{"message":"Gateway request failed"}});
    }
    payload["type"] = json!("error");
    payload["error"]["type"] = json!(match parts.status {
        StatusCode::UNAUTHORIZED => "authentication_error",
        StatusCode::FORBIDDEN => "permission_error",
        StatusCode::NOT_FOUND => "not_found_error",
        StatusCode::PAYLOAD_TOO_LARGE => "request_too_large",
        StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
        status if status.is_server_error() => "api_error",
        _ => "invalid_request_error",
    });
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Response::from_parts(parts, Body::from(payload.to_string()))
}

async fn execute(state: Arc<AppState>, request: Request, format: InboundFormat) -> Response {
    let active = state.active.load();
    if !authorized_for_format(request.headers(), &active.config.server.client_keys, format) {
        return unauthorized();
    }
    if catalog::has_devin(&active.config) {
        state.catalog.kick();
    }
    let start = state.dashboard.as_ref().map(|_| std::time::Instant::now());
    let response = execute_authorized(&active, request, format).await;
    if let (Some(dashboard), Some(start)) = (&state.dashboard, start) {
        dashboard.record_response(response.status(), start.elapsed());
    }
    response
}

fn unauthorized() -> Response {
    let mut response = error(
        StatusCode::UNAUTHORIZED,
        "invalid_api_key",
        "Invalid gateway API key",
    );
    response
        .headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    response
}

async fn execute_authorized(active: &Active, request: Request, format: InboundFormat) -> Response {
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
    let (parts, body) = request.into_parts();
    let body = match body::to_bytes(body, MAX_REQUEST_BYTES).await {
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
    let end_user = match policy::resolve_end_user(&active.config.server, &parts.headers, &body) {
        Ok(id) => id,
        Err(err) => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                err.message(),
            );
        }
    };
    let response = route_request(
        active,
        &parts.headers,
        body,
        format,
        context_overflow,
        end_user.clone(),
    )
    .await;
    policy::attach_end_user(response, end_user.as_deref())
}

async fn route_request(
    active: &Active,
    headers: &HeaderMap,
    body: Bytes,
    format: InboundFormat,
    context_overflow: Option<ContextOverflow>,
    end_user: Option<Arc<str>>,
) -> Response {
    let Ok(metadata) = RequestMetadata::parse(&body, format) else {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Expected a JSON object with a nonempty string model and optional boolean stream",
        );
    };
    if let Err(err) = policy::check_images(&body, &active.config.server) {
        return error(err.status(), err.code(), err.message());
    }
    if format == InboundFormat::OpenAiChat {
        if let Some(temperature) = metadata.temperature {
            match serde_json::from_str::<f64>(temperature.get()) {
                Ok(value) if (0.0..=2.0).contains(&value) => {}
                Ok(_) => return chat_bad_request("temperature: must be between 0 and 2"),
                Err(_) => return chat_bad_request("temperature: must be a number"),
            }
        }
    }

    let model: Arc<str> = Arc::from(metadata.model.as_ref());
    if active
        .config
        .model_names
        .get(model.as_ref())
        .and_then(|idx| active.config.model_allowed_keys.get(idx))
        .is_some_and(|keys| !authorized_for_format(headers, keys, format))
    {
        return error(
            StatusCode::FORBIDDEN,
            "model_not_allowed",
            "Gateway key is not allowed to use this model",
        );
    }
    let request_id = RequestId(Arc::from(uuid::Uuid::new_v4().to_string()));
    let upstream_body = if metadata.has_gateway_routing_fields {
        let Some(clean) = without_gateway_routing_fields(&body) else {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "Invalid JSON body",
            );
        };
        clean
    } else {
        body.clone()
    };
    let mut anthropic_headers = HeaderMap::new();
    if format == InboundFormat::AnthropicMessages {
        for name in ["anthropic-version", "anthropic-beta"] {
            for value in headers.get_all(name) {
                anthropic_headers.append(HeaderName::from_static(name), value.clone());
            }
        }
    }
    let request = |body: Bytes, prompt_bytes: u64| GatewayRequest {
        format,
        model: model.clone(),
        stream: metadata.stream,
        est_input_tokens: u32::try_from(prompt_bytes.div_ceil(4)).unwrap_or(u32::MAX),
        max_output_tokens: metadata.max_output_tokens,
        context_overflow,
        body,
        anthropic_headers: anthropic_headers.clone(),
        request_id: request_id.clone(),
        end_user: end_user.clone(),
    };

    let mut dropped = 0;
    let mut result = active
        .executor
        .execute_attributed(request(upstream_body.clone(), metadata.prompt_bytes))
        .await;
    if let Err(RoutedError {
        error:
            GatewayError::ContextLengthExceeded {
                limit,
                middle_out: true,
                ..
            },
        ..
    }) = result
    {
        if let Some(truncated) = truncate_middle_for_format(
            &upstream_body,
            metadata.prompt_bytes,
            metadata.max_output_tokens,
            limit,
            format,
        ) {
            dropped = truncated.dropped;
            result = active
                .executor
                .execute_attributed(request(truncated.body, truncated.prompt_bytes))
                .await;
        }
    }
    let routed = match result {
        Ok(routed) => routed,
        Err(RoutedError {
            error: GatewayError::UnknownModel,
            ..
        }) if format == InboundFormat::OpenAiChat => {
            return chat_bad_request("Invalid model format");
        }
        Err(failure) => {
            let mut response = gateway_error(failure.error);
            if active.config.server.expose_routing_headers && failure.attempts > 0 {
                add_routing_headers(
                    &mut response,
                    failure.deployment.as_deref(),
                    failure.attempts,
                );
            }
            return response;
        }
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
                    Err(_) if format == InboundFormat::AnthropicMessages => Event::default()
                        .event("error")
                        .data(r#"{"type":"error","error":{"type":"api_error","message":"Upstream stream ended unexpectedly"}}"#),
                    Err(_) => Event::default().data(
                        r#"{"error":{"message":"Upstream stream ended unexpectedly","type":"upstream_error","param":null,"code":"stream_broken"}}"#,
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
    if active.config.server.expose_routing_headers {
        add_routing_headers(&mut response, Some(&routed.deployment), routed.attempts);
    }
    response
}

fn add_routing_headers(response: &mut Response, deployment: Option<&str>, attempts: usize) {
    if let Some(value) = deployment.and_then(|id| HeaderValue::from_str(id).ok()) {
        response.headers_mut().insert(DEPLOYMENT_HEADER, value);
    }
    response
        .headers_mut()
        .insert(ATTEMPTS_HEADER, HeaderValue::from(attempts));
}

fn authorized_for_format(headers: &HeaderMap, keys: &[Arc<str>], format: InboundFormat) -> bool {
    if format == InboundFormat::AnthropicMessages && !headers.contains_key(header::AUTHORIZATION) {
        let mut values = headers.get_all(ANTHROPIC_KEY_HEADER).iter();
        return values
            .next()
            .is_some_and(|value| values.next().is_none() && matches_key(value.as_bytes(), keys));
    }
    authorized(headers, keys)
}

fn without_gateway_routing_fields(body: &[u8]) -> Option<Bytes> {
    let mut fields: BTreeMap<String, &RawValue> = serde_json::from_slice(body).ok()?;
    for field in ["min_discount_percent", "zdr", "ranking"] {
        fields.remove(field);
    }
    serde_json::to_vec(&fields).ok().map(Bytes::from)
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
    matches_key(value.trim_start_matches(' ').as_bytes(), keys)
}

fn matches_key(value: &[u8], keys: &[Arc<str>]) -> bool {
    keys.iter()
        .fold(subtle::Choice::from(0), |matched, key| {
            matched | key.as_bytes().ct_eq(value)
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
            rate_limited(retry_after)
        }
        GatewayError::Client(
            UpstreamError::BadRequest { status, body }
            | UpstreamError::ModelNotFound { status, body },
        ) => upstream_client_error(status, body),
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

fn rate_limited(wait: Option<Duration>) -> Response {
    let mut response = error(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limit_exceeded",
        "No upstream currently available",
    );
    if let Some(wait) = wait {
        let seconds = wait
            .as_secs()
            .saturating_add(u64::from(wait.subsec_nanos() > 0));
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
    }
    response
}

fn upstream_client_error(status: StatusCode, body: Bytes) -> Response {
    let Ok(mut payload) = serde_json::from_slice::<Value>(&body) else {
        return error(
            status,
            "upstream_client_error",
            "Upstream rejected the request",
        );
    };
    let Some(details) = payload.get_mut("error").and_then(Value::as_object_mut) else {
        return error(
            status,
            "upstream_client_error",
            "Upstream rejected the request",
        );
    };
    let has_message = details.get("message").is_some_and(Value::is_string);
    let has_type = details.get("type").is_some_and(Value::is_string);
    if has_message && has_type && details.contains_key("param") && details.contains_key("code") {
        return json_body(status, Body::from(body));
    }
    if !has_message {
        details.insert("message".into(), json!("Upstream rejected the request"));
    }
    if !has_type {
        details.insert("type".into(), json!("invalid_request_error"));
    }
    details.entry("param").or_insert(Value::Null);
    details
        .entry("code")
        .or_insert_with(|| json!("upstream_client_error"));
    json_body(status, Body::from(payload.to_string()))
}

fn chat_bad_request(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error":{"message":message,"type":"invalid_request_error","code":"bad_request"}})),
    )
        .into_response()
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
