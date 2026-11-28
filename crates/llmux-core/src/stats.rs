use crate::GatewayError;
use llmux_types::{Deployment, ErrorKind};
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::{collections::BTreeMap, sync::Mutex};

#[derive(Default, Serialize, Clone)]
pub struct CatchAllStats {
    models: BTreeMap<String, u64>,
    other: u64,
}

impl CatchAllStats {
    pub(crate) fn record(&mut self, model: &str) {
        if let Some(count) = self.models.get_mut(model) {
            *count += 1;
        } else if self.models.len() < 256 && model.len() <= 256 {
            self.models.insert(model.to_owned(), 1);
        } else {
            self.other += 1;
        }
    }
}

#[derive(Serialize)]
pub struct FallbackStats {
    pub routed: u64,
    pub total_routed: u64,
    pub share: f64,
}

impl FallbackStats {
    pub fn from_deployments(deployments: &[DeploymentStats<'_>]) -> Self {
        let total_routed = deployments.iter().map(|dep| dep.routed).sum();
        let routed = deployments
            .iter()
            .filter(|dep| dep.fallback)
            .map(|dep| dep.routed)
            .sum();
        Self {
            routed,
            total_routed,
            share: if total_routed == 0 {
                0.0
            } else {
                routed as f64 / total_routed as f64
            },
        }
    }
}

pub(crate) type CatchAllCounter = Mutex<CatchAllStats>;

const FAILURE_CLASSES: [&str; 11] = [
    "rate_limited",
    "quota_exhausted",
    "not_provisioned",
    "auth",
    "bad_request",
    "context_length",
    "server_error",
    "timeout",
    "connect_error",
    "stream_broken",
    "internal",
];

fn failure_index(kind: ErrorKind) -> usize {
    match kind {
        ErrorKind::RateLimited => 0,
        ErrorKind::QuotaExhausted => 1,
        ErrorKind::NotProvisioned => 2,
        ErrorKind::Auth => 3,
        ErrorKind::BadRequest => 4,
        ErrorKind::ContextLength => 5,
        ErrorKind::ServerError => 6,
        ErrorKind::Timeout => 7,
        ErrorKind::ConnectError => 8,
        ErrorKind::StreamBroken => 9,
    }
}

#[derive(Clone, Copy)]
pub(crate) enum SkipReason {
    Disabled,
    UnsupportedFormat,
    ContextLength,
    Cooldown,
    AdmissionLimited,
    UnsupportedConversion,
    UpstreamUnavailable,
    Expired,
    /// The provider does not provision this deployment; parked until reload.
    NotProvisioned,
    /// A non-stream request asked for more output tokens than the leg serves.
    OutputTokenLimit,
}

const SKIP_REASONS: [&str; 10] = [
    "disabled",
    "unsupported_format",
    "context_length",
    "cooldown",
    "admission_limited",
    "unsupported_conversion",
    "upstream_unavailable",
    "expired",
    "not_provisioned",
    "output_token_limit",
];

fn snapshot_counts<const N: usize>(
    labels: [&'static str; N],
    counts: &[AtomicU64; N],
) -> BTreeMap<&'static str, u64> {
    labels
        .into_iter()
        .zip(counts)
        .filter_map(|(label, count)| {
            let count = count.load(Relaxed);
            (count != 0).then_some((label, count))
        })
        .collect()
}

/// The routing-visible deployment state a snapshot reports alongside the
/// recorded counters.
pub(crate) struct LiveState {
    pub disabled: bool,
    pub not_provisioned: bool,
    pub cooldown_ms: Option<u64>,
    /// "credential" when the cooldown is a credential-shared quota reset.
    pub cooldown_scope: Option<&'static str>,
    pub in_flight: Option<u32>,
}

#[derive(Default)]
pub(crate) struct AttemptStats {
    routed: AtomicU64,
    failed_attempts: AtomicU64,
    stream_errors: AtomicU64,
    /// Failed attempts that handed the request to another deployment.
    handoffs: AtomicU64,
    failure_classes: [AtomicU64; FAILURE_CLASSES.len()],
    skip_reasons: [AtomicU64; SKIP_REASONS.len()],
}

#[derive(Serialize)]
pub struct DeploymentStats<'a> {
    fallback: bool,
    model: &'a str,
    id: &'a str,
    provider: &'a str,
    priority: i32,
    disabled: bool,
    not_provisioned: bool,
    expired: bool,
    expires_at_unix: Option<u64>,
    cooldown_ms: Option<u64>,
    /// "credential" when the cooldown is a credential-shared quota reset.
    cooldown_scope: Option<&'static str>,
    rpm_limit: Option<u32>,
    concurrency_limit: Option<u32>,
    in_flight: Option<u32>,
    routed: u64,
    failed_attempts: u64,
    stream_errors: u64,
    handoffs: u64,
    failure_classes: BTreeMap<&'static str, u64>,
    skip_reasons: BTreeMap<&'static str, u64>,
}

