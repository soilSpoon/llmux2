//! Axum boundary for the OpenAI-compatible gateway.

mod admin;
mod body_spool;
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
use llmux_config::{ApiKey, RuntimeConfig};
use llmux_core::{Executor, GatewayError, InitError, LiveModels, RequestLog, RoutedError};
use llmux_providers::nous_portal::{NousPortalConfig, NousPortalSource};
use llmux_providers::oauth::{OAuthConfig, OAuthCredentialSource};
use llmux_providers::{
    truncate_middle_for_format, AcpAgent, Anthropic, Antigravity, AwsQuick, DeepAi, DevinAcp,
    DevinAcpConfig, Freebuff, NoAuthSource, OpenAiCompat, OpenAiResponses, OpenCode,
    RequestMetadata, StaticKey, Xai,
};
use llmux_types::{
    provider, AcpAgentSpec, Billing, ContextOverflow, CredentialSource, GatewayRequest,
    GatewayResponse, InboundFormat, ProviderRegistry, RequestId, SpoolStore, TrafficClass,
    UpstreamError, MINIMAL_OPENAI_CHAT_COST,
};
use serde_json::{json, value::RawValue, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, convert::Infallible, error::Error, sync::Arc, time::Duration};
use subtle::ConstantTimeEq;

const DEPLOYMENT_HEADER: HeaderName = HeaderName::from_static("x-llmux-deployment");
const ATTEMPTS_HEADER: HeaderName = HeaderName::from_static("x-llmux-attempts");
const CONTEXT_OVERFLOW_HEADER: HeaderName = HeaderName::from_static("x-llmux-context-overflow");
const FREE_ONLY_HEADER: HeaderName = HeaderName::from_static("x-llmux-free-only");
const DROPPED_MESSAGES_HEADER: HeaderName = HeaderName::from_static("x-llmux-dropped-messages");
const MAX_MESSAGES_ERROR_BYTES: usize = 16 * 1024 * 1024;
const ANTHROPIC_KEY_HEADER: HeaderName = HeaderName::from_static(provider::ANTHROPIC_KEY_HEADER);
const DEEPAI_KEY_HEADER: HeaderName = HeaderName::from_static(provider::DEEPAI_KEY_HEADER);

struct Active {
    config: Arc<RuntimeConfig>,
    executor: Arc<Executor>,
}

struct AppState {
    active: arc_swap::ArcSwap<Active>,
    spool: SpoolStore,
    dashboard: Option<dashboard::Dashboard>,
    config_path: Option<std::path::PathBuf>,
    reload_lock: tokio::sync::Mutex<()>,
    catalog: Arc<catalog::Catalog>,
}

fn provider_registry(
    devin: DevinAcpConfig,
    acp_agents: &std::collections::HashMap<Arc<str>, AcpAgentSpec>,
) -> ProviderRegistry {
    let mut registry = ProviderRegistry::default();
    for kind in [
        provider::OPENAI,
        provider::OPENAI_COMPAT,
        provider::DEEPAI_CODE,
    ] {
        registry.register(Arc::new(OpenAiCompat::new(kind)));
    }
    registry.register(Arc::new(Anthropic::new(provider::ANTHROPIC)));
    registry.register(Arc::new(OpenAiResponses::new()));
    registry.register(Arc::new(OpenCode::new()));
    registry.register(Arc::new(DeepAi::new()));
    registry.register(Arc::new(Freebuff::new()));
    registry.register(Arc::new(Xai::new()));
    registry.register(Arc::new(Antigravity::new()));
    registry.register(Arc::new(AwsQuick::new()));
    registry.register(Arc::new(DevinAcp::new(devin)));
    registry.register(Arc::new(AcpAgent::new(acp_agents.clone())));
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
                provider::DEEPAI_CODE => StaticKey::header(DEEPAI_KEY_HEADER, &key)?,
                _ => StaticKey::bearer(&key)?,
            };
            Ok((id, Arc::new(source) as Arc<dyn CredentialSource>))
        })
        .collect()
}

pub fn validate_startup(mut config: RuntimeConfig) -> Result<(), Box<dyn Error + Send + Sync>> {
    let registry = provider_registry(DevinAcpConfig::from_env(), &config.acp_agents);
    let sources = static_sources(&mut config)?;
    let credentials = sources
        .keys()
        .chain(config.oauth.keys())
        .chain(config.nous.keys())
        .chain(config.acp_agents.keys())
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
    Ok(app_with_state(config, config_path, devin).await?.0)
}

