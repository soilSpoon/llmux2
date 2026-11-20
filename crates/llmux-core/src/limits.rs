use llmux_types::QuotaSpec;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU32, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::time::Instant;

const RPM_WINDOW: Duration = Duration::from_secs(60);
const CONCURRENCY_RETRY: Duration = Duration::from_secs(1);

pub(super) struct DeploymentLimits {
    baseline: Instant,
    cooldown_until_ns: AtomicU64,
    concurrent: Option<u32>,
    inflight: AtomicU32,
    rpm: Option<(u32, Mutex<VecDeque<Instant>>)>,
}

impl DeploymentLimits {
    pub fn inflight(&self) -> Option<u32> {
        self.concurrent
            .map(|_| self.inflight.load(Ordering::Relaxed))
    }

    pub fn new(spec: QuotaSpec) -> Self {
        Self {
            baseline: Instant::now(),
            cooldown_until_ns: AtomicU64::new(0),
            concurrent: spec.concurrent,
            inflight: AtomicU32::new(0),
            rpm: spec.rpm.map(|rpm| (rpm, Mutex::default())),
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

    pub fn available_at(&self, now: Instant) -> Instant {
        let mut ready = self.cooldown_until(now).unwrap_or(now);
        if let Some((max, queue)) = &self.rpm {
            let starts = queue.lock().unwrap_or_else(|err| err.into_inner());
            if starts.len() >= *max as usize {
                ready = ready.max(starts[0] + RPM_WINDOW);
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

    pub fn admit(self: &Arc<Self>) -> Option<Permit> {
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
        if let Some((max, queue)) = &self.rpm {
            let mut starts = queue.lock().unwrap_or_else(|err| err.into_inner());
            let now = Instant::now();
            while starts
                .front()
                .is_some_and(|at| now.duration_since(*at) >= RPM_WINDOW)
            {
                starts.pop_front();
            }
            if starts.len() >= *max as usize {
                return None;
            }
            starts.push_back(now);
        }
        Some(permit)
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
        let permit = limits.admit().unwrap();
        assert!(limits.admit().is_none());
        assert_eq!(limits.available_at(now), now + CONCURRENCY_RETRY);
        drop(permit);
        assert!(limits.admit().is_some());
    }

    #[test]
    fn a_short_late_retry_after_cannot_shorten_an_existing_cooldown() {
        let limits = DeploymentLimits::new(QuotaSpec::default());
        let reset = limits.cool_down(Duration::from_secs(60));
        assert_eq!(limits.cool_down(Duration::from_secs(1)), reset);
        assert_eq!(limits.cooldown_until(Instant::now()), Some(reset));
        assert_eq!(limits.cooldown_until(reset), None);
    }
}
