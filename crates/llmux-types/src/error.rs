use bytes::Bytes;
use http::StatusCode;
use serde::Deserialize;
use std::time::{Duration, SystemTime};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum UpstreamError {
    #[error("rate limited")]
    RateLimited { retry_after: Option<Duration> },
    #[error("quota exhausted")]
    QuotaExhausted { reset_at: Option<SystemTime> },
    #[error("auth failed")]
    Auth { refreshable: bool },
    #[error("bad request")]
    BadRequest { status: StatusCode, body: Bytes },
    #[error("context too long")]
    ContextLength,
    #[error("server error ({status})")]
    Server { status: StatusCode },
    #[error("timeout")]
    Timeout,
    #[error("connect error")]
    Connect,
    #[error("stream broken after {bytes_sent} bytes")]
    StreamBroken { bytes_sent: u64 },
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    RateLimited,
    QuotaExhausted,
    Auth,
    BadRequest,
    ContextLength,
    ServerError,
    Timeout,
    ConnectError,
    StreamBroken,
}

impl UpstreamError {
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::RateLimited { .. } => ErrorKind::RateLimited,
            Self::QuotaExhausted { .. } => ErrorKind::QuotaExhausted,
            Self::Auth { .. } => ErrorKind::Auth,
            Self::BadRequest { .. } => ErrorKind::BadRequest,
            Self::ContextLength => ErrorKind::ContextLength,
            Self::Server { .. } => ErrorKind::ServerError,
            Self::Timeout => ErrorKind::Timeout,
            Self::Connect => ErrorKind::ConnectError,
            Self::StreamBroken { .. } => ErrorKind::StreamBroken,
        }
    }
}

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("unsupported inbound format")]
    UnsupportedFormat,
    #[error("invalid request body")]
    InvalidBody,
    #[error("invalid upstream URI or request headers")]
    InvalidRequest(#[from] http::Error),
    #[error("invalid upstream URI")]
    InvalidUri(#[from] http::uri::InvalidUri),
    #[error("invalid upstream credential")]
    InvalidAuth,
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("credential is unavailable")]
    Unavailable,
    #[error("credential refresh failed")]
    RefreshFailed,
}
