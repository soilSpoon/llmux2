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
