use llmux_config::RuntimeConfig;
use serde_json::{json, Value};

fn fixture() -> Value {
    json!({
        "providerNodes":[{"id":"node","prefix":"gateway","type":"openai-compatible","baseUrl":"https://example.com/v1","modelsPath":"/models"}],
        "providerConnections":[
            {"id":"later","provider":"node","authType":"apikey","apiKey":"synthetic-later","priority":20},
            {"id":"first","provider":"node","authType":"apikey","apiKey":"synthetic-first","priority":1},
            {"id":"builtin","provider":"anthropic","authType":"apikey","apiKey":"synthetic-builtin"}
        ],
        "combos":[{"name":"pinned","models":[{"providerId":"node","model":"gateway/deepseek/text","connectionId":"later"},"anthropic/text"]}],
        "apiKeys":[{"key":"synthetic-client"}]
    })
}

#[test]
fn direct_ids_use_all_eligible_connections_in_priority_order() {
    let root = fixture();
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    llmux_import::import(&root.to_string(), &output, None).unwrap();
    let config = RuntimeConfig::load(output).unwrap();
    for name in ["gateway/deepseek/text", "node/deepseek/text"] {
        let model = &config.models[*config.model_names.get(name).expect("direct name missing")];
        assert_eq!(model.deployments.len(), 2);
        for (i, credential) in ["upstream-first", "upstream-later"].iter().enumerate() {
            let deployment = &config.deployments[model.deployments[i].0 as usize];
            assert_eq!(deployment.credential.as_ref(), *credential);
            assert_eq!(deployment.upstream_model.as_ref(), "deepseek/text");
            assert_eq!(deployment.priority, i as i32);
        }
    }
    assert!(config.model_names.contains_key("anthropic/text"));
    let pinned = &config.models[config.model_names["pinned"]];
    assert_eq!(
        config.deployments[pinned.deployments[0].0 as usize]
            .credential
            .as_ref(),
        "upstream-later"
    );
    assert!(!config.model_names.contains_key("gateway/models"));
}

#[test]
fn registry_aliases_and_alias_targets_are_source_backed() {
    let mut root = fixture();
    root["providerConnections"].as_array_mut().unwrap().extend([
        json!({"id":"deep","provider":"deepseek","authType":"apikey","apiKey":"synthetic-deep"}),
        json!({"id":"kilo","provider":"kilocode","authType":"none"}),
    ]);
    root["combos"][0]["models"] = json!([
        "ds/vendor/text",
        "kc/vendor/free",
        "deepseek/vendor/canonical",
        "kilocode/vendor/canonical"
    ]);
    root["modelAliases"] = json!({"public":"gateway/openai/alias-target"});
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    llmux_import::import(&root.to_string(), &output, None).unwrap();
    let config = RuntimeConfig::load(output).unwrap();
    for name in [
        "ds/vendor/text",
        "deepseek/vendor/text",
        "kc/vendor/free",
        "kilocode/vendor/free",
        "ds/vendor/canonical",
        "deepseek/vendor/canonical",
        "kc/vendor/canonical",
        "kilocode/vendor/canonical",
        "gateway/openai/alias-target",
        "node/openai/alias-target",
        "public",
    ] {
        assert!(config.model_names.contains_key(name), "{name}");
    }
    let model = &config.models[config.model_names["kc/vendor/free"]];
    assert!(config.deployments[model.deployments[0].0 as usize]
        .credential
        .is_empty());
    assert!(!config.model_names.contains_key("anthropic/not-observed"));
}

#[test]
fn direct_name_collisions_keep_combos_and_aliases() {
    let mut root = fixture();
    root["combos"].as_array_mut().unwrap().extend([
        json!({"name":"gateway/deepseek/text","models":["anthropic/combo-wins"]}),
        json!({"name":"other","models":[{"providerId":"node","model":"alias-wins"}]}),
    ]);
    root["modelAliases"] = json!({"gateway/alias-wins":"gateway/deepseek/text"});
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let report = llmux_import::import(&root.to_string(), &output, None).unwrap();
    let config = RuntimeConfig::load(output).unwrap();
    let winner = config.model_names["gateway/deepseek/text"];
    assert_eq!(config.model_names["gateway/alias-wins"], winner);
    let model = &config.models[winner];
    assert_eq!(
        config.deployments[model.deployments[0].0 as usize]
            .upstream_model
            .as_ref(),
        "combo-wins"
    );
    let warnings = report.warnings.join("\n");
    assert!(warnings.contains("direct gateway/deepseek/text: collision"));
    assert!(warnings.contains("direct gateway/alias-wins: collision"));
}

