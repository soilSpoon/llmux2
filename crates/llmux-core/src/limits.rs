use llmux_types::{FixedUnit, LimitObservation, QuotaSpec};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::{Duration, SystemTime},
};
use tokio::time::Instant;

const MINUTE: Duration = Duration::from_secs(60);
const DAY: Duration = Duration::from_secs(86_400);
const CONCURRENCY_RETRY: Duration = Duration::from_secs(1);

pub(super) struct DeploymentLimits {
    baseline: Instant,
    cooldown_until_ns: AtomicU64,
    /// Whether the current cooldown is a credential-shared quota reset.
    credential_cooldown: AtomicBool,
    /// The provider does not provision this deployment at all: it stays
    /// parked until a config reload replaces this `DeploymentLimits`.
    not_provisioned: AtomicBool,
    concurrent: Option<u32>,
    inflight: AtomicU32,
    windows: Box<[Arc<Window>]>,
    limit_strikes: AtomicU32,
    /// After a limit, one request at a time probes the deployment until one succeeds.
    probe: AtomicBool,
}

impl DeploymentLimits {
    pub fn inflight(&self) -> Option<u32> {
        self.concurrent
            .map(|_| self.inflight.load(Ordering::Relaxed))
    }

    /// Headroom left under the request-per-minute and concurrency limits.
    pub fn remaining(&self, now: Instant) -> crate::request_log::QuotaRemaining {
        // Shared (credential) windows bound admission too: report the
        // tightest RPM window, not just the deployment's own.
        let rpm = self
            .windows
            .iter()
            .filter(|window| window.span == MINUTE && matches!(window.unit, Unit::Requests))
            .map(|window| window.max.saturating_sub(window.lock(now).total()))
            .min();
        let concurrent = self
            .concurrent
            .map(|max| max.saturating_sub(self.inflight.load(Ordering::Relaxed)));
        crate::request_log::QuotaRemaining { rpm, concurrent }
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
            credential_cooldown: AtomicBool::new(false),
            not_provisioned: AtomicBool::new(false),
            concurrent: spec.concurrent,
            inflight: AtomicU32::new(0),
            windows: Window::for_quota(spec)
                .into_iter()
                .chain(shared.iter().cloned())
                .collect(),
            limit_strikes: AtomicU32::new(0),
            probe: AtomicBool::new(false),
        }
    }

    /// Cools down after a 429. Without a provider hint, each consecutive limit doubles the wait;
    /// a limit that lands while the deployment is already cooling does not count as another one.
    pub fn limited(&self, hint: Option<Duration>, default: Duration, max: Duration) -> Instant {
        let backoff = self.strike_backoff(default);
        self.cool_down_with_scope(hint.unwrap_or(backoff).min(max), false)
    }

    /// Cools down after a credential-quota exhaustion. An explicit reset is
    /// authoritative — it is honored up to `quota_max` rather than the
    /// estimate cap `max` — and the cooldown is marked credential-scope
    /// either way since quotas belong to the key, not the model.
    pub fn quota_limited(
        &self,
        hint: Option<Duration>,
        default: Duration,
        max: Duration,
        quota_max: Duration,
    ) -> Instant {
        let backoff = self.strike_backoff(default);
        let wait = match hint {
            Some(reset) => reset.min(quota_max),
            None => backoff.min(max),
        };
        self.cool_down_with_scope(wait, true)
    }

    fn strike_backoff(&self, default: Duration) -> Duration {
        let counted = self.limit_strikes.load(Ordering::Relaxed);
        let strikes = if counted > 0 && self.cooldown_until(Instant::now()).is_some() {
            counted
        } else {
            self.limit_strikes
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1)
        };
        default.saturating_mul(1u32.checked_shl(strikes - 1).unwrap_or(u32::MAX))
    }

    /// Skips the deployment until reset when the provider reports no requests or tokens left.
    /// The reset is authoritative: `max` should be the quota cap, not the backoff cap.
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
        self.cool_down_with_scope(wait, false)
    }

    /// Cools down every deployment that shares the credential's quota.
    pub fn cool_down_credential(&self, wait: Duration) -> Instant {
        self.cool_down_with_scope(wait, true)
    }

    fn cool_down_with_scope(&self, wait: Duration, credential: bool) -> Instant {
        let offset = self.baseline.elapsed() + wait;
        let offset_ns = offset.as_nanos().min(u64::MAX as u128) as u64;
        let previous = self
            .cooldown_until_ns
            .fetch_max(offset_ns, Ordering::Relaxed);
        if offset_ns > previous {
            self.credential_cooldown
                .store(credential, Ordering::Relaxed);
        }
        self.baseline + Duration::from_nanos(offset_ns.max(previous))
    }

    /// Whether the active cooldown is a credential-shared quota reset
    /// (`"credential"`) or only this deployment's (`"deployment"`).
    pub fn cooldown_scope(&self, now: Instant) -> Option<&'static str> {
        self.cooldown_until(now)?;
        Some(if self.credential_cooldown.load(Ordering::Relaxed) {
            "credential"
        } else {
            "deployment"
        })
    }

    pub fn mark_not_provisioned(&self) {
        self.not_provisioned.store(true, Ordering::Relaxed);
    }

    /// A not-provisioned deployment can never serve until a config reload.
    pub fn not_provisioned(&self) -> bool {
        self.not_provisioned.load(Ordering::Relaxed)
    }

    pub fn available_at(&self, now: Instant, tokens: u64) -> Instant {
        let mut ready = self.cooldown_until(now).unwrap_or(now);
        for window in self.windows.iter() {
            let used = window.lock(now);
            let cost = window.unit.cost(tokens);
            if used.total() + cost > window.max && (window.hard || used.total() > 0) {
                ready = ready.max(window.free_at(&used, window.max.saturating_sub(cost), now));
            }
        }
        if self
            .concurrent
            .is_some_and(|max| self.inflight.load(Ordering::Relaxed) >= max)
            || (self.limit_strikes.load(Ordering::Relaxed) > 0
                && self.probe.load(Ordering::Relaxed))
        {
            ready = ready.max(now + CONCURRENCY_RETRY);
        }
        ready
    }

    pub fn admit(self: &Arc<Self>, tokens: u64) -> Option<Permit> {
        let mut probe = Permit(None, None);
        if self.limit_strikes.load(Ordering::Relaxed) > 0 {
            if self
                .probe
                .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
            {
                return None;
            }
            probe.1 = Some(self.clone());
        }
        let mut permit = if let Some(max) = self.concurrent {
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
            Permit(Some(self.clone()), None)
        } else {
            Permit(None, None)
        };
        if !self.charge_windows(tokens, Instant::now()) {
            return None;
        }
        permit.1 = probe.1.take();
        Some(permit)
    }

    pub fn admit_repair(&self, tokens: u64) -> bool {
        let now = Instant::now();
        self.cooldown_until(now).is_none() && self.charge_windows(tokens, now)
    }

    fn charge_windows(&self, tokens: u64, now: Instant) -> bool {
        let mut windows: Vec<_> = self
            .windows
            .iter()
            .map(|window| (window, window.lock_for_admission(now)))
            .collect();
        for (window, used) in &windows {
            let cost = window.unit.cost(tokens);
            if used.total() + cost > window.max && (window.hard || used.total() > 0) {
                return false;
            }
        }
        for (window, used) in &mut windows {
            let cost = window.unit.cost(tokens);
            if cost > 0 {
                used.charge(now, cost);
            }
        }
        true
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

/// Sliding or fixed-anchored window of admitted requests or estimated tokens.
pub(super) struct Window {
    max: u64,
    span: Duration,
    unit: Unit,
    /// Credential windows are hard ceilings: they reject a request whose cost
    /// exceeds the remaining budget even when nothing has been used yet.
    /// Deployment windows admit one oversized request to an idle deployment
    /// so an undersized quota cannot deadlock it.
    hard: bool,
    kind: WindowKind,
}

enum WindowKind {
    Sliding(Mutex<Usage>),
    /// A wall-clock-anchored fixed window: usage accumulates into the current
    /// window and resets wholesale at the boundary, so the budget — unlike a
    /// sliding window's — can be front-loaded right after a provider reset.
    Fixed {
        anchor: SystemTime,
        used: Mutex<FixedUsage>,
    },
}

#[derive(Default)]
struct Usage {
    entries: VecDeque<(Instant, u64)>,
    total: u64,
}

struct FixedUsage {
    /// The window `total` belongs to, in spans since the anchor; when the
    /// clock crosses into another window the count resets wholesale.
    index: i64,
    total: u64,
    /// Monotonic/wall clock bases captured together at construction. The
    /// engine runs on tokio's monotonic clock — pausable in tests — while
    /// fixed windows align to wall-clock anchors; pairing the two clocks
    /// once keeps both frames consistent without re-reading a wall clock
    /// that could jump.
    mono_base: Instant,
    wall_base: SystemTime,
}

impl FixedUsage {
    fn new() -> Self {
        Self {
            index: i64::MIN,
            total: 0,
            mono_base: Instant::now(),
            wall_base: SystemTime::now(),
        }
    }

    fn wall_at(&self, mono: Instant) -> SystemTime {
        self.wall_base
            + mono
                .checked_duration_since(self.mono_base)
                .unwrap_or_default()
    }
}

/// Signed nanoseconds from `earlier` to `later`.
fn nanos_between(earlier: SystemTime, later: SystemTime) -> i128 {
    match later.duration_since(earlier) {
        Ok(d) => d.as_nanos() as i128,
        Err(err) => -(err.duration().as_nanos() as i128),
    }
}

enum WindowGuard<'a> {
    Sliding(MutexGuard<'a, Usage>),
    Fixed(MutexGuard<'a, FixedUsage>),
}

impl WindowGuard<'_> {
    fn total(&self) -> u64 {
        match self {
            Self::Sliding(used) => used.total,
            Self::Fixed(used) => used.total,
        }
    }

    fn charge(&mut self, now: Instant, cost: u64) {
        match self {
            Self::Sliding(used) => {
                used.entries.push_back((now, cost));
                used.total += cost;
            }
            Self::Fixed(used) => used.total += cost,
        }
    }
}

impl Window {
    /// Deployment-level quota windows. A request larger than the limit is
    /// admitted on an empty window so an undersized quota cannot deadlock an
    /// idle deployment.
    pub fn for_quota(spec: QuotaSpec) -> Vec<Arc<Self>> {
        Self::windows(spec, false)
    }

    /// Credential-level quota windows: a hard ceiling shared by every
    /// deployment on the credential. A request whose cost exceeds the
    /// remaining budget is rejected even on an empty window, so a configured
    /// daily ceiling (for example a provider's free allowance) cannot be
    /// passed.
    pub fn for_credential(spec: QuotaSpec) -> Vec<Arc<Self>> {
        Self::windows(spec, true)
    }

    fn windows(spec: QuotaSpec, hard: bool) -> Vec<Arc<Self>> {
        let sliding = [
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
                    hard,
                    kind: WindowKind::Sliding(Mutex::default()),
                })
            })
        });
        let fixed = spec.fixed.iter().map(|window| {
            Arc::new(Self {
                max: u64::from(window.max),
                span: Duration::from_secs(u64::from(window.span_secs)),
                unit: match window.unit {
                    FixedUnit::Requests => Unit::Requests,
                    FixedUnit::Tokens => Unit::Tokens,
                },
                hard,
                kind: WindowKind::Fixed {
                    anchor: window.anchor,
                    used: Mutex::new(FixedUsage::new()),
                },
            })
        });
        sliding.chain(fixed).collect()
    }

    /// The fixed window index covering `wall`: spans since the anchor,
    /// floored so times before the anchor count negative windows.
    fn fixed_index(anchor: SystemTime, span: Duration, wall: SystemTime) -> i64 {
        let span_nanos = i128::try_from(span.as_nanos()).unwrap_or(i128::MAX);
        let offset = nanos_between(anchor, wall);
        i64::try_from(offset.div_euclid(span_nanos)).unwrap_or(if offset < 0 {
            i64::MIN
        } else {
            i64::MAX
        })
    }

    /// Read-path lock. Availability and remaining-budget queries are evaluated
    /// at the caller's scan instant; fixed-window indices still only move
    /// forward so a stale scan cannot erase current-window charges.
    fn lock(&self, now: Instant) -> WindowGuard<'_> {
        self.lock_with_clock_source(now, false)
    }

    /// Admission-path lock. A caller can capture `now` before a reset boundary
    /// but reach the mutex only inside the next window, so fixed-window
    /// placement is decided after acquisition. Late admissions then charge the
    /// window current at acquisition instead of rolling state backward.
    fn lock_for_admission(&self, now: Instant) -> WindowGuard<'_> {
        self.lock_with_clock_source(now, true)
    }

    fn lock_with_clock_source(&self, now: Instant, admission: bool) -> WindowGuard<'_> {
        match &self.kind {
            WindowKind::Sliding(mutex) => {
                let mut used = mutex.lock().unwrap_or_else(|err| err.into_inner());
                while let Some(&(at, cost)) = used.entries.front() {
                    if now.duration_since(at) < self.span {
                        break;
                    }
                    used.entries.pop_front();
                    used.total -= cost;
                }
                WindowGuard::Sliding(used)
            }
            WindowKind::Fixed { anchor, used } => {
                let mut used = used.lock().unwrap_or_else(|err| err.into_inner());
                let placement = if admission { Instant::now() } else { now };
                let index = Self::fixed_index(*anchor, self.span, used.wall_at(placement));
                if index > used.index {
                    used.index = index;
                    used.total = 0;
                }
                WindowGuard::Fixed(used)
            }
        }
    }

    /// When enough usage expires for the window to admit `target` more units.
    /// Sliding windows use `target` to find the earliest entry expiry that
    /// frees enough room. Fixed windows intentionally ignore it: their budget
    /// has no partial refresh, so the first time any more units can be admitted
    /// is the next anchored boundary, when the whole allowance returns.
    fn free_at(&self, guard: &WindowGuard<'_>, target: u64, now: Instant) -> Instant {
        match (&self.kind, guard) {
            (WindowKind::Sliding(_), WindowGuard::Sliding(used)) => {
                let mut total = used.total;
                for &(at, cost) in &used.entries {
                    total -= cost;
                    if total <= target {
                        return at + self.span;
                    }
                }
                // No entries to wait on: reached only when the request can never
                // fit the window (cost > max on a hard window), so the reset is a
                // full span out rather than "now".
                used.entries
                    .back()
                    .map_or_else(|| now + self.span, |&(at, _)| at + self.span)
            }
            (WindowKind::Fixed { anchor, .. }, WindowGuard::Fixed(used)) => {
                let span_nanos = i128::try_from(self.span.as_nanos()).unwrap_or(i128::MAX);
                let wall_now = used.wall_at(now);
                let elapsed = nanos_between(*anchor, wall_now);
                let next_boundary = (i128::from(used.index) + 1) * span_nanos;
                let wait = u64::try_from((next_boundary - elapsed).max(0)).unwrap_or(u64::MAX);
                now + Duration::from_nanos(wait)
            }
            _ => unreachable!("guard and window kinds match by construction"),
        }
    }
}