impl AttemptStats {
    pub(crate) fn record_attempt(&self, error: Option<&GatewayError>) {
        if let Some(error) = error {
            self.failed_attempts.fetch_add(1, Relaxed);
            let index = match error {
                GatewayError::Client(error) => failure_index(error.kind()),
                _ => 10,
            };
            self.failure_classes[index].fetch_add(1, Relaxed);
        } else {
            self.routed.fetch_add(1, Relaxed);
        }
    }

    pub(crate) fn record_handoff(&self) {
        self.handoffs.fetch_add(1, Relaxed);
    }

    pub(crate) fn record_stream_error(&self, kind: ErrorKind) {
        self.stream_errors.fetch_add(1, Relaxed);
        self.failure_classes[failure_index(kind)].fetch_add(1, Relaxed);
    }

    pub(crate) fn record_skip(&self, reason: SkipReason) {
        self.skip_reasons[reason as usize].fetch_add(1, Relaxed);
    }

    pub(crate) fn snapshot<'a>(
        &self,
        model: &'a str,
        dep: &'a Deployment,
        state: LiveState,
    ) -> DeploymentStats<'a> {
        DeploymentStats {
            fallback: dep.fallback,
            model,
            id: &dep.id,
            provider: &dep.provider.0,
            priority: dep.priority,
            disabled: state.disabled,
            not_provisioned: state.not_provisioned,
            expired: dep
                .expires_at
                .is_some_and(|end| std::time::SystemTime::now() >= end),
            expires_at_unix: dep.expires_at.and_then(|end| {
                end.duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|span| span.as_secs())
            }),
            cooldown_ms: state.cooldown_ms,
            cooldown_scope: state.cooldown_scope,
            rpm_limit: dep.quota.rpm,
            concurrency_limit: dep.quota.concurrent,
            in_flight: state.in_flight,
            routed: self.routed.load(Relaxed),
            failed_attempts: self.failed_attempts.load(Relaxed),
            stream_errors: self.stream_errors.load(Relaxed),
            handoffs: self.handoffs.load(Relaxed),
            failure_classes: snapshot_counts(FAILURE_CLASSES, &self.failure_classes),
            skip_reasons: snapshot_counts(SKIP_REASONS, &self.skip_reasons),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catch_all_ids_are_bounded_and_overflow_is_counted() {
        let mut stats = CatchAllStats::default();
        assert_eq!(stats.other, 0);
        for i in 0..300 {
            stats.record(&format!("model-{i}"));
        }
        stats.record("model-0");
        stats.record(&"x".repeat(257));
        assert_eq!(stats.models.len(), 256);
        assert_eq!(stats.models["model-0"], 2);
        assert_eq!(stats.other, 45);
        let empty = FallbackStats::from_deployments(&[]);
        assert_eq!(empty.share, 0.0);
    }
}
