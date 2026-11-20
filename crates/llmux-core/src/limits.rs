use llmux_types::{LimitObservation, QuotaSpec};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU32, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::time::Instant;

const MINUTE: Duration = Duration::from_secs(60);
const DAY: Duration = Duration::from_secs(86_400);
const CONCURRENCY_RETRY: Duration = Duration::from_secs(1);

pub(super) struct DeploymentLimits {
    baseline: Instant,
    cooldown_until_ns: AtomicU64,
    concurrent: Option<u32>,
    inflight: AtomicU32,
    windows: Box<[Arc<Window>]>,
    limit_strikes: AtomicU32,
}

impl DeploymentLimits {
    pub fn inflight(&self) -> Option<u32> {
        self.concurrent
            .map(|_| self.inflight.load(Ordering::Relaxed))
    }

    #[cfg(test)]
    pub fn new(spec: QuotaSpec) -> Self {
        Self::with_shared(spec, &[])
    }

    /// `shared` windows, such as a credential's quota, also bound this deployment.
    pub fn with_shared(spec: QuotaSpec, shared: &[Arc<Window>]) -> Self {
        Self {
            baseline: Instant::now(),
            cooldown_until_ns: AtomicU64::new(0),
            concurrent: spec.concurrent,
            inflight: AtomicU32::new(0),
            windows: Window::for_quota(spec)
                .into_iter()
                .chain(shared.iter().cloned())
                .collect(),
            limit_strikes: AtomicU32::new(0),
        }
    }

    /// Cools down after a 429. Without a provider hint, each consecutive limit doubles the wait;
    /// a limit that lands while the deployment is already cooling does not count as another one.
    pub fn limited(&self, hint: Option<Duration>, default: Duration, max: Duration) -> Instant {
        let strikes = if self.cooldown_until(Instant::now()).is_some() {
            self.limit_strikes.load(Ordering::Relaxed).max(1)
        } else {
            self.limit_strikes
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1)
        };
        let backoff = default.saturating_mul(1u32.checked_shl(strikes - 1).unwrap_or(u32::MAX));
        self.cool_down(hint.unwrap_or(backoff).min(max))
    }

    /// Skips the deployment until reset when the provider reports no requests or tokens left.
    pub fn observe(&self, observed: LimitObservation, max: Duration) {
        for (remaining, reset) in [
            (observed.remaining_requests, observed.reset_requests),
            (observed.remaining_tokens, observed.reset_tokens),
        ] {
            if let (Some(0), Some(reset)) = (remaining, reset) {
                self.cool_down(reset.min(max));
            }
        }
    }

    pub fn succeeded(&self) {
        if self.limit_strikes.load(Ordering::Relaxed) != 0 {
            self.limit_strikes.store(0, Ordering::Relaxed);
        }
    }

    pub fn cooldown_until(&self, now: Instant) -> Option<Instant> {
        let reset =
            self.baseline + Duration::from_nanos(self.cooldown_until_ns.load(Ordering::Relaxed));
        (reset > now).then_some(reset)
    }

    pub fn cool_down(&self, wait: Duration) -> Instant {
        let offset = self.baseline.elapsed() + wait;
        let offset_ns = offset.as_nanos().min(u64::MAX as u128) as u64;
        let previous = self
            .cooldown_until_ns
            .fetch_max(offset_ns, Ordering::Relaxed);
        self.baseline + Duration::from_nanos(offset_ns.max(previous))
    }

    pub fn available_at(&self, now: Instant, tokens: u64) -> Instant {
        let mut ready = self.cooldown_until(now).unwrap_or(now);
        for window in self.windows.iter() {
            let used = window.lock(now);
            let cost = window.unit.cost(tokens);
            if used.total > 0 && used.total + cost > window.max {
                ready = ready.max(used.free_at(window.max.saturating_sub(cost), window.span));
            }
        }
        if self
            .concurrent
            .is_some_and(|max| self.inflight.load(Ordering::Relaxed) >= max)
        {
            ready = ready.max(now + CONCURRENCY_RETRY);
        }
        ready
    }

    pub fn admit(self: &Arc<Self>, tokens: u64) -> Option<Permit> {
        let permit = if let Some(max) = self.concurrent {
            let mut inflight = self.inflight.load(Ordering::Relaxed);
            loop {
                if inflight >= max {
                    return None;
                }
                match self.inflight.compare_exchange_weak(
                    inflight,
                    inflight + 1,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(actual) => inflight = actual,
                }
            }
            Permit(Some(self.clone()))
        } else {
            Permit(None)
        };
        let now = Instant::now();
        let mut windows: Vec<_> = self
            .windows
            .iter()
            .map(|window| (window, window.lock(now)))
            .collect();
        for (window, used) in &windows {
            let cost = window.unit.cost(tokens);
            if used.total > 0 && used.total + cost > window.max {
                return None;
            }
        }
        for (window, used) in &mut windows {
            let cost = window.unit.cost(tokens);
            if cost > 0 {
                used.entries.push_back((now, cost));
                used.total += cost;
            }
        }
        Some(permit)
    }
}

