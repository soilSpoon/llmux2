use llmux_import::fallback_policy::{
    parse_fallback_policy, serialize_fallback_audit, write_fallback_audit, AuditAction,
    AuditDecision, AuditInputHashes, AuditSummary, DecisionSource, FallbackAudit, FallbackDecision,
    PolicyProvenance, SpellingAction,
};
use serde_json::{json, Value};

fn valid_policy_json() -> &'static str {
    include_str!("fixtures/fallback-policy-valid.json")
}

fn hash(byte: char) -> String {
    byte.to_string().repeat(64)
}

fn automatic_provenance(rule: &str) -> PolicyProvenance {
    PolicyProvenance {
        decision_source: DecisionSource::AutomaticRule,
        rule: Some(rule.to_owned()),
        approved_by: None,
        approval_source: None,
        evidence: None,
    }
}

fn decision(
    subject: &str,
    action: AuditAction,
    reason: &str,
    native_legs: Vec<&str>,
    target: Option<&str>,
) -> AuditDecision {
    AuditDecision {
        subject: subject.to_owned(),
        action,
        reason: reason.to_owned(),
        native_legs: native_legs.into_iter().map(str::to_owned).collect(),
        target: target.map(str::to_owned),
        policy_provenance: automatic_provenance("test-rule"),
    }
}

type AuditMutation = fn(&mut FallbackAudit);

fn audit() -> FallbackAudit {
    FallbackAudit {
        schema_version: 1,
        input_hashes: AuditInputHashes {
            export: hash('a'),
            overlay: hash('b'),
            fallback_policy: hash('c'),
            importer_binary_or_commit: "test-commit".to_owned(),
        },
        summary: AuditSummary {
            fallback_legs_emitted: 1,
            fallback_legs_suppressed: 1,
            aliases_added: 1,
            spellings_dropped: 1,
            catchall_retained: 1,
            needs_review: 1,
        },
        decisions: vec![
            decision(
                "canonical-model",
                AuditAction::SuppressedFallback,
                "native-working",
                vec!["canonical-model-z", "canonical-model-a"],
                None,
            ),
            decision(
                "catchall-spelling",
                AuditAction::CatchallRetained,
                "buyer-use-during-transition",
                vec![],
                None,
            ),
            decision(
                "dropped-spelling",
                AuditAction::SpellingDropped,
                "explicit-test-spelling",
                vec![],
                None,
            ),
            decision(
                "emitted-model",
                AuditAction::EmittedFallback,
                "buyer-use",
                vec![],
                None,
            ),
            decision(
                "needed-alias",
                AuditAction::AliasAdded,
                "reviewed spelling identity",
                vec![],
                Some("canonical-model"),
            ),
            decision(
                "unresolved-spelling",
                AuditAction::NeedsReview,
                "user decision required",
                vec![],
                None,
            ),
        ],
    }
}

#[test]
fn valid_policy_parses_wo177_states() {
    let policy = parse_fallback_policy(valid_policy_json()).unwrap();
    assert_eq!(policy.schema_version, 1);
    assert!(policy
        .models
        .iter()
        .any(|model| model.fallback == FallbackDecision::HoldExistingUnresolved));
    assert!(policy
        .spellings
        .iter()
        .any(|spelling| spelling.action == SpellingAction::NeedsReview));
    assert!(policy
        .spellings
        .iter()
        .any(|spelling| spelling.action == SpellingAction::AlreadyResolved));
}

#[test]
fn policy_rejects_missing_provenance() {
    let mut json: Value = serde_json::from_str(valid_policy_json()).unwrap();
    json["models"][0]
        .as_object_mut()
        .unwrap()
        .remove("provenance");
    let error = parse_fallback_policy(&json.to_string()).unwrap_err();
    assert!(error.contains("provenance"), "{error}");
}

#[test]
fn policy_rejects_secretbearing_metadata_fields() {
    for (object, field) in [
        ("/models/0", "credential_id"),
        ("/spellings/0", "client_key_label"),
    ] {
        let mut json: Value = serde_json::from_str(valid_policy_json()).unwrap();
        let target = json.pointer_mut(object).unwrap().as_object_mut().unwrap();
        target.insert(
            field.to_owned(),
            Value::String("synthetic-label".to_owned()),
        );
        let error = parse_fallback_policy(&json.to_string()).unwrap_err();
        assert!(error.contains(field), "{error}");
    }
}

#[test]
fn policy_rejects_secret_reference_strings() {
    let mut json: Value = serde_json::from_str(valid_policy_json()).unwrap();
    json["models"][0]["reason"] = json!("env:SYNTHETIC_KEY");
    let error = parse_fallback_policy(&json.to_string()).unwrap_err();
    assert!(error.contains("secret"), "{error}");
}

