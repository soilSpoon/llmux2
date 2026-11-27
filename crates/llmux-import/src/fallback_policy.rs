use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashSet, path::Path};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

pub const FALLBACK_POLICY_SCHEMA_VERSION: u32 = 1;
pub const FALLBACK_AUDIT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FallbackDecision {
    Suppress,
    Retain,
    TransitionRetain,
    HoldExistingUnresolved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SpellingAction {
    Alias,
    Drop,
    RetainCatchall,
    NeedsReview,
    AlreadyResolved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DecisionSource {
    AutomaticRule,
    ManualOverride,
    UserApproved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NativeCoverage {
    SuccessObserved,
    QuotaParkedAfterSuccess,
    ReviewedStatic,
    PresentUnverified,
    Failing,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnverifiedNativeDefault {
    RetainExistingFallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnclassifiedCatchallDefault {
    ReportOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CatchAllMode {
    Transition,
    Strict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuditAction {
    EmittedFallback,
    SuppressedFallback,
    AliasAdded,
    SpellingDropped,
    CatchallRetained,
    NeedsReview,
    NeedsVerification,
    AlreadyResolved,
    HeldExistingUnresolved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FallbackPolicy {
    pub schema_version: u32,
    pub generated_at: String,
    pub inputs: PolicyInputs,
    pub defaults: PolicyDefaults,
    pub models: Vec<PolicyModel>,
    pub spellings: Vec<PolicySpelling>,
    pub catch_all: CatchAllPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyInputs {
    pub inventory_sha256: String,
    pub override_sha256: String,
    pub planner_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyDefaults {
    pub unverified_native: UnverifiedNativeDefault,
    pub unclassified_catchall_spelling: UnclassifiedCatchallDefault,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyModel {
    pub name: String,
    pub fallback: FallbackDecision,
    pub reason: String,
    pub evidence: PolicyEvidence,
    pub provenance: PolicyProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyEvidence {
    pub inventory_row: String,
    pub native_legs: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<NativeCoverage>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyProvenance {
    pub decision_source: DecisionSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicySpelling {
    pub spelling: String,
    pub action: SpellingAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub reason: String,
    pub provenance: PolicyProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatchAllPolicy {
    pub mode: CatchAllMode,
    pub retain: Vec<CatchAllSpelling>,
    pub drop: Vec<CatchAllSpelling>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatchAllSpelling {
    pub spelling: String,
    pub reason: String,
    pub provenance: PolicyProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FallbackAudit {
    pub schema_version: u32,
    pub input_hashes: AuditInputHashes,
    pub summary: AuditSummary,
    pub decisions: Vec<AuditDecision>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditInputHashes {
    pub export: String,
    pub overlay: String,
    pub fallback_policy: String,
    pub importer_binary_or_commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditSummary {
    pub fallback_legs_emitted: usize,
    pub fallback_legs_suppressed: usize,
    pub aliases_added: usize,
    pub spellings_dropped: usize,
    pub catchall_retained: usize,
    pub needs_review: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditDecision {
    pub subject: String,
    pub action: AuditAction,
    pub reason: String,
    pub native_legs: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub policy_provenance: PolicyProvenance,
}

pub fn parse_fallback_policy(input: &str) -> Result<FallbackPolicy, String> {
    let raw: Value = serde_json::from_str(input)
        .map_err(|error| format!("invalid fallback policy JSON: {error}"))?;
    redaction_scan(&raw, "$")?;
    let policy: FallbackPolicy = serde_json::from_value(raw)
        .map_err(|error| format!("invalid fallback policy JSON: {error}"))?;
    validate_policy(&policy)?;
    Ok(policy)
}

pub fn serialize_fallback_audit(audit: &FallbackAudit) -> Result<String, String> {
    validate_audit(audit)?;
    let mut canonical = audit.clone();
    canonicalize_audit(&mut canonical);
    redaction_scan(
        &serde_json::to_value(&canonical).map_err(|error| {
            format!("could not encode fallback audit for redaction check: {error}")
        })?,
        "$",
    )?;
    serde_json::to_string_pretty(&canonical)
        .map_err(|error| format!("could not encode fallback audit JSON: {error}"))
}

pub fn write_fallback_audit(path: &Path, audit: &FallbackAudit) -> Result<(), String> {
    std::fs::write(path, serialize_fallback_audit(audit)?)
        .map_err(|error| format!("could not write fallback audit {}: {error}", path.display()))
}

fn validate_policy(policy: &FallbackPolicy) -> Result<(), String> {
    if policy.schema_version != FALLBACK_POLICY_SCHEMA_VERSION {
        return Err(format!(
            "fallback policy schema_version must be {FALLBACK_POLICY_SCHEMA_VERSION}"
        ));
    }
    validate_timestamp(&policy.generated_at, "generated_at")?;
    validate_sha256(&policy.inputs.inventory_sha256, "inputs.inventory_sha256")?;
    validate_sha256(&policy.inputs.override_sha256, "inputs.override_sha256")?;
    validate_nonempty(&policy.inputs.planner_version, "inputs.planner_version")?;

    let mut model_names = HashSet::new();
    for (index, model) in policy.models.iter().enumerate() {
        let path = format!("models[{index}]");
        validate_nonempty(&model.name, &format!("{path}.name"))?;
        validate_nonempty(&model.reason, &format!("{path}.reason"))?;
        if !model_names.insert(model.name.clone()) {
            return Err(format!("{path}.name duplicates a fallback policy model"));
        }
        validate_nonempty(
            &model.evidence.inventory_row,
            &format!("{path}.evidence.inventory_row"),
        )?;
        for (leg_index, leg) in model.evidence.native_legs.iter().enumerate() {
            validate_nonempty(leg, &format!("{path}.evidence.native_legs[{leg_index}]"))?;
        }
        validate_provenance(&model.provenance, &format!("{path}.provenance"))?;
    }

    let mut spellings = HashSet::new();
    for (index, spelling) in policy.spellings.iter().enumerate() {
        let path = format!("spellings[{index}]");
        validate_spelling_row(
            &spelling.spelling,
            spelling.target.as_deref(),
            matches!(
                spelling.action,
                SpellingAction::Alias | SpellingAction::AlreadyResolved
            ),
            &spelling.reason,
            &spelling.provenance,
            &path,
        )?;
        if !spellings.insert(spelling.spelling.clone()) {
            return Err(format!(
                "{path}.spelling duplicates a fallback policy spelling"
            ));
        }
    }

    let mut catchall_spellings = HashSet::new();
    for (field, rows) in [
        ("retain", &policy.catch_all.retain),
        ("drop", &policy.catch_all.drop),
    ] {
        for (index, row) in rows.iter().enumerate() {
            let path = format!("catch_all.{field}[{index}]");
            validate_spelling_row(
                &row.spelling,
                None,
                false,
                &row.reason,
                &row.provenance,
                &path,
            )?;
            if !catchall_spellings.insert(row.spelling.clone()) {
                return Err(format!(
                    "{path}.spelling duplicates a catch-all policy spelling"
                ));
            }
        }
    }

    Ok(())
}

fn validate_spelling_row(
    spelling: &str,
    target: Option<&str>,
    target_required: bool,
    reason: &str,
    provenance: &PolicyProvenance,
    path: &str,
) -> Result<(), String> {
    validate_nonempty(spelling, &format!("{path}.spelling"))?;
    validate_nonempty(reason, &format!("{path}.reason"))?;
    if target_required && target.is_none_or(str::is_empty) {
        return Err(format!(
            "{path}.target is required for this spelling action"
        ));
    }
    if let Some(target) = target {
        validate_nonempty(target, &format!("{path}.target"))?;
    }
    validate_provenance(provenance, &format!("{path}.provenance"))
}

fn validate_provenance(provenance: &PolicyProvenance, path: &str) -> Result<(), String> {
    if provenance.decision_source == DecisionSource::AutomaticRule
        && provenance.rule.as_deref().is_none_or(str::is_empty)
    {
        return Err(format!(
            "{path}.rule is required for automatic-rule provenance"
        ));
    }
    if provenance.decision_source == DecisionSource::UserApproved
        && provenance
            .approval_source
            .as_deref()
            .is_none_or(str::is_empty)
    {
        return Err(format!(
            "{path}.approval_source is required for user-approved provenance"
        ));
    }
    for (field, value) in [
        ("rule", &provenance.rule),
        ("approved_by", &provenance.approved_by),
        ("approval_source", &provenance.approval_source),
        ("evidence", &provenance.evidence),
    ] {
        if let Some(value) = value {
            validate_nonempty(value, &format!("{path}.{field}"))?;
        }
    }
    Ok(())
}

fn validate_audit(audit: &FallbackAudit) -> Result<(), String> {
    if audit.schema_version != FALLBACK_AUDIT_SCHEMA_VERSION {
        return Err(format!(
            "fallback audit schema_version must be {FALLBACK_AUDIT_SCHEMA_VERSION}"
        ));
    }
    validate_sha256(&audit.input_hashes.export, "input_hashes.export")?;
    validate_sha256(&audit.input_hashes.overlay, "input_hashes.overlay")?;
    validate_sha256(
        &audit.input_hashes.fallback_policy,
        "input_hashes.fallback_policy",
    )?;
    validate_nonempty(
        &audit.input_hashes.importer_binary_or_commit,
        "input_hashes.importer_binary_or_commit",
    )?;

    for (index, decision) in audit.decisions.iter().enumerate() {
        let path = format!("decisions[{index}]");
        validate_nonempty(&decision.subject, &format!("{path}.subject"))?;
        validate_nonempty(&decision.reason, &format!("{path}.reason"))?;
        for (leg_index, leg) in decision.native_legs.iter().enumerate() {
            validate_nonempty(leg, &format!("{path}.native_legs[{leg_index}]"))?;
        }
        if matches!(
            decision.action,
            AuditAction::AliasAdded | AuditAction::AlreadyResolved
        ) && decision.target.as_deref().is_none_or(str::is_empty)
        {
            return Err(format!("{path}.target is required for this audit action"));
        }
        if let Some(target) = &decision.target {
            validate_nonempty(target, &format!("{path}.target"))?;
        }
        validate_provenance(
            &decision.policy_provenance,
            &format!("{path}.policy_provenance"),
        )?;
    }

    let expected = decision_summary(&audit.decisions);
    if audit.summary != expected {
        return Err("fallback audit summary does not match decision actions".to_owned());
    }
    Ok(())
}

fn decision_summary(decisions: &[AuditDecision]) -> AuditSummary {
    let mut summary = AuditSummary {
        fallback_legs_emitted: 0,
        fallback_legs_suppressed: 0,
        aliases_added: 0,
        spellings_dropped: 0,
        catchall_retained: 0,
        needs_review: 0,
    };
    for decision in decisions {
        match decision.action {
            AuditAction::EmittedFallback => summary.fallback_legs_emitted += 1,
            AuditAction::SuppressedFallback => summary.fallback_legs_suppressed += 1,
            AuditAction::AliasAdded => summary.aliases_added += 1,
            AuditAction::SpellingDropped => summary.spellings_dropped += 1,
            AuditAction::CatchallRetained => summary.catchall_retained += 1,
            AuditAction::NeedsReview
            | AuditAction::NeedsVerification
            | AuditAction::HeldExistingUnresolved => summary.needs_review += 1,
            AuditAction::AlreadyResolved => {}
        }
    }
    summary
}

fn canonicalize_audit(audit: &mut FallbackAudit) {
    for decision in &mut audit.decisions {
        decision.native_legs.sort();
    }
    audit.decisions.sort_by(|left, right| {
        left.subject
            .cmp(&right.subject)
            .then(left.action.cmp(&right.action))
            .then(left.reason.cmp(&right.reason))
            .then(left.target.cmp(&right.target))
            .then(left.native_legs.cmp(&right.native_legs))
            .then(left.policy_provenance.cmp(&right.policy_provenance))
    });
}

fn validate_nonempty(value: &str, path: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{path} must be nonempty"));
    }
    Ok(())
}

fn validate_timestamp(value: &str, path: &str) -> Result<(), String> {
    OffsetDateTime::parse(value, &Rfc3339)
        .map(|_| ())
        .map_err(|_| format!("{path} must be an RFC 3339 timestamp"))
}

fn validate_sha256(value: &str, path: &str) -> Result<(), String> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Ok(());
    }
    Err(format!("{path} must be a lowercase SHA-256 hex digest"))
}

fn redaction_scan(value: &Value, path: &str) -> Result<(), String> {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if secret_like(key) {
                    return Err(format!(
                        "{path} contains an object key with a secret reference or token that must not appear in fallback artifacts"
                    ));
                }
                let normalized: String = key
                    .bytes()
                    .filter(|byte| byte.is_ascii_alphanumeric())
                    .map(|byte| byte.to_ascii_lowercase() as char)
                    .collect();
                if normalized.contains("credential")
                    || normalized.contains("clientkey")
                    || normalized.contains("apikey")
                    || normalized.contains("token")
                    || normalized.contains("secret")
                    || normalized == "key"
                {
                    return Err(format!(
                        "{path} must not copy credential, client key, token, or secret metadata in an object key"
                    ));
                }
                redaction_scan(value, &format!("{path}.{key}"))?;
            }
        }
        Value::Array(items) => {
            for (index, value) in items.iter().enumerate() {
                redaction_scan(value, &format!("{path}[{index}]"))?;
            }
        }
        Value::String(text) if secret_like(text) => {
            return Err(format!(
                "{path} contains a secret reference or token that must not appear in fallback artifacts"
            ));
        }
        _ => {}
    }
    Ok(())
}

fn secret_like(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("begin private key")
        || ["env:", "file:", "sk-", "ghp_", "gho_", "xoxb-", "xoxp-"]
            .iter()
            .any(|marker| has_secret_marker(&lower, marker))
}

fn has_secret_marker(text: &str, marker: &str) -> bool {
    text.match_indices(marker)
        .any(|(index, _)| index == 0 || is_secret_marker_boundary(text.as_bytes()[index - 1]))
}

fn is_secret_marker_boundary(byte: u8) -> bool {
    !byte.is_ascii_alphanumeric()
}
