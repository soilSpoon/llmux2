use llmux_types::upstream_expiry::{
    evidence_record_sha256, inventory_sha256_without_signature, inventory_signature_payload,
    lane_key_sha256, parse_regeneration_evidence_record, parse_server_deployment_evidence_record,
    parse_signed_inventory_pr1, render_wire_type_inventory, sha256_hex, to_canonical_json,
    wire_type_inventory, AttestationStatus, CapacitySubject, ConfirmationSatisfaction,
    EvidenceLaneKey, LifecycleState, ObservedDeploymentOutcome, ObservedRegenerationOutcome,
    RecoveryBlocker, RecoveryOrigin, RegenerationFact, RegenerationInclusionOutcome,
};
use serde_json::{json, Value};

const TS: &str = "2026-11-29T00:00:00.000000000Z";
const TS_LATER: &str = "2026-11-29T00:30:00.000000000Z";
const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const HASH_C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const SIG: &str = "ed25519:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

fn regeneration_lane_hash() -> String {
    let lane: EvidenceLaneKey = serde_json::from_value(regeneration_lane()).unwrap();
    lane_key_sha256(&lane).unwrap()
}
fn evidence_ref() -> Value {
    let lane_hash = regeneration_lane_hash();
    json!({"kind":"evidence_artifact","uri_or_journal_key":format!("evidence-artifact:regeneration_importer:{lane_hash}:1:1:{HASH_A}"),"observed_at":TS,"sha256":HASH_A})
}
fn subject() -> Value {
    json!({"tag":"provider_account","payload":{"upstream_account_key":"acct-1"}})
}
fn upstream_ref() -> Value {
    json!({"identity":{"provider_key":"xai","endpoint":{"scheme":"https","host":"api.example.com","port":443,"base_path":"/"},"subject":subject()},"upstream_model":"grok-test"})
}
fn regeneration_lane() -> Value {
    json!({"kind":"regeneration_importer","logical_path":"/evidence/regeneration.jsonl"})
}
fn deployment_lane() -> Value {
    json!({"kind":"server_deployment","logical_path":"/evidence/deployment.jsonl","server_instance_id":"server-a"})
}
fn source_cursors() -> Value {
    let lane = regeneration_lane();
    json!({"request_log":{"inode":99,"byte_offset":10,"generation":1,"dropped_records_total":0,"last_record_ts":TS,"trustworthy":true,"gap_reason":null},"catalogs":{"xai":{"last_valid_generation":1,"last_valid_hash":HASH_B,"last_valid_at":TS,"last_attempt_at":TS,"consecutive_invalid":0}},"evidence_lanes":[{"lane":lane,"cursor":{"lane":lane,"log_epoch":1,"last_contiguous_artifact_seq":1,"last_contiguous_record_sha256":HASH_A,"next_scan_byte_offset":128,"quality":{"kind":"valid"},"deferred_records":[],"updated_at":TS}}]})
}
fn lifecycle_evidence() -> Value {
    json!({"absence_run_count":0,"first_valid_absent_at":null,"latest_valid_absent_at":null,"latest_valid_absent_generation":null,"exact_notfound_24h":{"window":86400000,"events":[]},"latest_runtime_success_at":null,"latest_zero_cost_success_at":null,"latest_present_any_at":null,"latest_present_any_generation":null,"latest_present_free_authoritative_at":null,"latest_present_notfree_at":null,"latest_present_free_unconfirmed_at":null,"failed_recovery_attempts_7d":{"window":604800000,"events":[]},"latest_exact_notfound":null})
}
fn recovery_evaluation() -> Value {
    json!({"evaluated_at":TS,"evaluated_generation":2,"confirmation_state":{"tag":"pending_catalog_confirmation","payload":null},"inclusion_state":"pending","loaded_deployment_state":"pending","ready_for_serving":false,"blocking_reasons":[],"required_evidence_inputs":[{"input":"deployment_evidence","requirement":"pending","required_lanes":[deployment_lane()]},{"input":"regeneration_evidence","requirement":"pending","required_lanes":[regeneration_lane()]}],"failed_attempts_7d_count":0,"evidence_refs":[]})
}
fn state_entry() -> Value {
    json!({"upstream_ref":upstream_ref(),"lifecycle":{"tag":"quarantined","payload":{"sources":["catalog_absent"]}},"cause_states":[],"evidence":lifecycle_evidence(),"state_since":TS,"last_transition_seq":1,"last_transition_generation":2,"recovery_evaluation":recovery_evaluation(),"adapter_confidence":"exact_machine","operator_attestations":[],"holds":{"active":[{"key":"rate_limited","value":{"kind":"rate_limited","subject":subject(),"until":TS_LATER,"reason":evidence_ref()}}]}})
}
fn signed_inventory() -> Value {
    json!({"schema_version":10,"writer":"llmux2-health-reconcile","writer_instance":"instance-a","generation":2,"previous_generation":1,"previous_sha256":HASH_B,"generated_at":TS,"heartbeat_at":TS,"source_cursors":source_cursors(),"states":[{"key":upstream_ref(),"value":state_entry()}],"bindings":[{"key":"cred-local-1","value":{"local_credential_ref":"cred-local-1","provider_key":"xai","endpoint":upstream_ref()["identity"]["endpoint"],"subject":subject(),"first_seen_config_generation":1,"latest_seen_config_generation":2}}],"scope_resolutions":[],"transition_watermark":1,"signature":SIG})
}
fn refresh_record_hash(mut record: Value) -> Value {
    let mut unsigned = record.clone();
    unsigned.as_object_mut().unwrap().remove("record_sha256");
    let digest = sha256_hex(to_canonical_json(&unsigned).unwrap().as_bytes());
    record["record_sha256"] = json!(digest);
    record
}
fn regeneration_record() -> Value {
    refresh_record_hash(
        json!({"schema_version":3,"artifact_seq":1,"log_epoch":1,"previous_record_sha256":null,"record_sha256":HASH_A,"continuity_reinitialization":null,"import_run_id":"import-1","started_at":TS,"finished_at":TS,"input_inventory_generation":2,"input_inventory_sha256":HASH_B,"config_generation":7,"config_sha256":HASH_C,"fingerprint_algorithm":"v1","status":{"tag":"success","payload":null},"failure_coverage":null,"rows":[{"upstream_ref":upstream_ref(),"row":{"inclusion":{"tag":"included","payload":null},"expected_deployment_fingerprint":HASH_C}}]}),
    )
}
fn deployment_record() -> Value {
    refresh_record_hash(
        json!({"schema_version":3,"artifact_seq":1,"log_epoch":1,"previous_record_sha256":null,"record_sha256":HASH_A,"continuity_reinitialization":null,"server_instance_id":"server-a","boot_id":"boot-1","record_kind":"config_load","emitted_at":TS,"load_started_at":TS,"load_finished_at":TS,"input_inventory_generation":2,"input_inventory_sha256":HASH_B,"loaded_config_generation":7,"loaded_config_sha256":HASH_C,"fingerprint_algorithm":"v1","traffic_role":"active_traffic","rows":[{"upstream_ref":upstream_ref(),"row":{"present":true,"actual_deployment_fingerprint":HASH_C}}]}),
    )
}
fn mutated(mut value: Value, path: &[&str], f: impl FnOnce(&mut Value)) -> Value {
    let mut target = &mut value;
    for key in path {
        target = if let Ok(index) = key.parse::<usize>() {
            target.get_mut(index).expect("test path exists")
        } else {
            target.get_mut(key).expect("test path exists")
        };
    }
    f(target);
    value
}
fn text(value: &Value) -> String {
    serde_json::to_string(value).unwrap()
}

