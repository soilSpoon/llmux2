use llmux_config::RuntimeConfig;
use serde_json::{json, Value};

fn fixture() -> Value {
    json!({
        "providerConnections": [
            {"id":"custom", "provider":"node", "authType":"apikey", "apiKey":"synthetic-key"}
        ],
        "providerNodes": [
            {"id":"node", "prefix":"gateway", "type":"openai-compatible", "baseUrl":"https://example.com/v1"}
        ],
        "combos":[{"name":"test", "models":["gateway/deepseek/test"]}],
        "apiKeys":[{"key":"synthetic-client"}]
    })
}

#[test]
fn gemini_endpoint_deployments_strip_only_seed_and_user_through_loader() {
    use llmux_types::ChatRequestField;
    for (url, model, expected) in [
        (
            "https://generativelanguage.googleapis.com/v1beta/openai",
            "gemini-3-flash-preview",
            vec![ChatRequestField::Seed, ChatRequestField::User],
        ),
        (
            "https://GENERATIVELANGUAGE.GOOGLEAPIS.COM:443/v1beta/openai/",
            "gemini-3-flash-preview",
            vec![ChatRequestField::Seed, ChatRequestField::User],
        ),
        (
            "https://generativelanguage.googleapis.com.evil.example/v1beta/openai",
            "gemini-3-flash-preview",
            vec![],
        ),
        ("https://example.com/v1", "gemini-3-flash-preview", vec![]),
        (
            "https://generativelanguage.googleapis.com/v1beta/openai",
            "deepseek-r1",
            vec![
                ChatRequestField::Seed,
                ChatRequestField::User,
                ChatRequestField::SafetyIdentifier,
                ChatRequestField::ReasoningEffort,
            ],
        ),
    ] {
        let mut root = fixture();
        root["providerNodes"][0]["baseUrl"] = json!(url);
        root["combos"][0]["models"] = json!([format!("gateway/{model}")]);
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.toml");
        llmux_import::import(&root.to_string(), &output, None).unwrap();
        let config = RuntimeConfig::load(output).unwrap();
        assert!(!config.deployments.is_empty());
        for dep in &config.deployments {
            assert_eq!(dep.strip_request_fields.as_ref(), expected, "{url}");
        }
    }
}

#[test]
fn deepseek_models_on_anthropic_nodes_import_without_stripping() {
    let mut root = fixture();
    root["providerNodes"][0]["type"] = json!("anthropic-compatible");
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    llmux_import::import(&root.to_string(), &output, None).unwrap();
    let config = RuntimeConfig::load(output).unwrap();
    assert!(!config.deployments.is_empty());
    for dep in &config.deployments {
        assert_eq!(dep.upstream_model.as_ref(), "deepseek/test");
        assert_eq!(dep.provider.0.as_ref(), "anthropic");
        assert!(dep.strip_request_fields.is_empty());
    }
}

#[test]
fn custom_headers_are_secret_files_and_connection_overrides_replace_node_headers() {
    for override_headers in [
        None,
        Some(json!({"X-Override":"override-secret"})),
        Some(json!({})),
    ] {
        let mut root = fixture();
        root["providerNodes"][0]["prefix"] = json!("hms");
        root["providerNodes"][0]["customHeaders"] = json!({"X-Node":"node-secret"});
        root["combos"][0]["models"] = json!(["hms/test"]);
        if let Some(headers) = &override_headers {
            root["providerConnections"][0]["providerSpecificData"] =
                json!({"customHeaders":headers});
        }
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.toml");
        llmux_import::import(&root.to_string(), &output, None).unwrap();
        let config = RuntimeConfig::load(&output).unwrap();
        let headers = &config.deployments[0].headers;
        if let Some(overrides) = override_headers {
            assert!(!headers.contains_key("x-node"));
            if !overrides.as_object().unwrap().is_empty() {
                assert_eq!(headers["x-override"], "override-secret");
            }
        } else {
            assert_eq!(headers["x-node"], "node-secret");
        }
        let text = std::fs::read_to_string(&output).unwrap();
        assert!(!text.contains("node-secret") && !text.contains("override-secret"));
    }
}

