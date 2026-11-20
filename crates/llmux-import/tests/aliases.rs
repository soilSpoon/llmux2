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