#[derive(Clone, Copy)]
enum Unit {
    Requests,
    Tokens,
}

impl Unit {
    fn cost(self, tokens: u64) -> u64 {
        match self {
            Self::Requests => 1,
            Self::Tokens => tokens,
        }
    }
}

/// Sliding window of admitted requests or estimated tokens.
pub(super) struct Window {
    max: u64,
    span: Duration,
    unit: Unit,
    used: Mutex<Usage>,
}

#[derive(Default)]
struct Usage {
    entries: VecDeque<(Instant, u64)>,
    total: u64,
}

impl Window {
    pub fn for_quota(spec: QuotaSpec) -> Vec<Arc<Self>> {
        [
            (spec.rpm, MINUTE, Unit::Requests),
            (spec.rpd, DAY, Unit::Requests),
            (spec.tpm, MINUTE, Unit::Tokens),
            (spec.tpd, DAY, Unit::Tokens),
        ]
        .into_iter()
        .filter_map(|(max, span, unit)| {
            max.map(|max| {
                Arc::new(Self {
                    max: u64::from(max),
                    span,
                    unit,
                    used: Mutex::default(),
                })
            })
        })
        .collect()
    }

    fn lock(&self, now: Instant) -> std::sync::MutexGuard<'_, Usage> {
        let mut used = self.used.lock().unwrap_or_else(|err| err.into_inner());
        while let Some(&(at, cost)) = used.entries.front() {
            if now.duration_since(at) < self.span {
                break;
            }
            used.entries.pop_front();
            used.total -= cost;
        }
        used
    }
}

impl Usage {
    /// When enough entries expire for the total to fall to `target`.
    fn free_at(&self, target: u64, span: Duration) -> Instant {
        let mut total = self.total;
        for &(at, cost) in &self.entries {
            total -= cost;
            if total <= target {
                return at + span;
            }
        }
        self.entries
            .back()
            .map_or_else(Instant::now, |&(at, _)| at + span)
    }
}

pub(super) struct Permit(Option<Arc<DeploymentLimits>>);