pub(super) struct Permit(Option<Arc<DeploymentLimits>>, Option<Arc<DeploymentLimits>>);

impl Drop for Permit {
    fn drop(&mut self) {
        if let Some(limits) = &self.0 {
            limits.inflight.fetch_sub(1, Ordering::Relaxed);
        }
        if let Some(limits) = &self.1 {
            limits.probe.store(false, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn provider_retry_and_proactive_reset_respect_the_cooldown_cap() {
        let cap = Duration::from_secs(5);
        let hint = Duration::from_secs(60);
        let limits = DeploymentLimits::new(QuotaSpec::default());
        let now = Instant::now();
        assert_eq!(
            limits.limited(Some(hint), Duration::from_secs(1), cap),
            now + cap
        );

        let limits = DeploymentLimits::new(QuotaSpec::default());
        limits.observe(
            LimitObservation {
                remaining_tokens: Some(0),
                reset_tokens: Some(hint),
                ..LimitObservation::default()
            },
            cap,
        );
        assert_eq!(limits.cooldown_until(now), Some(now + cap));
        assert_eq!(limits.cooldown_until(now + cap), None);
    }

    /// A provider's explicit quota reset is authoritative: it can outlast the
    /// backoff cap and is only bounded by `quota_max`. Estimated backoffs and
    /// rate-limit hints still stop at `max`.
    #[tokio::test(start_paused = true)]
    async fn quota_resets_use_the_quota_cap_and_mark_the_scope() {
        let (default, max, quota_max) = (
            Duration::from_secs(1),
            Duration::from_secs(5),
            Duration::from_secs(86_400),
        );
        let now = Instant::now();
        let limits = DeploymentLimits::new(QuotaSpec::default());
        assert_eq!(
            limits.quota_limited(Some(Duration::from_secs(3_600)), default, max, quota_max),
            now + Duration::from_secs(3_600),
            "an explicit reset is honored beyond the backoff cap"
        );
        assert_eq!(limits.cooldown_scope(now), Some("credential"));

        let limits = DeploymentLimits::new(QuotaSpec::default());
        assert_eq!(
            limits.quota_limited(
                Some(Duration::from_secs(30 * 86_400)),
                default,
                max,
                quota_max
            ),
            now + quota_max,
            "the quota cap — not the backoff cap — bounds an explicit reset"
        );

        let limits = DeploymentLimits::new(QuotaSpec::default());
        assert_eq!(
            limits.quota_limited(None, default, max, quota_max),
            now + default,
            "an unhinted quota exhaustion keeps the estimate cap"
        );

        let limits = DeploymentLimits::new(QuotaSpec::default());
        assert_eq!(
            limits.limited(Some(Duration::from_secs(3_600)), default, max),
            now + max,
            "a rate-limit hint still clamps to the backoff cap"
        );
        assert_eq!(limits.cooldown_scope(now), Some("deployment"));

        // A proactive reset from observed headers is just as authoritative.
        let limits = DeploymentLimits::new(QuotaSpec::default());
        limits.observe(
            LimitObservation {
                remaining_requests: Some(0),
                reset_requests: Some(Duration::from_secs(3_600)),
                ..LimitObservation::default()
            },
            quota_max,
        );
        assert_eq!(
            limits.cooldown_until(now),
            Some(now + Duration::from_secs(3_600))
        );

        // A shorter cooldown landing later does not erase the scope marker.
        limits.cool_down(Duration::from_secs(1));
        assert_eq!(limits.cooldown_scope(now), Some("deployment"));
        let before = DeploymentLimits::new(QuotaSpec::default());
        before.cool_down_credential(Duration::from_secs(600));
        before.cool_down(Duration::from_secs(1));
        assert_eq!(before.cooldown_scope(now), Some("credential"));
        assert_eq!(before.cooldown_scope(now + Duration::from_secs(601)), None);
    }

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
            let limits = Arc::new(DeploymentLimits::new(spec.clone()));
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
        assert!(
            matches!(limits.windows[0].lock(Instant::now()), WindowGuard::Sliding(ref used) if used.entries.is_empty()),
            "zero-token requests leave no sliding entries"
        );
    }

    #[test]
    fn after_a_limit_only_one_probe_is_in_flight_until_a_success() {
        let limits = Arc::new(DeploymentLimits::new(QuotaSpec::default()));
        limits.limited(None, Duration::from_millis(1), Duration::from_secs(1));
        std::thread::sleep(Duration::from_millis(5));
        let probe = limits.admit(0).expect("one probe after the cooldown");
        assert!(limits.admit(0).is_none(), "others wait for the probe");
        drop(probe);
        let probe = limits.admit(0).expect("a new probe once the last one ends");
        limits.succeeded();
        drop(probe);
        let (a, b) = (limits.admit(0), limits.admit(0));
        assert!(
            a.is_some() && b.is_some(),
            "a success restores full traffic"
        );
    }

    /// Credential windows are hard ceilings: an oversized request is rejected
    /// even on an empty window, while a deployment quota admits it to keep an
    /// undersized quota from deadlocking the deployment.
    #[test]
    fn a_credential_window_rejects_a_request_larger_than_the_empty_window() {
        let key = Window::for_credential(QuotaSpec {
            tpd: Some(100),
            ..QuotaSpec::default()
        });
        let shared = Arc::new(DeploymentLimits::with_shared(QuotaSpec::default(), &key));
        assert!(shared.admit(150).is_none());
        assert!(shared.admit(90).is_some());
        assert!(shared.admit(20).is_none(), "90 + 20 exceeds tpd = 100");

        let own_quota = Arc::new(DeploymentLimits::new(QuotaSpec {
            tpd: Some(100),
            ..QuotaSpec::default()
        }));
        assert!(
            own_quota.admit(150).is_some(),
            "a deployment quota still admits an oversized request when idle"
        );
    }

    /// A request that can never fit a credential window must report a retry
    /// when the window resets, not "now": an empty window's `free_at` has no
    /// entries to wait on, so it would otherwise answer immediately.
    #[test]
    fn an_impossible_request_on_a_credential_window_retries_when_it_resets() {
        let key = Window::for_credential(QuotaSpec {
            tpd: Some(100),
            ..QuotaSpec::default()
        });
        let shared = Arc::new(DeploymentLimits::with_shared(QuotaSpec::default(), &key));
        let now = Instant::now();
        assert!(shared.admit(150).is_none());
        assert!(
            shared.available_at(now, 150) >= now + Duration::from_secs(86_399),
            "a request that can never fit waits the whole window"
        );
    }

    #[test]
    fn deployments_sharing_a_key_quota_draw_on_one_window() {
        let key = Window::for_credential(QuotaSpec {
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

    #[test]
    fn a_limit_after_a_header_cooldown_still_starts_probing() {
        let limits = Arc::new(DeploymentLimits::new(QuotaSpec::default()));
        limits.observe(
            LimitObservation {
                remaining_requests: Some(0),
                reset_requests: Some(Duration::from_millis(1)),
                ..LimitObservation::default()
            },
            Duration::from_secs(1),
        );
        limits.limited(None, Duration::from_millis(1), Duration::from_secs(1));
        std::thread::sleep(Duration::from_millis(5));
        let now = Instant::now();
        let _probe = limits.admit(0).expect("one probe");
        assert!(
            limits.admit(0).is_none(),
            "a header cooldown must not hide the limit"
        );
        assert_eq!(limits.available_at(now, 0), now + CONCURRENCY_RETRY);
    }

    fn fixed_requests_at(span_secs: u32, max: u32, anchor: SystemTime) -> QuotaSpec {
        QuotaSpec {
            fixed: vec![llmux_types::FixedWindowSpec {
                unit: llmux_types::FixedUnit::Requests,
                max,
                span_secs,
                anchor,
            }],
            ..QuotaSpec::default()
        }
    }

    fn fixed_requests(span_secs: u32, max: u32) -> QuotaSpec {
        fixed_requests_at(span_secs, max, SystemTime::now())
    }

    /// A fixed anchored window spends its whole budget, then parks every
    /// request until the boundary — and at the boundary the WHOLE budget
    /// returns at once. That wholesale refresh is the difference from a
    /// sliding window, which would ration the budget back one entry at a
    /// time and could never front-load a fresh window.
    #[tokio::test(start_paused = true)]
    async fn a_fixed_window_refreshes_the_whole_budget_at_the_boundary() {
        let limits = Arc::new(DeploymentLimits::new(fixed_requests(3_600, 2)));
        assert!(limits.admit(0).is_some());
        assert!(limits.admit(0).is_some());
        assert!(limits.admit(0).is_none(), "the window budget is spent");
        let now = Instant::now();
        let ready = limits.available_at(now, 0);
        assert!(
            ready > now + Duration::from_secs(3_500) && ready <= now + Duration::from_secs(3_601),
            "the retry is the window boundary, not a gradual expiry: {ready:?} vs {now:?}"
        );
        tokio::time::advance(Duration::from_secs(3_601)).await;
        assert!(
            limits.admit(0).is_some(),
            "crossing the boundary restores the budget"
        );
        assert!(
            limits.admit(0).is_some(),
            "the whole budget returns at once — none of it waits for the first charge to age out"
        );
        assert!(limits.admit(0).is_none());
    }

    /// Availability is an estimate at the caller's scan instant, not a hidden
    /// read of whatever time it is when the mutex happens to be acquired. This
    /// scan was captured while the window was still exhausted; even after the
    /// real clock crosses the boundary, evaluating that scan must report the
    /// boundary the caller saw.
    #[tokio::test(start_paused = true)]
    async fn fixed_window_availability_is_evaluated_at_the_scan_instant() {
        let anchor = SystemTime::now() - Duration::from_secs(3_500);
        let limits = Arc::new(DeploymentLimits::new(fixed_requests_at(3_600, 1, anchor)));
        assert!(limits.admit(0).is_some());
        let scan = Instant::now();
        tokio::time::advance(Duration::from_secs(101)).await;

        let ready = limits.available_at(scan, 0);
        assert!(
            ready > scan + Duration::from_secs(90) && ready <= scan + Duration::from_secs(101),
            "the scan-time estimate is the anchored boundary, not the later lock-acquisition time: {ready:?} vs {scan:?}"
        );
    }

    /// A caller that captured `now` before a reset boundary can acquire the
    /// window lock only after another caller charged the new window. The stale
    /// timestamp must neither move the index backward nor erase that charge;
    /// the delayed admission belongs to the current window too.
    #[tokio::test(start_paused = true)]
    async fn stale_fixed_window_timestamps_cannot_erase_current_window_charges() {
        let anchor = SystemTime::now() - Duration::from_secs(3_500);
        let window = Window::for_credential(fixed_requests_at(3_600, 10, anchor))
            .pop()
            .unwrap();
        let stale = Instant::now();
        tokio::time::advance(Duration::from_secs(101)).await;
        let current = Instant::now();
        let current_index = {
            let mut guard = window.lock_for_admission(current);
            guard.charge(current, 1);
            assert_eq!(guard.total(), 1);
            match &guard {
                WindowGuard::Fixed(used) => used.index,
                WindowGuard::Sliding(_) => panic!("fixed window expected"),
            }
        };

        {
            let guard = window.lock(stale);
            assert_eq!(
                guard.total(),
                1,
                "a stale lock must not erase the current window"
            );
            match &guard {
                WindowGuard::Fixed(used) => assert_eq!(used.index, current_index),
                WindowGuard::Sliding(_) => panic!("fixed window expected"),
            }
        }
        let mut delayed = window.lock_for_admission(stale);
        delayed.charge(stale, 1);
        assert_eq!(
            delayed.total(),
            2,
            "the delayed admission is accounted in the current window"
        );
    }

    /// Start partway through an anchored window and stagger the charges. At
    /// the boundary both charges are still younger than the span, so a sliding
    /// implementation would keep them; the fixed window must nevertheless
    /// return the whole allowance exactly at that boundary.
    #[tokio::test(start_paused = true)]
    async fn fixed_reset_is_anchored_not_sliding_for_staggered_charges() {
        let anchor = SystemTime::now() - Duration::from_secs(1_000);
        let limits = Arc::new(DeploymentLimits::new(fixed_requests_at(3_600, 2, anchor)));
        assert!(limits.admit(0).is_some());
        tokio::time::advance(Duration::from_secs(100)).await;
        assert!(limits.admit(0).is_some());
        assert!(limits.admit(0).is_none(), "the window budget is spent");

        let now = Instant::now();
        let ready = limits.available_at(now, 0);
        assert!(
            ready > now + Duration::from_secs(2_490) && ready <= now + Duration::from_secs(2_501),
            "retry is the next anchored boundary, not either charge's sliding expiry: {ready:?} vs {now:?}"
        );
        tokio::time::advance(Duration::from_secs(2_501)).await;
        assert!(
            limits.admit(0).is_some(),
            "the boundary resets even though the first charge is only about 2,601s old"
        );
        assert!(
            limits.admit(0).is_some(),
            "the second slot returns too even though its charge is only about 2,501s old"
        );
        assert!(limits.admit(0).is_none());
    }

    /// Token-unit fixed windows charge estimated tokens, and a credential's
    /// fixed window is a hard ceiling even on an empty window — an oversized
    /// request can never slip through a free allowance by being first.
    #[tokio::test(start_paused = true)]
    async fn a_fixed_credential_window_counts_tokens_as_a_hard_ceiling() {
        let spec = QuotaSpec {
            fixed: vec![llmux_types::FixedWindowSpec {
                unit: llmux_types::FixedUnit::Tokens,
                max: 100,
                span_secs: 3_600,
                anchor: SystemTime::now(),
            }],
            ..QuotaSpec::default()
        };
        let shared = Window::for_credential(spec.clone());
        let keyed = Arc::new(DeploymentLimits::with_shared(QuotaSpec::default(), &shared));
        assert!(
            keyed.admit(150).is_none(),
            "an oversized request is rejected even when the window is empty"
        );
        assert!(keyed.admit(60).is_some());
        assert!(keyed.admit(60).is_none(), "60 + 60 exceeds the window");
        assert!(keyed.admit(40).is_some());
        tokio::time::advance(Duration::from_secs(3_601)).await;
        assert!(
            keyed.admit(100).is_some(),
            "the full token budget returns at the boundary"
        );

        let own = Arc::new(DeploymentLimits::new(spec));
        assert!(
            own.admit(150).is_some(),
            "a deployment-level fixed window still admits an oversized request when idle"
        );
    }
}
