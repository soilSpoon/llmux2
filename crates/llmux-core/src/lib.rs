//! Priority routing, deployment limits, and HTTP execution.

mod limits;

use bytes::Bytes;
use futures_util::{stream, StreamExt};
use http::StatusCode;
use http_body_util::Full;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::TokioExecutor,
};
use limits::{DeploymentLimits, Permit};
use llmux_config::RuntimeConfig;
use llmux_types::{
    CredentialSource, Deployment, DeploymentIdx, ErrorKind, GatewayRequest, GatewayResponse,
    ProviderAdapter, ProviderRegistry, Support, UpstreamError,
};
use std::{
    collections::HashMap,
    ops::Range,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use thiserror::Error;
use tokio::time::{timeout, timeout_at, Instant};

#[derive(Debug, Error)]
pub enum InitError {
    #[error("no registered adapter for provider {0}")]
    UnknownProvider(Arc<str>),
    #[error("deployment has no credential source")]
    MissingCredential,
}

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("unknown model")]
    UnknownModel,
    #[error("model does not support streaming")]
    UnsupportedStream,
    #[error("all candidates are rate limited")]
    AllCandidatesExhausted {
        attempts: usize,
        retry_after: Duration,
    },
    #[error(transparent)]
    Client(UpstreamError),
    #[error("gateway configuration or adapter error")]
    Internal,
}

pub struct RoutedResponse {
    pub deployment: Arc<str>,
    pub attempts: usize,
    pub response: GatewayResponse,
}

type HttpClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

struct Binding {
    adapter: Arc<dyn ProviderAdapter>,
    auth: Arc<dyn CredentialSource>,
    limits: Arc<DeploymentLimits>,
}

struct ModelOrder {
    next: AtomicUsize,
    groups: Box<[Range<usize>]>,
}

pub struct Executor {
    cfg: Arc<RuntimeConfig>,
    bindings: Box<[Binding]>,
    orders: Box<[ModelOrder]>,
    http: HttpClient,
}

impl Executor {
    pub fn new(
        cfg: Arc<RuntimeConfig>,
        registry: &ProviderRegistry,
        credentials: &HashMap<Arc<str>, Arc<dyn CredentialSource>>,
    ) -> Result<Self, InitError> {
        let bindings = cfg
            .deployments
            .iter()
            .map(|dep| {
                let adapter = registry
                    .get(&dep.provider)
                    .ok_or_else(|| InitError::UnknownProvider(dep.provider.0.clone()))?
                    .clone();
                let auth = credentials
                    .get(&dep.credential)
                    .ok_or(InitError::MissingCredential)?
                    .clone();
                Ok(Binding {
                    adapter,
                    auth,
                    limits: Arc::new(DeploymentLimits::new(dep.quota)),
                })
            })
            .collect::<Result<Box<[_]>, _>>()?;

        let orders = cfg
            .models
            .iter()
            .map(|model| {
                let mut start = 0;
                let groups = model
                    .deployments
                    .chunk_by(|a, b| {
                        cfg.deployments[a.0 as usize].priority
                            == cfg.deployments[b.0 as usize].priority
                    })
                    .map(|group| {
                        let end = start + group.len();
                        let range = start..end;
                        start = end;
                        range
                    })
                    .collect();
                ModelOrder {
                    next: AtomicUsize::new(0),
                    groups,
                }
            })
            .collect();

        let connector = HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .build();
        let mut builder = Client::builder(TokioExecutor::new());
        builder.retry_canceled_requests(false);
        Ok(Self {
            cfg,
            bindings,
            orders,
            http: builder.build(connector),
        })
    }