impl Drop for Permit {
    fn drop(&mut self) {
        if let Some(limits) = &self.0 {
            limits.inflight.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_permit_releases_concurrency_on_drop() {
        let limits = Arc::new(DeploymentLimits::new(QuotaSpec {
            concurrent: Some(1),
            ..QuotaSpec::default()
        }));
        let now = Instant::now();
        let permit = limits.admit(0).unwrap();
        assert!(limits.admit(0).is_none());
        assert_eq!(limits.available_at(now, 1), now + CONCURRENCY_RETRY);
        drop(permit);
        assert!(limits.admit(0).is_some());
    }

    #[test]
    fn a_short_late_retry_after_cannot_shorten_an_existing_cooldown() {
        let limits = DeploymentLimits::new(QuotaSpec::default());
        let reset = limits.cool_down(Duration::from_secs(60));
        assert_eq!(limits.cool_down(Duration::from_secs(1)), reset);
        assert_eq!(limits.cooldown_until(Instant::now()), Some(reset));
        assert_eq!(limits.cooldown_until(reset), None);
    }

    #[test]
    fn repeated_unhinted_limits_back_off_exponentially_until_success() {
        let limits = DeploymentLimits::new(QuotaSpec::default());
        let (default, max) = (Duration::from_secs(1), Duration::from_secs(5));
        let wait_with = |limits: &DeploymentLimits, default, max| {
            let start = Instant::now();
            limits.limited(None, default, max).duration_since(start)
        };
        let wait = |limits: &DeploymentLimits, hint| {
            let start = Instant::now();
            limits.limited(hint, default, max).duration_since(start)
        };
        let near = |actual: Duration, expected: Duration| {
            assert!(
                actual.abs_diff(expected) < Duration::from_millis(50),
                "{actual:?} vs {expected:?}"
            );
        };
        near(wait(&limits, None), default);
        // A concurrent 429 that lands during the cooldown does not escalate it.
        near(wait(&limits, None), default);
        limits.cooldown_until_ns.store(0, Ordering::Relaxed);
        near(wait(&limits, None), 2 * default);
        limits.cooldown_until_ns.store(0, Ordering::Relaxed);
        near(wait(&limits, None), 4 * default);
        limits.cooldown_until_ns.store(0, Ordering::Relaxed);
        near(wait(&limits, None), max);
        limits.cooldown_until_ns.store(0, Ordering::Relaxed);
        near(
            wait(&limits, Some(Duration::from_millis(300))),
            Duration::from_millis(300),
        );
        limits.cooldown_until_ns.store(0, Ordering::Relaxed);
        limits.succeeded();
        near(wait(&limits, None), default);
        for _ in 0..40 {
            limits.cooldown_until_ns.store(0, Ordering::Relaxed);
            limits.limited(None, Duration::from_millis(1), Duration::from_secs(3_600));
        }
        limits.cooldown_until_ns.store(0, Ordering::Relaxed);
        near(
            wait_with(
                &limits,
                Duration::from_millis(1),
                Duration::from_secs(3_600),
            ),
            Duration::from_secs(3_600),
        );
    }

    #[test]
    fn token_and_daily_quotas_skip_the_deployment_until_the_window_frees() {
        let tpm = Arc::new(DeploymentLimits::new(QuotaSpec {
            tpm: Some(100),
            ..QuotaSpec::default()
        }));
        assert!(tpm.admit(60).is_some());
        assert!(tpm.admit(60).is_none(), "tpm counts estimated tokens");
        assert!(tpm.admit(40).is_some());

        for spec in [
            QuotaSpec {
                rpd: Some(1),
                ..QuotaSpec::default()
            },
            QuotaSpec {
                tpd: Some(10),
                ..QuotaSpec::default()
            },
        ] {
            let limits = Arc::new(DeploymentLimits::new(spec));
            let now = Instant::now();
            assert!(limits.admit(10).is_some());
            assert!(limits.admit(1).is_none(), "{spec:?} is a daily quota");
            assert!(limits.available_at(now, 1) >= now + Duration::from_secs(86_399));
        }
    }

    #[test]
    fn a_request_larger_than_the_token_quota_still_reaches_an_idle_deployment() {
        let limits = Arc::new(DeploymentLimits::new(QuotaSpec {
            tpm: Some(100),
            ..QuotaSpec::default()
        }));
        assert!(limits.admit(500).is_some());
        assert!(limits.admit(1).is_none());
    }

    #[test]
    fn token_quota_retry_time_accounts_for_the_request_size() {
        let limits = Arc::new(DeploymentLimits::new(QuotaSpec {
            tpm: Some(100),
            ..QuotaSpec::default()
        }));
        let now = Instant::now();
        assert!(limits.admit(60).is_some());
        assert!(limits.available_at(now, 60) >= now + Duration::from_secs(59));
        assert_eq!(limits.available_at(now, 40), now);
    }

    #[test]
    fn zero_token_requests_leave_no_token_window_entries() {
        let limits = Arc::new(DeploymentLimits::new(QuotaSpec {
            tpd: Some(10),
            ..QuotaSpec::default()
        }));
        for _ in 0..3 {
            assert!(limits.admit(0).is_some());
        }
        assert!(limits.windows[0].lock(Instant::now()).entries.is_empty());
    }

    #[test]
    fn deployments_sharing_a_key_quota_draw_on_one_window() {
        let key = Window::for_quota(QuotaSpec {
            rpm: Some(1),
            tpd: Some(1_000),
            ..QuotaSpec::default()
        });
        let a = Arc::new(DeploymentLimits::with_shared(QuotaSpec::default(), &key));
        let b = Arc::new(DeploymentLimits::with_shared(
            QuotaSpec {
                rpm: Some(10),
                ..QuotaSpec::default()
            },
            &key,
        ));
        let now = Instant::now();
        assert!(a.admit(10).is_some());
        assert!(
            b.admit(10).is_none(),
            "the key's rpm is shared across its deployments"
        );
        assert!(b.available_at(now, 1) >= now + Duration::from_secs(59));
    }
}
