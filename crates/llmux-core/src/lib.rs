//! Priority routing, deployment limits, and HTTP execution.

mod limits;
mod machine;
mod request_log;
mod stats;

#[cfg(test)]
mod tests;

use bytes::Bytes;
use futures_util::{stream, StreamExt};
use http::{HeaderMap, Request, StatusCode};
use http_body_util::Full;
use hyper_util::{
    client::legacy::Client,
    rt::{TokioExecutor, TokioTimer},
};
use limits::{DeploymentLimits, Permit, Window};
use llmux_config::RuntimeConfig;
use llmux_types::{
    provider, AuthMaterial, Billing, BuildError, ContextOverflow, CredentialSource, Deployment,
    DeploymentIdx, ErrorKind, GatewayRequest, GatewayResponse, InboundFormat, ProviderAdapter,
    ProviderRegistry, QuotaSpec, SseCommitment, Support, UpstreamError,
};
use machine::{Built, Effect, Event, Outcome, Phase, State};
pub use request_log::RequestLog;
use request_log::RequestRecord;
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
    #[error("cannot open request log: {0}")]
    RequestLog(std::io::Error),
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
        retry_after: Option<Duration>,
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
    /// Provider-side request id from the last attempt, for flag tracing.
    upstream_request_id: Option<Arc<str>>,
    /// Every upstream send, in send order — failover hides the losing legs
    /// from `last_attempted`, so they are kept here for the request record.
    legs: Vec<request_log::AttemptLeg>,
}

type HttpClient = llmux_types::HttpClient;