#[test]
fn signed_inventory_projection_round_trips_schema_current_shape() {
    let parsed =
        parse_signed_inventory_pr1(&text(&signed_inventory())).expect("valid projection parses");
    let canonical = to_canonical_json(&parsed).expect("canonical serialization");
    let reparsed = parse_signed_inventory_pr1(&canonical).expect("canonical output reparses");
    assert_eq!(parsed, reparsed);
    assert_eq!(to_canonical_json(&reparsed).unwrap(), canonical);
    assert!(canonical.contains("\"bindings\""));
    assert!(canonical.contains("\"sources\":[\"catalog_absent\"]"));
    assert!(canonical.contains("\"required_evidence_inputs\":[{\"input\":\"deployment_evidence\""));
    assert!(canonical.contains("\"holds\":{\"active\""));
}

#[test]
fn parser_rejects_wrong_quarantined_shape_and_empty_sources() {
    let as_unit = mutated(
        signed_inventory(),
        &["states", "0", "value", "lifecycle"],
        |v| *v = json!("quarantined"),
    );
    assert!(parse_signed_inventory_pr1(&text(&as_unit)).is_err());
    let empty = mutated(
        signed_inventory(),
        &["states", "0", "value", "lifecycle", "payload", "sources"],
        |v| *v = json!([]),
    );
    assert!(parse_signed_inventory_pr1(&text(&empty)).is_err());
}

#[test]
fn parser_rejects_required_inputs_as_plain_string_array() {
    let wrong = mutated(
        signed_inventory(),
        &[
            "states",
            "0",
            "value",
            "recovery_evaluation",
            "required_evidence_inputs",
        ],
        |v| *v = json!(["deployment_evidence", "regeneration_evidence"]),
    );
    assert!(parse_signed_inventory_pr1(&text(&wrong)).is_err());
}

#[test]
fn parser_rejects_duplicate_members_unknown_fields_and_later_pr_roots() {
    let good = text(&signed_inventory());
    let duplicate = good.replacen(
        "\"schema_version\":10",
        "\"schema_version\":10,\"schema_version\":10",
        1,
    );
    assert!(parse_signed_inventory_pr1(&duplicate).is_err());
    let with_discovery = mutated(signed_inventory(), &[], |v| {
        v.as_object_mut()
            .unwrap()
            .insert("discovery".into(), json!({}));
    });
    assert!(parse_signed_inventory_pr1(&text(&with_discovery)).is_err());
    let missing_optional_null = mutated(
        signed_inventory(),
        &["source_cursors", "request_log"],
        |v| {
            v.as_object_mut().unwrap().remove("gap_reason");
        },
    );
    assert!(parse_signed_inventory_pr1(&text(&missing_optional_null)).is_err());
}

#[test]
fn evidence_artifacts_round_trip_and_enforce_row_and_option_shape() {
    let regeneration = parse_regeneration_evidence_record(&text(&regeneration_record())).unwrap();
    let regeneration_canonical = to_canonical_json(&regeneration).unwrap();
    assert_eq!(
        parse_regeneration_evidence_record(&regeneration_canonical).unwrap(),
        regeneration
    );
    let deployment = parse_server_deployment_evidence_record(&text(&deployment_record())).unwrap();
    let deployment_canonical = to_canonical_json(&deployment).unwrap();
    assert_eq!(
        parse_server_deployment_evidence_record(&deployment_canonical).unwrap(),
        deployment
    );
    let flat_row = mutated(
        regeneration_record(),
        &["rows", "0"],
        |v| *v = json!({"upstream_ref":upstream_ref(),"inclusion":{"tag":"included","payload":null},"expected_deployment_fingerprint":HASH_C}),
    );
    assert!(parse_regeneration_evidence_record(&text(&refresh_record_hash(flat_row))).is_err());
    let missing_null = mutated(regeneration_record(), &[], |v| {
        v.as_object_mut().unwrap().remove("failure_coverage");
    });
    assert!(parse_regeneration_evidence_record(&text(&refresh_record_hash(missing_null))).is_err());
}

#[test]
fn canonicalization_sorts_objects_and_rejects_noncanonical_integer_input() {
    let value = json!({"z":1,"a":{"d":true,"b":null},"aa":"x"});
    assert_eq!(
        to_canonical_json(&value).unwrap(),
        "{\"a\":{\"b\":null,\"d\":true},\"aa\":\"x\",\"z\":1}"
    );
    let mut bad_number = signed_inventory();
    bad_number["generation"] = json!(2.5);
    assert!(parse_signed_inventory_pr1(&text(&bad_number)).is_err());
}

