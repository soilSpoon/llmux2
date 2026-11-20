use llmux_config::RuntimeConfig;
use serde_json::{json, Value};

fn fixture() -> Value {
    json!({
        "provider_connections":[{"id":"keyed","provider":"openai","auth_type":"apikey","api_key":"synthetic-key","is_active":1}],
        "api_keys":[{"key":"synthetic-client"}],
        "combos":[
            {"id":"combo-id","name":"canonical","data":{"name":"canonical","models":["openai/test"]}},
            {"id":"empty-id","name":"empty","data":{"name":"empty","models":["unsupported/test"]}}
        ],
        "key_value":{"modelAliases":{
            "direct":"openai/other", "chain":"direct", "combo-alias":"canonical",
            "skipped":"empty", "cycle-a":"cycle-b", "cycle-b":"cycle-a",
            "canonical":"openai/wrong", "broken":"unknown/test"
        }},
        "model_combo_mappings":[
            {"pattern":"mapped","combo_id":"combo-id","enabled":1,"priority":1},
            {"pattern":"direct","combo_id":"combo-id","enabled":1,"priority":2},
            {"pattern":"disabled","combo_id":"combo-id","enabled":0},
            {"pattern":"wild-*","combo_id":"combo-id","enabled":1}
        ]
    })
}

#[test]
fn aliases_share_canonical_models_and_mapping_precedence() {
    let root = fixture();
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let report = llmux_import::import(&root.to_string(), &output, None).unwrap();
    let config = RuntimeConfig::load(&output).unwrap();
    let canonical = config.model_names["canonical"];
    for name in ["mapped", "combo-alias", "direct", "chain"] {
        assert_eq!(config.model_names.get(name), Some(&canonical), "{name}");
    }
    for name in [
        "skipped", "broken", "cycle-a", "cycle-b", "disabled", "wild-*",
    ] {
        assert!(!config.model_names.contains_key(name), "{name}");
    }
    assert_eq!(
        (report.models, report.deployments, report.credentials),
        (1, 1, 1)
    );
    let warnings = report.warnings.join("\n");
    assert!(warnings.contains("collision"));
    assert!(warnings.contains("cycle"));
    assert!(warnings.contains("target unavailable"));
    assert!(warnings.contains("wildcard"));
}

#[test]
fn direct_aliases_use_shared_provider_expansion_and_rows_are_supported() {
    let mut root = fixture();
    root["model_combo_mappings"] = json!([]);
    root["key_value"] = json!([
        {"namespace":"modelAliases","key":"direct","value":"\"openai/vendor/test\""},
        {"namespace":"modelAliases","key":"second","value":"\"direct\""},
        {"namespace":"modelAliases","key":"bad","value":"not-json"},
        {"namespace":"modelAliases","key":"object","value":"{\"provider\":\"openai\",\"model\":\"object-test\"}"}
    ]);
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let report = llmux_import::import(&root.to_string(), &output, None).unwrap();
    let config = RuntimeConfig::load(&output).unwrap();
    assert_eq!(
        config.model_names.get("direct"),
        config.model_names.get("second")
    );
    let model = &config.models[*config.model_names.get("direct").unwrap()];
    assert_eq!(
        config.deployments[model.deployments[0].0 as usize]
            .upstream_model
            .as_ref(),
        "vendor/test"
    );
    assert!(config.model_names.contains_key("object"));
    assert_eq!(
        (report.models, report.deployments, report.credentials),
        (3, 3, 1)
    );
    assert!(report
        .warnings
        .iter()
        .any(|w| w.contains("malformed alias")));
}

#[test]
fn wildcard_mapping_priority_and_unavailable_targets_do_not_fall_through() {
    let mut root = fixture();
    root["key_value"]["modelAliases"] =
        json!({"public-name":"openai/test", "public-safe":"openai/test"});
    root["model_combo_mappings"] = json!([
        {"pattern":"public-*","combo_id":"empty-id","enabled":1,"priority":10},
        {"pattern":"public-name","combo_id":"combo-id","enabled":1,"priority":1},
        {"pattern":"public-safe","combo_id":"combo-id","enabled":1,"priority":20}
    ]);
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let report = llmux_import::import(&root.to_string(), &output, None).unwrap();
    let config = RuntimeConfig::load(&output).unwrap();
    assert!(!config.model_names.contains_key("public-name"));
    assert_eq!(
        config.model_names.get("public-safe"),
        config.model_names.get("canonical")
    );
    assert!(report
        .warnings
        .iter()
        .any(|w| w.contains("public-name: target unavailable")));
}

