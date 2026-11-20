use llmux_types::Deployment;
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

#[derive(Default)]
pub(crate) struct AttemptStats {
    routed: AtomicU64,
    failed_attempts: AtomicU64,
    stream_errors: AtomicU64,
}

#[derive(Serialize)]
pub struct DeploymentStats<'a> {
    model: &'a str,
    id: &'a str,
    provider: &'a str,
    priority: i32,
    disabled: bool,
    cooldown_ms: Option<u64>,
    rpm_limit: Option<u32>,
    concurrency_limit: Option<u32>,
    in_flight: Option<u32>,
    routed: u64,
    failed_attempts: u64,
    stream_errors: u64,
}

impl AttemptStats {
    pub(crate) fn record_attempt(&self, routed: bool) {
        let counter = if routed {
            &self.routed
        } else {
            &self.failed_attempts
        };
        counter.fetch_add(1, Relaxed);
    }

    pub(crate) fn record_stream_error(&self) {
        self.stream_errors.fetch_add(1, Relaxed);
    }

    pub(crate) fn snapshot<'a>(
        &self,
        model: &'a str,
        dep: &'a Deployment,
        disabled: bool,
        cooldown_ms: Option<u64>,
        in_flight: Option<u32>,
    ) -> DeploymentStats<'a> {
        DeploymentStats {
            model,
            id: &dep.id,
            provider: &dep.provider.0,
            priority: dep.priority,
            disabled,
            cooldown_ms,
            rpm_limit: dep.quota.rpm,
            concurrency_limit: dep.quota.concurrent,
            in_flight,
            routed: self.routed.load(Relaxed),
            failed_attempts: self.failed_attempts.load(Relaxed),
            stream_errors: self.stream_errors.load(Relaxed),
        }
    }
}