#[test]
fn machine_type_inventory_is_derived_and_covers_corrected_closed_list() {
    let inventory = wire_type_inventory();
    let names: Vec<_> = inventory.iter().map(|row| row.type_name).collect();
    for required in [
        "SignedInventory",
        "InventoryStateEntry",
        "SourceCursors",
        "CatalogGeneration",
        "QuarantineSource",
        "LocalCredentialBinding",
        "RecoveryEvaluation",
        "EvidenceInputRequirementSnapshot",
        "RegenerationFact",
        "CatalogCursor",
    ] {
        assert!(names.contains(&required), "missing {required}");
    }
    for row in inventory
        .iter()
        .filter(|row| row.fields.is_empty() && row.kind != "enum" && row.kind != "brand")
    {
        eprintln!("empty struct row: {}", row.type_name);
    }
    assert!(inventory
        .iter()
        .all(|row| !row.fields.is_empty() || row.kind == "enum" || row.kind == "brand"));
}

#[test]
fn hash_inputs_remove_fields_instead_of_nulling_and_signature_is_domain_separated() {
    let record = parse_regeneration_evidence_record(&text(&regeneration_record())).unwrap();
    let digest = evidence_record_sha256(&record).unwrap();
    assert_ne!(digest, HASH_A);
    let inventory = parse_signed_inventory_pr1(&text(&signed_inventory())).unwrap();
    let inventory_digest = inventory_sha256_without_signature(&inventory).unwrap();
    let payload = inventory_signature_payload(&inventory).unwrap();
    assert!(payload.starts_with(b"LLMUX2_INVENTORY_SIGNATURE_V1\0"));
    assert_ne!(&payload[31..], inventory_digest.as_bytes());
}

fn valid_deferred_inventory() -> Value {
    let mut value = signed_inventory();
    let cursor = &mut value["source_cursors"]["evidence_lanes"][0]["cursor"];
    cursor["last_contiguous_artifact_seq"] = json!(2);
    cursor["last_contiguous_record_sha256"] = json!(HASH_B);
    cursor["quality"] = json!({"kind":"deferred_unknown_inventory_generation"});
    cursor["deferred_records"] = json!([
        deferred_record(1, 0, HASH_A, HASH_A),
        deferred_record(2, 50, HASH_B, HASH_B)
    ]);
    value
}

fn inventory_with_deferred_records(records: Vec<Value>, tip: u64, tip_hash: &str) -> Value {
    let mut value = valid_deferred_inventory();
    let cursor = &mut value["source_cursors"]["evidence_lanes"][0]["cursor"];
    cursor["deferred_records"] = json!(records);
    cursor["last_contiguous_artifact_seq"] = json!(tip);
    cursor["last_contiguous_record_sha256"] = json!(tip_hash);
    value
}

fn recovery_state() -> Value {
    json!({
        "incident_id":"incident-1",
        "entered_at":TS,
        "entered_generation":2,
        "origin":{"tag":"quarantined_positive_clearance","payload":null},
        "confirmation":{
            "catalog_evidence_required":true,
            "authoritative_free_observations":[],
            "runtime_success_at":null,
            "zero_cost_success_at":null,
            "last_organic_admission_at":null,
            "organic_admissions_24h":{"window":86400000,"events":[]},
            "confirmation_satisfied_at":null,
            "confirmation_satisfied_by":null
        },
        "regeneration":{
            "inclusion_required":true,
            "never_excluded_since_incident":false,
            "latest_regeneration":null,
            "latest_deployment":null
        },
        "flap":{"consecutive_failures":0,"damped_until":null}
    })
}

fn recovering_inventory() -> Value {
    let mut value = signed_inventory();
    value["states"][0]["value"]["lifecycle"] =
        json!({"tag":"recovering","payload":{"recovery":recovery_state()}});
    value["states"][0]["value"]["recovery_evaluation"] = recovery_evaluation();
    value
}

fn interrupted_cause_entry() -> Value {
    json!({
        "status":"clearing",
        "activated_at":TS,
        "activation_evidence":evidence_ref(),
        "clearing_started_at":TS,
        "clearing_evidence":evidence_ref(),
        "last_interrupted_clearing":{
            "started_at":TS,
            "evidence":evidence_ref(),
            "interrupted_at":TS_LATER,
            "interrupting_evidence":evidence_ref()
        }
    })
}

fn inventory_with_cause(cause: Value) -> Value {
    let mut value = signed_inventory();
    value["states"][0]["value"]["cause_states"] = json!([{"key":"absent_catalog","value":cause}]);
    value
}

fn unknown_subject() -> Value {
    json!({"tag":"unknown_keyed","payload":{"subject_fingerprint":HASH_C}})
}

fn scope_resolution() -> Value {
    json!({
        "from":unknown_subject(),
        "to":subject(),
        "evidence":{
            "provider_key":"xai",
            "from_subject":unknown_subject(),
            "to_subject":subject(),
            "shared_account_key":"acct-1",
            "shared_quota_scope_key":null,
            "source":"exact_provider_response_envelope",
            "evidence_ref":evidence_ref(),
            "observed_at":TS,
            "confidence":"exact_machine"
        },
        "resolved_at":TS
    })
}

fn inventory_with_scope_resolutions(resolutions: Vec<Value>) -> Value {
    let mut value = signed_inventory();
    value["scope_resolutions"] = json!(resolutions);
    value
}

fn epoch2_checkpoint_record() -> Value {
    let mut record = regeneration_record();
    record["log_epoch"] = json!(2);
    record["continuity_reinitialization"] = json!({
        "lane":regeneration_lane(),
        "new_epoch":2,
        "prior_epoch":1,
        "prior_terminal_artifact_seq":1,
        "prior_terminal_record_sha256":HASH_A,
        "reason":"sequence_gap"
    });
    refresh_record_hash(record)
}

fn epoch2_deployment_checkpoint_record() -> Value {
    let mut record = deployment_record();
    record["log_epoch"] = json!(2);
    record["continuity_reinitialization"] = json!({
        "lane":deployment_lane(),
        "new_epoch":2,
        "prior_epoch":1,
        "prior_terminal_artifact_seq":1,
        "prior_terminal_record_sha256":HASH_A,
        "reason":"sequence_gap"
    });
    refresh_record_hash(record)
}

fn failed_regeneration_record(coverage: &str, rows: Value) -> Value {
    let mut record = regeneration_record();
    record["status"] = json!({"tag":"failed","payload":{"kind":"inventory_invalid"}});
    record["failure_coverage"] = json!(coverage);
    record["config_generation"] = Value::Null;
    record["config_sha256"] = Value::Null;
    record["rows"] = rows;
    refresh_record_hash(record)
}