#[test]
fn malformed_or_forbidden_custom_headers_skip_the_connection_without_leaking() {
    for headers in [
        json!({"Host":"private-secret"}),
        json!({"Authorization":"private-secret"}),
        json!({"X-Test":"private-secret\r\nInjected: true"}),
        json!({"bad name":"private-secret"}),
        json!({"X-Test":true}),
        json!([]),
        json!("invalid-private-secret"),
        json!({"X-Test":"a", "x-test":"b"}),
    ] {
        let mut root = fixture();
        root["providerConnections"].as_array_mut().unwrap().push(json!({"id":"bad", "provider":"bad-node", "authType":"apikey", "apiKey":"bad-key-secret"}));
        root["providerNodes"].as_array_mut().unwrap().push(json!({"id":"bad-node", "prefix":"bad", "type":"openai-compatible", "baseUrl":"https://bad.example/v1", "customHeaders":headers}));
        root["combos"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"bad", "models":["bad/test"]}));
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.toml");
        let report = llmux_import::import(&root.to_string(), &output, None).unwrap();
        assert_eq!(report.credentials, 1);
        let text = std::fs::read_to_string(output).unwrap() + &report.warnings.join("\n");
        assert!(!text.contains("private-secret") && !text.contains("bad-key-secret"));
        assert!(report
            .warnings
            .iter()
            .any(|warning| warning.contains("custom headers")));
    }
}

