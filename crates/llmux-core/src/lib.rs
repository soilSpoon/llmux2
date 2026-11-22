//! Priority routing, deployment limits, and HTTP execution.

mod limits;
mod machine;
mod stats;

#[cfg(test)]
mod tests;

use bytes::Bytes;
use futures_util::{stream, StreamExt};
use http::{Request, StatusCode};
use http_body_util::Full;
use hyper_util::{
    client::legacy::Client,
    rt::{TokioExecutor, TokioTimer},
};
use limits::{DeploymentLimits, Permit, Window};
use llmux_config::RuntimeConfig;
use llmux_types::{
    provider, AuthMaterial, BuildError, ContextOverflow, CredentialSource, Deployment,
    DeploymentIdx, ErrorKind, GatewayRequest, GatewayResponse, ProviderAdapter, ProviderRegistry,
    Support, UpstreamError,
};
use machine::{Built, Effect, Event, Outcome, Phase, State};
use stats::{AttemptStats, SkipReason};
pub use stats::{CatchAllStats, DeploymentStats, FallbackStats};
use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, RwLock,
    },
    time::{Duration, SystemTime},
};
use thiserror::Error;

/// A rejected request body may be repaired and resent to the same deployment
/// at most this many times (one per removable field).
const MAX_REPAIRS: usize = 3;
use tokio::time::{timeout_at, Instant};

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

#[derive(Debug)]
pub struct RoutedError {
    pub deployment: Option<Arc<str>>,
    pub attempts: usize,
    pub error: GatewayError,
}

#[derive(Default)]
struct RouteTrace {
    last_attempted: Option<usize>,
    attempts: usize,
}

type HttpClient = llmux_types::HttpClient;

struct Binding {
    adapter: Arc<dyn ProviderAdapter>,
    auth: Option<Arc<dyn CredentialSource>>,
    disabled: Arc<AtomicBool>,
    limits: Arc<DeploymentLimits>,
    /// Other routes on the same credential, cooled together when its quota runs out.
    key_peers: Box<[Arc<DeploymentLimits>]>,
    stats: Option<Arc<AttemptStats>>,
}

impl Binding {
    fn record_skip(&self, reason: SkipReason) {
        if let Some(stats) = &self.stats {
            stats.record_skip(reason);
        }
    }

    fn skip_reason(
        &self,
        support: Support,
        fits: bool,
        expires_at: Option<SystemTime>,
    ) -> Option<SkipReason> {
        if self.disabled.load(Ordering::Relaxed) {
            Some(SkipReason::Disabled)
        } else if support == Support::No {
            Some(SkipReason::UnsupportedFormat)
        } else if expires_at.is_some_and(|end| SystemTime::now() >= end) {
            Some(SkipReason::Expired)
        } else if !fits {
            Some(SkipReason::ContextLength)
        } else {
            None
        }
    }
}

struct ModelOrder {
    next: AtomicUsize,
    groups: Box<[Range<usize>]>,
    /// Smallest deployment context window; requests within it skip per-deployment checks.
    min_context: u32,
}

/// Upstream ids from the last successful Devin CLI catalog. `None` means no catalog has loaded.
#[derive(Debug, Default)]
pub struct LiveModels {
    ids: RwLock<Option<Arc<HashSet<Arc<str>>>>>,
}

impl LiveModels {
    pub fn get(&self) -> Option<Arc<HashSet<Arc<str>>>> {
        self.ids
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }

    pub fn replace(&self, ids: impl IntoIterator<Item = Arc<str>>) {
        *self.ids.write().unwrap_or_else(|err| err.into_inner()) =
            Some(Arc::new(ids.into_iter().collect()));
    }
}