fn deferred_record(
    artifact_seq: u64,
    byte_start: u64,
    record_hash: &str,
    evidence_hash: &str,
) -> Value {
    let lane_hash = regeneration_lane_hash();
    json!({
        "log_epoch": 1,
        "artifact_seq": artifact_seq,
        "byte_start": byte_start,
        "byte_len": 50,
        "input_inventory_generation": 2,
        "input_inventory_sha256": HASH_B,
        "record_sha256": record_hash,
        "evidence_ref": {"kind":"evidence_artifact","uri_or_journal_key":format!("evidence-artifact:regeneration_importer:{lane_hash}:1:{artifact_seq}:{record_hash}"),"observed_at":TS,"sha256":evidence_hash}
    })
}

fn regeneration_fact() -> Value {
    json!({
        "lane": regeneration_lane(),
        "log_epoch": 1,
        "artifact_seq": 1,
        "input_inventory_generation": 2,
        "input_inventory_sha256": HASH_B,
        "fingerprint_algorithm": "v1",
        "outcome": {"tag":"included","payload":{"config_generation":7,"config_sha256":HASH_C,"expected_deployment_fingerprint":HASH_C}},
        "observed_at": TS,
        "evidence_ref": evidence_ref()
    })
}

#[test]
fn deferred_descriptors_reject_coordinate_and_hash_mismatch() {
    let with_bad_hash = mutated(
        signed_inventory(),
        &[
            "source_cursors",
            "evidence_lanes",
            "0",
            "cursor",
            "deferred_records",
        ],
        |v| {
            *v = json!([deferred_record(1, 0, HASH_A, HASH_B)]);
        },
    );
    assert!(
        parse_signed_inventory_pr1(&text(&with_bad_hash)).is_err(),
        "descriptor evidence hash must equal record hash"
    );

    let overlapping = mutated(
        signed_inventory(),
        &[
            "source_cursors",
            "evidence_lanes",
            "0",
            "cursor",
            "deferred_records",
        ],
        |v| {
            *v = json!([
                deferred_record(1, 0, HASH_A, HASH_A),
                deferred_record(2, 25, HASH_B, HASH_B)
            ]);
        },
    );
    assert!(
        parse_signed_inventory_pr1(&text(&overlapping)).is_err(),
        "descriptor byte ranges must not overlap"
    );
}

#[test]
fn canonical_collections_and_flattened_lane_representations_are_mutation_sensitive() {
    let wrong_lane_shape = mutated(
        signed_inventory(),
        &["source_cursors", "evidence_lanes", "0", "lane"],
        |v| {
            *v = json!({"tag":"regeneration_importer","payload":{"logical_path":"/evidence/regeneration.jsonl"}});
        },
    );
    assert!(parse_signed_inventory_pr1(&text(&wrong_lane_shape)).is_err());

    let reversed_lanes = mutated(
        signed_inventory(),
        &[
            "states",
            "0",
            "value",
            "recovery_evaluation",
            "required_evidence_inputs",
            "1",
            "required_lanes",
        ],
        |v| {
            *v = json!([deployment_lane(), regeneration_lane()]);
        },
    );
    assert!(
        parse_signed_inventory_pr1(&text(&reversed_lanes)).is_err(),
        "required lanes must be canonical-byte sorted"
    );

    let mismatched_hold = mutated(
        signed_inventory(),
        &[
            "states", "0", "value", "holds", "active", "0", "value", "kind",
        ],
        |v| {
            *v = json!("quota_exhausted");
        },
    );
    assert!(
        parse_signed_inventory_pr1(&text(&mismatched_hold)).is_err(),
        "persistent hold key must match contained kind"
    );
}

#[test]
fn regeneration_fact_requires_v9_coordinates() {
    let fact = regeneration_fact();
    serde_json::from_value::<RegenerationFact>(fact.clone()).expect("schema-current fact parses");
    for field in ["lane", "log_epoch", "input_inventory_sha256"] {
        let mut missing = fact.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<RegenerationFact>(missing).is_err(),
            "missing {field} must reject"
        );
    }
}

#[test]
fn hash_mutations_distinguish_removed_nulled_and_domainless_inputs() {
    let record = parse_regeneration_evidence_record(&text(&regeneration_record())).unwrap();
    let correct = evidence_record_sha256(&record).unwrap();
    let mut nulled = regeneration_record();
    nulled["record_sha256"] = Value::Null;
    let nulled_digest = sha256_hex(to_canonical_json(&nulled).unwrap().as_bytes());
    assert_ne!(
        correct, nulled_digest,
        "removal must not be confused with nulling"
    );

    let inventory = parse_signed_inventory_pr1(&text(&signed_inventory())).unwrap();
    let signatureless = inventory_sha256_without_signature(&inventory).unwrap();
    let payload = inventory_signature_payload(&inventory).unwrap();
    assert_ne!(
        sha256_hex(&payload),
        signatureless,
        "domain separation must change signature input"
    );
}

#[test]
fn generated_inventory_report_matches_rust_source() {
    let expected = include_str!("upstream_expiry_wire_inventory.tsv");
    assert_eq!(render_wire_type_inventory(), expected);
}