#[test]
fn anonymous_custom_nodes_can_supply_authorization_headers_without_credentials() {
    let mut root = fixture();
    root["providerConnections"][0]["authType"] = json!("none");
    root["providerNodes"][0]["customHeaders"] =
        json!("{\"Authorization\":\"Bearer synthetic-header\"}");
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    llmux_import::import(&root.to_string(), &output, None).unwrap();
    let config = RuntimeConfig::load(&output).unwrap();
    assert!(config.deployments[0].credential.is_empty());
    assert_eq!(
        config.deployments[0].headers["authorization"],
        "Bearer synthetic-header"
    );
    #[cfg(unix)]
    for entry in std::fs::read_dir(dir.path().join("secrets")).unwrap() {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            entry.unwrap().metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn deepseek_deployments_strip_only_the_four_rejected_fields_through_loader() {
    use llmux_types::ChatRequestField;
    let mut root = fixture();
    root["providerConnections"].as_array_mut().unwrap().push(json!({"id":"deepseek", "provider":"deepseek", "authType":"apikey", "apiKey":"synthetic-deepseek"}));
    root["combos"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name":"deepseek-combo", "models":["ds/deepseek-chat"]}));
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    llmux_import::import(&root.to_string(), &output, None).unwrap();
    let config = RuntimeConfig::load(&output).unwrap();
    let mut deepseek = 0;
    for dep in &config.deployments {
        if dep.credential.as_ref() == "upstream-deepseek"
            || dep.upstream_model.as_ref() == "deepseek/test"
        {
            deepseek += 1;
            assert_eq!(
                dep.strip_request_fields.as_ref(),
                &[
                    ChatRequestField::Seed,
                    ChatRequestField::User,
                    ChatRequestField::SafetyIdentifier,
                    ChatRequestField::ReasoningEffort
                ]
            );
            assert!(!dep
                .strip_request_fields
                .contains(&ChatRequestField::Logprobs));
            assert!(!dep
                .strip_request_fields
                .contains(&ChatRequestField::TopLogprobs));
        } else {
            assert!(dep.strip_request_fields.is_empty());
        }
    }
    assert!(deepseek >= 2, "combo and direct deployments");
}

#[test]
fn strips_custom_routing_segments_but_preserves_vendor_segments() {
    for (provider, model, expected) in [
        ("node", "gateway/deepseek/test", "deepseek/test"),
        ("node", "node/openai/test", "openai/test"),
        ("gateway", "node/openai/test", "openai/test"),
        ("node", "openai/test", "openai/test"),
        ("node", "gateway/gateway/test", "test"),
    ] {
        let mut root = fixture();
        root["combos"][0]["models"] = json!([{
            "kind":"model", "providerId":provider,
            "model":model, "connectionId":"custom"
        }]);
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.toml");
        llmux_import::import(&root.to_string(), &output, None).unwrap();
        let config = RuntimeConfig::load(&output).unwrap();
        assert_eq!(config.deployments[0].upstream_model.as_ref(), expected);
    }
}

#[test]
fn connection_url_wins_and_public_builtins_are_mapped() {
    for (provider, metadata, expected) in [
        (
            "node",
            json!({"baseUrl":"https://public.example/v1"}),
            "https://public.example/v1",
        ),
        (
            "node",
            json!("{\"baseUrl\":\"https://public.example/v1/chat/completions\"}"),
            "https://public.example/v1",
        ),
        (
            "gemini",
            json!({}),
            "https://generativelanguage.googleapis.com/v1beta/openai",
        ),
        ("morph", json!({}), "https://api.morphllm.com/v1"),
    ] {
        let mut root = fixture();
        root["providerNodes"][0]["baseUrl"] = json!("http://127.0.0.1:8788/v1");
        root["providerConnections"][0]["provider"] = json!(provider);
        root["providerConnections"][0]["providerSpecificData"] = metadata;
        root["combos"][0]["models"] = json!([format!("{provider}/test")]);
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.toml");
        llmux_import::import(&root.to_string(), &output, None).unwrap();
        let config = RuntimeConfig::load(&output).unwrap();
        assert_eq!(config.deployments[0].base_url.to_string(), expected);
    }
}

#[test]
fn gemini_overrides_use_only_supported_google_compatibility_roots() {
    let expected = "https://generativelanguage.googleapis.com/v1beta/openai";
    for base_url in [
        "https://generativelanguage.googleapis.com/v1beta",
        "https://generativelanguage.googleapis.com/v1beta/",
        "https://generativelanguage.googleapis.com/v1beta/openai",
        "https://generativelanguage.googleapis.com/v1beta/openai/",
    ] {
        let mut root = fixture();
        root["providerConnections"][0]["provider"] = json!("gemini");
        root["providerConnections"][0]["providerSpecificData"] = json!({"baseUrl":base_url});
        root["combos"][0]["models"] = json!(["gemini/test"]);
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.toml");
        llmux_import::import(&root.to_string(), &output, None).unwrap();
        let config = RuntimeConfig::load(&output).unwrap();
        assert_eq!(config.deployments[0].base_url.to_string(), expected);
    }
    for base_url in [
        "https://generativelanguage.googleapis.com/v1beta/models",
        "https://unsupported.example/private-root",
    ] {
        let mut root = fixture();
        root["providerConnections"]
            .as_array_mut()
            .unwrap()
            .push(json!({
                "id":"gemini", "provider":"gemini", "authType":"apikey", "apiKey":"hidden-secret",
                "providerSpecificData":{"baseUrl":base_url}
            }));
        let dir = tempfile::tempdir().unwrap();
        let report =
            llmux_import::import(&root.to_string(), &dir.path().join("out.toml"), None).unwrap();
        assert_eq!(report.credentials, 1);
        let warnings = report.warnings.join("\n");
        assert!(warnings.contains("unsupported Gemini base URL"));
        assert!(!warnings.contains(base_url));
        assert!(!dir.path().join("secrets/upstream-gemini").exists());
    }
}

#[test]
fn invalid_metadata_is_skipped_without_printing_its_contents() {
    let mut root = fixture();
    root["providerConnections"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "id":"invalid", "provider":"openai", "authType":"apikey", "apiKey":"hidden-secret",
            "providerSpecificData":"sensitive-invalid-metadata"
        }));
    let dir = tempfile::tempdir().unwrap();
    let report =
        llmux_import::import(&root.to_string(), &dir.path().join("out.toml"), None).unwrap();
    assert_eq!(report.credentials, 1);
    assert!(report
        .warnings
        .iter()
        .any(|w| w.contains("invalid provider metadata")));
    assert!(!report
        .warnings
        .join("\n")
        .contains("sensitive-invalid-metadata"));
}

#[test]
fn skips_local_endpoints_and_nonofficial_morph_without_exposing_urls() {
    for host in [
        "127.0.0.1",
        "127.12.34.56",
        "localhost",
        "LOCALHOST.",
        "[::1]",
        "0.0.0.0",
    ] {
        let mut root = fixture();
        root["providerConnections"]
            .as_array_mut()
            .unwrap()
            .push(json!({
                "id":"local", "provider":"openai", "authType":"apikey",
                "apiKey":"hidden-secret",
                "providerSpecificData":{"baseUrl":format!("http://{host}:8788/v1")}
            }));
        root["combos"][0]["models"]
            .as_array_mut()
            .unwrap()
            .push(json!("openai/test"));
        let dir = tempfile::tempdir().unwrap();
        let report =
            llmux_import::import(&root.to_string(), &dir.path().join("out.toml"), None).unwrap();
        assert_eq!(report.credentials, 1);
        assert!(report
            .warnings
            .iter()
            .any(|w| w.contains("skipped loopback/localhost upstream")));
        assert!(!report.warnings.join("\n").contains("hidden-secret"));
        assert!(!dir.path().join("secrets/upstream-local").exists());
    }
    let mut root = fixture();
    root["providerConnections"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "id":"unofficial", "provider":"morph", "authType":"apikey",
            "apiKey":"hidden-secret", "providerSpecificData":{"baseUrl":"https://example.com/v1"}
        }));
    let dir = tempfile::tempdir().unwrap();
    let report =
        llmux_import::import(&root.to_string(), &dir.path().join("out.toml"), None).unwrap();
    assert_eq!(report.credentials, 1);
    assert!(report
        .warnings
        .iter()
        .any(|w| w.contains("official Morph endpoint")));
}