#[test]
fn skipped_connections_and_unusable_legs_never_create_direct_names() {
    let mut root = fixture();
    root["providerNodes"].as_array_mut().unwrap().push(json!({"id":"local","prefix":"local","type":"openai-compatible","baseUrl":"http://localhost:8788/v1"}));
    for (provider, auth, extra) in [
        ("codex", "apikey", json!({})),
        ("factory", "apikey", json!({})),
        ("openai", "oauth", json!({})),
        ("local", "apikey", json!({})),
        ("cloudflare-playground", "none", json!({})),
        ("mistral", "apikey", json!({"usable":false})),
        ("groq", "apikey", json!({"isActive":false})),
    ] {
        let mut connection =
            json!({"id":provider,"provider":provider,"authType":auth,"apiKey":"synthetic-skipped"});
        connection
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        root["providerConnections"]
            .as_array_mut()
            .unwrap()
            .push(connection);
        root["combos"][0]["models"]
            .as_array_mut()
            .unwrap()
            .push(json!(format!("{provider}/skipped")));
    }
    root["combos"][0]["models"].as_array_mut().unwrap().extend([
        json!({"providerId":"node","model":"unusable","usable":false}),
        json!("gateway/*"),
        json!({"kind":"combo-ref","comboName":"missing"}),
        json!("unqualified"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    llmux_import::import(&root.to_string(), &output, None).unwrap();
    let config = RuntimeConfig::load(output).unwrap();
    for name in [
        "codex/skipped",
        "factory/skipped",
        "openai/skipped",
        "local/skipped",
        "cloudflare-playground/skipped",
        "mistral/skipped",
        "groq/skipped",
        "gateway/unusable",
        "node/unusable",
        "gateway/*",
        "unqualified",
    ] {
        assert!(!config.model_names.contains_key(name), "{name}");
    }
    assert!(config.model_names.contains_key("gateway/deepseek/text"));
}

#[test]
fn direct_ids_work_for_raw_dump_rows_and_empty_sources_fail_closed() {
    let root = json!({
        "provider_nodes":[{"id":"node","prefix":"gateway","type":"openai-compatible","base_url":"https://example.com/v1"}],
        "provider_connections":[{"id":"key","provider":"node","auth_type":"apikey","api_key":"synthetic-key","is_active":1}],
        "api_keys":[{"key":"synthetic-client"}],
        "combos":[{"data":"{\"name\":\"pinned\",\"models\":[\"gateway/openai/text\"]}"}]
    });
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    llmux_import::import(&root.to_string(), &output, None).unwrap();
    let config = RuntimeConfig::load(output).unwrap();
    assert!(config.model_names.contains_key("gateway/openai/text"));
    let mut empty = fixture();
    empty["combos"] = json!([]);
    let empty_dir = tempfile::tempdir().unwrap();
    let empty_output = empty_dir.path().join("empty.toml");
    let error = llmux_import::import(&empty.to_string(), &empty_output, None)
        .err()
        .unwrap();
    assert!(error.contains("no importable models"), "{error}");
    assert!(!empty_output.exists());
}

#[test]
fn unavailable_mapping_winners_reserve_direct_names() {
    for pattern in ["gateway/deepseek/text", "gateway/*"] {
        let mut root = fixture();
        root["modelComboMappings"] =
            json!([{"pattern":pattern,"comboId":"missing-id","enabled":true}]);
        root["modelAliases"] = json!({"gateway/deepseek/text":"anthropic/legacy-fallback"});
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.toml");
        let report = llmux_import::import(&root.to_string(), &output, None).unwrap();
        let config = RuntimeConfig::load(output).unwrap();
        assert!(!config.model_names.contains_key("gateway/deepseek/text"));
        assert!(config.model_names.contains_key("node/deepseek/text"));
        assert!(report
            .warnings
            .iter()
            .any(|warning| warning.contains("direct gateway/deepseek/text: collision")));
    }
}

#[test]
fn repeated_custom_routing_segments_preserve_vendor_ids_and_observed_names() {
    let mut root = fixture();
    root["combos"][0]["models"].as_array_mut().unwrap().extend([
        json!("node/gateway/node/vendor/repeated"),
        json!("gateway/node/gateway/vendor/repeated"),
        json!({"provider":"node","model":"gateway/node/openai/object"}),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    llmux_import::import(&root.to_string(), &output, None).unwrap();
    let config = RuntimeConfig::load(output).unwrap();
    for (name, upstream) in [
        ("node/gateway/node/vendor/repeated", "vendor/repeated"),
        ("gateway/node/gateway/vendor/repeated", "vendor/repeated"),
        ("node/vendor/repeated", "vendor/repeated"),
        ("gateway/vendor/repeated", "vendor/repeated"),
        ("gateway/openai/object", "openai/object"),
        ("gateway/node/openai/object", "openai/object"),
    ] {
        let model = &config.models[config.model_names[name]];
        assert!(
            model
                .deployments
                .iter()
                .all(|deployment| config.deployments[deployment.0 as usize]
                    .upstream_model
                    .as_ref()
                    == upstream),
            "{name}"
        );
    }
}

#[test]
fn custom_node_ids_do_not_claim_another_providers_registry_alias() {
    let mut root = fixture();
    root["providerNodes"][0]["prefix"] = json!("ds");
    root["providerConnections"].as_array_mut().unwrap().push(
        json!({"id":"deep","provider":"deepseek","authType":"apikey","apiKey":"synthetic-deep"}),
    );
    root["combos"][0]["models"] = json!(["node/vendor/custom", "deepseek/vendor/native"]);
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    llmux_import::import(&root.to_string(), &output, None).unwrap();
    let config = RuntimeConfig::load(output).unwrap();
    let model = &config.models[config.model_names["node/vendor/custom"]];
    assert_eq!(model.deployments.len(), 2);
    assert!(!config.model_names.contains_key("ds/vendor/custom"));
    let native = &config.models[config.model_names["ds/vendor/native"]];
    assert_eq!(
        config.deployments[native.deployments[0].0 as usize]
            .credential
            .as_ref(),
        "upstream-deep"
    );
}

#[test]
fn synthetic_direct_steps_preserve_resolved_provider_identity() {
    for custom_connected in [false, true] {
        let mut root = fixture();
        root["providerNodes"][0]["prefix"] = json!("deepseek");
        if !custom_connected {
            root["providerConnections"]
                .as_array_mut()
                .unwrap()
                .retain(|connection| connection["provider"] != "node");
        }
        root["providerConnections"].as_array_mut().unwrap().push(json!({"id":"deep","provider":"deepseek","authType":"apikey","apiKey":"synthetic-deep"}));
        root["combos"][0]["models"] =
            json!(["ds/test", "ds/deepseek/vendor", {"providerId":"node","model":"vendor/custom"}]);
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.toml");
        llmux_import::import(&root.to_string(), &output, None).unwrap();
        let config = RuntimeConfig::load(output).unwrap();
        for (name, upstream) in [
            ("ds/test", "test"),
            ("deepseek/test", "test"),
            ("ds/deepseek/vendor", "deepseek/vendor"),
        ] {
            let model = &config.models[*config
                .model_names
                .get(name)
                .unwrap_or_else(|| panic!("missing {name}; custom_connected={custom_connected}"))];
            assert_eq!(model.deployments.len(), 1, "{name}");
            let deployment = &config.deployments[model.deployments[0].0 as usize];
            assert_eq!(deployment.credential.as_ref(), "upstream-deep", "{name}");
            assert_eq!(deployment.upstream_model.as_ref(), upstream, "{name}");
        }
        assert_eq!(
            config.model_names.contains_key("node/vendor/custom"),
            custom_connected
        );
    }
}

#[test]
fn malformed_restrictions_and_nested_combo_names_are_not_model_sources() {
    let mut root = fixture();
    root["combos"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name":"gateway/nested","models":["anthropic/nested"]}));
    root["combos"][0]["models"].as_array_mut().unwrap().extend([
        json!({"model":"gateway/malformed-pin","connectionId":123}),
        json!({"model":"gateway/malformed-allowed","allowedConnectionIds":[null]}),
        json!({"model":"gateway/nested"}),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    llmux_import::import(&root.to_string(), &output, None).unwrap();
    let config = RuntimeConfig::load(output).unwrap();
    for name in [
        "gateway/malformed-pin",
        "node/malformed-pin",
        "gateway/malformed-allowed",
        "node/malformed-allowed",
        "node/nested",
    ] {
        assert!(!config.model_names.contains_key(name), "{name}");
    }
}