#[test]
fn artifact_conditional_option_rules_are_mutation_sensitive() {
    let included_without_fingerprint = mutated(
        regeneration_record(),
        &["rows", "0", "row", "expected_deployment_fingerprint"],
        |v| *v = Value::Null,
    );
    assert!(
        parse_regeneration_evidence_record(&text(&refresh_record_hash(
            included_without_fingerprint
        )))
        .is_err()
    );

    let mut excluded = regeneration_record();
    excluded["rows"][0]["row"] = json!({"inclusion":{"tag":"excluded","payload":{"reasons":["lifecycle_suppressed"]}},"expected_deployment_fingerprint":HASH_C});
    assert!(parse_regeneration_evidence_record(&text(&refresh_record_hash(excluded))).is_err());

    let success_without_config = mutated(regeneration_record(), &["config_generation"], |v| {
        *v = Value::Null
    });
    assert!(
        parse_regeneration_evidence_record(&text(&refresh_record_hash(success_without_config)))
            .is_err()
    );

    let first_with_previous = mutated(regeneration_record(), &["previous_record_sha256"], |v| {
        *v = json!(HASH_B)
    });
    assert!(
        parse_regeneration_evidence_record(&text(&refresh_record_hash(first_with_previous)))
            .is_err()
    );

    let mut second_without_previous = regeneration_record();
    second_without_previous["artifact_seq"] = json!(2);
    assert!(
        parse_regeneration_evidence_record(&text(&refresh_record_hash(second_without_previous)))
            .is_err()
    );

    let present_without_fingerprint = mutated(
        deployment_record(),
        &["rows", "0", "row", "actual_deployment_fingerprint"],
        |v| *v = Value::Null,
    );
    assert!(
        parse_server_deployment_evidence_record(&text(&refresh_record_hash(
            present_without_fingerprint
        )))
        .is_err()
    );

    let absent_with_fingerprint = mutated(deployment_record(), &["rows", "0", "row"], |v| {
        *v = json!({"present":false,"actual_deployment_fingerprint":HASH_C})
    });
    assert!(
        parse_server_deployment_evidence_record(&text(&refresh_record_hash(
            absent_with_fingerprint
        )))
        .is_err()
    );
}

#[test]
fn deferred_evidence_ref_journal_key_grammar_is_mutation_sensitive() {
    let bad_epoch = mutated(
        signed_inventory(),
        &[
            "source_cursors",
            "evidence_lanes",
            "0",
            "cursor",
            "deferred_records",
        ],
        |v| {
            let mut record = deferred_record(1, 0, HASH_A, HASH_A);
            let lane_hash = regeneration_lane_hash();
            record["evidence_ref"]["uri_or_journal_key"] = json!(format!(
                "evidence-artifact:regeneration_importer:{lane_hash}:01:1:{HASH_A}"
            ));
            *v = json!([record]);
        },
    );
    assert!(
        parse_signed_inventory_pr1(&text(&bad_epoch)).is_err(),
        "journal integer must not have leading zeros"
    );

    let mismatched_terminal_hash = mutated(
        signed_inventory(),
        &[
            "source_cursors",
            "evidence_lanes",
            "0",
            "cursor",
            "deferred_records",
        ],
        |v| {
            let mut record = deferred_record(1, 0, HASH_A, HASH_A);
            record["evidence_ref"]["uri_or_journal_key"] = json!(format!(
                "evidence-artifact:regeneration_importer:{HASH_C}:1:1:{HASH_B}"
            ));
            *v = json!([record]);
        },
    );
    assert!(
        parse_signed_inventory_pr1(&text(&mismatched_terminal_hash)).is_err(),
        "terminal key hash must equal evidence hash"
    );
}

#[test]
fn required_lane_snapshots_match_requirement_states_and_lane_kinds() {
    let mismatch = mutated(
        signed_inventory(),
        &[
            "states",
            "0",
            "value",
            "recovery_evaluation",
            "required_evidence_inputs",
            "0",
            "requirement",
        ],
        |v| {
            *v = json!("satisfied");
        },
    );
    assert!(
        parse_signed_inventory_pr1(&text(&mismatch)).is_err(),
        "deployment snapshot requirement must equal loaded deployment state"
    );

    let wrong_lane_kind = mutated(
        signed_inventory(),
        &[
            "states",
            "0",
            "value",
            "recovery_evaluation",
            "required_evidence_inputs",
            "0",
            "required_lanes",
            "0",
        ],
        |v| {
            *v = regeneration_lane();
        },
    );
    assert!(
        parse_signed_inventory_pr1(&text(&wrong_lane_kind)).is_err(),
        "deployment input cannot require a regeneration lane"
    );
}

#[test]
fn payload_enum_variants_round_trip_across_the_closed_surface() {
    let lifecycle_values = [
        json!({"tag":"unknown","payload":null}),
        json!({"tag":"serving","payload":null}),
        json!({"tag":"suspect","payload":{"sources":["catalog_absent","cost_unconfirmed","exact_runtime"]}}),
        json!({"tag":"quarantined","payload":{"sources":["catalog_absent","exact_runtime"]}}),
        json!({"tag":"expired","payload":{"causes":["absent_catalog","absent_exact_runtime","deadline_passed","not_free"]}}),
        json!({"tag":"expiring_soon","payload":{"expires_at":TS}}),
    ];
    for value in lifecycle_values {
        let parsed: LifecycleState = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), value);
    }

    let blockers = [
        json!({"tag":"active_cause_remains","payload":null}),
        json!({"tag":"flap_damped_until","payload":{"until":TS}}),
        json!({"tag":"evidence_input_degraded","payload":{"input":"deployment_evidence"}}),
        json!({"tag":"evidence_input_deferred","payload":{"input":"regeneration_evidence"}}),
    ];
    for value in blockers {
        let parsed: RecoveryBlocker = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), value);
    }

    let origin: RecoveryOrigin = serde_json::from_value(
        json!({"tag":"operator_correction","payload":{"correction_id":"corr-1"}}),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(origin).unwrap()["tag"],
        "operator_correction"
    );
    let attestation: AttestationStatus = serde_json::from_value(
        json!({"tag":"superseded","payload":{"at":TS,"evidence":evidence_ref()}}),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(attestation).unwrap()["tag"],
        "superseded"
    );
    let satisfaction: ConfirmationSatisfaction = serde_json::from_value(
        json!({"tag":"operator_attested","payload":{"correction_id":"corr-1","accepted_at":TS}}),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(satisfaction).unwrap()["tag"],
        "operator_attested"
    );
    let subject: CapacitySubject = serde_json::from_value(json!({"tag":"provider_quota_scope","payload":{"upstream_account_key":null,"quota_scope_key":"quota-1"}})).unwrap();
    assert_eq!(
        serde_json::to_value(subject).unwrap()["tag"],
        "provider_quota_scope"
    );

    let regen_outcomes = [
        json!({"tag":"excluded","payload":{"reasons":["legacy_suppressed","lifecycle_suppressed","mapping_rejected","not_in_source_universe","revoked_discovery_candidate"]}}),
        json!({"tag":"import_failed","payload":{"kind":"inventory_invalid"}}),
    ];
    for value in regen_outcomes {
        let observed: ObservedRegenerationOutcome = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(observed).unwrap(), value);
        let row: RegenerationInclusionOutcome = serde_json::from_value(value).unwrap();
        assert!(!serde_json::to_value(row).unwrap()["payload"].is_null());
    }
    let absent: ObservedDeploymentOutcome = serde_json::from_value(json!({"tag":"absent","payload":{"loaded_config_generation":7,"loaded_config_sha256":HASH_C}})).unwrap();
    assert_eq!(serde_json::to_value(absent).unwrap()["tag"], "absent");
}