#[test]
fn empty_model_after_stripping_does_not_emit_a_deployment() {
    let mut root = fixture();
    root["combos"][0]["models"] = json!(["gateway/node/"]);
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let error = llmux_import::import(&root.to_string(), &output, None)
        .err()
        .unwrap();
    assert_eq!(error, "no importable models; no configuration written");
    assert!(!output.exists());
}

#[test]
fn deepseek_model_ids_on_compatible_hosts_strip_only_rejected_fields() {
    use llmux_types::ChatRequestField;
    let deepseek = [
        "deepseek/deepseek-v4.1-flash:free",
        "deepseek-v4-1-flash-1017",
        "deepseek-v4.1-flash",
        "deepseek/deepseek-v4.1-flash",
        "deepseek-ai/DeepSeek-V4-Flash-0731",
        "dealignai/deepseek-v4.1-flash-1017-uncensored",
        "dealignai/deepseek-v4.1-flash-uncensored-fp8",
        "deepseek/deepseek-chat",
        "deepseek/deepseek-chat-v3.1",
        "deepseek/deepseek-ocr-2",
        "deepseek/deepseek-r1-0528",
        "deepseek/deepseek-r1-0528-qwen3-8b",
        "deepseek/deepseek-r1-turbo",
        "deepseek/deepseek-v3.2",
        "deepseek-ai/DeepSeek-V4.1-Flash",
        "deepseek-v4.1-flash-free",
        "deepseek-v4-1-flash",
        "deepseek-v4-flash-1017",
        "DEEPSEEK/Reasoner",
        "DEEPSEEK-AI/R1",
        "vendor/DeEpSeEk-R1:free",
    ];
    let other = [
        "gpt-4.1",
        "gemini-3-flash-preview",
        "qwen/qwen3",
        "notdeepseek-r1",
        "vendor/not-deepseek-r1",
        "mydeepseek/model",
        "deepseekish/r1",
        "deepseek-ai-tools/other-model",
        "vendor/deepseek-helper/claude",
        "deepseek",
        "deepseek-",
        "vendor/deepseek-notes.txt/model",
    ];
    let mut root = fixture();
    root["combos"][0]["models"] = deepseek
        .iter()
        .chain(other.iter())
        .map(|id| json!(format!("gateway/{id}")))
        .collect();
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    llmux_import::import(&root.to_string(), &output, None).unwrap();
    let config = RuntimeConfig::load(output).unwrap();
    for dep in &config.deployments {
        if deepseek.contains(&dep.upstream_model.as_ref()) {
            assert_eq!(
                dep.strip_request_fields.as_ref(),
                &[
                    ChatRequestField::Seed,
                    ChatRequestField::User,
                    ChatRequestField::SafetyIdentifier,
                    ChatRequestField::ReasoningEffort
                ],
                "{}",
                dep.upstream_model
            );
            assert!(!dep
                .strip_request_fields
                .contains(&ChatRequestField::Logprobs));
            assert!(!dep
                .strip_request_fields
                .contains(&ChatRequestField::TopLogprobs));
        } else {
            assert!(other.contains(&dep.upstream_model.as_ref()));
            assert!(
                dep.strip_request_fields.is_empty(),
                "{}",
                dep.upstream_model
            );
        }
    }
    let model = &config.models[config.model_names["test"]];
    assert_eq!(model.deployments.len(), deepseek.len() + other.len());
}