struct Binding {
    adapter: Arc<dyn ProviderAdapter>,
    auth: Option<Arc<dyn CredentialSource>>,
    disabled: Arc<AtomicBool>,
    limits: Arc<DeploymentLimits>,
    /// Other routes on the same credential, cooled together when its quota runs out.
    key_peers: Arc<[Arc<DeploymentLimits>]>,
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
        } else if self.limits.not_provisioned() {
            Some(SkipReason::NotProvisioned)
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
    request_log: Option<RequestLog>,
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
        shared_caps: &mut Vec<(&'a Deployment, Vec<Arc<Window>>)>,
        local_windows: &mut Vec<(&'a Deployment, Vec<Arc<Window>>)>,
        group_specs: &HashMap<Arc<str>, QuotaSpec>,
        key_windows: &HashMap<Arc<str>, Vec<Arc<Window>>>,
        dep: &'a Deployment,
    ) -> Arc<DeploymentLimits> {
        let same_route = |other: &&Deployment| {
            other.provider == dep.provider
                && other.credential == dep.credential
                && other.base_url == dep.base_url
                && other.upstream_model == dep.upstream_model
                && other.quota == dep.quota
                && other.shared_quota == dep.shared_quota
        };
        if let Some((_, limits)) = shared.iter().find(|(other, _)| same_route(other)) {
            return limits.clone();
        }
        let key = key_windows
            .get(&dep.credential)
            .map_or(&[][..], Vec::as_slice);
        // Route-local quota budgets the route, not each binding variant:
        // siblings differing only in `shared_quota` draw on one local window
        // set — separate windows would double the route's allowance. The
        // family windows below stay per-binding by contrast.
        let same_local_route = |other: &&Deployment| {
            other.provider == dep.provider
                && other.credential == dep.credential
                && other.base_url == dep.base_url
                && other.upstream_model == dep.upstream_model
                && other.quota == dep.quota
        };
        let local = match local_windows
            .iter()
            .find(|(other, _)| same_local_route(other))
        {
            Some((_, windows)) => windows.clone(),
            None => {
                let windows = Window::for_quota(dep.quota.clone());
                local_windows.push((dep, windows.clone()));
                windows
            }
        };
        // A named shared quota is one account-wide window set: legs that bind
        // to the same group draw on it even when they serve different upstream
        // models (a provider's documented free allowance is per account, not
        // per model). Scope is the credential — or, for anonymous legs, the
        // provider plus base URL, approximating a per-IP allowance. Only the
        // windows are shared: cooldowns, strikes and inflight stay
        // per-deployment.
        let shared_group = dep.shared_quota.as_ref().map(|group| {
            let same_scope = |other: &&Deployment| {
                other.shared_quota.as_ref() == Some(group)
                    && if dep.credential.is_empty() {
                        other.credential.is_empty()
                            && other.provider == dep.provider
                            && other.base_url == dep.base_url
                    } else {
                        other.credential == dep.credential
                    }
            };
            match shared_caps.iter().find(|(other, _)| same_scope(other)) {
                Some((_, windows)) => windows.clone(),
                None => {
                    // Parse rejects dangling `shared_quota` names; a hand-built
                    // RuntimeConfig must still fail closed, never unlimited.
                    // Family caps are account-wide ceilings, so they use the
                    // hard credential variant — soft windows would admit one
                    // oversized request to an idle scope and blow the cap.
                    let windows = match group_specs.get(group) {
                        Some(spec) => Window::for_credential(spec.clone()),
                        None => {
                            debug_assert!(
                                false,
                                "shared_quota group `{group}` missing from shared_quotas"
                            );
                            Window::for_credential(llmux_types::QuotaSpec {
                                rpm: Some(0),
                                rpd: Some(0),
                                tpm: Some(0),
                                tpd: Some(0),
                                concurrent: None,
                                fixed: vec![],
                            })
                        }
                    };
                    shared_caps.push((dep, windows.clone()));
                    windows
                }
            }
        });
        let mut windows = local;
        windows.extend(key.iter().cloned());
        if let Some(family) = shared_group {
            windows.extend(family);
        }
        // `with_shared` would rebuild private local windows from `dep.quota`;
        // pass only `concurrent` so the deduped route-local set is used.
        let limits = Arc::new(DeploymentLimits::with_shared(
            QuotaSpec {
                concurrent: dep.quota.concurrent,
                ..Default::default()
            },
            &windows,
        ));
        shared.push((dep, limits.clone()));
        limits
    }

    /// `request_log` must already be open: the server opens it before starting
    /// credential sources so a bad path fails the build before they can rotate
    /// refresh tokens.
    pub fn new(
        cfg: Arc<RuntimeConfig>,
        registry: &ProviderRegistry,
        credentials: &HashMap<Arc<str>, Arc<dyn CredentialSource>>,
        request_log: Option<RequestLog>,
    ) -> Result<Self, InitError> {
        Self::validate(&cfg, registry, &credentials.keys().cloned().collect())?;
        let mut disabled = HashMap::<Arc<str>, Arc<AtomicBool>>::new();
        // Deployments that reach the same upstream route draw on one provider limit.
        let mut shared_limits = Vec::<(&Deployment, Arc<DeploymentLimits>)>::new();
        // Legs bound to one named shared quota draw on one set of windows.
        let mut shared_caps = Vec::<(&Deployment, Vec<Arc<Window>>)>::new();
        // Route-local quota windows are shared across bindings on one route.
        let mut local_windows = Vec::<(&Deployment, Vec<Arc<Window>>)>::new();
        // A credential's quota is one set of windows shared by all of its deployments.
        let key_windows: HashMap<Arc<str>, Vec<Arc<Window>>> = cfg
            .credential_quotas
            .iter()
            .map(|(id, quota)| (id.clone(), Window::for_credential(quota.clone())))
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
                    |id| {
                        disabled
                            .entry(id.clone())
                            .or_insert_with(|| {
                                Arc::new(AtomicBool::new(cfg.credential_disabled.contains(id)))
                            })
                            .clone()
                    },
                );
                Ok(Binding {
                    adapter,
                    auth,
                    disabled,
                    limits: Self::shared_limits(
                        &mut shared_limits,
                        &mut shared_caps,
                        &mut local_windows,
                        &cfg.shared_quotas,
                        &key_windows,
                        dep,
                    ),
                    key_peers: Arc::default(),
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
            disabled
                .entry(id.clone())
                .or_insert_with(|| Arc::new(AtomicBool::new(cfg.credential_disabled.contains(id))));
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
            request_log,
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

    /// Whether a deployment can admit a request costing `needed` in `format`
    /// right now, applying the same disabled, parked, expired,
    /// stale-upstream, cooldown, quota, concurrency, and format gates routing
    /// applies before `Send`. The probe is read-only: it charges no quota and
    /// takes no concurrency permit.
    pub fn deployment_available(
        &self,
        idx: DeploymentIdx,
        format: InboundFormat,
        needed: u64,
    ) -> bool {
        let idx = index(idx);
        let dep = &self.cfg.deployments[idx];
        let binding = &self.bindings[idx];
        if binding.disabled.load(Ordering::Relaxed)
            || binding.limits.not_provisioned()
            || binding.adapter.supports(format) == Support::No
            || dep.expires_at.is_some_and(|end| SystemTime::now() >= end)
            || self.devin_upstream_missing(dep)
        {
            return false;
        }
        let now = Instant::now();
        binding.limits.available_at(now, needed) == now
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
                            stats::LiveState {
                                disabled: binding.disabled.load(Ordering::Relaxed),
                                not_provisioned: binding.limits.not_provisioned(),
                                cooldown_ms: binding
                                    .limits
                                    .cooldown_until(now)
                                    .map(|end| (end.duration_since(now).as_millis() as u64).max(1)),
                                cooldown_scope: binding.limits.cooldown_scope(now),
                                in_flight: binding.limits.inflight(),
                            },
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
        let started = Instant::now();
        let request_id = req.request_id.0.clone();
        let model = req.model.clone();
        let client_key = req.client_key.clone();
        let image_count = req.image_count;
        let result = self.route(req, &mut trace).await;
        // A middle-out context rejection is a probe: the server truncates and
        // retries under the same request ID, so logging it would double-count
        // one client request.
        let preliminary = matches!(
            result,
            Err(GatewayError::ContextLengthExceeded {
                middle_out: true,
                ..
            })
        );
        if let Some(log) = self.request_log.as_ref().filter(|_| !preliminary) {
            let dep = trace.last_attempted.map(|idx| &self.cfg.deployments[idx]);
            log.record(&RequestRecord {
                ts_ms: request_log::now_ms(),
                request_id: &request_id,
                model: &model,
                deployment: dep.map(|dep| dep.id.as_ref()),
                provider: dep.map(|dep| dep.provider.0.as_ref()),
                client_key: client_key.as_deref(),
                image_count,
                upstream_request_id: trace.upstream_request_id.as_deref(),
                attempts: trace.attempts,
                legs: trace.legs,
                latency_ms: started.elapsed().as_millis() as u64,
                outcome: if result.is_ok() { "routed" } else { "error" },
                error_kind: result.as_ref().err().map(request_log::error_kind),
                quota_remaining: trace
                    .last_attempted
                    .map(|idx| self.bindings[idx].limits.remaining(Instant::now())),
            });
        }
        result.map_err(|error| RoutedError {
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
                    || binding.limits.not_provisioned()
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
        // A leg whose quota dies at a deadline promotes itself until then:
        // `priority_boost_until` reprioritizes per request, so regrouping is
        // recomputed only for models that carry one.
        let boosted = model.deployments.iter().any(|idx| {
            self.cfg.deployments[index(*idx)]
                .priority_boost_until
                .is_some()
        });
        let ordered;
        let (deployments, groups): (&[DeploymentIdx], &[Range<usize>]) = if boosted {
            let now = SystemTime::now();
            let mut legs: Vec<DeploymentIdx> = model.deployments.to_vec();
            legs.sort_by_key(|idx| effective_priority(&self.cfg.deployments[index(*idx)], now));
            let mut start = 0;
            let groups: Vec<Range<usize>> = legs
                .chunk_by(|a, b| {
                    effective_priority(&self.cfg.deployments[index(*a)], now)
                        == effective_priority(&self.cfg.deployments[index(*b)], now)
                })
                .map(|group| {
                    let end = start + group.len();
                    let range = start..end;
                    start = end;
                    range
                })
                .collect();
            ordered = (legs, groups);
            (&ordered.0[..], &ordered.1[..])
        } else {
            (&model.deployments[..], &order.groups[..])
        };
        let sequence = if groups.iter().any(|group| group.len() > 1) {
            order.next.fetch_add(1, Ordering::Relaxed)
        } else {
            0
        };
        let deadline = Instant::now() + self.cfg.server.request_timeout;
        let mut last_transient = None;
        let mut incompatible = Vec::new();
        let mut unavailable = false;
        let mut machine = State::new(self.cfg.routing.max_attempts as u32);
        'groups: for group in groups {
            let shift = sequence % group.len();
            let (before, after) = deployments[group.clone()].split_at(shift);
            for &idx in after.iter().chain(before) {
                let idx = index(idx);
                let dep = &self.cfg.deployments[idx];
                // A free-only probe never routes to a billed leg, so a sweep
                // cannot spend on paid or quota-capped legs — including billed
                // fallback legs a dying free leg would otherwise fall through
                // to. Skipping is not dead evidence: only an upstream's own
                // model-not-found counts, so exhaustion stays a retryable 429.
                if req.free_only && dep.billing == Billing::Billed {
                    continue;
                }
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
                                skip_reason = Some(deny_reason(&binding.limits));
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
                            if binding.limits.cooldown_until(Instant::now()).is_some() {
                                // A cooldown that landed during the build
                                // recalls even a permit granted before it, so
                                // a cooling deployment is never sent traffic.
                                permit = None;
                                skip_reason = Some(SkipReason::Cooldown);
                            } else if permit.is_none() {
                                // Convert deployments admit here, after a
                                // successful conversion build.
                                permit = binding.limits.admit(needed);
                                if permit.is_none() {
                                    skip_reason = Some(deny_reason(&binding.limits));
                                }
                            }
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
                            // A retry on the same deployment after a refresh
                            // is not a failover; moving to another one is.
                            if let Some(prev) = trace.last_attempted.filter(|prev| *prev != idx) {
                                if let Some(stats) = &self.bindings[prev].stats {
                                    stats.record_handoff();
                                }
                            }
                            trace.last_attempted = Some(idx);
                            let timed_out = Err(GatewayError::Client(UpstreamError::Timeout));
                            // A failed or timed-out build sent nothing upstream,
                            // so it leaves no leg.
                            let sent = matches!(upstream, Ok(Ok(_)));
                            result = match upstream {
                                Ok(Ok(upstream)) => timeout_at(
                                    attempt_deadline,
                                    self.attempt(&req, dep, binding, upstream, needed, trace),
                                )
                                .await
                                .unwrap_or(timed_out),
                                Ok(Err(_)) => Err(GatewayError::Internal),
                                Err(_) => timed_out,
                            };
                            if let Some(stats) = &binding.stats {
                                stats.record_attempt(result.as_ref().err());
                            }
                            if sent {
                                trace.legs.push(request_log::AttemptLeg {
                                    deployment: dep.id.as_ref().into(),
                                    provider: dep.provider.0.as_ref().into(),
                                    outcome: result
                                        .as_ref()
                                        .map_or_else(request_log::error_kind, |_| "routed"),
                                });
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
                                Err(GatewayError::Client(UpstreamError::NotProvisioned)) => {
                                    Outcome::NotProvisioned
                                }
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
                                    cool_credential(
                                        &binding.limits,
                                        &binding.key_peers,
                                        reset_at,
                                        self.cfg.cooldown.default,
                                        self.cfg.cooldown.max,
                                        self.cfg.cooldown.quota_max,
                                    );
                                }
                                (Outcome::NotProvisioned, Err(GatewayError::Client(err))) => {
                                    // The credential itself works: only this
                                    // leg is parked, until the config reloads.
                                    binding.limits.mark_not_provisioned();
                                    incompatible.push(idx);
                                    last_transient = Some(err);
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
                // Billed legs are never eligible under free_only, so they
                // must not drive the retry delay either.
                !(req.free_only && dep.billing == Billing::Billed)
                    && fits(dep)
                    && dep.expires_at.is_none_or(|end| SystemTime::now() < end)
                    && !self.devin_upstream_missing(dep)
            })
            .map(|idx| &self.bindings[index(*idx)])
            .filter(|binding| {
                !binding.disabled.load(Ordering::Relaxed)
                    && !binding.limits.not_provisioned()
                    && binding.adapter.supports(req.format) != Support::No
            })
            .map(|binding| binding.limits.available_at(now, needed))
            .min();
        Err(GatewayError::AllCandidatesExhausted {
            attempts: machine.attempts as _,
            retry_after: earliest.map(|ready| ready.saturating_duration_since(Instant::now())),
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

    /// A provider id found only in an error body still gets recorded, so a
    /// flag matched by `request_id` works for rejected requests too.
    fn record_error_request_id(
        adapter: &dyn ProviderAdapter,
        error: &UpstreamError,
        trace: &mut RouteTrace,
    ) {
        let body = match error {
            UpstreamError::BadRequest { body, .. } | UpstreamError::ModelNotFound { body, .. } => {
                body
            }
            _ => return,
        };
        // The response header id stays authoritative: a provider can place a
        // different id in an error body, and lookups search the header id.
        if trace.upstream_request_id.is_none() {
            trace.upstream_request_id = adapter.upstream_request_id(&HeaderMap::new(), Some(body));
        }
    }

    async fn attempt(
        &self,
        req: &GatewayRequest,
        dep: &Deployment,
        binding: &Binding,
        upstream: Request<Bytes>,
        needed: u64,
        trace: &mut RouteTrace,
    ) -> Result<GatewayResponse, GatewayError> {
        trace.upstream_request_id = None;
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
                        binding
                            .limits
                            .observe(observed, self.cfg.cooldown.quota_max);
                    }
                    trace.upstream_request_id = binding
                        .adapter
                        .upstream_request_id(response.headers(), None);
                    match binding.adapter.parse_response(req, dep, response).await {
                        Err(error) => {
                            Self::record_error_request_id(&*binding.adapter, &error, trace);
                            if repairs >= MAX_REPAIRS {
                                return Err(GatewayError::Client(error));
                            }
                            let Some(repaired) = binding.adapter.repair_rejected(&body, &error)
                            else {
                                return Err(GatewayError::Client(error));
                            };
                            if !binding.limits.admit_repair(needed) {
                                return Err(GatewayError::Client(error));
                            }
                            // The rejected send already reached upstream; the
                            // resend's own leg is recorded when it finishes.
                            trace.legs.push(request_log::AttemptLeg {
                                deployment: dep.id.as_ref().into(),
                                provider: dep.provider.0.as_ref().into(),
                                outcome: request_log::upstream_error_kind(&error),
                            });
                            upstream = Request::from_parts(parts, repaired);
                            repairs += 1;
                        }
                        parsed => break parsed.map_err(GatewayError::Client)?,
                    }
                }
            }
        };

        let response = match response {
            GatewayResponse::Complete { .. } if !req.stream => {
                if let GatewayResponse::Complete { headers, body, .. } = &response {
                    if let Some(id) = binding.adapter.upstream_request_id(headers, Some(body)) {
                        trace.upstream_request_id = Some(id);
                    }
                }
                return Ok(response);
            }
            GatewayResponse::Stream(mut events) if req.stream => {
                let mut preamble = None;
                let first = loop {
                    match events.next().await {
                        Some(Ok(event)) => match event.commitment() {
                            SseCommitment::Keepalive => {}
                            SseCommitment::RolePreamble => {
                                preamble.get_or_insert(event);
                            }
                            SseCommitment::Commit => break event,
                        },
                        Some(Err(err)) => return Err(GatewayError::Client(err)),
                        None => {
                            return Err(GatewayError::Client(UpstreamError::StreamBroken {
                                bytes_sent: 0,
                            }))
                        }
                    }
                };
                let idle_timeout = self.cfg.server.request_timeout.min(dep.timeout);
                let cooldown = (
                    self.cfg.cooldown.default,
                    self.cfg.cooldown.max,
                    self.cfg.cooldown.quota_max,
                );
                let events = stream::unfold(
                    Some((
                        events,
                        binding.stats.clone(),
                        binding.limits.clone(),
                        binding.key_peers.clone(),
                        Box::pin(tokio::time::sleep(idle_timeout)),
                    )),
                    move |state| async move {
                        let (mut events, stats, limits, key_peers, mut idle) = state?;
                        // Measure idle from each poll, not from the last event: a
                        // downstream pause must not eat into the upstream idle window.
                        idle.as_mut().reset(Instant::now() + idle_timeout);
                        let error = tokio::select! {
                            next = events.next() => match next {
                                Some(Ok(event)) => {
                                    return Some((
                                        Ok(event),
                                        Some((events, stats, limits, key_peers, idle)),
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
                        match &error {
                            UpstreamError::RateLimited { retry_after, .. } => {
                                limits.limited(*retry_after, cooldown.0, cooldown.1);
                            }
                            UpstreamError::QuotaExhausted { reset_at } => {
                                cool_credential(
                                    &limits, &key_peers, *reset_at, cooldown.0, cooldown.1,
                                    cooldown.2,
                                );
                            }
                            UpstreamError::NotProvisioned => {
                                limits.mark_not_provisioned();
                            }
                            _ => {}
                        }
                        Some((Err(error), None))
                    },
                );
                Ok(GatewayResponse::Stream(Box::pin(
                    stream::iter(preamble.into_iter().chain([first]).map(Ok)).chain(events),
                )))
            }
            _ => Err(GatewayError::Client(UpstreamError::Server {
                status: StatusCode::BAD_GATEWAY,
            })),
        };
        response
    }
}

/// A refused admission is a cooldown when the deployment is cooling — even
/// one that landed after the caller's own check — and a quota or concurrency
/// limit otherwise.
fn deny_reason(limits: &DeploymentLimits) -> SkipReason {
    if limits.cooldown_until(Instant::now()).is_some() {
        SkipReason::Cooldown
    } else {
        SkipReason::AdmissionLimited
    }
}

fn cool_credential(
    limits: &DeploymentLimits,
    peers: &[Arc<DeploymentLimits>],
    reset_at: Option<SystemTime>,
    default: Duration,
    max: Duration,
    quota_max: Duration,
) {
    let hint = reset_at.map(|reset| reset.duration_since(SystemTime::now()).unwrap_or_default());
    let until = limits.quota_limited(hint, default, max, quota_max);
    for peer in peers {
        match hint {
            // An explicit reset is authoritative: the peers' cooldowns follow
            // the same quota cap, not the backoff estimate cap.
            Some(wait) => {
                peer.cool_down_credential(wait.min(quota_max));
            }
            None => {
                peer.cool_down_credential(until.saturating_duration_since(Instant::now()));
            }
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

/// Priority a deployment competes with right now: `priority_boost` while its
/// `priority_boost_until` deadline is still ahead, `priority` otherwise.
fn effective_priority(dep: &Deployment, now: SystemTime) -> i32 {
    match dep.priority_boost_until {
        Some(until) if now < until => dep.priority_boost.unwrap_or(0),
        _ => dep.priority,
    }
}
