//! Operator management surface: one core behind `/admin`, reused by CLI and MCP.

use crate::{authorized, build, Active, AppState};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use http::{header, HeaderMap};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

pub(crate) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/admin/status", get(status))
        .route("/admin/deployments", get(deployments))
        .route("/admin/models", get(models))
        .route("/admin/credentials", get(credentials))
        .route("/admin/credentials/{id}/disabled", post(set_disabled))
        .route("/admin/reload", post(reload))
}

fn authorize(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    // The live config owns the admin key, so a reload that rotates or
    // removes `server.dashboard_key` revokes the old key immediately.
    let active = state.active.load();
    match active.config.server.dashboard_key.as_ref() {
        Some(key) if authorized(headers, std::slice::from_ref(key)) => None,
        _ => Some(
            (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Bearer")],
            )
                .into_response(),
        ),
    }
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

async fn status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(denied) = authorize(&state, &headers) {
        return denied;
    }
    let dashboard = state.dashboard.as_ref().unwrap();
    no_store(
        Json(json!({
            "uptime_ms": dashboard.uptime_ms(),
            "version": env!("CARGO_PKG_VERSION"),
            "responses": dashboard.counters(),
        }))
        .into_response(),
    )
}

async fn deployments(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(denied) = authorize(&state, &headers) {
        return denied;
    }
    let active = state.active.load();
    no_store(Json(json!({"deployments": active.executor.deployment_stats()})).into_response())
}

async fn models(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(denied) = authorize(&state, &headers) {
        return denied;
    }
    let active = state.active.load();
    let config = &active.config;
    let models: Vec<Value> = config
        .models
        .iter()
        .enumerate()
        .filter(|(index, model)| model.name.as_ref() != "*" && config.catch_all != Some(*index))
        .map(|(index, model)| {
            let deployments: Vec<&str> = model
                .deployments
                .iter()
                .map(|idx| config.deployments[idx.0 as usize].id.as_ref())
                .collect();
            json!({
                "name": model.name.as_ref(),
                "deployments": deployments,
                "restricted": config.model_allowed_keys.contains_key(&index),
                "stream": model.capabilities.stream,
                "max_context": model.capabilities.max_context,
            })
        })
        .collect();
    no_store(Json(json!({"models": models})).into_response())
}

async fn credentials(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(denied) = authorize(&state, &headers) {
        return denied;
    }
    let active = state.active.load();
    let config = &active.config;
    let mut credentials: Vec<Value> = config
        .credential_meta
        .iter()
        .map(|(id, meta)| {
            json!({
                "id": id.as_ref(),
                "kind": if meta.oauth { "oauth" } else { "api_key" },
                "provider": meta.provider.0.as_ref(),
                "disabled": active.executor.credential_disabled(id).unwrap_or(false),
                "disabled_in_config": config.credential_disabled.contains(id),
                "quota": config.credential_quotas.get(id),
            })
        })
        .collect();
    credentials.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    no_store(Json(json!({"credentials": credentials})).into_response())
}

#[derive(Deserialize)]
struct Disabled {
    disabled: bool,
}

async fn set_disabled(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Disabled>,
) -> Response {
    if let Some(denied) = authorize(&state, &headers) {
        return denied;
    }
    let active = state.active.load();
    if !active.executor.set_credential_disabled(&id, body.disabled) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error":{"message":"Unknown credential","type":"invalid_request_error","param":null,"code":"credential_not_found"}})),
        )
            .into_response();
    }
    no_store(Json(json!({"id": id, "disabled": body.disabled})).into_response())
}

async fn reload(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(denied) = authorize(&state, &headers) {
        return denied;
    }
    let Some(path) = &state.config_path else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":{"message":"No config path recorded; cannot reload","type":"invalid_request_error","param":null,"code":"reload_unavailable"}})),
        )
            .into_response();
    };
    // Serialize reloads: an older build must never overwrite a newer swap.
    let _guard = state.reload_lock.lock().await;
    let config = match llmux_config::RuntimeConfig::load(path) {
        Ok(config) => config,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":{"message":format!("Reload rejected: {err}"),"type":"invalid_request_error","param":null,"code":"invalid_config"}})),
            )
                .into_response()
        }
    };
    let devin = llmux_providers::DevinAcpConfig::from_env();
    state.catalog.set_config(devin.clone());
    match build(config, devin, state.catalog.live()).await {
        Ok((config, executor)) => {
            let models: Vec<String> = config
                .models
                .iter()
                .map(|model| model.name.to_string())
                .collect();
            // Fields only `run` reads at startup cannot change at runtime.
            let previous = state.active.load();
            let restart_only = [
                ("listen", previous.config.server.listen != config.server.listen),
                (
                    "shutdown_timeout",
                    previous.config.server.shutdown_timeout != config.server.shutdown_timeout,
                ),
                (
                    "drain_grace",
                    previous.config.server.drain_grace != config.server.drain_grace,
                ),
            ];
            let warnings: Vec<String> = restart_only
                .into_iter()
                .filter(|(_, changed)| *changed)
                .map(|(field, _)| {
                    format!("server.{field} changes apply on restart only")
                })
                .collect();
            state.active.store(Arc::new(Active {
                config,
                executor: Arc::new(executor),
            }));
            no_store(
                Json(json!({"reloaded": true, "models": models, "warnings": warnings}))
                    .into_response(),
            )
        }
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":{"message":format!("Reload rejected: {err}"),"type":"invalid_request_error","param":null,"code":"invalid_config"}})),
        )
            .into_response(),
    }
}