#[test]
fn deepseek_stripping_uses_model_identity_not_provider_or_public_combo_name() {
    let mut root = fixture();
    root["providerConnections"].as_array_mut().unwrap().push(json!({"id":"official","provider":"deepseek","authType":"apikey","apiKey":"synthetic-official"}));
    root["combos"] = json!([
        {"name":"deepseek-public-name","models":["gateway/gpt-4.1"]},
        {"name":"other-provider-model","models":["deepseek/qwen3"]}
    ]);
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    llmux_import::import(&root.to_string(), &output, None).unwrap();
    let config = RuntimeConfig::load(output).unwrap();
    assert!(config
        .deployments
        .iter()
        .all(|dep| dep.strip_request_fields.is_empty()));
}

#[test]
fn deepseek_matching_also_applies_to_fallback_upstream_ids_but_not_catch_all() {
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
    let mut fallback_match = false;
    for dep in &config.deployments {
        match dep.upstream_model.as_ref() {
            "deepseek/test" | "gateway/deepseek/test" | "node/deepseek/test" => {
                assert_eq!(dep.strip_request_fields.len(), 4);
                fallback_match |= dep.fallback;
            }
            _ => assert!(dep.strip_request_fields.is_empty()),
        }
    }
    assert!(fallback_match);
}

mod anthropic_sampling {
    use llmux_config::RuntimeConfig;
    use llmux_types::ChatRequestField;
    use serde_json::json;

    #[test]
    fn models_that_reject_sampling_parameters_import_with_strip_request_fields() {
        let stripped = [
            "claude-haiku-5-5",
            "claude-haiku-5.5",
            "claude-sonnet-5-5",
            "claude-sonnet-5",
            "claude-opus-5-5",
            "claude-opus-5.5",
            "claude-fable-5-1",
            "claude-fable-5.2",
            "claude-fable-5",
            "claude-opus-4-8",
            "claude-opus-4.8",
        ];
        let kept = [
            "claude-sonnet-4-6",
            "claude-opus-4-7",
            "claude-haiku-4-5",
            "claude-sonnet-45",
        ];
        for (model, expected) in stripped
            .iter()
            .map(|m| (m, true))
            .chain(kept.iter().map(|m| (m, false)))
        {
            let root = json!({
                "providerConnections": [{"id":"claude","provider":"anthropic","authType":"apikey","apiKey":"synthetic-claude"}],
                "combos": [{"name":"c","models":[format!("anthropic/{model}")]}],
                "apiKeys": [{"key":"synthetic-client"}]
            });
            let dir = tempfile::tempdir().unwrap();
            let output = dir.path().join("out.toml");
            llmux_import::import(&root.to_string(), &output, None).unwrap();
            let config = RuntimeConfig::load(output).unwrap();
            let dep = config
                .deployments
                .iter()
                .find(|dep| dep.upstream_model.as_ref() == *model)
                .unwrap_or_else(|| panic!("{model} not imported"));
            assert_eq!(dep.provider.0.as_ref(), "anthropic");
            let actual: &[ChatRequestField] = dep.strip_request_fields.as_ref();
            if expected {
                assert_eq!(
                    actual,
                    [ChatRequestField::Temperature, ChatRequestField::TopP],
                    "{model}"
                );
            } else {
                assert!(actual.is_empty(), "{model}");
            }
        }
    }
}