#[test]
fn user_approved_decisions_require_a_named_approval_source() {
    let mut json: Value = serde_json::from_str(valid_policy_json()).unwrap();
    let provenance = json["spellings"][2]["provenance"].as_object_mut().unwrap();
    provenance.remove("approval_source");
    let error = parse_fallback_policy(&json.to_string()).unwrap_err();
    assert!(error.contains("approval_source"), "{error}");
}

#[test]
fn alias_actions_require_a_target() {
    let mut json: Value = serde_json::from_str(valid_policy_json()).unwrap();
    json["spellings"][2]
        .as_object_mut()
        .unwrap()
        .remove("target");
    let error = parse_fallback_policy(&json.to_string()).unwrap_err();
    assert!(error.contains("target"), "{error}");
}

#[test]
fn policy_rejects_invalid_schema_scalars() {
    for (pointer, value, expected) in [
        ("/schema_version", json!(2), "schema_version"),
        ("/generated_at", json!("not-a-time"), "RFC 3339"),
        ("/inputs/inventory_sha256", json!("not-a-hash"), "SHA-256"),
        ("/inputs/planner_version", json!(""), "nonempty"),
        ("/models/0/evidence/native_legs/0", json!(""), "nonempty"),
    ] {
        let mut json: Value = serde_json::from_str(valid_policy_json()).unwrap();
        *json.pointer_mut(pointer).unwrap() = value;
        let error = parse_fallback_policy(&json.to_string()).unwrap_err();
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn policy_rejects_duplicate_subjects_and_ruleless_automatic_provenance() {
    let mut json: Value = serde_json::from_str(valid_policy_json()).unwrap();
    let model = json["models"][0].clone();
    json["models"].as_array_mut().unwrap().push(model);
    let error = parse_fallback_policy(&json.to_string()).unwrap_err();
    assert!(
        error.contains("duplicates a fallback policy model"),
        "{error}"
    );

    let mut json: Value = serde_json::from_str(valid_policy_json()).unwrap();
    let spelling = json["spellings"][0].clone();
    json["spellings"].as_array_mut().unwrap().push(spelling);
    let error = parse_fallback_policy(&json.to_string()).unwrap_err();
    assert!(
        error.contains("duplicates a fallback policy spelling"),
        "{error}"
    );

    let mut json: Value = serde_json::from_str(valid_policy_json()).unwrap();
    let row = json["catch_all"]["retain"][0].clone();
    json["catch_all"]["drop"].as_array_mut().unwrap().push(row);
    let error = parse_fallback_policy(&json.to_string()).unwrap_err();
    assert!(
        error.contains("duplicates a catch-all policy spelling"),
        "{error}"
    );

    let mut json: Value = serde_json::from_str(valid_policy_json()).unwrap();
    json["models"][0]["provenance"]
        .as_object_mut()
        .unwrap()
        .remove("rule");
    let error = parse_fallback_policy(&json.to_string()).unwrap_err();
    assert!(error.contains("rule is required"), "{error}");
}

#[test]
fn fallback_audit_rejects_invalid_schema_scalars() {
    let cases: [(AuditMutation, &str); 4] = [
        (
            |audit: &mut FallbackAudit| audit.schema_version = 2,
            "schema_version",
        ),
        (
            |audit: &mut FallbackAudit| audit.input_hashes.export = "bad".to_owned(),
            "SHA-256",
        ),
        (
            |audit: &mut FallbackAudit| audit.decisions[0].subject.clear(),
            "nonempty",
        ),
        (
            |audit: &mut FallbackAudit| audit.decisions[4].target = None,
            "target is required",
        ),
    ];
    for (mutate, expected) in cases {
        let mut audit = audit();
        mutate(&mut audit);
        let error = serialize_fallback_audit(&audit).unwrap_err();
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn fallback_audit_serializes_deterministically() {
    let expected = include_str!("fixtures/fallback-audit-expected.json").trim_end();
    let serialized = serialize_fallback_audit(&audit()).unwrap();
    assert_eq!(serialized, expected);

    let mut reordered = audit();
    reordered.decisions.reverse();
    for decision in &mut reordered.decisions {
        decision.native_legs.reverse();
    }
    assert_eq!(serialize_fallback_audit(&reordered).unwrap(), expected);
}

#[test]
fn fallback_audit_file_write_uses_the_same_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fallback-audit.json");
    write_fallback_audit(&path, &audit()).unwrap();
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        serialize_fallback_audit(&audit()).unwrap()
    );
}

#[test]
fn fallback_audit_rejects_secret_reference_strings() {
    let mut audit = audit();
    audit.decisions[0].reason = "file:/tmp/synthetic-secret".to_owned();
    let error = serialize_fallback_audit(&audit).unwrap_err();
    assert!(error.contains("secret"), "{error}");
}

#[test]
fn fallback_audit_rejects_a_summary_that_does_not_match_decisions() {
    let mut audit = audit();
    audit.summary.fallback_legs_emitted += 1;
    let error = serialize_fallback_audit(&audit).unwrap_err();
    assert!(error.contains("summary"), "{error}");
}
