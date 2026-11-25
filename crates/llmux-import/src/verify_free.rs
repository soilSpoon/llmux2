//! Lifecycle verification for legs the config currently marks free.
//!
//! The verifier is deliberately conservative: a probe runs only where the
//! provider is known to fail closed on billing (or is a loopback test
//! fixture), and quota exhaustion is a parking signal rather than removal
//! evidence. Reports carry normalized evidence, never credential material.

use crate::discovery::{self, ProbeOutcome};
use llmux_config::RuntimeConfig;
use llmux_types::{provider, Billing, Deployment, DeploymentIdx, HttpClient};
use serde::Serialize;
use std::{collections::HashMap, net::IpAddr, path::Path, sync::Arc, time::SystemTime};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    StillFree,
    PromoEnded,
    QuotaDrained,
    Unknown,
    PromoExtended,
}

#[derive(Debug, Serialize)]
pub struct VerifyRow {
    pub leg_id: String,
    pub canonical_model: String,
    pub provider: String,
    pub upstream_host: String,
    pub model: String,
    pub credential_id: Option<String>,
    pub classification: Classification,
    pub removal_candidate: bool,
    pub probed: bool,
    pub cost: Option<f64>,
    pub error_class: Option<String>,
    pub http_status: Option<u16>,
    pub probed_at: Option<String>,
    pub expires_at: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct VerifyReport {
    pub generated_at: String,
    pub legs: Vec<VerifyRow>,
}

/// Verify every `billing = "free"` deployment in a parsed config. Free is the
/// explicit marker only: the gateway's own sweep and free-only routing treat a
/// missing marker as billed, so quota or expiry metadata alone never admits a
/// leg into verification.
pub async fn verify_runtime_config(config: &RuntimeConfig, client: &HttpClient) -> VerifyReport {
    let canonical = canonical_names(config);
    let mut legs = Vec::new();
    for deployment in config
        .deployments
        .iter()
        .filter(|deployment| deployment.billing == Billing::Free)
    {
        let canonical = canonical.get(&deployment.idx).cloned().unwrap_or_default();
        legs.push(verify_leg(config, client, deployment, canonical).await);
    }
    VerifyReport {
        generated_at: rfc3339(SystemTime::now()),
        legs,
    }
}

/// Load and verify a config file. Secret references resolve only inside the
/// verifier process; report rows never carry the resolved values.
pub async fn verify_config(path: &Path) -> Result<VerifyReport, String> {
    let config = RuntimeConfig::load(path)
        .map_err(|error| format!("could not load config for --verify-free: {error}"))?;
    let client = discovery::client()?;
    Ok(verify_runtime_config(&config, &client).await)
}

fn canonical_names(config: &RuntimeConfig) -> HashMap<DeploymentIdx, Arc<str>> {
    let mut names = HashMap::new();
    for model in &config.models {
        for idx in &model.deployments {
            names.insert(*idx, model.name.clone());
        }
    }
    names
}

enum ProbeAuth<'a> {
    Anonymous,
    Bearer(&'a str),
}

fn probe_auth<'a>(
    config: &'a RuntimeConfig,
    deployment: &Deployment,
) -> Result<ProbeAuth<'a>, String> {
    if !matches!(
        deployment.provider.0.as_ref(),
        provider::OPENAI | provider::OPENAI_COMPAT
    ) {
        return Err(format!(
            "provider kind {} does not use the OpenAI chat-completions bearer probe",
            deployment.provider.0
        ));
    }
    if deployment.credential.is_empty() {
        return Ok(ProbeAuth::Anonymous);
    }
    config
        .api_keys
        .get(&deployment.credential)
        .map(|key| ProbeAuth::Bearer(key.key.as_ref()))
        .ok_or_else(|| "credential kind cannot supply a static API key for the probe".to_string())
}

fn host_is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Whether probing this host can be made fail-closed. Experiential is the
/// only non-loopback row with reviewed evidence today: a zero-balance org
/// returns billing errors instead of silently accepting billed models, and
/// successful responses expose `usage.cost`. Every other host stays unknown
/// until per-provider evidence is reviewed; in particular a provider's free
/// allowance returning 402 does not prove paid-only models fail closed.
fn probe_policy(host: &str) -> Result<(), String> {
    if host_is_loopback(host) || host.eq_ignore_ascii_case("api.experientiallabs.ai") {
        Ok(())
    } else {
        Err(format!(
            "probe policy unknown for upstream host {host}; not probing"
        ))
    }
}

async fn verify_leg(
    config: &RuntimeConfig,
    client: &HttpClient,
    deployment: &Deployment,
    canonical_model: Arc<str>,
) -> VerifyRow {
    let mut row = base_row(deployment, canonical_model);
    if let Err(reason) = probe_policy(&row.upstream_host) {
        row.reason = Some(reason);
        return row;
    }
    let auth = match probe_auth(config, deployment) {
        Ok(auth) => auth,
        Err(reason) => {
            row.reason = Some(reason);
            return row;
        }
    };
    let key = match auth {
        ProbeAuth::Anonymous => None,
        ProbeAuth::Bearer(key) => Some(key),
    };
    let probed_at = SystemTime::now();
    let base_url = deployment.base_url.to_string();
    let evidence =
        discovery::probe_model_detailed(client, &base_url, key, deployment.upstream_model.as_ref())
            .await;
    let classification = match evidence.outcome {
        ProbeOutcome::Free => {
            if deployment
                .expires_at
                .is_some_and(|expires| expires < probed_at)
            {
                Classification::PromoExtended
            } else {
                Classification::StillFree
            }
        }
        ProbeOutcome::Billed | ProbeOutcome::Unavailable => Classification::PromoEnded,
        ProbeOutcome::Quota => Classification::QuotaDrained,
        ProbeOutcome::Unknown => Classification::Unknown,
    };
    if classification == Classification::Unknown {
        row.reason = Some(format!(
            "probe inconclusive: {}",
            evidence.error_class.as_deref().unwrap_or("unclassified")
        ));
    }
    row.classification = classification;
    row.removal_candidate = classification == Classification::PromoEnded;
    row.probed = true;
    row.cost = evidence.cost;
    row.error_class = evidence.error_class;
    row.http_status = evidence.http_status;
    row.probed_at = Some(rfc3339(probed_at));
    row
}

fn base_row(deployment: &Deployment, canonical_model: Arc<str>) -> VerifyRow {
    VerifyRow {
        leg_id: deployment.id.to_string(),
        canonical_model: canonical_model.to_string(),
        provider: deployment.provider.0.to_string(),
        upstream_host: deployment.base_url.host().unwrap_or("").to_string(),
        model: deployment.upstream_model.to_string(),
        credential_id: (!deployment.credential.is_empty())
            .then(|| deployment.credential.to_string()),
        classification: Classification::Unknown,
        removal_candidate: false,
        probed: false,
        cost: None,
        error_class: None,
        http_status: None,
        probed_at: None,
        expires_at: deployment.expires_at.map(rfc3339),
        reason: None,
    }
}

fn rfc3339(stamp: SystemTime) -> String {
    let nanos: i128 = match stamp.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos() as i128,
        Err(error) => -(error.duration().as_nanos() as i128),
    };
    OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .expect("config timestamp out of range")
        .format(&Rfc3339)
        .expect("RFC3339 formatting cannot fail")
}