    pub async fn execute(&self, req: GatewayRequest) -> Result<RoutedResponse, GatewayError> {
        let &model_idx = self
            .cfg
            .model_names
            .get(req.model.as_ref())
            .ok_or(GatewayError::UnknownModel)?;
        let model = &self.cfg.models[model_idx];
        if req.stream && !model.capabilities.stream {
            return Err(GatewayError::UnsupportedStream);
        }
        let order = &self.orders[model_idx];
        let sequence = if order.groups.iter().any(|group| group.len() > 1) {
            order.next.fetch_add(1, Ordering::Relaxed)
        } else {
            0
        };
        let deadline = Instant::now() + self.cfg.server.request_timeout;
        let mut attempts = 0;
        let mut earliest = None;
        let mut supported = false;
        for group in &order.groups {
            let shift = sequence % group.len();
            let (before, after) = model.deployments[group.clone()].split_at(shift);
            for &idx in after.iter().chain(before) {
                let idx = index(idx);
                let dep = &self.cfg.deployments[idx];
                let binding = &self.bindings[idx];
                if binding.adapter.supports(req.format) == Support::No {
                    continue;
                }
                supported = true;
                let now = Instant::now();
                if let Some(reset) = binding.limits.cooldown_until(now) {
                    earliest = min_reset(earliest, reset);
                    continue;
                }
                if attempts == self.cfg.routing.max_attempts {
                    continue;
                }
                if now >= deadline {
                    return Err(GatewayError::Client(UpstreamError::Timeout));
                }
                let permit = match binding.limits.admit(now) {
                    Ok(permit) => permit,
                    Err(reset) => {
                        earliest = min_reset(earliest, reset);
                        continue;
                    }
                };

                attempts += 1;
                let attempt_deadline = deadline.min(now + dep.timeout);
                match timeout_at(attempt_deadline, self.attempt(&req, dep, binding)).await {
                    Ok(Ok(response)) => {
                        return Ok(RoutedResponse {
                            deployment: dep.id.clone(),
                            attempts,
                            response: hold_permit(response, permit),
                        })
                    }
                    Ok(Err(GatewayError::Client(UpstreamError::RateLimited {
                        retry_after,
                        scope,
                    }))) => {
                        let wait = retry_after
                            .unwrap_or(self.cfg.cooldown.default)
                            .min(self.cfg.cooldown.max);
                        let reset = binding.limits.cool_down(wait);
                        earliest = min_reset(earliest, reset);
                        if !self
                            .cfg
                            .routing
                            .fallback_on
                            .contains(&ErrorKind::RateLimited)
                        {
                            return Err(GatewayError::Client(UpstreamError::RateLimited {
                                retry_after: Some(wait),
                                scope,
                            }));
                        }
                    }
                    Ok(Err(err)) => return Err(err),
                    Err(_) => return Err(GatewayError::Client(UpstreamError::Timeout)),
                }
            }
        }
        if !supported {
            return Err(GatewayError::Internal);
        }
        Err(GatewayError::AllCandidatesExhausted {
            attempts,
            retry_after: earliest.map_or(Duration::ZERO, |t: Instant| {
                t.saturating_duration_since(Instant::now())
            }),
        })
    }

    async fn attempt(
        &self,
        req: &GatewayRequest,
        dep: &Deployment,
        binding: &Binding,
    ) -> Result<GatewayResponse, GatewayError> {
        let auth = binding.auth.current().map_err(|_| GatewayError::Internal)?;
        let upstream = binding
            .adapter
            .build_request(req, dep, &auth)
            .await
            .map_err(|_| GatewayError::Internal)?;
        let response = self
            .http
            .request(upstream.map(Full::new))
            .await
            .map_err(|_| GatewayError::Client(UpstreamError::Connect))?;
        let response = binding
            .adapter
            .parse_response(req, dep, response)
            .await
            .map_err(GatewayError::Client)?;

        match response {
            GatewayResponse::Complete { .. } if !req.stream => Ok(response),
            GatewayResponse::Stream(mut events) if req.stream => {
                // Commit downstream headers only once a complete upstream event is available.
                let first = events
                    .next()
                    .await
                    .ok_or(GatewayError::Client(UpstreamError::StreamBroken {
                        bytes_sent: 0,
                    }))?
                    .map_err(GatewayError::Client)?;
                let events = stream::unfold(Some(events), {
                    let idle_timeout = self.cfg.server.request_timeout.min(dep.timeout);
                    move |events| async move {
                        let mut events = events?;
                        match timeout(idle_timeout, events.next()).await {
                            Ok(Some(Ok(event))) => Some((Ok(event), Some(events))),
                            Ok(Some(Err(err))) => Some((Err(err), None)),
                            Ok(None) => None,
                            Err(_) => Some((Err(UpstreamError::Timeout), None)),
                        }
                    }
                });
                Ok(GatewayResponse::Stream(Box::pin(
                    stream::once(async { Ok(first) }).chain(events),
                )))
            }
            _ => Err(GatewayError::Client(UpstreamError::Server {
                status: StatusCode::BAD_GATEWAY,
            })),
        }
    }
}

fn hold_permit(response: GatewayResponse, permit: Permit) -> GatewayResponse {
    match response {
        GatewayResponse::Stream(events) => {
            GatewayResponse::Stream(Box::pin(events.map(move |event| {
                let _ = &permit;
                event
            })))
        }
        complete => complete,
    }
}

fn min_reset(previous: Option<Instant>, reset: Instant) -> Option<Instant> {
    Some(previous.map_or(reset, |earliest| earliest.min(reset)))
}

fn index(idx: DeploymentIdx) -> usize {
    idx.0 as usize
}