async fn app_with_state(
    config: RuntimeConfig,
    config_path: Option<std::path::PathBuf>,
    devin: DevinAcpConfig,
) -> Result<(Router, Arc<AppState>), Box<dyn Error + Send + Sync>> {
    let catalog = Arc::new(catalog::Catalog::new(devin.clone()));
    let spool = SpoolStore::open(
        &config.server.request_body_spool_dir,
        config.server.request_body_spool_budget_bytes,
    )?;
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
        spool,
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
    let router = router.with_state(Arc::clone(&state));
    Ok((router, state))
}

async fn build(
    mut config: RuntimeConfig,
    devin: DevinAcpConfig,
    live: Arc<LiveModels>,
) -> Result<(Arc<RuntimeConfig>, Executor), Box<dyn Error + Send + Sync>> {
    let registry = provider_registry(devin, &config.acp_agents);
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
            .chain(config.acp_agents.keys())
            .cloned()
            .collect(),
    )?;
    // Open the request log before starting credential sources: their first
    // refresh can rotate tokens, so every fallible build step must finish
    // before they start.
    let request_log = config
        .server
        .request_log
        .as_deref()
        .map(|path| RequestLog::open_with_traffic_classes(path, traffic_class_map(&config.server)))
        .transpose()
        .map_err(InitError::RequestLog)?;
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
        if let Some(token_field) = credential.token_field {
            oauth.token_field = token_field;
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
    // acp_agent credentials name a local child process, not remote auth; the
    // executor still requires a source per referenced credential id.
    for id in config.acp_agents.keys() {
        sources.insert(id.clone(), Arc::new(NoAuthSource));
    }

    let config = Arc::new(config);
    let mut executor = Executor::new(config.clone(), &registry, &sources, request_log)?;
    executor.set_devin_catalog(live);
    Ok((config, executor))
}

async fn models(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let active = state.active.load();
    // Bearer-only identity, same extraction as `authorized`.
    let Some(client_key) = matched_key(
        &headers,
        &active.config.server.client_keys,
        InboundFormat::OpenAiChat,
    ) else {
        return unauthorized();
    };
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
    let deny = &active.config.server.deny;
    let key_policy = active
        .config
        .server
        .key_policies
        .get(client_key.index)
        .and_then(Option::as_ref);
    // A usable free leg is a non-fallback deployment marked free that could
    // admit an OpenAI chat request right now. Quota, cooldown, parking, expiry,
    // and stale-catalog state count because ci otherwise keeps buying models
    // whose every free leg can only return all_candidates_exhausted.
    let has_usable_free_leg = |id: &str| match active.config.model_names.get(id) {
        Some(&index) => active.config.models[index].deployments.iter().any(|idx| {
            let dep = &active.config.deployments[idx.0 as usize];
            !dep.fallback
                && dep.billing == Billing::Free
                && active.executor.deployment_available(
                    *idx,
                    InboundFormat::OpenAiChat,
                    u64::from(MINIMAL_OPENAI_CHAT_COST),
                )
        }),
        None => false,
    };
    let data: Vec<_> = ids
        .into_iter()
        .filter(|&id| !policy::model_denied(deny, id))
        .filter(|&id| key_policy.is_none_or(|p| policy::allows_model(p, id)))
        .filter(|&id| !key_policy.is_some_and(|p| p.free_only) || has_usable_free_leg(id))
        .map(|id| {
            // `free` only when the model has at least one non-fallback leg and
            // every non-fallback leg is marked free; a missing marker is billed.
            // Billed fallback legs never fire under the probe's
            // x-llmux-free-only requests, so they do not gate sweeping, but a
            // fallback-only model has no usable free leg and must not be swept.
            let billing = match active.config.model_names.get(id) {
                Some(&index) => {
                    let mut saw_usable_free = false;
                    let mut all_free = true;
                    for idx in &active.config.models[index].deployments {
                        let dep = &active.config.deployments[idx.0 as usize];
                        if dep.fallback {
                            continue;
                        }
                        if dep.billing == Billing::Free {
                            saw_usable_free = true;
                        } else {
                            all_free = false;
                        }
                    }
                    if saw_usable_free && all_free {
                        "free"
                    } else {
                        "billed"
                    }
                }
                _ => "billed",
            };
            json!({"id": id, "object": "model", "created": 0, "owned_by": "llmux2", "billing": billing})
        })
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
    let mut payload: Value = body::to_bytes(body, MAX_MESSAGES_ERROR_BYTES)
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
    let Some(client_key) =
        matched_key(request.headers(), &active.config.server.client_keys, format)
    else {
        return unauthorized();
    };
    let catalog = catalog::has_devin(&active.config).then(|| Arc::clone(&state.catalog));
    let start = state.dashboard.as_ref().map(|_| std::time::Instant::now());
    let response =
        execute_authorized(&active, &state.spool, request, format, client_key, catalog).await;
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

struct Ingress {
    store: SpoolStore,
    captured: body_spool::CapturedBody,
    body: Bytes,
}

fn capture_error(kind: body_spool::CaptureError) -> Response {
    match kind {
        body_spool::CaptureError::TooLarge => error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request_error",
            "Request body too large",
        ),
        body_spool::CaptureError::Read => error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Could not read request body",
        ),
        body_spool::CaptureError::BudgetExhausted => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "service_unavailable",
            "Request body storage capacity is temporarily exhausted",
        ),
        body_spool::CaptureError::Io => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "Gateway could not store the request body",
        ),
    }
}