pub struct Executor {
    catch_all_stats: Option<stats::CatchAllCounter>,
    cfg: Arc<RuntimeConfig>,
    bindings: Box<[Binding]>,
    orders: Box<[ModelOrder]>,
    credentials: HashMap<Arc<str>, Arc<AtomicBool>>,
    /// Shared with the server so a refresh is visible without rebuilding the executor.
    devin_catalog: Arc<LiveModels>,
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
        let mut bindings = cfg
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
                    key_peers: Box::default(),
                    stats: cfg.server.dashboard_key.is_some().then(Arc::default),
                })
            })
            .collect::<Result<Box<[_]>, _>>()?;
        let mut by_key = HashMap::<&str, Vec<Arc<DeploymentLimits>>>::new();
        for (dep, binding) in cfg.deployments.iter().zip(bindings.iter()) {
            if !dep.credential.is_empty() {
                let peers = by_key.entry(&dep.credential).or_default();
                if !peers.iter().any(|peer| Arc::ptr_eq(peer, &binding.limits)) {
                    peers.push(binding.limits.clone());
                }
            }
        }
        for (dep, binding) in cfg.deployments.iter().zip(bindings.iter_mut()) {
            if let Some(peers) = by_key.get(&*dep.credential) {
                binding.key_peers = peers
                    .iter()
                    .filter(|peer| !Arc::ptr_eq(peer, &binding.limits))
                    .cloned()
                    .collect();
            }
        }
        // A credential without deployments is still manageable: keep a
        // flag for every configured credential, not only deployed ones.
        for id in credentials.keys() {
            disabled.entry(id.clone()).or_default();
        }

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

        let connector = llmux_types::https_connector();
        let mut builder = Client::builder(TokioExecutor::new());
        builder
            .pool_timer(TokioTimer::new())
            .pool_idle_timeout(Duration::from_secs(30))
            .retry_canceled_requests(false);
        Ok(Self {
            catch_all_stats: cfg.server.dashboard_key.is_some().then(Default::default),
            cfg,
            bindings,
            orders,
            credentials: disabled,
            devin_catalog: Arc::new(LiveModels::default()),
            http: builder.build(connector),
        })
    }

    /// Use the process catalog. A loaded catalog skips a `devin_acp` deployment
    /// whose upstream id is absent, the same way a cooldown does.
    pub fn set_devin_catalog(&mut self, catalog: Arc<LiveModels>) {
        self.devin_catalog = catalog;
    }

    fn devin_upstream_missing(&self, dep: &Deployment) -> bool {
        dep.provider.0.as_ref() == provider::DEVIN_ACP
            && self
                .devin_catalog
                .get()
                .is_some_and(|ids| !ids.contains(dep.upstream_model.as_ref()))
    }

    /// Whether the credential is disabled in routing; `None` for unknown ids.
    pub fn credential_disabled(&self, id: &str) -> Option<bool> {
        self.credentials
            .get(id)
            .map(|flag| flag.load(Ordering::Relaxed))
    }

    /// Enable or disable every deployment drawing on this credential.
    /// Returns false for unknown credential ids.
    pub fn set_credential_disabled(&self, id: &str, disabled: bool) -> bool {
        match self.credentials.get(id) {
            Some(flag) => {
                flag.store(disabled, Ordering::Relaxed);
                true
            }
            None => false,
        }
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

    pub fn catch_all_stats(&self) -> CatchAllStats {
        self.catch_all_stats
            .as_ref()
            .map(|stats| stats.lock().unwrap().clone())
            .unwrap_or_default()
    }

    pub async fn execute(&self, req: GatewayRequest) -> Result<RoutedResponse, GatewayError> {
        self.execute_attributed(req)
            .await
            .map_err(|failure| failure.error)
    }

    pub async fn execute_attributed(
        &self,
        req: GatewayRequest,
    ) -> Result<RoutedResponse, RoutedError> {
        let mut trace = RouteTrace::default();
        self.route(req, &mut trace)
            .await
            .map_err(|error| RoutedError {
                deployment: trace
                    .last_attempted
                    .map(|idx| self.cfg.deployments[idx].id.clone()),
                attempts: trace.attempts,
                error,
            })
    }

    async fn route(
        &self,
        req: GatewayRequest,
        trace: &mut RouteTrace,
    ) -> Result<RoutedResponse, GatewayError> {
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
                    || dep.expires_at.is_some_and(|end| SystemTime::now() >= end)
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
                if self.cfg.server.dashboard_key.is_some() {
                    for &idx in &model.deployments {
                        let binding = &self.bindings[index(idx)];
                        let dep = &self.cfg.deployments[index(idx)];
                        let support = binding.adapter.supports(req.format);
                        if let Some(reason) =
                            binding.skip_reason(support, fits(dep), dep.expires_at)
                        {
                            binding.record_skip(reason);
                        }
                    }
                }
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
        let mut last_transient = None;
        let mut incompatible = Vec::new();
        let mut unavailable = false;
        let mut machine = State::new(self.cfg.routing.max_attempts as u32);
        'groups: for group in &order.groups {
            let shift = sequence % group.len();
            let (before, after) = model.deployments[group.clone()].split_at(shift);
            for &idx in after.iter().chain(before) {
                let idx = index(idx);
                let dep = &self.cfg.deployments[idx];
                let binding = &self.bindings[idx];
                let support = binding.adapter.supports(req.format);
                let now = Instant::now();
                machine.select();
                let mut skip_reason = binding.skip_reason(support, fits(dep), dep.expires_at);
                let mut event = if skip_reason.is_some() {
                    Event::Ineligible
                } else if support != Support::Convert
                    && binding.limits.cooldown_until(now).is_some()
                {
                    skip_reason = Some(SkipReason::Cooldown);
                    Event::Cooling
                } else if support != Support::Convert && self.devin_upstream_missing(dep) {
                    skip_reason = Some(SkipReason::UpstreamUnavailable);
                    Event::Cooling
                } else if now >= deadline {
                    Event::DeadlinePassed
                } else {
                    Event::Eligible
                };
                let mut permit = None;
                let mut upstream = None;
                let mut attempt_deadline = deadline;
                let mut result = Err(GatewayError::Internal);
                loop {
                    match machine.step(event) {
                        Effect::StopGroups => break 'groups,
                        Effect::Skip {
                            unavailable: cooling,
                        } => {
                            if let Some(reason) = skip_reason {
                                binding.record_skip(reason);
                            }
                            if cooling {
                                unavailable = true;
                                last_transient = None;
                            }
                            break;
                        }
                        Effect::FailTimeout => {
                            return Err(GatewayError::Client(UpstreamError::Timeout));
                        }
                        Effect::Admit if machine.phase == Phase::Admit => {
                            permit = if support == Support::Convert {
                                None
                            } else {
                                binding.limits.admit(needed)
                            };
                            if support != Support::Convert && permit.is_none() {
                                skip_reason = Some(SkipReason::AdmissionLimited);
                            }
                            event = Event::Admit {
                                permit: support == Support::Convert || permit.is_some(),
                            };
                        }
                        Effect::Build => {
                            attempt_deadline = deadline.min(Instant::now() + dep.timeout);
                            let built = timeout_at(
                                attempt_deadline,
                                Self::build_request(&req, dep, binding),
                            )
                            .await;
                            event = Event::Built(
                                if matches!(built, Ok(Err(BuildError::UnsupportedConversion))) {
                                    Built::Unsupported
                                } else {
                                    Built::Other
                                },
                            );
                            upstream = Some(built);
                        }
                        Effect::Incompatible => {
                            binding.record_skip(SkipReason::UnsupportedConversion);
                            incompatible.push(idx);
                            break;
                        }
                        Effect::Admit => {
                            permit = permit.take().or_else(|| {
                                if binding.limits.cooldown_until(Instant::now()).is_some() {
                                    skip_reason = Some(SkipReason::Cooldown);
                                    None
                                } else {
                                    let permit = binding.limits.admit(needed);
                                    if permit.is_none() {
                                        skip_reason = Some(SkipReason::AdmissionLimited);
                                    }
                                    permit
                                }
                            });
                            event = Event::Gate {
                                permit: permit.is_some(),
                            };
                        }
                        Effect::Send => {
                            let upstream = upstream.take().expect("built before send");
                            if matches!(
                                upstream,
                                Ok(Err(BuildError::InvalidBody | BuildError::UnsupportedFormat))
                            ) {
                                return Err(GatewayError::InvalidRequest);
                            }
                            trace.attempts = machine.attempts as _;
                            trace.last_attempted = Some(idx);
                            let timed_out = Err(GatewayError::Client(UpstreamError::Timeout));
                            result = match upstream {
                                Ok(Ok(upstream)) => timeout_at(
                                    attempt_deadline,
                                    self.attempt(&req, dep, binding, upstream),
                                )
                                .await
                                .unwrap_or(timed_out),
                                Ok(Err(_)) => Err(GatewayError::Internal),
                                Err(_) => timed_out,
                            };
                            if let Some(stats) = &binding.stats {
                                stats.record_attempt(result.as_ref().err());
                            }
                            last_transient = None;
                            event = Event::Result(match &result {
                                Ok(_) => Outcome::Success,
                                Err(GatewayError::Client(UpstreamError::Auth { refreshable })) => {
                                    // Expired deployments never retry.
                                    Outcome::Refreshable {
                                        can_refresh: binding.auth.is_some()
                                            && *refreshable
                                            && dep
                                                .expires_at
                                                .is_none_or(|end| SystemTime::now() < end),
                                    }
                                }
                                Err(GatewayError::Client(UpstreamError::RateLimited {
                                    ..
                                })) => Outcome::Limited {
                                    fallback: self
                                        .cfg
                                        .routing
                                        .fallback_on
                                        .contains(&ErrorKind::RateLimited),
                                },
                                Err(GatewayError::Client(UpstreamError::QuotaExhausted {
                                    ..
                                })) => Outcome::QuotaExhausted {
                                    fallback: self
                                        .cfg
                                        .routing
                                        .fallback_on
                                        .contains(&ErrorKind::QuotaExhausted),
                                },
                                Err(GatewayError::Client(UpstreamError::ModelNotFound {
                                    ..
                                })) => Outcome::ModelNotFound,
                                Err(GatewayError::Client(
                                    UpstreamError::Server { .. }
                                    | UpstreamError::Timeout
                                    | UpstreamError::Connect
                                    | UpstreamError::StreamBroken { .. },
                                )) => Outcome::Transient,
                                Err(_) => Outcome::Fatal,
                            });
                        }
                        Effect::Respond => {
                            if self.cfg.catch_all == Some(model_idx) {
                                if let Some(stats) = &self.catch_all_stats {
                                    stats.lock().unwrap().record(&req.model);
                                }
                            }
                            let Ok(response) = result else {
                                unreachable!("respond follows success")
                            };
                            if matches!(response, GatewayResponse::Complete { .. }) {
                                binding.limits.succeeded();
                            }
                            return Ok(RoutedResponse {
                                deployment: dep.id.clone(),
                                attempts: machine.attempts as _,
                                response: match permit.take() {
                                    Some(permit) => hold_permit(response, permit),
                                    None => response,
                                },
                            });
                        }
                        Effect::Abort => {
                            return result.map(|_| unreachable!("abort follows an error"))
                        }
                        Effect::Refresh => {
                            drop(permit.take());
                            let refreshed = match binding.auth.as_ref() {
                                Some(source) => matches!(
                                    timeout_at(deadline, source.refresh()).await,
                                    Ok(Ok(()))
                                ),
                                None => false,
                            };
                            event = Event::Refreshed(refreshed);
                        }
                        Effect::Apply {
                            outcome,
                            stop_groups,
                        } => {
                            drop(permit.take());
                            match (outcome, result) {
                                (Outcome::AuthFatal, _) => {
                                    // An expired deployment must not poison the
                                    // credential's shared disabled flag.
                                    if dep.expires_at.is_none_or(|end| SystemTime::now() < end) {
                                        binding.disabled.store(true, Ordering::Relaxed);
                                    }
                                }
                                (
                                    _,
                                    Err(GatewayError::Client(UpstreamError::RateLimited {
                                        retry_after,
                                        ..
                                    })),
                                ) => {
                                    binding.limits.limited(
                                        retry_after,
                                        self.cfg.cooldown.default,
                                        self.cfg.cooldown.max,
                                    );
                                }
                                (
                                    _,
                                    Err(GatewayError::Client(UpstreamError::QuotaExhausted {
                                        reset_at,
                                    })),
                                ) => {
                                    let hint = reset_at.map(|reset| {
                                        reset.duration_since(SystemTime::now()).unwrap_or_default()
                                    });
                                    let until = binding.limits.limited(
                                        hint,
                                        self.cfg.cooldown.default,
                                        self.cfg.cooldown.max,
                                    );
                                    for peer in &binding.key_peers {
                                        match hint {
                                            Some(wait) => {
                                                peer.cool_down(wait.min(self.cfg.cooldown.max));
                                            }
                                            None => peer.cool_down_until(until),
                                        }
                                    }
                                }
                                (Outcome::ModelNotFound, Err(GatewayError::Client(err))) => {
                                    incompatible.push(idx);
                                    last_transient = Some(err);
                                }
                                (_, Err(GatewayError::Client(err))) => last_transient = Some(err),
                                _ => {}
                            }
                            if stop_groups {
                                break 'groups;
                            }
                            break;
                        }
                    }
                }
            }
        }
        if let Some(err) = last_transient {
            return Err(GatewayError::Client(err));
        }
        if machine.attempts == 0 && !unavailable && !incompatible.is_empty() {
            return Err(GatewayError::InvalidRequest);
        }
        // The next request can use any compatible deployment, even if retries stopped early.
        let now = Instant::now();
        let earliest = model
            .deployments
            .iter()
            .filter(|idx| !incompatible.contains(&index(**idx)))
            .filter(|idx| {
                let dep = &self.cfg.deployments[index(**idx)];
                fits(dep) && dep.expires_at.is_none_or(|end| SystemTime::now() < end)
            })
            .map(|idx| &self.bindings[index(*idx)])
            .filter(|binding| {
                !binding.disabled.load(Ordering::Relaxed)
                    && binding.adapter.supports(req.format) != Support::No
            })
            .map(|binding| binding.limits.available_at(now, needed))
            .min()
            .unwrap_or(now);
        Err(GatewayError::AllCandidatesExhausted {
            attempts: machine.attempts as _,
            retry_after: earliest.saturating_duration_since(Instant::now()),
        })
    }

    async fn build_request(
        req: &GatewayRequest,
        dep: &Deployment,
        binding: &Binding,
    ) -> Result<Request<Bytes>, BuildError> {
        let auth = binding
            .auth
            .as_ref()
            .map_or(Ok(AuthMaterial::None), |source| source.current())
            .map_err(|_| BuildError::InvalidAuth)?;
        binding.adapter.build_request(req, dep, &auth).await
    }

    async fn attempt(
        &self,
        req: &GatewayRequest,
        dep: &Deployment,
        binding: &Binding,
        upstream: Request<Bytes>,
    ) -> Result<GatewayResponse, GatewayError> {
        let response = match binding.adapter.invoke(req, dep).await {
            Some(local) => local.map_err(GatewayError::Client)?,
            None => {
                let mut upstream = upstream;
                let mut repairs = 0;
                loop {
                    let (parts, body) = upstream.into_parts();
                    let response = self
                        .http
                        .request(Request::from_parts(parts.clone(), Full::new(body.clone())))
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
                    match binding.adapter.parse_response(req, dep, response).await {
                        Err(error) if repairs < MAX_REPAIRS => {
                            let Some(repaired) = binding.adapter.repair_rejected(&body, &error)
                            else {
                                return Err(GatewayError::Client(error));
                            };
                            upstream = Request::from_parts(parts, repaired);
                            repairs += 1;
                        }
                        parsed => break parsed.map_err(GatewayError::Client)?,
                    }
                }
            }
        };

        match response {
            GatewayResponse::Complete { .. } if !req.stream => Ok(response),
            GatewayResponse::Stream(mut events) if req.stream => {
                // Commit on a real chunk or [DONE], not on an OpenAI-compat
                // keepalive. Drop those before commit so a later error frame
                // fails the attempt. The caller's attempt deadline (request
                // timeout intersected with the deployment timeout) bounds the wait.
                let first = loop {
                    match events.next().await {
                        Some(Ok(event)) if !event.commits() => continue,
                        Some(Ok(event)) => break event,
                        Some(Err(err)) => return Err(GatewayError::Client(err)),
                        None => {
                            return Err(GatewayError::Client(UpstreamError::StreamBroken {
                                bytes_sent: 0,
                            }))
                        }
                    }
                };
                let idle_timeout = self.cfg.server.request_timeout.min(dep.timeout);
                let cooldown = (self.cfg.cooldown.default, self.cfg.cooldown.max);
                let events = stream::unfold(
                    Some((
                        events,
                        binding.stats.clone(),
                        binding.limits.clone(),
                        Box::pin(tokio::time::sleep(idle_timeout)),
                    )),
                    move |state| async move {
                        let (mut events, stats, limits, mut idle) = state?;
                        // Measure idle from each poll, not from the last event: a
                        // downstream pause must not eat into the upstream idle window.
                        idle.as_mut().reset(Instant::now() + idle_timeout);
                        let error = tokio::select! {
                            next = events.next() => match next {
                                Some(Ok(event)) => {
                                    return Some((
                                        Ok(event),
                                        Some((events, stats, limits, idle)),
                                    ));
                                }
                                Some(Err(err)) => err,
                                None => {
                                    limits.succeeded();
                                    return None;
                                }
                            },
                            () = &mut idle => UpstreamError::Timeout,
                        };
                        if let Some(stats) = &stats {
                            stats.record_stream_error(error.kind());
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