#[test]
fn artifact_record_hash_is_self_validating() {
    let valid = regeneration_record();
    assert!(parse_regeneration_evidence_record(&text(&valid)).is_ok());
    let mut wrong_hash = valid.clone();
    wrong_hash["record_sha256"] = json!(HASH_A);
    assert!(
        parse_regeneration_evidence_record(&text(&wrong_hash)).is_err(),
        "a record whose self-hash does not match the canonical hash input must reject"
    );
}

#[test]
fn deferred_cursor_shape_and_invariants_are_exact() {
    let valid = valid_deferred_inventory();
    assert!(
        parse_signed_inventory_pr1(&text(&valid)).is_ok(),
        "schema-v8 descriptor shape has no duplicated lane field"
    );

    let mut quality_valid = valid.clone();
    quality_valid["source_cursors"]["evidence_lanes"][0]["cursor"]["quality"] =
        json!({"kind":"valid"});
    assert!(
        parse_signed_inventory_pr1(&text(&quality_valid)).is_err(),
        "valid quality cannot retain deferred descriptors"
    );

    let mut empty_deferred = valid.clone();
    empty_deferred["source_cursors"]["evidence_lanes"][0]["cursor"]["deferred_records"] = json!([]);
    assert!(
        parse_signed_inventory_pr1(&text(&empty_deferred)).is_err(),
        "deferred quality requires a nonempty deferred chain"
    );

    let mut tip_mismatch = valid.clone();
    tip_mismatch["source_cursors"]["evidence_lanes"][0]["cursor"]["last_contiguous_artifact_seq"] =
        json!(1);
    assert!(
        parse_signed_inventory_pr1(&text(&tip_mismatch)).is_err(),
        "contiguous tip must equal the greatest deferred artifact sequence"
    );

    let mut unpaired_tip = valid.clone();
    unpaired_tip["source_cursors"]["evidence_lanes"][0]["cursor"]
        ["last_contiguous_record_sha256"] = Value::Null;
    assert!(
        parse_signed_inventory_pr1(&text(&unpaired_tip)).is_err(),
        "cursor sequence and hash options must be present together"
    );

    let mut wrong_tip_hash = valid.clone();
    wrong_tip_hash["source_cursors"]["evidence_lanes"][0]["cursor"]
        ["last_contiguous_record_sha256"] = json!(HASH_A);
    assert!(
        parse_signed_inventory_pr1(&text(&wrong_tip_hash)).is_err(),
        "contiguous tip hash must equal the greatest deferred record hash"
    );

    let mut noncontiguous = valid_deferred_inventory();
    noncontiguous["source_cursors"]["evidence_lanes"][0]["cursor"]["deferred_records"] = json!([
        deferred_record(1, 0, HASH_A, HASH_A),
        deferred_record(3, 50, HASH_B, HASH_B)
    ]);
    noncontiguous["source_cursors"]["evidence_lanes"][0]["cursor"]
        ["last_contiguous_artifact_seq"] = json!(3);
    assert!(
        parse_signed_inventory_pr1(&text(&noncontiguous)).is_err(),
        "deferred artifact sequences must be contiguous"
    );
}

#[test]
fn checkpoint_and_failure_coverage_invariants_are_mutation_sensitive() {
    let valid_checkpoint = epoch2_checkpoint_record();
    assert!(parse_regeneration_evidence_record(&text(&valid_checkpoint)).is_ok());

    let missing_checkpoint = refresh_record_hash(mutated(
        valid_checkpoint.clone(),
        &["continuity_reinitialization"],
        |v| *v = Value::Null,
    ));
    assert!(
        parse_regeneration_evidence_record(&text(&missing_checkpoint)).is_err(),
        "first artifact of a later epoch must carry its checkpoint"
    );

    let half_terminal = refresh_record_hash(mutated(
        valid_checkpoint.clone(),
        &[
            "continuity_reinitialization",
            "prior_terminal_record_sha256",
        ],
        |v| *v = Value::Null,
    ));
    assert!(
        parse_regeneration_evidence_record(&text(&half_terminal)).is_err(),
        "checkpoint terminal sequence and hash must be null together"
    );

    let mut seq2 = valid_checkpoint.clone();
    seq2["artifact_seq"] = json!(2);
    seq2["previous_record_sha256"] = json!(HASH_B);
    let seq2 = refresh_record_hash(seq2);
    assert!(
        parse_regeneration_evidence_record(&text(&seq2)).is_err(),
        "non-initial artifacts cannot carry a checkpoint"
    );

    let wrong_checkpoint_lane = refresh_record_hash(mutated(
        valid_checkpoint.clone(),
        &["continuity_reinitialization", "lane"],
        |v| *v = deployment_lane(),
    ));
    assert!(
        parse_regeneration_evidence_record(&text(&wrong_checkpoint_lane)).is_err(),
        "regeneration checkpoints cannot name a deployment lane"
    );

    let valid_deployment_checkpoint = epoch2_deployment_checkpoint_record();
    assert!(
        parse_server_deployment_evidence_record(&text(&valid_deployment_checkpoint)).is_ok(),
        "matching deployment checkpoint lane parses"
    );
    let wrong_server_lane = refresh_record_hash(mutated(
        valid_deployment_checkpoint.clone(),
        &["continuity_reinitialization", "lane", "server_instance_id"],
        |v| *v = json!("server-b"),
    ));
    assert!(
        parse_server_deployment_evidence_record(&text(&wrong_server_lane)).is_err(),
        "deployment checkpoint lane must match the record server instance"
    );

    let unavailable_with_rows = failed_regeneration_record(
        "unavailable_before_inventory_enumeration",
        json!([{"upstream_ref":upstream_ref(),"row":{"inclusion":{"tag":"import_failed","payload":{"kind":"inventory_invalid"}},"expected_deployment_fingerprint":null}}]),
    );
    assert!(
        parse_regeneration_evidence_record(&text(&unavailable_with_rows)).is_err(),
        "unavailable-before-enumeration failure coverage requires empty rows"
    );

    let success_import_failed = refresh_record_hash(mutated(
        regeneration_record(),
        &["rows", "0", "row"],
        |v| *v = json!({"inclusion":{"tag":"import_failed","payload":{"kind":"inventory_invalid"}},"expected_deployment_fingerprint":null}),
    ));
    assert!(
        parse_regeneration_evidence_record(&text(&success_import_failed)).is_err(),
        "success artifacts cannot carry import-failed rows"
    );

    let complete_with_included = failed_regeneration_record(
        "complete_input_lifecycle_universe",
        json!([{"upstream_ref":upstream_ref(),"row":{"inclusion":{"tag":"included","payload":null},"expected_deployment_fingerprint":HASH_C}}]),
    );
    assert!(
        parse_regeneration_evidence_record(&text(&complete_with_included)).is_err(),
        "complete failure coverage rows must be import failures"
    );
}