async fn execute_authorized(
    active: &Active,
    spool: &SpoolStore,
    request: Request,
    format: InboundFormat,
    client_key: MatchedKey,
    catalog: Option<Arc<catalog::Catalog>>,
) -> Response {
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
    let captured = match body_spool::capture_body(
        body,
        spool.clone(),
        active.config.server.request_body_memory_bytes,
        active.config.server.max_request_bytes,
    )
    .await
    {
        Ok(captured) => captured,
        Err(error) => return capture_error(error),
    };
    let body = match captured.bytes().await {
        Ok(body) => body,
        Err(error) => return capture_error(error),
    };
    if let Some(catalog) = catalog {
        catalog.kick();
    }
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
        Ingress {
            store: spool.clone(),
            captured,
            body,
        },
        &parts.headers,
        format,
        context_overflow,
        end_user.clone(),
        client_key,
    )
    .await;
    policy::attach_end_user(response, end_user.as_deref())
}

async fn route_request(
    active: &Active,
    ingress: Ingress,
    headers: &HeaderMap,
    format: InboundFormat,
    context_overflow: Option<ContextOverflow>,
    end_user: Option<Arc<str>>,
    client_key: MatchedKey,
) -> Response {
    let Ingress {
        store,
        captured,
        body,
    } = ingress;
    let Ok(metadata) = RequestMetadata::parse(&body, format) else {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Expected a JSON object with a nonempty string model and optional boolean stream",
        );
    };
    let has_gateway_routing_fields = metadata.has_gateway_routing_fields;
    let request_stream = metadata.stream;
    let max_output_tokens = metadata.max_output_tokens;
    let prompt_bytes = metadata.prompt_bytes;
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

    let free_only = match headers.get(FREE_ONLY_HEADER) {
        None => false,
        Some(value) if value == "true" => true,
        Some(value) if value == "false" => false,
        Some(_) => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "x-llmux-free-only must be true or false",
            );
        }
    };
    let model: Arc<str> = Arc::from(metadata.model.as_ref());
    // The global deny list wins over everything — checked on the requested id
    // before any alias, canonical, or catch-all resolution, for every key.
    if policy::model_denied(&active.config.server.deny, &model) {
        return error(
            StatusCode::FORBIDDEN,
            "model_not_allowed",
            "Model is denied by gateway policy",
        );
    }
    // A listed key may request only names its policy allows — checked on the
    // requested id before any alias, canonical, or catch-all resolution. A
    // free-only key never routes to a billed leg, header or not.
    let key_policy = active
        .config
        .server
        .key_policies
        .get(client_key.index)
        .and_then(Option::as_ref);
    let free_only = free_only || key_policy.is_some_and(|p| p.free_only);
    if key_policy.is_some_and(|key_policy| !policy::allows_model(key_policy, &model)) {
        return error(
            StatusCode::FORBIDDEN,
            "model_not_allowed",
            "Gateway key is not allowed to use this model",
        );
    }
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
    let mut replay = match captured {
        body_spool::CapturedBody::Memory(_) => None,
        body_spool::CapturedBody::Spooled(spooled) => Some(spooled),
    };
    let upstream_body = if has_gateway_routing_fields {
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
    if has_gateway_routing_fields && replay.is_some() {
        match body_spool::copy_to_spool(store.clone(), upstream_body.clone()).await {
            Ok(spooled) => replay = Some(spooled),
            Err(error) => return capture_error(error),
        }
    }
    let mut anthropic_headers = HeaderMap::new();
    if format == InboundFormat::AnthropicMessages {
        for name in ["anthropic-version", "anthropic-beta"] {
            for value in headers.get_all(name) {
                anthropic_headers.append(HeaderName::from_static(name), value.clone());
            }
        }
    }
    // The record describes the inbound request; a middle-out retry sends a
    // truncated body but must not shrink the logged image count.
    let image_count = policy::count_images(upstream_body.as_ref());
    drop(body);
    let inline_body = if replay.is_some() {
        drop(upstream_body);
        None
    } else {
        Some(upstream_body)
    };
    let request = |body: Bytes, prompt_bytes: u64| {
        let estimate = u32::try_from(prompt_bytes.div_ceil(4)).unwrap_or(u32::MAX);
        GatewayRequest {
            format,
            model: model.clone(),
            stream: request_stream,
            est_input_tokens: if format == InboundFormat::OpenAiChat {
                estimate.max(MINIMAL_OPENAI_CHAT_COST)
            } else {
                estimate
            },
            max_output_tokens,
            context_overflow,
            free_only,
            image_count,
            body,
            anthropic_headers: anthropic_headers.clone(),
            request_id: request_id.clone(),
            end_user: end_user.clone(),
            client_key: Some(client_key.fingerprint.clone()),
        }
    };

    let mut dropped = 0;
    let mut result = active
        .executor
        .execute_attributed_with_replay(
            request(inline_body.clone().unwrap_or_default(), prompt_bytes),
            replay.clone(),
        )
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
        let truncated = {
            let truncate_source = match &inline_body {
                Some(body) => body.clone(),
                None => match body_spool::read_spool_bytes(
                    replay.as_ref().expect("spooled body replay").clone(),
                )
                .await
                {
                    Ok(body) => body,
                    Err(error) => return capture_error(error),
                },
            };
            truncate_middle_for_format(
                &truncate_source,
                prompt_bytes,
                max_output_tokens,
                limit,
                format,
            )
        };
        if let Some(truncated) = truncated {
            dropped = truncated.dropped;
            let truncated_prompt_bytes = truncated.prompt_bytes;
            let truncated_source = truncated.body;
            drop(inline_body);
            let truncated_replay =
                if replay.is_some() {
                    let spooled =
                        match body_spool::copy_to_spool(store.clone(), truncated_source.clone())
                            .await
                        {
                            Ok(spooled) => spooled,
                            Err(error) => return capture_error(error),
                        };
                    drop(replay);
                    Some(spooled)
                } else {
                    None
                };
            let truncated_body = if truncated_replay.is_some() {
                drop(truncated_source);
                Bytes::new()
            } else {
                truncated_source
            };
            result = active
                .executor
                .execute_attributed_with_replay(
                    request(truncated_body, truncated_prompt_bytes),
                    truncated_replay,
                )
                .await;
        }
    }
    let routed = match result {
        Ok(routed) => routed,
        Err(RoutedError {
            error: GatewayError::UnknownModel,
            ..
        }) if format == InboundFormat::OpenAiChat && !free_only => {
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
    matched_key(headers, keys, format).is_some()
}

/// Which configured client key a request presented: the index into
/// `server.client_keys` (for per-key policy) and the `key_` fingerprint that
/// attributes the request log without holding the secret.
struct MatchedKey {
    index: usize,
    fingerprint: Arc<str>,
}

/// Constant-time scan that also returns which key matched.
fn matched_key(
    headers: &HeaderMap,
    keys: &[Arc<str>],
    format: InboundFormat,
) -> Option<MatchedKey> {
    let presented: Vec<u8> = if format == InboundFormat::AnthropicMessages
        && !headers.contains_key(header::AUTHORIZATION)
    {
        let mut values = headers.get_all(ANTHROPIC_KEY_HEADER).iter();
        let first = values.next()?;
        if values.next().is_some() {
            return None;
        }
        first.as_bytes().to_vec()
    } else {
        let mut auth = headers.get_all(header::AUTHORIZATION).iter();
        let value = auth
            .next()
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.split_once(' '))?;
        if auth.next().is_some() || !value.0.eq_ignore_ascii_case("bearer") {
            return None;
        }
        value.1.trim_start_matches(' ').as_bytes().to_vec()
    };
    keys.iter()
        .position(|key| bool::from(key.as_bytes().ct_eq(&presented)))
        .map(|index| MatchedKey {
            index,
            fingerprint: key_fingerprint(keys, &keys[index]),
        })
}

