//! Priority routing, deployment limits, and HTTP execution.

mod limits;
mod stats;

#[cfg(test)]
mod tests;

use bytes::Bytes;
use futures_util::{stream, StreamExt};
use http::StatusCode;
use http_body_util::Full;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::{TokioExecutor, TokioTimer},
};
use limits::{DeploymentLimits, Permit, Window};
use llmux_config::RuntimeConfig;
use llmux_types::{
    AuthMaterial, BuildError, ContextOverflow, CredentialSource, Deployment, DeploymentIdx,
    ErrorKind, GatewayRequest, GatewayResponse, ProviderAdapter, ProviderRegistry, Support,
    UpstreamError,
};
use stats::AttemptStats;
pub use stats::DeploymentStats;
use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, SystemTime},
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
    #[error("invalid or unsupported request body")]
    InvalidRequest,
    #[error("request does not fit any deployment's context window")]
    ContextLengthExceeded {
        limit: u32,
        prompt_tokens: u32,
        max_output_tokens: Option<u32>,
        /// Whether the request or model opted into middle-out truncation.
        middle_out: bool,
    },
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
    auth: Option<Arc<dyn CredentialSource>>,
    disabled: Arc<AtomicBool>,
    limits: Arc<DeploymentLimits>,
    stats: Option<Arc<AttemptStats>>,
}

struct ModelOrder {
    next: AtomicUsize,
    groups: Box<[Range<usize>]>,
    /// Smallest deployment context window; requests within it skip per-deployment checks.
    min_context: u32,
}

pub struct Executor {
    cfg: Arc<RuntimeConfig>,
    bindings: Box<[Binding]>,
    orders: Box<[ModelOrder]>,
    http: HttpClient,
}

impl Executor {
    pub fn validate(
        cfg: &RuntimeConfig,
        registry: &ProviderRegistry,
        credentials: &HashSet<Arc<str>>,
    ) -> Result<(), InitError> {
        for dep in &cfg.deployments {
            if registry.get(&dep.provider).is_none() {
                return Err(InitError::UnknownProvider(dep.provider.0.clone()));
            }
            if !dep.credential.is_empty() && !credentials.contains(&dep.credential) {
                return Err(InitError::MissingCredential);
            }
        }
        Ok(())
    }

    fn shared_limits<'a>(
        shared: &mut Vec<(&'a Deployment, Arc<DeploymentLimits>)>,
        key_windows: &HashMap<Arc<str>, Vec<Arc<Window>>>,
        dep: &'a Deployment,
    ) -> Arc<DeploymentLimits> {
        let same_route = |other: &&Deployment| {
            other.provider == dep.provider
                && other.credential == dep.credential
                && other.base_url == dep.base_url
                && other.upstream_model == dep.upstream_model
                && other.quota == dep.quota
        };
        if let Some((_, limits)) = shared.iter().find(|(other, _)| same_route(other)) {
            return limits.clone();
        }
        let key = key_windows
            .get(&dep.credential)
            .map_or(&[][..], Vec::as_slice);
        let limits = Arc::new(DeploymentLimits::with_shared(dep.quota, key));
        shared.push((dep, limits.clone()));
        limits
    }

