use crate::{authorized, AppState};
use axum::{
    extract::State,
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use http::{header, HeaderMap, StatusCode};
use serde_json::json;
use std::{
    sync::{
        atomic::{AtomicU64, Ordering::Relaxed},
        Arc,
    },
    time::{Duration, Instant},
};

pub(crate) struct Dashboard {
    key: Arc<str>,
    started: Instant,
    ok: AtomicU64,
    rejected: AtomicU64,
    unavailable: AtomicU64,
    failed: AtomicU64,
    ready_us: AtomicU64,
}

impl Dashboard {
    pub(crate) fn new(key: Arc<str>) -> Self {
        Self {
            key,
            started: Instant::now(),
            ok: 0.into(),
            rejected: 0.into(),
            unavailable: 0.into(),
            failed: 0.into(),
            ready_us: 0.into(),
        }
    }

    pub(crate) fn record_response(&self, status: StatusCode, elapsed: Duration) {
        let count = if status.is_success() {
            &self.ok
        } else if status == StatusCode::TOO_MANY_REQUESTS {
            &self.unavailable
        } else if status.is_client_error() {
            &self.rejected
        } else {
            &self.failed
        };
        self.ready_us.fetch_add(elapsed.as_micros() as u64, Relaxed);
        count.fetch_add(1, Relaxed);
    }
}

pub(crate) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/dashboard", get(page))
        .route("/dashboard/app.css", get(stylesheet))
        .route("/dashboard/app.js", get(script))
        .route("/dashboard/stats", get(stats))
}

async fn page() -> impl IntoResponse {
    (
        [
            (header::CONTENT_SECURITY_POLICY, "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        Html(include_str!("dashboard/index.html")),
    )
}

async fn stylesheet() -> impl IntoResponse {
    asset("text/css; charset=utf-8", include_str!("dashboard/app.css"))
}

async fn script() -> impl IntoResponse {
    asset(
        "text/javascript; charset=utf-8",
        include_str!("dashboard/app.js"),
    )
}

fn asset(content_type: &'static str, body: &'static str) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
}

async fn stats(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let Some(dashboard) = &state.dashboard else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut response = if authorized(&headers, std::slice::from_ref(&dashboard.key)) {
        let deployments = state.executor.deployment_stats();
        Json(json!({
            "fallback": llmux_core::FallbackStats::from_deployments(&deployments),
            "catch_all": state.executor.catch_all_stats(),
            "uptime_ms": dashboard.started.elapsed().as_millis() as u64,
            "responses": {
                "ok": dashboard.ok.load(Relaxed),
                "rejected": dashboard.rejected.load(Relaxed),
                "unavailable": dashboard.unavailable.load(Relaxed),
                "failed": dashboard.failed.load(Relaxed),
                "ready_us": dashboard.ready_us.load(Relaxed),
            },
            "deployments": deployments,
        }))
        .into_response()
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
        )
            .into_response()
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}
