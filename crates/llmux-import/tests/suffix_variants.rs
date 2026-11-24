use llmux_config::RuntimeConfig;
use llmux_import::ImportOptions;
use serde_json::{json, Value};

fn fixture() -> Value {
    json!({
        "_meta": {"version": "omniroute-v3-legacy-export"},
        "providerNodes": [
            {"id":"openai-compatible-vendor","prefix":"vendor","type":"openai-compatible","baseUrl":"https://vendor.example/v1"}
        ],
        "providerConnections": [
            {"id":"vendor-conn","provider":"openai-compatible-vendor","authType":"apikey","apiKey":"synthetic-vendor"}
        ],
        "combos": [
            {"id":"fable-5-id","name":"claude-fable-5","models":["vendor/claude-fable-5"]},
            {"id":"fable-5-low-id","name":"claude-fable-5-low","models":["vendor/claude-fable-5-low"]}
        ],
        "apiKeys": [{"key":"synthetic-client"}]
    })
}

#[test]
fn suffixed_mapping_alias_does_not_collapse_onto_the_base_name() {
    let mut root = fixture();
    root["modelComboMappings"] = json!([
        {"pattern":"vendor/claude-fable-5","comboId":"fable-5-id","enabled":true},
        {"pattern":"vendor/claude-fable-5-low","comboId":"fable-5-id","enabled":true}
    ]);
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let report = llmux_import::import(&root.to_string(), &output, None).unwrap();
    let config = RuntimeConfig::load(&output).unwrap();
    let text = std::fs::read_to_string(&output).unwrap();
    let base = config.model_names["claude-fable-5"];
    // An unsuffixed spelling still lands on the base model as an alias.
    assert_eq!(config.model_names["vendor/claude-fable-5"], base);
    // A suffixed spelling must not: the effort is part of the model id, so
    // the name stays distinct and falls through to the catch-all instead of
    // silently serving the base upstream at default effort.
    assert_ne!(
        config.model_names.get("vendor/claude-fable-5-low"),
        Some(&base)
    );
    // The mapping still reserves the direct name, so no own model entry
    // either — an unmatched request reaches the `*` catch-all, which forwards
    // the suffixed id to the real suffixed upstream.
    assert!(!config.model_names.contains_key("vendor/claude-fable-5-low"));
    let _ = text;
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("vendor/claude-fable-5-low")
                && warning.contains("suffix"))
    );
}

#[test]
fn suffixed_mapping_alias_is_kept_when_the_target_carries_the_same_suffix() {
    let mut root = fixture();
    root["modelComboMappings"] = json!([
        {"pattern":"vendor/claude-fable-5-low","comboId":"fable-5-low-id","enabled":true}
    ]);
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    llmux_import::import(&root.to_string(), &output, None).unwrap();
    let config = RuntimeConfig::load(&output).unwrap();
    assert_eq!(
        config.model_names["vendor/claude-fable-5-low"],
        config.model_names["claude-fable-5-low"]
    );
}

#[test]
fn canonical_map_keeps_suffixed_names_as_distinct_entries() {
    let mut root = fixture();
    root["modelComboMappings"] = json!([
        {"pattern":"vendor/claude-fable-5","comboId":"fable-5-id","enabled":true},
        {"pattern":"vendor/claude-fable-5-low","comboId":"fable-5-id","enabled":true}
    ]);
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let map_path = dir.path().join("canonical-map.json");
    let options = ImportOptions {
        canonical_map: Some(&map_path),
        ..ImportOptions::default()
    };
    llmux_import::import_with_options(&root.to_string(), &output, None, &options).unwrap();
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(&map_path).unwrap()).unwrap();
    let map = &doc["map"];
    // Values uniformly lead with the name itself, then the spellings that
    // resolve to it — so the metadata counts are consistent across entries.
    let fable = map["claude-fable-5"].as_array().unwrap();
    assert_eq!(fable[0], "claude-fable-5");
    assert!(fable.iter().any(|name| name == "vendor/claude-fable-5"));
    assert!(!fable.iter().any(|name| name == "vendor/claude-fable-5-low"));
    assert_eq!(
        map["vendor/claude-fable-5-low"],
        json!(["vendor/claude-fable-5-low"])
    );
    let spellings: usize = map
        .as_object()
        .unwrap()
        .values()
        .map(|names| names.as_array().unwrap().len())
        .sum();
    assert_eq!(doc["entries_before"], json!(spellings));
    assert_eq!(doc["entries_after"], json!(map.as_object().unwrap().len()));
    // A cluster is a name plus at least one other spelling.
    let merged: usize = map
        .as_object()
        .unwrap()
        .values()
        .filter(|names| names.as_array().unwrap().len() > 1)
        .count();
    assert_eq!(doc["clusters"], json!(merged));
}

#[test]
fn canonical_map_drops_suffixed_names_excluded_from_the_import() {
    let mut root = fixture();
    root["modelComboMappings"] =
        json!([{"pattern":"vendor/claude-fable-5-low","comboId":"fable-5-id","enabled":true}]);
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let map_path = dir.path().join("canonical-map.json");
    let exclude_path = dir.path().join("exclude.json");
    std::fs::write(&exclude_path, "[\"vendor/claude-fable-5-low\"]").unwrap();
    let options = ImportOptions {
        canonical_map: Some(&map_path),
        exclude_models: Some(&exclude_path),
        ..ImportOptions::default()
    };
    llmux_import::import_with_options(&root.to_string(), &output, None, &options).unwrap();
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(&map_path).unwrap()).unwrap();
    // The catch-all's exclude list blocks this spelling, so the map must not
    // advertise it as a reachable self-resolving entry.
    assert!(doc["map"].get("vendor/claude-fable-5-low").is_none());
}

#[test]
fn canonical_map_rejects_paths_colliding_with_import_files() {
    let root = fixture();
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let options = ImportOptions {
        canonical_map: Some(&output),
        ..ImportOptions::default()
    };
    assert!(llmux_import::import_with_options(&root.to_string(), &output, None, &options).is_err());
    assert!(!output.exists());

    let secrets = dir.path().join("secrets");
    let options = ImportOptions {
        canonical_map: Some(&secrets.join("conn-1")),
        ..ImportOptions::default()
    };
    assert!(llmux_import::import_with_options(&root.to_string(), &output, None, &options).is_err());
    assert!(!secrets.exists());
}
