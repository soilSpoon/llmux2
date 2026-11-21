//! Cached `devin models` catalog. Config stays the allowlist: a successful load
//! can only hide a Devin-only name, never publish one the config does not name.

use llmux_config::RuntimeConfig;
use llmux_core::LiveModels;
use llmux_providers::{list_devin_models, DevinAcpConfig};
use llmux_types::{provider, DevinModel};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Notify;

const REFRESH_INTERVAL: Duration = Duration::from_secs(60);

struct Inner {
    generation: u64,
    last_attempt: Option<Instant>,
    inflight: bool,
}

pub(crate) struct Kick {
    pub target: u64,
    pub inflight: bool,
}

pub(crate) struct Catalog {
    config: Mutex<DevinAcpConfig>,
    live: Arc<LiveModels>,
    inner: Mutex<Inner>,
    notify: Notify,
}

impl Catalog {
    pub(crate) fn new(config: DevinAcpConfig) -> Self {
        Self {
            config: Mutex::new(config),
            live: Arc::new(LiveModels::default()),
            inner: Mutex::new(Inner {
                generation: 0,
                last_attempt: None,
                inflight: false,
            }),
            notify: Notify::new(),
        }
    }

    pub(crate) fn live(&self) -> Arc<LiveModels> {
        Arc::clone(&self.live)
    }

    pub(crate) fn devin_config(&self) -> DevinAcpConfig {
        self.config.lock().expect("devin config").clone()
    }

    pub(crate) fn set_config(&self, config: DevinAcpConfig) {
        *self.config.lock().expect("devin config") = config;
    }

    pub(crate) fn loaded(&self) -> Option<Arc<HashSet<Arc<str>>>> {
        self.live.get()
    }

    /// Starts a refresh if the last attempt is at least 60 seconds old and none
    /// is running. Never waits.
    pub(crate) fn kick(self: &Arc<Self>) -> Kick {
        let mut inner = self.inner.lock().expect("catalog");
        let due = match inner.last_attempt {
            None => true,
            Some(started) => started.elapsed() >= REFRESH_INTERVAL,
        };
        if inner.inflight {
            return Kick {
                target: inner.generation + 1,
                inflight: true,
            };
        }
        if !due {
            return Kick {
                target: inner.generation,
                inflight: false,
            };
        }
        inner.inflight = true;
        inner.last_attempt = Some(Instant::now());
        let target = inner.generation + 1;
        drop(inner);
        let config = self.devin_config();
        let catalog = Arc::clone(self);
        tokio::spawn(async move {
            catalog.finish(list_devin_models(&config).await);
        });
        Kick {
            target,
            inflight: true,
        }
    }

    fn finish(&self, result: Result<Vec<DevinModel>, llmux_types::UpstreamError>) {
        if let Ok(models) = result {
            self.live
                .replace(models.into_iter().map(|model| Arc::<str>::from(model.id)));
        }
        {
            let mut inner = self.inner.lock().expect("catalog");
            inner.inflight = false;
            inner.generation += 1;
        }
        self.notify.notify_waiters();
    }

    pub(crate) async fn wait_for(&self, target: u64) {
        loop {
            let notified = self.notify.notified();
            if self.inner.lock().expect("catalog").generation >= target {
                return;
            }
            notified.await;
        }
    }
}

pub(crate) fn has_devin(config: &RuntimeConfig) -> bool {
    config
        .deployments
        .iter()
        .any(|dep| dep.provider.0.as_ref() == provider::DEVIN_ACP)
}

/// Configured names and aliases. A loaded catalog omits a name only when every
/// deployment is `devin_acp` and none of their upstream ids are in it.
pub(crate) fn listed_ids<'a>(
    config: &'a RuntimeConfig,
    live: Option<&HashSet<Arc<str>>>,
) -> Vec<&'a str> {
    let mut ids: Vec<&str> = config.model_names.keys().map(AsRef::as_ref).collect();
    if let Some(live) = live {
        ids.retain(|id| {
            let Some(&index) = config.model_names.get(*id) else {
                return true;
            };
            let model = &config.models[index];
            let all_devin = model.deployments.iter().all(|idx| {
                config.deployments[idx.0 as usize].provider.0.as_ref() == provider::DEVIN_ACP
            });
            !all_devin
                || model.deployments.iter().any(|idx| {
                    live.contains(config.deployments[idx.0 as usize].upstream_model.as_ref())
                })
        });
    }
    ids.sort_unstable();
    ids
}