    pub fn new(
        cfg: Arc<RuntimeConfig>,
        registry: &ProviderRegistry,
        credentials: &HashMap<Arc<str>, Arc<dyn CredentialSource>>,
    ) -> Result<Self, InitError> {
        Self::validate(&cfg, registry, &credentials.keys().cloned().collect())?;
        let mut disabled = HashMap::<Arc<str>, Arc<AtomicBool>>::new();
        // Deployments that reach the same upstream route draw on one provider limit.
        let mut shared_limits = Vec::<(&Deployment, Arc<DeploymentLimits>)>::new();
        // A credential's quota is one set of windows shared by all of its deployments.
        let key_windows: HashMap<Arc<str>, Vec<Arc<Window>>> = cfg
            .credential_quotas
            .iter()
            .map(|(id, quota)| (id.clone(), Window::for_quota(*quota)))
            .collect();
        let bindings = cfg
            .deployments
            .iter()
            .map(|dep| {
                let adapter = registry
                    .get(&dep.provider)
                    .ok_or_else(|| InitError::UnknownProvider(dep.provider.0.clone()))?
                    .clone();
                let credential = (!dep.credential.is_empty()).then_some(&dep.credential);
                let auth = credential
                    .map(|id| {
                        credentials
                            .get(id)
                            .cloned()
                            .ok_or(InitError::MissingCredential)
                    })
                    .transpose()?;
                let disabled = credential.map_or_else(
                    || Arc::new(AtomicBool::new(false)),
                    |id| disabled.entry(id.clone()).or_default().clone(),
                );
                Ok(Binding {
                    adapter,
                    auth,
                    disabled,
                    limits: Self::shared_limits(&mut shared_limits, &key_windows, dep),
                    stats: cfg.server.dashboard_key.is_some().then(Arc::default),
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
                    min_context: model
                        .deployments
                        .iter()
                        .filter_map(|idx| cfg.deployments[idx.0 as usize].max_context)
                        .min()
                        .unwrap_or(u32::MAX),
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
        builder
            .pool_timer(TokioTimer::new())
            .pool_idle_timeout(Duration::from_secs(30))
            .retry_canceled_requests(false);
        Ok(Self {
            cfg,
            bindings,
            orders,
            http: builder.build(connector),
        })
    }

    pub fn deployment_stats(&self) -> Vec<DeploymentStats<'_>> {
        let now = Instant::now();
        self.cfg
            .models
            .iter()
            .flat_map(|model| {
                model.deployments.iter().filter_map(move |&idx| {
                    let dep = &self.cfg.deployments[index(idx)];
                    let binding = &self.bindings[index(idx)];
                    Some(
                        binding.stats.as_ref()?.snapshot(
                            &model.name,
                            dep,
                            binding.disabled.load(Ordering::Relaxed),
                            binding
                                .limits
                                .cooldown_until(now)
                                .map(|end| (end.duration_since(now).as_millis() as u64).max(1)),
                            binding.limits.inflight(),
                        ),
                    )
                })
            })
            .collect()
    }

    pub async fn execute(&self, req: GatewayRequest) -> Result<RoutedResponse, GatewayError> {
        let model_idx = self
            .cfg
            .model_names
            .get(req.model.as_ref())
            .copied()
            .or_else(|| {
                let catch_all = self.cfg.catch_all?;
                if req.model.as_ref() == "*" {
                    return None;
                }
                let requested = req.model.to_ascii_lowercase();
                let excluded = self.cfg.catch_all_exclude.iter().any(|pattern| {
                    pattern.strip_suffix('*').map_or_else(
                        || requested == *pattern,
                        |prefix| requested.starts_with(prefix),
                    )
                });
                (!excluded).then_some(catch_all)
            })
            .ok_or(GatewayError::UnknownModel)?;
        let model = &self.cfg.models[model_idx];
        if req.stream && !model.capabilities.stream {
            return Err(GatewayError::UnsupportedStream);
        }
        let order = &self.orders[model_idx];
        let needed =
            u64::from(req.est_input_tokens) + u64::from(req.max_output_tokens.unwrap_or(0));
        let check_context = needed > u64::from(order.min_context);
        let fits = |dep: &Deployment| {
            !check_context
                || dep
                    .max_context
                    .is_none_or(|limit| needed <= u64::from(limit))
        };
        if check_context {
            let mut limit = None;
            for &idx in &model.deployments {
                let dep = &self.cfg.deployments[index(idx)];
                let binding = &self.bindings[index(idx)];
                if binding.disabled.load(Ordering::Relaxed)
                    || binding.adapter.supports(req.format) == Support::No
                {
                    continue;
                }
                if fits(dep) {
                    limit = None;
                    break;
                }
                limit = limit.max(dep.max_context);
            }
            if let Some(limit) = limit {
                return Err(GatewayError::ContextLengthExceeded {
                    limit,
                    prompt_tokens: req.est_input_tokens,
                    max_output_tokens: req.max_output_tokens,
                    middle_out: req.context_overflow.unwrap_or(model.context_overflow)
                        == ContextOverflow::MiddleOut,
                });
            }
        }
        let sequence = if order.groups.iter().any(|group| group.len() > 1) {
            order.next.fetch_add(1, Ordering::Relaxed)
        } else {
            0
        };
        let deadline = Instant::now() + self.cfg.server.request_timeout;
        let mut attempts = 0;
        let mut last_transient = None;
        'groups: for group in &order.groups {
            let shift = sequence % group.len();
            let (before, after) = model.deployments[group.clone()].split_at(shift);
            for &idx in after.iter().chain(before) {
                if attempts == self.cfg.routing.max_attempts {
                    break 'groups;
                }
                let idx = index(idx);
                let dep = &self.cfg.deployments[idx];
                let binding = &self.bindings[idx];
                if binding.disabled.load(Ordering::Relaxed)
                    || binding.adapter.supports(req.format) == Support::No
                    || !fits(dep)
                {
                    continue;
                }
                let now = Instant::now();
                if binding.limits.cooldown_until(now).is_some() {
                    last_transient = None;
                    continue;
                }
                if now >= deadline {
                    return Err(GatewayError::Client(UpstreamError::Timeout));
                }
                let mut refreshed = false;
                loop {
                    if attempts == self.cfg.routing.max_attempts {
                        break 'groups;
                    }
                    let Some(permit) = binding.limits.admit(needed) else {
                        last_transient = None;
                        break;
                    };
                    attempts += 1;
                    let attempt_deadline = deadline.min(Instant::now() + dep.timeout);
                    let result = timeout_at(attempt_deadline, self.attempt(&req, dep, binding))
                        .await
                        .unwrap_or(Err(GatewayError::Client(UpstreamError::Timeout)));
                    if let Some(stats) = &binding.stats {
                        stats.record_attempt(result.is_ok());
                    }
                    last_transient = None;
                    match result {
                        Ok(response) => {
                            if matches!(response, GatewayResponse::Complete { .. }) {
                                binding.limits.succeeded();
                            }
                            return Ok(RoutedResponse {
                                deployment: dep.id.clone(),
                                attempts,
                                response: hold_permit(response, permit),
                            });
                        }
                        Err(GatewayError::Client(UpstreamError::Auth { refreshable })) => {
                            if let Some(source) =
                                binding.auth.as_ref().filter(|_| refreshable && !refreshed)
                            {
                                refreshed = true;
                                if matches!(
                                    timeout_at(deadline, source.refresh()).await,
                                    Ok(Ok(()))
                                ) {
                                    continue;
                                }
                            }
                            binding.disabled.store(true, Ordering::Relaxed);
                        }
                        Err(GatewayError::Client(UpstreamError::RateLimited {
                            retry_after,
                            ..
                        })) => {
                            binding.limits.limited(
                                retry_after,
                                self.cfg.cooldown.default,
                                self.cfg.cooldown.max,
                            );
                            if !self
                                .cfg
                                .routing
                                .fallback_on
                                .contains(&ErrorKind::RateLimited)
                            {
                                break 'groups;
                            }
                        }
                        Err(GatewayError::Client(UpstreamError::QuotaExhausted { reset_at })) => {
                            binding.limits.limited(
                                reset_at.map(|reset| {
                                    reset.duration_since(SystemTime::now()).unwrap_or_default()
                                }),
                                self.cfg.cooldown.default,
                                self.cfg.cooldown.max,
                            );
                            if !self
                                .cfg
                                .routing
                                .fallback_on
                                .contains(&ErrorKind::QuotaExhausted)
                            {
                                break 'groups;
                            }
                        }
                        Err(GatewayError::Client(
                            // A stream error here precedes the first downstream byte.
                            err @ (UpstreamError::Server { .. }
                            | UpstreamError::Timeout
                            | UpstreamError::Connect
                            | UpstreamError::StreamBroken { .. }),
                        )) => last_transient = Some(err),
                        Err(err) => return Err(err),
                    }
                    break;
                }
            }
        }
        if let Some(err) = last_transient {
            return Err(GatewayError::Client(err));
        }
        // The next request can use any compatible deployment, even if retries stopped early.
        let now = Instant::now();
        let earliest = model
            .deployments
            .iter()
            .filter(|idx| fits(&self.cfg.deployments[index(**idx)]))
            .map(|idx| &self.bindings[index(*idx)])
            .filter(|binding| {
                !binding.disabled.load(Ordering::Relaxed)
                    && binding.adapter.supports(req.format) != Support::No
            })
            .map(|binding| binding.limits.available_at(now, needed))
            .min()
            .unwrap_or(now);
        Err(GatewayError::AllCandidatesExhausted {
            attempts,
            retry_after: earliest.saturating_duration_since(Instant::now()),
        })
    }

    async fn attempt(
        &self,
        req: &GatewayRequest,
        dep: &Deployment,
        binding: &Binding,
    ) -> Result<GatewayResponse, GatewayError> {
        let auth = binding
            .auth
            .as_ref()
            .map_or(Ok(AuthMaterial::None), |source| source.current())
            .map_err(|_| GatewayError::Internal)?;
        let upstream = binding
            .adapter
            .build_request(req, dep, &auth)
            .await
            .map_err(|err| match err {
                BuildError::InvalidBody | BuildError::UnsupportedFormat => {
                    GatewayError::InvalidRequest
                }
                _ => GatewayError::Internal,
            })?;
        let response = self
            .http
            .request(upstream.map(Full::new))
            .await
            .map_err(|_| GatewayError::Client(UpstreamError::Connect))?;
        if let Some(observed) = response
            .status()
            .is_success()
            .then(|| binding.adapter.observe_limits(response.headers()))
            .flatten()
        {
            binding.limits.observe(observed, self.cfg.cooldown.max);
        }
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
                let idle_timeout = self.cfg.server.request_timeout.min(dep.timeout);
                let cooldown = (self.cfg.cooldown.default, self.cfg.cooldown.max);
                let events = stream::unfold(
                    Some((events, binding.stats.clone(), binding.limits.clone())),
                    move |state| async move {
                        let (mut events, stats, limits) = state?;
                        let error = match timeout(idle_timeout, events.next()).await {
                            Ok(Some(Ok(event))) => {
                                return Some((Ok(event), Some((events, stats, limits))))
                            }
                            Ok(Some(Err(err))) => err,
                            Ok(None) => {
                                limits.succeeded();
                                return None;
                            }
                            Err(_) => UpstreamError::Timeout,
                        };
                        if let Some(stats) = &stats {
                            stats.record_stream_error();
                        }
                        let hint = match &error {
                            UpstreamError::RateLimited { retry_after, .. } => Some(*retry_after),
                            UpstreamError::QuotaExhausted { reset_at } => {
                                Some(reset_at.map(|reset| {
                                    reset.duration_since(SystemTime::now()).unwrap_or_default()
                                }))
                            }
                            _ => None,
                        };
                        if let Some(hint) = hint {
                            limits.limited(hint, cooldown.0, cooldown.1);
                        }
                        Some((Err(error), None))
                    },
                );
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

fn index(idx: DeploymentIdx) -> usize {
    idx.0 as usize
}
