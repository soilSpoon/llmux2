use llmux_config::{ConfigInventory, DeploymentInventory, ModelInventory};
use llmux_types::{Billing, FixedUnit, QuotaSpec};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::File,
    io::{BufRead, BufReader},
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime},
};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

pub struct ExportOptions {
    pub config: PathBuf,
    pub request_log: Option<PathBuf>,
    pub runtime_deployments: Option<PathBuf>,
    pub now: Option<String>,
    pub no_request_log: bool,
}

pub fn run(options: ExportOptions) -> Result<String, String> {
    let config_input = input_provenance(&options.config);
    let inventory = ConfigInventory::load(&options.config)
        .map_err(|err| format!("loading config inventory failed: {err}"))?;
    let now = match options.now.as_deref() {
        Some(text) => OffsetDateTime::parse(text, &Rfc3339)
            .map_err(|err| format!("--now must be RFC 3339: {err}"))?,
        None => OffsetDateTime::now_utc(),
    };
    let explicit_log = options.request_log.is_some();
    let log_path = if options.no_request_log {
        None
    } else {
        options
            .request_log
            .clone()
            .or_else(|| inventory.request_log.clone())
    };
    let request_log_state = if options.no_request_log {
        "disabled_by_flag"
    } else if explicit_log {
        "provided"
    } else if log_path.is_some() {
        "configured_default"
    } else {
        "not_configured"
    };
    let request_log_input = log_path.as_ref().map(input_provenance);
    let log = match log_path {
        Some(path) => Some(read_request_log(&path, unix_ms(now))?),
        None => None,
    };
    let runtime_input = options.runtime_deployments.as_ref().map(input_provenance);
    let runtime = match &options.runtime_deployments {
        Some(path) => Some(read_runtime_deployments(path)?),
        None => None,
    };
    let inputs = ExportInputs {
        config: config_input,
        request_log: request_log_input,
        request_log_state,
        runtime_deployments: runtime_input,
    };
    let export = build_export(&inventory, log.as_ref(), runtime.as_ref(), now, inputs);
    serde_json::to_string_pretty(&export).map_err(|err| format!("serializing export failed: {err}"))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UpstreamExport {
    generated_at: String,
    source: &'static str,
    inputs: ExportInputs,
    rows: Vec<UpstreamRow>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportInputs {
    config: InputProvenance,
    request_log: Option<InputProvenance>,
    request_log_state: &'static str,
    runtime_deployments: Option<InputProvenance>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InputProvenance {
    path: String,
    modified_at: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UpstreamRow {
    model: String,
    provider_model_id: Option<String>,
    legs: Vec<ExportLeg>,
    free_or_membership: Option<&'static str>,
    quota_windows: Vec<QuotaWindowOut>,
    remaining_units: Option<u64>,
    reset_at: Option<String>,
    marginal_cost_per1k_req: Option<f64>,
    parked: Option<bool>,
    recent429: Option<Vec<Recent429>>,
    attempted_units1h: Option<u64>,
    attempted_units24h: Option<u64>,
    served_units1h: Option<u64>,
    served_units24h: Option<u64>,
    zero_candidate_units1h: Option<u64>,
    zero_candidate_units24h: Option<u64>,
    capacity_units_per_window: Option<u64>,
    capacity_saturation: Option<f64>,
    last_error_kind: Option<String>,
    median_failure_latency_ms: Option<u64>,
    availability: BTreeMap<String, String>,
    field_sources: BTreeMap<String, String>,
    source: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportLeg {
    id: String,
    credential_id: Option<String>,
    provider: String,
    upstream_model: String,
    priority: i32,
    billing: &'static str,
    shared_quota: Option<String>,
    parked: Option<bool>,
    park_reasons: Vec<&'static str>,
    reset_at: Option<String>,
    remaining_units: Option<u64>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct QuotaWindowOut {
    unit: &'static str,
    max: u64,
    span_secs: u64,
    kind: &'static str,
    scope: String,
    scope_id: String,
    anchor: Option<String>,
    reset_at: Option<String>,
    used_units: Option<u64>,
    remaining_units: Option<u64>,
    source: &'static str,
    usage_source: Option<&'static str>,
    confidence: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Recent429 {
    at: String,
    count: u64,
    kind: String,
}

#[derive(Deserialize)]
struct RuntimeDeployments {
    deployments: Vec<RuntimeDeployment>,
}

#[derive(Deserialize)]
struct RuntimeDeployment {
    id: String,
    disabled: Option<bool>,
    not_provisioned: Option<bool>,
    expired: Option<bool>,
    cooldown_ms: Option<u64>,
}

#[derive(Deserialize)]
struct LogRecord {
    ts_ms: u64,
    model: String,
    outcome: String,
    error_kind: Option<String>,
    latency_ms: u64,
    attempts: u64,
    #[serde(default)]
    legs: Vec<LogLeg>,
}

#[derive(Deserialize)]
struct LogLeg {
    deployment: String,
    outcome: String,
}

#[derive(Default)]
struct LogData {
    models: HashMap<String, ModelLog>,
    deployment_requests: HashMap<String, Vec<u64>>,
    first_ts: Option<u64>,
}

#[derive(Default)]
struct ModelLog {
    attempted1h: u64,
    attempted24h: u64,
    served1h: u64,
    served24h: u64,
    zero1h: u64,
    zero24h: u64,
    failure_latencies24h: Vec<u64>,
    last_error24h: Option<(u64, String)>,
    recent429: BTreeMap<String, (u64, u64)>,
}

fn input_provenance(path: &PathBuf) -> InputProvenance {
    let modified_at = std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .map(format_time);
    InputProvenance {
        path: path.display().to_string(),
        modified_at,
    }
}

fn read_request_log(path: &PathBuf, now_ms: u64) -> Result<LogData, String> {
    let file = File::open(path)
        .map_err(|err| format!("reading request log {} failed: {err}", path.display()))?;
    let mut data = LogData::default();
    for (line_no, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|err| {
            format!(
                "reading request log {} line {} failed: {err}",
                path.display(),
                line_no + 1
            )
        })?;
        if line.trim().is_empty() {
            continue;
        }
        let record: LogRecord = serde_json::from_str(&line).map_err(|err| {
            format!(
                "request log {} line {} is invalid JSON: {err}",
                path.display(),
                line_no + 1
            )
        })?;
        data.first_ts = Some(
            data.first_ts
                .map_or(record.ts_ms, |first| first.min(record.ts_ms)),
        );
        let model = data.models.entry(record.model.clone()).or_default();
        model.ingest(&record, now_ms);
        for leg in record.legs {
            data.deployment_requests
                .entry(leg.deployment)
                .or_default()
                .push(record.ts_ms);
        }
    }
    for timestamps in data.deployment_requests.values_mut() {
        timestamps.sort_unstable();
    }
    Ok(data)
}

impl ModelLog {
    fn ingest(&mut self, record: &LogRecord, now_ms: u64) {
        let day_start = now_ms.saturating_sub(86_400_000);
        let hour_start = now_ms.saturating_sub(3_600_000);
        if record.ts_ms < day_start {
            return;
        }
        let in_hour = record.ts_ms >= hour_start;
        self.attempted24h += 1;
        self.attempted1h += u64::from(in_hour);
        if record.outcome == "routed" {
            self.served24h += 1;
            self.served1h += u64::from(in_hour);
        }
        if record.attempts == 0 {
            self.zero24h += 1;
            self.zero1h += u64::from(in_hour);
        }
        if record.outcome == "error" {
            self.failure_latencies24h.push(record.latency_ms);
            if let Some(kind) = &record.error_kind {
                match self.last_error24h {
                    Some((at, _)) if at > record.ts_ms => {}
                    _ => self.last_error24h = Some((record.ts_ms, kind.clone())),
                }
            }
        }
        let mut saw_leg_limit = false;
        for leg in &record.legs {
            if matches!(leg.outcome.as_str(), "rate_limited" | "quota_exhausted") {
                saw_leg_limit = true;
                let entry = self.recent429.entry(leg.outcome.clone()).or_default();
                entry.0 += 1;
                entry.1 = entry.1.max(record.ts_ms);
            }
        }
        if !saw_leg_limit && record.legs.is_empty() && record.outcome == "error" {
            if let Some(kind) = &record.error_kind {
                if matches!(kind.as_str(), "rate_limited" | "quota_exhausted") {
                    let entry = self.recent429.entry(kind.clone()).or_default();
                    entry.0 += 1;
                    entry.1 = entry.1.max(record.ts_ms);
                }
            }
        }
    }
}

fn read_runtime_deployments(path: &PathBuf) -> Result<RuntimeDeployments, String> {
    let text = std::fs::read_to_string(path).map_err(|err| {
        format!(
            "reading runtime deployments {} failed: {err}",
            path.display()
        )
    })?;
    serde_json::from_str(&text).map_err(|err| {
        format!(
            "runtime deployments {} is invalid JSON: {err}",
            path.display()
        )
    })
}

fn build_export(
    inventory: &ConfigInventory,
    log: Option<&LogData>,
    runtime: Option<&RuntimeDeployments>,
    now: OffsetDateTime,
    inputs: ExportInputs,
) -> UpstreamExport {
    let now_ms = unix_ms(now);
    let runtime_by_id: HashMap<&str, &RuntimeDeployment> = runtime
        .map(|runtime| {
            runtime
                .deployments
                .iter()
                .map(|deployment| (deployment.id.as_str(), deployment))
                .collect()
        })
        .unwrap_or_default();
    let rows = inventory
        .models
        .iter()
        .filter(|model| model.name.as_ref() != "*")
        .map(|model| build_row(inventory, model, log, &runtime_by_id, now, now_ms))
        .collect();
    UpstreamExport {
        generated_at: format_time(system_time(now)),
        source: "llmux2",
        inputs,
        rows,
    }
}

fn build_row(
    inventory: &ConfigInventory,
    model: &ModelInventory,
    log: Option<&LogData>,
    runtime_by_id: &HashMap<&str, &RuntimeDeployment>,
    now: OffsetDateTime,
    now_ms: u64,
) -> UpstreamRow {
    let mut built_legs = Vec::new();
    let mut all_windows = Vec::new();
    for deployment in &model.deployments {
        let built = build_leg(inventory, deployment, log, runtime_by_id, now, now_ms);
        all_windows.extend(built.windows.clone());
        built_legs.push(built);
    }
    let capacity_leg = built_legs
        .iter()
        .filter_map(|built| {
            built
                .leg
                .remaining_units
                .map(|remaining| (remaining, built))
        })
        .max_by_key(|(remaining, _)| *remaining)
        .map(|(_, built)| built);
    let (remaining_units, capacity) = capacity_leg
        .map(|built| {
            let capacity = built
                .windows
                .iter()
                .filter(|window| window.unit == "requests" && window.remaining_units.is_some())
                .max_by_key(|window| window.span_secs)
                .and_then(|window| {
                    window.used_units.map(|used| {
                        (
                            window.max,
                            if window.max == 0 {
                                None
                            } else {
                                Some(used as f64 / window.max as f64)
                            },
                        )
                    })
                });
            (built.leg.remaining_units, capacity)
        })
        .unwrap_or((None, None));
    let mut legs: Vec<_> = built_legs.into_iter().map(|built| built.leg).collect();
    legs.sort_by_key(|leg| leg.priority);

    let mut deduped = BTreeMap::new();
    for window in all_windows {
        let key = format!(
            "{}:{}:{}:{}:{}:{}",
            window.scope, window.scope_id, window.unit, window.max, window.span_secs, window.kind
        );
        deduped.entry(key).or_insert(window);
    }
    let quota_windows: Vec<_> = deduped.into_values().collect();

    let mut combined = CombinedLog::default();
    if let Some(log) = log {
        for name in
            std::iter::once(model.name.as_ref()).chain(model.aliases.iter().map(Arc::as_ref))
        {
            if let Some(model_log) = log.models.get(name) {
                combined.add(model_log);
            }
        }
    }

    let reset_at = quota_windows
        .iter()
        .filter_map(|window| window.reset_at.clone())
        .min();
    let free_count = legs.iter().filter(|leg| leg.billing == "free").count();
    let free_or_membership = if legs.is_empty() {
        None
    } else if free_count == legs.len() {
        Some("free")
    } else if free_count != 0 {
        Some("mixed")
    } else {
        Some("billed")
    };
    let marginal_cost = None;
    let parked = if !legs.is_empty() && legs.iter().all(|leg| leg.parked == Some(true)) {
        Some(true)
    } else if legs.iter().any(|leg| leg.parked == Some(false)) {
        Some(false)
    } else {
        None
    };

    let mut availability = BTreeMap::new();
    if remaining_units.is_none() {
        availability.insert(
            "remainingUnits".into(),
            "no leg has a fully reconstructable request-denominated remaining window; token windows cannot be converted to requests".into()
        );
    }
    if marginal_cost.is_none() {
        availability.insert(
            "marginalCostPer1kReq".into(),
            "direct paid prices are not present in llmux2 config, and the free/membership opportunity cost requires cross-model demand and value data outside this export".into(),
        );
    }
    if parked.is_none() {
        availability.insert(
            "parked".into(),
            "live runtime disable/cooldown state was not supplied".into(),
        );
    }
    if log.is_none() {
        for field in [
            "recent429",
            "attemptedUnits1h",
            "attemptedUnits24h",
            "servedUnits1h",
            "servedUnits24h",
            "zeroCandidateUnits1h",
            "zeroCandidateUnits24h",
            "lastErrorKind",
            "medianFailureLatencyMs",
        ] {
            availability.insert(field.into(), "request_log was not provided".into());
        }
    }
    if capacity.is_none() {
        availability.insert(
            "capacityUnitsPerWindow".into(),
            "no request-denominated configured window has full-log coverage".into(),
        );
        availability.insert(
            "capacitySaturation".into(),
            "capacity usage is unknown without full-window request-log coverage".into(),
        );
    }
    let mut field_sources = BTreeMap::new();
    field_sources.insert("legs".into(), "config".into());
    field_sources.insert(
        "quotaWindows".into(),
        "config; usage reconstructed from request_log when coverage is complete".into(),
    );
    field_sources.insert(
        "remainingUnits".into(),
        "conservative single-leg lower bound; overlapping shared quota scopes are not summed"
            .into(),
    );
    field_sources.insert(
        "parked".into(),
        if runtime_by_id.is_empty() {
            "config disabled only; live runtime missing".into()
        } else {
            "runtime deployments plus config".into()
        },
    );
    if log.is_some() {
        field_sources.insert("demandAndFailureFields".into(), "request_log".into());
    }

    let (capacity_units, capacity_saturation) = match capacity {
        Some((units, saturation)) => (Some(units), saturation),
        None => (None, None),
    };
    let recent429 = log.map(|_| combined.recent429());
    UpstreamRow {
        model: model.name.to_string(),
        provider_model_id: single_provider_model(model),
        legs,
        free_or_membership,
        quota_windows,
        remaining_units,
        reset_at,
        marginal_cost_per1k_req: marginal_cost,
        parked,
        recent429,
        attempted_units1h: log.map(|_| combined.attempted1h),
        attempted_units24h: log.map(|_| combined.attempted24h),
        served_units1h: log.map(|_| combined.served1h),
        served_units24h: log.map(|_| combined.served24h),
        zero_candidate_units1h: log.map(|_| combined.zero1h),
        zero_candidate_units24h: log.map(|_| combined.zero24h),
        capacity_units_per_window: capacity_units,
        capacity_saturation,
        last_error_kind: combined.last_error.clone(),
        median_failure_latency_ms: combined.median_failure_latency(),
        availability,
        field_sources,
        source: "llmux2",
    }
}

struct BuiltLeg {
    leg: ExportLeg,
    windows: Vec<QuotaWindowOut>,
}

fn build_leg(
    inventory: &ConfigInventory,
    deployment: &DeploymentInventory,
    log: Option<&LogData>,
    runtime_by_id: &HashMap<&str, &RuntimeDeployment>,
    now: OffsetDateTime,
    now_ms: u64,
) -> BuiltLeg {
    let runtime = runtime_by_id.get(deployment.id.as_ref()).copied();
    let mut windows = effective_windows(inventory, deployment);
    for window in &mut windows {
        fill_window_usage(window, inventory, deployment, log, now, now_ms);
    }
    let expired = deployment
        .expires_at
        .is_some_and(|end| system_time(now) >= end)
        || runtime.and_then(|state| state.expired).unwrap_or(false);
    let mut reasons = Vec::new();
    if deployment
        .credential
        .as_ref()
        .and_then(|id| inventory.credentials.get(id))
        .is_some_and(|credential| credential.disabled)
    {
        reasons.push("credential_disabled_in_config");
    }
    if runtime.and_then(|state| state.disabled).unwrap_or(false) {
        reasons.push("disabled_at_runtime");
    }
    if runtime
        .and_then(|state| state.not_provisioned)
        .unwrap_or(false)
    {
        reasons.push("not_provisioned");
    }
    if expired {
        reasons.push("expired");
    }
    if runtime.and_then(|state| state.cooldown_ms).is_some() {
        reasons.push("cooldown");
    }
    let parked = if !reasons.is_empty() {
        Some(true)
    } else if runtime.is_some() {
        Some(false)
    } else {
        None
    };
    let runtime_reset = runtime
        .and_then(|state| state.cooldown_ms)
        .map(|ms| format_time(system_time(now) + Duration::from_millis(ms)));
    let fixed_reset = windows
        .iter()
        .filter_map(|window| window.reset_at.clone())
        .min();
    let reset_at = [runtime_reset, fixed_reset].into_iter().flatten().min();
    let has_token_window = windows.iter().any(|window| window.unit == "tokens");
    let has_windows = !windows.is_empty();
    let remaining = if has_windows && !has_token_window {
        windows
            .iter()
            .try_fold(u64::MAX, |minimum, window| {
                window
                    .remaining_units
                    .map(|remaining| minimum.min(remaining))
            })
            .and_then(|value| (value != u64::MAX).then_some(value))
    } else {
        None
    };
    BuiltLeg {
        leg: ExportLeg {
            id: deployment.id.to_string(),
            credential_id: deployment.credential.as_ref().map(ToString::to_string),
            provider: deployment.provider.0.to_string(),
            upstream_model: deployment.upstream_model.to_string(),
            priority: deployment.priority,
            billing: match deployment.billing {
                Billing::Free => "free",
                Billing::Billed => "billed",
            },
            shared_quota: deployment.shared_quota.as_ref().map(ToString::to_string),
            parked,
            park_reasons: reasons,
            reset_at,
            remaining_units: remaining,
        },
        windows,
    }
}

#[derive(Clone)]
struct WindowKey {
    unit: &'static str,
    max: u64,
    span_secs: u64,
    kind: &'static str,
    scope: String,
    scope_id: String,
    anchor: Option<SystemTime>,
}

fn effective_windows(
    inventory: &ConfigInventory,
    deployment: &DeploymentInventory,
) -> Vec<QuotaWindowOut> {
    let mut windows = Vec::new();
    push_spec_windows(
        &mut windows,
        deployment.quota.clone(),
        "deployment",
        deployment.id.to_string(),
    );
    if let Some(credential_id) = &deployment.credential {
        if let Some(credential) = inventory.credentials.get(credential_id) {
            push_spec_windows(
                &mut windows,
                credential.quota.clone(),
                "credential",
                credential_id.to_string(),
            );
        }
    }
    if let Some(shared_name) = &deployment.shared_quota {
        if let Some(spec) = inventory.shared_quotas.get(shared_name) {
            let scope_id = match (&deployment.credential, &deployment.base_url) {
                (Some(credential), _) => format!("credential:{credential}:{shared_name}"),
                (None, base_url) => format!(
                    "anonymous:{}:{}:{shared_name}",
                    deployment.provider.0,
                    base_url.as_deref().unwrap_or("default")
                ),
            };
            push_spec_windows(&mut windows, spec.clone(), "shared_quota", scope_id);
        }
    }
    windows
}

fn push_spec_windows(
    windows: &mut Vec<QuotaWindowOut>,
    spec: QuotaSpec,
    scope: &str,
    scope_id: String,
) {
    for (max, span, unit) in [
        (spec.rpm, 60, "requests"),
        (spec.rpd, 86_400, "requests"),
        (spec.tpm, 60, "tokens"),
        (spec.tpd, 86_400, "tokens"),
    ] {
        if let Some(max) = max {
            windows.push(window_out(WindowKey {
                unit,
                max: u64::from(max),
                span_secs: span,
                kind: "sliding",
                scope: scope.to_string(),
                scope_id: scope_id.clone(),
                anchor: None,
            }));
        }
    }
    for fixed in spec.fixed {
        windows.push(window_out(WindowKey {
            unit: match fixed.unit {
                FixedUnit::Requests => "requests",
                FixedUnit::Tokens => "tokens",
            },
            max: u64::from(fixed.max),
            span_secs: u64::from(fixed.span_secs),
            kind: "fixed",
            scope: scope.to_string(),
            scope_id: scope_id.clone(),
            anchor: Some(fixed.anchor),
        }));
    }
}

fn window_out(key: WindowKey) -> QuotaWindowOut {
    QuotaWindowOut {
        unit: key.unit,
        max: key.max,
        span_secs: key.span_secs,
        kind: key.kind,
        scope: key.scope,
        scope_id: key.scope_id,
        anchor: key.anchor.map(format_time),
        reset_at: None,
        used_units: None,
        remaining_units: None,
        source: "config",
        usage_source: None,
        confidence: "configured_limit; usage unknown".into(),
    }
}

fn fill_window_usage(
    window: &mut QuotaWindowOut,
    inventory: &ConfigInventory,
    _deployment: &DeploymentInventory,
    log: Option<&LogData>,
    now: OffsetDateTime,
    now_ms: u64,
) {
    if window.kind == "fixed" {
        if let Some(anchor) = &window.anchor {
            if let Ok(anchor_time) = OffsetDateTime::parse(anchor, &Rfc3339) {
                let (_, end) = fixed_bounds(anchor_time, window.span_secs, now);
                window.reset_at = Some(format_time(system_time(end)));
            }
        }
    }
    if window.unit == "tokens" {
        window.confidence =
            "configured_limit; token usage cannot be reconstructed from the request-log contract"
                .into();
        return;
    }
    let Some(log) = log else {
        window.confidence = "configured_limit; request_log not provided".into();
        return;
    };
    let Some((start_ms, end_ms)) = window_bounds_ms(window, now, now_ms) else {
        return;
    };
    let deployment_ids = deployment_ids_for_scope(inventory, &window.scope, &window.scope_id);
    let used: u64 = deployment_ids
        .iter()
        .filter_map(|id| log.deployment_requests.get(id))
        .map(|timestamps| {
            let start = timestamps.partition_point(|ts| *ts < start_ms);
            let end = timestamps.partition_point(|ts| *ts < end_ms);
            (end - start) as u64
        })
        .sum();
    window.used_units = Some(used);
    window.usage_source = Some("request_log");
    let full_coverage = log.first_ts.is_some_and(|first| first <= start_ms);
    if full_coverage {
        window.remaining_units = Some(window.max.saturating_sub(used));
        window.confidence = "configured_limit; request-unit usage reconstructed from a full window of request_log, assuming no writer drops".into();
    } else {
        window.confidence = "configured_limit; request_log starts inside the window, so remainingUnits is unknown rather than overestimated".into();
    }
}

fn window_bounds_ms(
    window: &QuotaWindowOut,
    now: OffsetDateTime,
    now_ms: u64,
) -> Option<(u64, u64)> {
    if window.kind == "sliding" {
        let span_ms = window.span_secs.saturating_mul(1000);
        return Some((now_ms.saturating_sub(span_ms), now_ms));
    }
    let anchor = window.anchor.as_deref()?;
    let anchor = OffsetDateTime::parse(anchor, &Rfc3339).ok()?;
    let (start, end) = fixed_bounds(anchor, window.span_secs, now);
    Some((unix_ms(start), unix_ms(end)))
}

fn fixed_bounds(
    anchor: OffsetDateTime,
    span_secs: u64,
    now: OffsetDateTime,
) -> (OffsetDateTime, OffsetDateTime) {
    let span = span_secs.max(1) as i64;
    let index = (now.unix_timestamp() - anchor.unix_timestamp()).div_euclid(span);
    let start = anchor + time::Duration::seconds(index * span);
    (start, start + time::Duration::seconds(span))
}

fn deployment_ids_for_scope(
    inventory: &ConfigInventory,
    scope: &str,
    scope_id: &str,
) -> Vec<String> {
    let mut ids = HashSet::new();
    for model in &inventory.models {
        for deployment in &model.deployments {
            let matches = match scope {
                "deployment" => deployment.id.as_ref() == scope_id,
                "credential" => deployment.credential.as_deref() == Some(scope_id),
                "shared_quota" => deployment.shared_quota.as_ref().is_some_and(|name| {
                    let expected = match (&deployment.credential, &deployment.base_url) {
                        (Some(credential), _) => format!("credential:{credential}:{name}"),
                        (None, base_url) => format!(
                            "anonymous:{}:{}:{name}",
                            deployment.provider.0,
                            base_url.as_deref().unwrap_or("default")
                        ),
                    };
                    expected == scope_id
                }),
                _ => false,
            };
            if matches {
                ids.insert(deployment.id.to_string());
            }
        }
    }
    ids.into_iter().collect()
}

#[derive(Default)]
struct CombinedLog {
    attempted1h: u64,
    attempted24h: u64,
    served1h: u64,
    served24h: u64,
    zero1h: u64,
    zero24h: u64,
    failure_latencies24h: Vec<u64>,
    last_error: Option<String>,
    last_error_ts: u64,
    recent: BTreeMap<String, (u64, u64)>,
}

impl CombinedLog {
    fn add(&mut self, model: &ModelLog) {
        self.attempted1h += model.attempted1h;
        self.attempted24h += model.attempted24h;
        self.served1h += model.served1h;
        self.served24h += model.served24h;
        self.zero1h += model.zero1h;
        self.zero24h += model.zero24h;
        self.failure_latencies24h
            .extend(model.failure_latencies24h.iter().copied());
        if let Some((ts, kind)) = &model.last_error24h {
            if *ts >= self.last_error_ts {
                self.last_error_ts = *ts;
                self.last_error = Some(kind.clone());
            }
        }
        for (kind, (count, at)) in &model.recent429 {
            let entry = self.recent.entry(kind.clone()).or_default();
            entry.0 += count;
            entry.1 = entry.1.max(*at);
        }
    }

    fn median_failure_latency(&self) -> Option<u64> {
        let mut values = self.failure_latencies24h.clone();
        values.sort_unstable();
        match values.len() {
            0 => None,
            len if len % 2 == 1 => Some(values[len / 2]),
            len => Some((values[len / 2 - 1] + values[len / 2]) / 2),
        }
    }

    fn recent429(&self) -> Vec<Recent429> {
        self.recent
            .iter()
            .map(|(kind, (count, at))| Recent429 {
                at: format_time(SystemTime::UNIX_EPOCH + Duration::from_millis(*at)),
                count: *count,
                kind: kind.clone(),
            })
            .collect()
    }
}

fn single_provider_model(model: &ModelInventory) -> Option<String> {
    let mut ids = model
        .deployments
        .iter()
        .map(|deployment| deployment.upstream_model.as_ref());
    let first = ids.next()?;
    ids.all(|id| id == first).then(|| first.to_string())
}

fn unix_ms(time: OffsetDateTime) -> u64 {
    u64::try_from(time.unix_timestamp_nanos() / 1_000_000).unwrap_or(u64::MAX)
}

fn system_time(time: OffsetDateTime) -> SystemTime {
    SystemTime::UNIX_EPOCH
        + Duration::from_nanos(u64::try_from(time.unix_timestamp_nanos()).unwrap_or(0))
}

fn format_time(time: SystemTime) -> String {
    OffsetDateTime::from(time)
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into())
}
