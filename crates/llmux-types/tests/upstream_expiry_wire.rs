use llmux_types::upstream_expiry::{
    evidence_record_sha256, inventory_sha256_without_signature, inventory_signature_payload,
    parse_regeneration_evidence_record, parse_server_deployment_evidence_record,
    parse_signed_inventory_pr1, render_wire_type_inventory, sha256_hex, to_canonical_json,
    wire_type_inventory, AttestationStatus, CapacitySubject, ConfirmationSatisfaction,
    LifecycleState, ObservedDeploymentOutcome, ObservedRegenerationOutcome, RecoveryBlocker,
    RecoveryOrigin, RegenerationFact, RegenerationInclusionOutcome,
};
use serde_json::{json, Value};

const TS: &str = "2026-11-29T00:00:00.000000000Z";
const TS_LATER: &str = "2026-11-29T00:30:00.000000000Z";
const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const HASH_C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const SIG: &str = "ed25519:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

fn evidence_ref() -> Value {
    json!({"kind":"evidence_artifact","uri_or_journal_key":format!("evidence-artifact:regeneration_importer:{HASH_C}:1:1:{HASH_A}"),"observed_at":TS,"sha256":HASH_A})
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
fn regeneration_record() -> Value {
    json!({"schema_version":3,"artifact_seq":1,"log_epoch":1,"previous_record_sha256":null,"record_sha256":HASH_A,"continuity_reinitialization":null,"import_run_id":"import-1","started_at":TS,"finished_at":TS,"input_inventory_generation":2,"input_inventory_sha256":HASH_B,"config_generation":7,"config_sha256":HASH_C,"fingerprint_algorithm":"v1","status":{"tag":"success","payload":null},"failure_coverage":null,"rows":[{"upstream_ref":upstream_ref(),"row":{"inclusion":{"tag":"included","payload":null},"expected_deployment_fingerprint":HASH_C}}]})
}
fn deployment_record() -> Value {
    json!({"schema_version":3,"artifact_seq":1,"log_epoch":1,"previous_record_sha256":null,"record_sha256":HASH_A,"continuity_reinitialization":null,"server_instance_id":"server-a","boot_id":"boot-1","record_kind":"config_load","emitted_at":TS,"load_started_at":TS,"load_finished_at":TS,"input_inventory_generation":2,"input_inventory_sha256":HASH_B,"loaded_config_generation":7,"loaded_config_sha256":HASH_C,"fingerprint_algorithm":"v1","traffic_role":"active_traffic","rows":[{"upstream_ref":upstream_ref(),"row":{"present":true,"actual_deployment_fingerprint":HASH_C}}]})
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
    assert!(parse_regeneration_evidence_record(&text(&flat_row)).is_err());
    let missing_null = mutated(regeneration_record(), &[], |v| {
        v.as_object_mut().unwrap().remove("failure_coverage");
    });
    assert!(parse_regeneration_evidence_record(&text(&missing_null)).is_err());
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

fn deferred_record(
    artifact_seq: u64,
    byte_start: u64,
    record_hash: &str,
    evidence_hash: &str,
) -> Value {
    json!({
        "lane": regeneration_lane(),
        "log_epoch": 1,
        "artifact_seq": artifact_seq,
        "byte_start": byte_start,
        "byte_len": 50,
        "input_inventory_generation": 2,
        "input_inventory_sha256": HASH_B,
        "record_sha256": record_hash,
        "evidence_ref": {"kind":"evidence_artifact","uri_or_journal_key":format!("evidence-artifact:regeneration_importer:{HASH_C}:1:{artifact_seq}:{record_hash}"),"observed_at":TS,"sha256":evidence_hash}
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
    assert!(parse_regeneration_evidence_record(&text(&included_without_fingerprint)).is_err());

    let mut excluded = regeneration_record();
    excluded["rows"][0]["row"] = json!({"inclusion":{"tag":"excluded","payload":{"reasons":["lifecycle_suppressed"]}},"expected_deployment_fingerprint":HASH_C});
    assert!(parse_regeneration_evidence_record(&text(&excluded)).is_err());

    let success_without_config = mutated(regeneration_record(), &["config_generation"], |v| {
        *v = Value::Null
    });
    assert!(parse_regeneration_evidence_record(&text(&success_without_config)).is_err());

    let first_with_previous = mutated(regeneration_record(), &["previous_record_sha256"], |v| {
        *v = json!(HASH_B)
    });
    assert!(parse_regeneration_evidence_record(&text(&first_with_previous)).is_err());

    let mut second_without_previous = regeneration_record();
    second_without_previous["artifact_seq"] = json!(2);
    assert!(parse_regeneration_evidence_record(&text(&second_without_previous)).is_err());

    let present_without_fingerprint = mutated(
        deployment_record(),
        &["rows", "0", "row", "actual_deployment_fingerprint"],
        |v| *v = Value::Null,
    );
    assert!(parse_server_deployment_evidence_record(&text(&present_without_fingerprint)).is_err());

    let absent_with_fingerprint = mutated(deployment_record(), &["rows", "0", "row"], |v| {
        *v = json!({"present":false,"actual_deployment_fingerprint":HASH_C})
    });
    assert!(parse_server_deployment_evidence_record(&text(&absent_with_fingerprint)).is_err());
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
            record["evidence_ref"]["uri_or_journal_key"] = json!(format!(
                "evidence-artifact:regeneration_importer:{HASH_C}:01:1:{HASH_A}"
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
fn canonical_json_handles_unicode_keys_and_strings() {
    let value = json!({"😀":"control\u{1}","é":["\u{7}", "snowman ☃"]});
    assert_eq!(
        to_canonical_json(&value).unwrap(),
        "{\"é\":[\"\\u0007\",\"snowman ☃\"],\"😀\":\"control\\u0001\"}"
    );
}