#[test]
fn unavailable_winning_mapping_blocks_lower_mappings_and_legacy_aliases() {
    for target in ["missing-id", "inactive-id", "malformed-id", "empty-id"] {
        let mut root = fixture();
        root["combos"].as_array_mut().unwrap().extend([
            json!({"id":"inactive-id","data":{"name":"inactive","isActive":false,"models":["openai/inactive"]}}),
            json!({"id":"malformed-id","data":{"name":"malformed","models":{}}}),
        ]);
        root["key_value"]["modelAliases"] = json!({
            "public-name":"openai/legacy", "chain":"public-name"
        });
        root["model_combo_mappings"] = json!([
            {"pattern":"public-*","combo_id":target,"enabled":1,"priority":10},
            {"pattern":"public-name","combo_id":"combo-id","enabled":1,"priority":1}
        ]);
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.toml");
        let report = llmux_import::import(&root.to_string(), &output, None).unwrap();
        let config = RuntimeConfig::load(output).unwrap();
        for name in ["public-name", "chain", "openai/legacy"] {
            assert!(!config.model_names.contains_key(name), "{target}: {name}");
        }
        assert!(report
            .warnings
            .iter()
            .any(|w| w.contains("public-name: target unavailable")));
    }
}

#[test]
fn mapping_to_skipped_duplicate_combo_never_binds_to_first_combo() {
    for model in ["openai/test", "openai/different"] {
        let mut root = fixture();
        root["combos"].as_array_mut().unwrap().push(json!({
            "id":"duplicate-id", "name":"canonical",
            "data":{"name":"canonical","models":[model]}
        }));
        root["key_value"]["modelAliases"] = json!({"duplicate-alias":"openai/legacy"});
        root["model_combo_mappings"] = json!([
            {"pattern":"duplicate-alias","combo_id":"duplicate-id","enabled":1,"priority":10},
            {"pattern":"duplicate-alias","combo_id":"combo-id","enabled":1,"priority":1},
            {"pattern":"retained-alias","combo_id":"combo-id","enabled":1}
        ]);
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.toml");
        let report = llmux_import::import(&root.to_string(), &output, None).unwrap();
        let config = RuntimeConfig::load(output).unwrap();
        assert!(!config.model_names.contains_key("duplicate-alias"));
        assert_eq!(
            config.model_names.get("retained-alias"),
            config.model_names.get("canonical")
        );
        let warnings = report.warnings.join("\n");
        assert!(warnings.contains("skipped empty/duplicate name"));
        assert!(warnings.contains("duplicate-alias: target unavailable"));
    }
}

#[test]
fn equal_priority_mappings_use_earliest_created_at() {
    let mut root = fixture();
    root["combos"].as_array_mut().unwrap().push(json!({
        "id":"newer-id","data":{"name":"newer","models":["openai/newer"]}
    }));
    root["key_value"]["modelAliases"] = json!({});
    root["model_combo_mappings"] = json!([
        {"pattern":"dated","combo_id":"newer-id","enabled":1,"priority":10,"created_at":"2026-01-02T00:00:00Z"},
        {"pattern":"dated","combo_id":"combo-id","enabled":1,"priority":10,"created_at":"2026-01-01T00:00:00Z"},
        {"pattern":"dated","combo_id":"missing","enabled":0,"priority":20,"created_at":"2025-01-01T00:00:00Z"}
    ]);
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    llmux_import::import(&root.to_string(), &output, None).unwrap();
    let config = RuntimeConfig::load(output).unwrap();
    assert_eq!(
        config.model_names.get("dated"),
        config.model_names.get("canonical")
    );
}

#[test]
fn combo_prefixed_alias_targets_preserve_unavailable_targets() {
    let mut root = fixture();
    root["key_value"]["modelAliases"] = json!({
        "prefixed":"combo/canonical", "empty-prefixed":"combo/empty", "missing-prefixed":"combo/missing"
    });
    root["model_combo_mappings"] = json!([]);
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let report = llmux_import::import(&root.to_string(), &output, None).unwrap();
    let config = RuntimeConfig::load(output).unwrap();
    assert_eq!(
        config.model_names.get("prefixed"),
        config.model_names.get("canonical")
    );
    for name in ["empty-prefixed", "missing-prefixed"] {
        assert!(!config.model_names.contains_key(name));
        assert!(report
            .warnings
            .iter()
            .any(|w| w.contains(&format!("alias {name}: target unavailable"))));
    }
}

#[test]
fn alias_chain_boundary_counts_alias_hops_not_terminal_targets() {
    for target in ["canonical", "combo/canonical", "openai/direct-test"] {
        for hops in [127, 128, 129] {
            let mut root = fixture();
            let mut aliases = serde_json::Map::new();
            for i in 0..hops {
                let next = if i + 1 == hops {
                    target.into()
                } else {
                    format!("chain-{}", i + 1)
                };
                aliases.insert(format!("chain-{i}"), Value::String(next));
            }
            root["key_value"]["modelAliases"] = aliases.into();
            root["model_combo_mappings"] = json!([]);
            let dir = tempfile::tempdir().unwrap();
            let output = dir.path().join("out.toml");
            let report = llmux_import::import(&root.to_string(), &output, None).unwrap();
            let config = RuntimeConfig::load(output).unwrap();
            assert_eq!(
                config.model_names.contains_key("chain-0"),
                hops <= 128,
                "{target}: {hops}"
            );
            assert!(config.model_names.contains_key("chain-1"));
            assert_eq!(
                report
                    .warnings
                    .iter()
                    .any(|w| w.contains("alias chain-0: skipped cycle or chain safety limit")),
                hops > 128
            );
        }
    }
}