fn traffic_class_map(
    server: &llmux_config::ServerConfig,
) -> std::collections::HashMap<Arc<str>, TrafficClass> {
    let mut seen = std::collections::HashSet::new();
    server
        .client_keys
        .iter()
        .enumerate()
        .filter_map(|(index, key)| {
            let fingerprint = key_fingerprint(&server.client_keys, key);
            // Duplicate key values authenticate as their first entry; later
            // copies must not overwrite that entry's accounting class.
            if !seen.insert(fingerprint.clone()) {
                return None;
            }
            let class = server
                .key_policies
                .get(index)
                .and_then(Option::as_ref)?
                .traffic_class?;
            Some((fingerprint, class))
        })
        .collect()
}

fn key_fingerprint(keys: &[Arc<str>], key: &Arc<str>) -> Arc<str> {
    // Pepper the fingerprint with a hash of the configured key set so
    // a log reader cannot confirm a guessed low-entropy key without
    // knowing every configured key. Keys are hashed sorted so a
    // reload that only reorders them keeps the same fingerprints.
    let mut ordered: Vec<&Arc<str>> = keys.iter().collect();
    ordered.sort();
    let mut pepper = Sha256::new();
    for configured in ordered {
        pepper.update(configured.as_bytes());
        pepper.update([0]);
    }
    let pepper = pepper.finalize();
    let mut hasher = Sha256::new();
    hasher.update(pepper);
    hasher.update(key.as_bytes());
    let digest = hasher.finalize();
    let mut fingerprint = String::from("key_");
    for byte in &digest[..8] {
        fingerprint.push_str(&format!("{byte:02x}"));
    }
    Arc::from(fingerprint)
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

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::{BodyExt, Full};
    use hyper_util::{client::legacy::Client, rt::TokioExecutor};
    use llmux_testkit::TestServer;
    use std::fs;
    use tokio::time::timeout;

    #[tokio::test]
    async fn oversized_body_is_rejected_before_a_devin_catalog_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let client = dir.path().join("client");
        fs::write(&client, "client-key\n").unwrap();
        let binary = dir.path().join("devin");
        fs::write(&binary, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let config = format!(
            r#"
[server]
client_keys = ["file:{}"]
max_request_bytes = 64

[[models]]
name = "retired"

[[models.deployments]]
id = "retired-devin"
auth = "none"
provider = "devin_acp"
billing = "free"
upstream_model = "swe-2-retired"
priority = 0
"#,
            client.display()
        );
        let (router, state) = app_with_state(
            RuntimeConfig::parse(&config).unwrap(),
            None,
            DevinAcpConfig {
                binary,
                home: dir.path().to_owned(),
                turn_timeout: Duration::from_secs(10),
            },
        )
        .await
        .unwrap();
        let server = TestServer::start(router).await.unwrap();
        let payload = json!({
            "model": "retired",
            "messages": [{"role": "user", "content": "Hello"}],
            "pad": "x".repeat(64)
        })
        .to_string();
        assert!(payload.len() > 64);
        let request = http::Request::post(format!("{}/v1/chat/completions", server.url()))
            .header(header::AUTHORIZATION, "Bearer client-key")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(payload)))
            .unwrap();

        let response = timeout(
            Duration::from_secs(5),
            Client::builder(TokioExecutor::new())
                .build_http()
                .request(request),
        )
        .await
        .expect("oversized request timed out")
        .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let bytes = timeout(Duration::from_secs(5), response.into_body().collect())
            .await
            .unwrap()
            .unwrap()
            .to_bytes();
        let payload: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(payload["error"]["message"], "Request body too large");
        assert_eq!(payload["error"]["code"], "invalid_request_error");
        assert!(
            !state.catalog.refresh_was_started(),
            "the size rejection must happen before a catalog refresh is scheduled"
        );
    }

    #[test]
    fn key_fingerprint_survives_key_reorder() {
        let keys: Vec<Arc<str>> = vec!["sk-alpha".into(), "sk-beta".into()];
        let mut reordered = keys.clone();
        reordered.reverse();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer sk-beta"),
        );
        let before = matched_key(&headers, &keys, InboundFormat::OpenAiChat)
            .map(|matched| matched.fingerprint);
        let after = matched_key(&headers, &reordered, InboundFormat::OpenAiChat)
            .map(|matched| matched.fingerprint);
        assert_eq!(before, after);
    }
}