#[test]
fn cause_clearing_interruption_and_recovery_confirmation_options_are_exact() {
    let valid = inventory_with_cause(interrupted_cause_entry());
    assert!(
        parse_signed_inventory_pr1(&text(&valid)).is_ok(),
        "schema-v5 interrupted-clearing history parses"
    );

    let mut missing_history_field = interrupted_cause_entry();
    missing_history_field
        .as_object_mut()
        .unwrap()
        .remove("last_interrupted_clearing");
    assert!(
        parse_signed_inventory_pr1(&text(&inventory_with_cause(missing_history_field))).is_err(),
        "optional interrupted-clearing field must still be present, possibly null"
    );

    let mut active_with_clearing = interrupted_cause_entry();
    active_with_clearing["status"] = json!("active");
    assert!(
        parse_signed_inventory_pr1(&text(&inventory_with_cause(active_with_clearing))).is_err(),
        "an active cause cannot retain current clearing fields"
    );

    let mut half_clearing = interrupted_cause_entry();
    half_clearing["clearing_evidence"] = Value::Null;
    assert!(
        parse_signed_inventory_pr1(&text(&inventory_with_cause(half_clearing))).is_err(),
        "clearing timestamp and evidence must be present together"
    );

    let mut unpaired_confirmation = recovering_inventory();
    unpaired_confirmation["states"][0]["value"]["lifecycle"]["payload"]["recovery"]
        ["confirmation"]["confirmation_satisfied_at"] = json!(TS);
    assert!(
        parse_signed_inventory_pr1(&text(&unpaired_confirmation)).is_err(),
        "confirmation timestamp and satisfaction value must be present together"
    );
}

#[test]
fn scope_resolution_evidence_and_edge_invariants_are_mutation_sensitive() {
    let valid = inventory_with_scope_resolutions(vec![scope_resolution()]);
    assert!(parse_signed_inventory_pr1(&text(&valid)).is_ok());

    let mut same_edge = scope_resolution();
    same_edge["from"] = subject();
    let same_edge = inventory_with_scope_resolutions(vec![same_edge]);
    assert!(
        parse_signed_inventory_pr1(&text(&same_edge)).is_err(),
        "identity resolution edge cannot start and end at the same subject"
    );

    let mut no_shared_key = scope_resolution();
    no_shared_key["evidence"]["shared_account_key"] = Value::Null;
    let no_shared_key = inventory_with_scope_resolutions(vec![no_shared_key]);
    assert!(
        parse_signed_inventory_pr1(&text(&no_shared_key)).is_err(),
        "resolution evidence requires at least one shared key"
    );

    let mut weak_confidence = scope_resolution();
    weak_confidence["evidence"]["confidence"] = json!("provider_heuristic");
    let weak_confidence = inventory_with_scope_resolutions(vec![weak_confidence]);
    assert!(
        parse_signed_inventory_pr1(&text(&weak_confidence)).is_err(),
        "resolution edges require exact or authoritative confidence"
    );

    let mut reverse = scope_resolution();
    reverse["from"] = scope_resolution()["to"].clone();
    reverse["to"] = scope_resolution()["from"].clone();
    reverse["evidence"]["from_subject"] = scope_resolution()["to"].clone();
    reverse["evidence"]["to_subject"] = scope_resolution()["from"].clone();
    let cyclic = inventory_with_scope_resolutions(vec![scope_resolution(), reverse]);
    assert!(
        parse_signed_inventory_pr1(&text(&cyclic)).is_err(),
        "reverse resolution edges are invalid"
    );
}

#[test]
fn deferred_cursor_coordinate_variants_are_rejected() {
    let zero_seq =
        inventory_with_deferred_records(vec![deferred_record(0, 0, HASH_A, HASH_A)], 0, HASH_A);
    assert!(parse_signed_inventory_pr1(&text(&zero_seq)).is_err());

    let exceeds_tip =
        inventory_with_deferred_records(vec![deferred_record(3, 0, HASH_A, HASH_A)], 2, HASH_B);
    assert!(parse_signed_inventory_pr1(&text(&exceeds_tip)).is_err());

    let mut epoch_record = deferred_record(1, 0, HASH_A, HASH_A);
    epoch_record["log_epoch"] = json!(2);
    let epoch_mismatch = inventory_with_deferred_records(vec![epoch_record], 1, HASH_A);
    assert!(parse_signed_inventory_pr1(&text(&epoch_mismatch)).is_err());

    let mut external_record = deferred_record(1, 0, HASH_A, HASH_A);
    external_record["evidence_ref"]["uri_or_journal_key"] = json!("https://example.com/evidence");
    let external_ref = inventory_with_deferred_records(vec![external_record], 1, HASH_A);
    assert!(parse_signed_inventory_pr1(&text(&external_ref)).is_err());

    let mut wrong_lane_record = deferred_record(1, 0, HASH_A, HASH_A);
    wrong_lane_record["evidence_ref"]["uri_or_journal_key"] = json!(format!(
        "evidence-artifact:regeneration_importer:{HASH_C}:1:1:{HASH_A}"
    ));
    let wrong_lane = inventory_with_deferred_records(vec![wrong_lane_record], 1, HASH_A);
    assert!(parse_signed_inventory_pr1(&text(&wrong_lane)).is_err());

    let mut zero_len_record = deferred_record(1, 0, HASH_A, HASH_A);
    zero_len_record["byte_len"] = json!(0);
    let zero_len = inventory_with_deferred_records(vec![zero_len_record], 1, HASH_A);
    assert!(parse_signed_inventory_pr1(&text(&zero_len)).is_err());
}

