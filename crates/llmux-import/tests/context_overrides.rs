use llmux_config::RuntimeConfig;
use serde_json::{json, Value};

fn fixture() -> Value {
    json!({
        "providerConnections":[
            {"id":"official","provider":"openai","authType":"apikey","apiKey":"synthetic-official"},
            {"id":"custom","provider":"ds","authType":"apikey","apiKey":"synthetic-custom"}
        ],
        "providerNodes":[{"id":"ds","prefix":"gateway","type":"openai-compatible","baseUrl":"https://example.com/v1"}],
        "combos":[
            {"name":"mixed","models":["openai/small","gateway/deepseek/test"]},
            {"name":"nested","models":[{"kind":"combo-ref","comboName":"mixed"}]},
            {"name":"unchecked","models":["openai/unknown"]}
        ],
        "modelAliases":{"alias":"mixed"},
        "apiKeys":[{"key":"synthetic-client"}],
        "modelContextOverrides":[
            {"provider":"openai","modelId":"small","realContext":4096,"source":"manual"},
            {"provider":"ds","modelId":"deepseek/test","realContext":16384,"source":"auto:discovery"},
            {"provider":"openai","modelId":"absent","realContext":8192}
        ],
        "modelCapabilityOverrides":[
            {"provider":"openai","modelId":"small","overrideKey":"max_input_tokens","overrideValue":"1000"},
            {"provider":"openai","modelId":"small","overrideKey":"max_message_parts","overrideValue":"10"}
        ]
    })
}

fn raw_dump(root: &Value) -> Value {
    json!({
        "provider_connections":root["providerConnections"],
        "provider_nodes":root["providerNodes"],
        "combos":root["combos"],
        "api_keys":root["apiKeys"],
        "model_context_overrides":[
            {"provider":"openai","model_id":"small","real_context":4096,"source":"manual"},
            {"provider":"ds","model_id":"deepseek/test","real_context":16384,"source":"auto:discovery"},
            {"provider":"openai","model_id":"absent","real_context":8192}
        ],
        "model_capability_overrides":root["modelCapabilityOverrides"]
    })
}

#[test]
fn context_overrides_load_on_direct_and_nested_deployments_without_changing_other_limits() {
    for root in [fixture(), raw_dump(&fixture())] {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.toml");
        let report = llmux_import::import(&root.to_string(), &output, None).unwrap();
        let config = RuntimeConfig::load(output).unwrap();
        if root.get("modelAliases").is_some() {
            assert_eq!(config.model_names["alias"], config.model_names["mixed"]);
        }
        for name in ["mixed", "nested"] {
            let model = &config.models[config.model_names[name]];
            let limits: Vec<_> = model
                .deployments
                .iter()
                .map(|idx| config.deployments[idx.0 as usize].max_context)
                .collect();
            assert_eq!(limits, vec![Some(4096), Some(16384)], "{name}");
        }
        for (name, limit) in [
            ("openai/small", Some(4096)),
            ("gateway/deepseek/test", Some(16384)),
            ("unchecked", None),
        ] {
            let model = &config.models[config.model_names[name]];
            assert_eq!(
                config.deployments[model.deployments[0].0 as usize].max_context, limit,
                "{name}"
            );
        }
        assert!(!config.model_names.contains_key("openai/absent"));
        assert!(report
            .warnings
            .iter()
            .any(|w| w
                == "context overrides: 2 applied, 1 ignored (no imported deployment), 0 invalid"));
        assert!(report
            .warnings
            .iter()
            .any(|w| w.contains("2 capability overrides ignored")));
    }
}

#[test]
fn invalid_context_values_warn_without_echoing_input_and_never_emit_limits() {
    for value in [
        json!(0),
        json!(-1),
        json!(1.5),
        json!(4294967296u64),
        json!("private-value"),
        json!(true),
        Value::Null,
    ] {
        let mut root = fixture();
        root["modelContextOverrides"] =
            json!([{"provider":"openai","modelId":"small","realContext":value}]);
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.toml");
        let report = llmux_import::import(&root.to_string(), &output, None).unwrap();
        let config = RuntimeConfig::load(output).unwrap();
        assert!(config
            .deployments
            .iter()
            .all(|dep| dep.max_context.is_none()));
        assert!(report
            .warnings
            .iter()
            .any(|w| w.contains("invalid context override")));
        assert!(!report.warnings.join("\n").contains("private-value"));
    }
}

#[test]
fn absent_or_empty_context_overrides_leave_deployments_unchecked() {
    let mut root = fixture();
    root.as_object_mut()
        .unwrap()
        .remove("modelContextOverrides");
    root.as_object_mut()
        .unwrap()
        .remove("modelCapabilityOverrides");
    for empty in [false, true] {
        if empty {
            root["modelContextOverrides"] = json!([]);
            root["modelCapabilityOverrides"] = json!([]);
        }
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.toml");
        let report = llmux_import::import(&root.to_string(), &output, None).unwrap();
        let config = RuntimeConfig::load(output).unwrap();
        assert!(config
            .deployments
            .iter()
            .all(|dep| dep.max_context.is_none()));
        assert!(!report
            .warnings
            .iter()
            .any(|w| w.starts_with("context overrides:")));
    }
}

