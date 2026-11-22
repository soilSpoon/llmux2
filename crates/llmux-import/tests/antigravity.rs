use llmux_config::RuntimeConfig;
use llmux_import::{ImportOptions, OmniRouteFallback};
use llmux_types::ChatRequestField;
use serde_json::{json, Value};

fn fixture() -> Value {
    json!({
        "providerConnections":[
            {"id":"ag","provider":"antigravity","authType":"oauth","accessToken":"synthetic-oauth"},
            {"id":"slow","provider":"gemini","authType":"apikey","apiKey":"synthetic-slow","priority":9},
            {"id":"fast","provider":"gemini","authType":"apikey","apiKey":"synthetic-fast","priority":1},
            {"id":"inactive","provider":"gemini","authType":"apikey","apiKey":"synthetic-inactive","isActive":false},
            {"id":"bad-url","provider":"gemini","authType":"apikey","apiKey":"synthetic-bad","providerSpecificData":{"baseUrl":"https://unverified.example/v1"}},
            {"id":"baseline","provider":"openai","authType":"apikey","apiKey":"synthetic-baseline"}
        ],
        "combos":[
            {"id":"reviewed","name":"ag-combo","models":[{"providerId":"antigravity","model":"gemini-3-flash","connectionId":"ag"}]},
            {"name":"ag-allowed","models":[{"providerId":"antigravity","model":"gemini-3-flash","allowedConnectionIds":["ag"]}]},
            {"name":"nested","models":[{"kind":"combo-ref","comboName":"ag-combo"}]},
            {"name":"unsupported","models":["antigravity/gemini-3-pro"]},
            {"name":"baseline","models":["openai/test"]}
        ],
        "modelAliases":{"ag-alias":"antigravity/gemini-3-flash","chain":"ag-alias","unsupported-alias":"antigravity/gemini-3-pro"},
        "modelComboMappings":[{"pattern":"ag-mapped","comboId":"reviewed","enabled":true}],
        "apiKeys":[{"key":"synthetic-client"}]
    })
}

#[test]
fn reviewed_antigravity_model_routes_aliases_and_combos_through_gemini_with_fallback_last() {
    assert_reviewed_route("gemini-3-flash", "gemini-3-flash-preview");
}

#[test]
fn reviewed_flash_lite_routes_aliases_and_combos_through_gemini_with_fallback_last() {
    assert_reviewed_route("gemini-3.1-flash-lite", "gemini-3.1-flash-lite");
}

fn assert_reviewed_route(source: &str, upstream: &str) {
    for fallback in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.toml");
        let key = dir.path().join("fallback-key");
        std::fs::write(&key, "synthetic-fallback").unwrap();
        let reference = format!("file:{}", key.display());
        let options = ImportOptions {
            omniroute_fallback: fallback.then_some(OmniRouteFallback {
                base_url: "http://127.0.0.1:20128/v1",
                key: &reference,
            }),
            ..Default::default()
        };
        let report = llmux_import::import_with_options(
            &fixture().to_string().replace("gemini-3-flash", source),
            &output,
            None,
            &options,
        )
        .unwrap();
        assert!(report.warnings.iter().any(|warning| warning.contains("reviewed Antigravity replacement uses eligible Gemini connections instead of source connection restrictions")));
        let config = RuntimeConfig::load(&output).unwrap();
        for name in [
            "antigravity/gemini-3-flash",
            "ag-combo",
            "ag-allowed",
            "nested",
            "ag-alias",
            "chain",
            "ag-mapped",
        ] {
            let name = name.replace("gemini-3-flash", source);
            let model = &config.models[*config
                .model_names
                .get(name.as_str())
                .unwrap_or_else(|| panic!("missing {name}"))];
            let deps: Vec<_> = model
                .deployments
                .iter()
                .map(|idx| &config.deployments[idx.0 as usize])
                .collect();
            assert_eq!(deps.len(), if fallback { 3 } else { 2 }, "{name}");
            assert_eq!(deps[0].credential.as_ref(), "upstream-fast", "{name}");
            assert_eq!(deps[1].credential.as_ref(), "upstream-slow", "{name}");
            for dep in &deps[..2] {
                assert_eq!(dep.upstream_model.as_ref(), upstream);
                assert_eq!(
                    dep.strip_request_fields.as_ref(),
                    &[ChatRequestField::Seed, ChatRequestField::User]
                );
                assert_eq!(
                    dep.base_url.to_string(),
                    "https://generativelanguage.googleapis.com/v1beta/openai"
                );
                assert!(!dep.fallback);
            }
            if fallback {
                assert!(deps[2].fallback);
                assert!(deps[2].priority > deps[1].priority);
                assert_eq!(deps[2].upstream_model, model.name);
            }
        }
        assert!(!config.model_names.contains_key("antigravity/gemini-3-pro"));
        assert!(!config.model_names.contains_key("unsupported-alias"));
        assert!(!config
            .api_keys
            .keys()
            .any(|key| key.as_ref() == "upstream-ag"));
        assert!(!std::fs::read_to_string(output)
            .unwrap()
            .contains("synthetic-oauth"));
    }
}

#[test]
fn unusable_or_malformed_source_steps_do_not_get_replacement_deployments() {
    let mut root = fixture();
    root["combos"].as_array_mut().unwrap().extend([
        json!({"name":"disabled","models":[{"providerId":"antigravity","model":"gemini-3-flash","usable":false}]}),
        json!({"name":"malformed","models":[{"providerId":"antigravity","model":"gemini-3-flash","allowedConnectionIds":"ag"}]})
    ]);
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let report = llmux_import::import(&root.to_string(), &output, None).unwrap();
    let config = RuntimeConfig::load(output).unwrap();
    assert!(!config.model_names.contains_key("disabled"));
    assert!(!config.model_names.contains_key("malformed"));
    assert!(report
        .warnings
        .iter()
        .any(|w| w.contains("skipped usable: false")));
    assert!(report
        .warnings
        .iter()
        .any(|w| w.contains("malformed connection restriction")));
}

#[test]
fn reviewed_route_is_unavailable_without_an_eligible_gemini_connection() {
    assert_unavailable_route("gemini-3-flash");
}

#[test]
fn reviewed_flash_lite_is_unavailable_without_an_eligible_gemini_connection() {
    assert_unavailable_route("gemini-3.1-flash-lite");
}

fn assert_unavailable_route(source: &str) {
    let mut root = fixture();
    root["providerConnections"]
        .as_array_mut()
        .unwrap()
        .retain(|c| c["provider"] != "gemini" || c["id"] == "inactive" || c["id"] == "bad-url");
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let report = llmux_import::import(
        &root.to_string().replace("gemini-3-flash", source),
        &output,
        None,
    )
    .unwrap();
    let config = RuntimeConfig::load(output).unwrap();
    for name in [
        "antigravity/gemini-3-flash",
        "ag-combo",
        "nested",
        "ag-alias",
        "chain",
        "ag-mapped",
    ] {
        assert!(
            !config
                .model_names
                .contains_key(name.replace("gemini-3-flash", source).as_str()),
            "{name}"
        );
    }
    assert!(report
        .warnings
        .iter()
        .any(|w| w.contains("no eligible imported connection")));
}