#[test]
fn scope_resolution_target_variants_are_rejected() {
    let mut endpoint_mismatch = scope_resolution();
    endpoint_mismatch["evidence"]["from_subject"] = subject();
    assert!(
        parse_signed_inventory_pr1(&text(&inventory_with_scope_resolutions(vec![
            endpoint_mismatch
        ])))
        .is_err()
    );

    let mut account_missing_key = scope_resolution();
    account_missing_key["evidence"]["shared_account_key"] = Value::Null;
    account_missing_key["evidence"]["shared_quota_scope_key"] = json!("quota-1");
    assert!(
        parse_signed_inventory_pr1(&text(&inventory_with_scope_resolutions(vec![
            account_missing_key
        ])))
        .is_err()
    );

    let quota_target = json!({"tag":"provider_quota_scope","payload":{"upstream_account_key":"acct-1","quota_scope_key":"quota-1"}});
    let mut quota_missing_key = scope_resolution();
    quota_missing_key["to"] = quota_target.clone();
    quota_missing_key["evidence"]["to_subject"] = quota_target.clone();
    assert!(
        parse_signed_inventory_pr1(&text(&inventory_with_scope_resolutions(vec![
            quota_missing_key
        ])))
        .is_err()
    );

    let conflicting_quota_target = json!({"tag":"provider_quota_scope","payload":{"upstream_account_key":"acct-2","quota_scope_key":"quota-1"}});
    let mut conflicting_account = scope_resolution();
    conflicting_account["to"] = conflicting_quota_target.clone();
    conflicting_account["evidence"]["to_subject"] = conflicting_quota_target;
    conflicting_account["evidence"]["shared_quota_scope_key"] = json!("quota-1");
    assert!(
        parse_signed_inventory_pr1(&text(&inventory_with_scope_resolutions(vec![
            conflicting_account
        ])))
        .is_err()
    );

    let mut unknown_target = scope_resolution();
    unknown_target["from"] = subject();
    unknown_target["to"] = unknown_subject();
    unknown_target["evidence"]["from_subject"] = subject();
    unknown_target["evidence"]["to_subject"] = unknown_subject();
    assert!(
        parse_signed_inventory_pr1(&text(&inventory_with_scope_resolutions(vec![
            unknown_target
        ])))
        .is_err()
    );

    let mut second_target = scope_resolution();
    let second_subject =
        json!({"tag":"provider_account","payload":{"upstream_account_key":"acct-2"}});
    second_target["to"] = second_subject.clone();
    second_target["evidence"]["to_subject"] = second_subject;
    second_target["evidence"]["shared_account_key"] = json!("acct-2");
    let multiple_targets =
        inventory_with_scope_resolutions(vec![scope_resolution(), second_target]);
    assert!(parse_signed_inventory_pr1(&text(&multiple_targets)).is_err());
}

#[test]
fn artifact_checkpoint_edge_variants_are_rejected() {
    let zero_seq = refresh_record_hash(mutated(regeneration_record(), &["artifact_seq"], |v| {
        *v = json!(0)
    }));
    assert!(parse_regeneration_evidence_record(&text(&zero_seq)).is_err());

    let initial_epoch_with_checkpoint =
        refresh_record_hash(mutated(epoch2_checkpoint_record(), &["log_epoch"], |v| {
            *v = json!(1)
        }));
    assert!(parse_regeneration_evidence_record(&text(&initial_epoch_with_checkpoint)).is_err());

    let new_epoch_mismatch = refresh_record_hash(mutated(
        epoch2_checkpoint_record(),
        &["continuity_reinitialization", "new_epoch"],
        |v| *v = json!(1),
    ));
    assert!(parse_regeneration_evidence_record(&text(&new_epoch_mismatch)).is_err());

    let prior_epoch_mismatch = refresh_record_hash(mutated(
        epoch2_checkpoint_record(),
        &["continuity_reinitialization", "prior_epoch"],
        |v| *v = json!(0),
    ));
    assert!(parse_regeneration_evidence_record(&text(&prior_epoch_mismatch)).is_err());

    let coverage_on_success =
        refresh_record_hash(mutated(regeneration_record(), &["failure_coverage"], |v| {
            *v = json!("complete_input_lifecycle_universe")
        }));
    assert!(parse_regeneration_evidence_record(&text(&coverage_on_success)).is_err());

    let deployment_previous_on_first = refresh_record_hash(mutated(
        deployment_record(),
        &["previous_record_sha256"],
        |v| *v = json!(HASH_B),
    ));
    assert!(parse_server_deployment_evidence_record(&text(&deployment_previous_on_first)).is_err());
}

#[test]
fn canonical_json_handles_unicode_keys_and_strings() {
    let value = json!({"😀":"control\u{1}","é":["\u{7}", "snowman ☃"]});
    assert_eq!(
        to_canonical_json(&value).unwrap(),
        "{\"é\":[\"\\u0007\",\"snowman ☃\"],\"😀\":\"control\\u0001\"}"
    );
}

#[test]
fn lane_quality_variants_hash_helper_and_invalid_brands_are_covered() {
    use llmux_types::upstream_expiry::{
        EvidenceLaneKey, EvidenceLaneQuality, ProviderKey, Sha256, Timestamp,
    };
    for value in [
        json!({"kind":"valid"}),
        json!({"kind":"deferred_unknown_inventory_generation"}),
        json!({"kind":"degraded","reason":"sequence_gap"}),
    ] {
        let parsed: EvidenceLaneQuality = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), value);
    }
    let lane: EvidenceLaneKey = serde_json::from_value(regeneration_lane()).unwrap();
    assert_eq!(lane_key_sha256(&lane).unwrap().len(), 64);
    assert!(ProviderKey::parse("bad key").is_err());
    assert!(
        Sha256::parse("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").is_err()
    );
    assert!(Timestamp::parse("2026-02-29T00:00:00.000000000Z").is_err());
}