#[test]
fn malformed_override_tables_warn_without_aborting_import() {
    for raw in [false, true] {
        let mut root = if raw { raw_dump(&fixture()) } else { fixture() };
        let context = if raw {
            "model_context_overrides"
        } else {
            "modelContextOverrides"
        };
        let capability = if raw {
            "model_capability_overrides"
        } else {
            "modelCapabilityOverrides"
        };
        root[context] = json!("private-malformed-table");
        root[capability] = json!({"private-malformed-table":1});
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.toml");
        let report = llmux_import::import(&root.to_string(), &output, None).unwrap();
        let config = RuntimeConfig::load(output).unwrap();
        assert!(config
            .deployments
            .iter()
            .all(|dep| dep.max_context.is_none()));
        assert!(report
            .warnings
            .iter()
            .any(|w| w.contains("invalid context override table")));
        assert!(report
            .warnings
            .iter()
            .any(|w| w.contains("invalid capability override table")));
        assert!(!report
            .warnings
            .join("\n")
            .contains("private-malformed-table"));
    }
}

#[test]
fn invalid_raw_rows_and_duplicate_overrides_warn_and_preserve_the_first_valid_limit() {
    let mut root = raw_dump(&fixture());
    root["model_context_overrides"] = json!([
        {"provider":"openai","model_id":"small","real_context":4096},
        {"provider":"openai","model_id":"small","real_context":8192},
        {"provider":"openai","model_id":"unknown","real_context":0},
        {"model_id":"unknown","real_context":8192},
        "private-malformed-row"
    ]);
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let report = llmux_import::import(&root.to_string(), &output, None).unwrap();
    let config = RuntimeConfig::load(output).unwrap();
    let model = &config.models[config.model_names["openai/small"]];
    assert_eq!(
        config.deployments[model.deployments[0].0 as usize].max_context,
        Some(4096)
    );
    assert!(report.warnings.iter().any(
        |w| w == "context overrides: 1 applied, 0 ignored (no imported deployment), 4 invalid"
    ));
    assert!(!report.warnings.join("\n").contains("private-malformed-row"));
}

#[test]
fn deepai_overrides_apply_to_native_text_deployments_when_a_custom_node_owns_the_prefix() {
    let root = json!({
        "providerConnections":[
            {"id":"native","provider":"deepai","authType":"apikey","apiKey":"synthetic-deepai"},
            {"id":"code","provider":"custom-deepai","authType":"apikey","apiKey":"synthetic-sidecar"}
        ],
        "providerNodes":[{"id":"custom-deepai","prefix":"deepai","type":"openai-compatible","apiType":"chat","baseUrl":"http://127.0.0.1:8790/deepai/v1"}],
        "combos":[{"name":"text","models":["deepai/text-generator","deepai/example-code"]}],
        "apiKeys":[{"key":"synthetic-client"}],
        "modelContextOverrides":[
            {"provider":"deepai","modelId":"text-generator","realContext":4096},
            {"provider":"custom-deepai","modelId":"example-code","realContext":8192}
        ]
    });
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let report = llmux_import::import(&root.to_string(), &output, None).unwrap();
    let config = RuntimeConfig::load(&output).unwrap();
    let model = &config.models[config.model_names["text"]];
    let limits: Vec<(String, Option<u32>)> = model
        .deployments
        .iter()
        .map(|idx| {
            let dep = &config.deployments[idx.0 as usize];
            (dep.upstream_model.to_string(), dep.max_context)
        })
        .collect();
    assert_eq!(
        limits,
        vec![
            ("text-generator".to_owned(), Some(4096)),
            ("example-code".to_owned(), Some(8192))
        ]
    );
    let native = &config.deployments[model.deployments[0].0 as usize];
    assert_eq!(native.provider.0.as_ref(), "deepai");
    assert_eq!(
        config.deployments[model.deployments[1].0 as usize]
            .provider
            .0
            .as_ref(),
        "openai_compat"
    );
    assert!(report.warnings.iter().any(
        |w| w == "context overrides: 2 applied, 0 ignored (no imported deployment), 0 invalid"
    ));
}

#[test]
fn context_overrides_do_not_constrain_the_omniroute_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let key = dir.path().join("fallback-key");
    std::fs::write(&key, "synthetic-fallback").unwrap();
    let reference = format!("file:{}", key.display());
    llmux_import::import_with_fallback(
        &fixture().to_string(),
        &output,
        None,
        Some(llmux_import::OmniRouteFallback {
            base_url: "http://127.0.0.1:20128/v1",
            key: &reference,
        }),
    )
    .unwrap();
    let config = RuntimeConfig::load(output).unwrap();
    let model = &config.models[config.model_names["mixed"]];
    assert_eq!(model.capabilities.max_context, None);
    let limits: Vec<_> = model
        .deployments
        .iter()
        .map(|idx| config.deployments[idx.0 as usize].max_context)
        .collect();
    assert_eq!(limits, vec![Some(4096), Some(16384), None]);
}
