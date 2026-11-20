use llmux_config::RuntimeConfig;
use serde_json::{json, Value};
use std::fs;

fn fixture() -> Value {
    json!({
        "providerConnections":[{"id":"baseline", "provider":"openai", "authType":"apikey", "apiKey":"test-baseline-key"}],
        "combos":[{"name":"baseline", "models":["openai/test"]}],
        "apiKeys":[{"key":"test-client-key"}]
    })
}

#[test]
fn horde_overrides_map_native_roots_and_reject_nonofficial_urls() {
    for auth in ["no-auth", "apikey"] {
        for base_url in [
            "https://aihorde.net/api/v2",
            "https://aihorde.net/api/v2/",
            "https://oai.aihorde.net/v1",
            "https://oai.aihorde.net/v1/",
        ] {
            let mut root = fixture();
            root["providerConnections"][0]["provider"] = json!("aihorde");
            root["providerConnections"][0]["authType"] = json!(auth);
            root["providerConnections"][0]["providerSpecificData"] = json!({"baseUrl":base_url});
            root["combos"][0]["models"] = json!(["aihorde/koboldcpp/test"]);
            let dir = tempfile::tempdir().unwrap();
            let output = dir.path().join("out.toml");
            llmux_import::import(&root.to_string(), &output, None).unwrap();
            let config = RuntimeConfig::load(&output).unwrap();
            assert_eq!(
                config.deployments[0].base_url.to_string(),
                "https://oai.aihorde.net/v1"
            );
            assert_eq!(
                config.deployments[0].upstream_model.as_ref(),
                "koboldcpp/test"
            );
        }
    }
    for (base_url, expected) in [
        (
            "https://unsupported.example/private-root",
            "unsupported AI Horde base URL",
        ),
        (
            "https://aihorde.net/api/v3",
            "unsupported AI Horde base URL",
        ),
        (
            "http://127.0.0.1:9999/api/v2",
            "loopback/localhost upstream",
        ),
    ] {
        let mut root = fixture();
        root["providerConnections"]
            .as_array_mut()
            .unwrap()
            .push(json!({
                "id":"horde", "provider":"aihorde", "authType":"no-auth",
                "providerSpecificData":{"baseUrl":base_url}
            }));
        let dir = tempfile::tempdir().unwrap();
        let report =
            llmux_import::import(&root.to_string(), &dir.path().join("out.toml"), None).unwrap();
        assert_eq!(report.credentials, 1);
        let warnings = report.warnings.join("\n");
        assert!(warnings.contains(expected));
        assert!(!warnings.contains(base_url));
        assert!(!dir.path().join("secrets/upstream-horde").exists());
    }
}

#[test]
fn imports_horde_anonymous_and_keyed_and_kilo_keyed() {
    let mut root = fixture();
    for (id, provider, auth, key, model) in [
        ("horde-anon", "aihorde", "no-auth", "", "koboldcpp/test"),
        ("horde-none", "aihorde", "none", "", "aphrodite/vendor/test"),
        (
            "horde-keyed",
            "aihorde",
            "apikey",
            "test-horde-key",
            "google/test",
        ),
        (
            "kilo-keyed",
            "kilo",
            "apikey",
            "test-kilo-key",
            "vendor/test:free",
        ),
        (
            "kilocode-keyed",
            "kilocode",
            "apikey",
            "test-kilocode-key",
            "openrouter/free",
        ),
    ] {
        root["providerConnections"]
            .as_array_mut()
            .unwrap()
            .push(json!({
                "id":id, "provider":provider, "authType":auth, "apiKey":key
            }));
        root["combos"].as_array_mut().unwrap().push(json!({
            "name":id, "models":[{"providerId":provider, "model":model, "connectionId":id}]
        }));
    }
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let report = llmux_import::import(&root.to_string(), &output, None).unwrap();
    assert_eq!(
        (report.models, report.deployments, report.credentials),
        (6, 6, 6)
    );
    let config = RuntimeConfig::load(&output).unwrap();
    for (name, expected_model, expected_url) in [
        ("horde-anon", "koboldcpp/test", "https://oai.aihorde.net/v1"),
        (
            "horde-none",
            "aphrodite/vendor/test",
            "https://oai.aihorde.net/v1",
        ),
        ("horde-keyed", "google/test", "https://oai.aihorde.net/v1"),
        (
            "kilo-keyed",
            "vendor/test:free",
            "https://ai-gateway.kilo.ai/api/v1",
        ),
        (
            "kilocode-keyed",
            "openrouter/free",
            "https://ai-gateway.kilo.ai/api/v1",
        ),
    ] {
        let model = &config.models[config.model_names[name]];
        let deployment = &config.deployments[model.deployments[0].0 as usize];
        assert_eq!(deployment.upstream_model.as_ref(), expected_model);
        assert_eq!(deployment.base_url.to_string(), expected_url);
    }
    assert_eq!(
        fs::read_to_string(dir.path().join("secrets/upstream-horde-anon")).unwrap(),
        "0000000000"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("secrets/upstream-horde-keyed")).unwrap(),
        "test-horde-key"
    );
    let warnings = report.warnings.join("\n");
    assert_eq!(
        warnings
            .matches("anonymous use may be restricted under load")
            .count(),
        2
    );
    let text = fs::read_to_string(output).unwrap();
    for secret in [
        "0000000000",
        "test-horde-key",
        "test-kilo-key",
        "test-kilocode-key",
    ] {
        assert!(!text.contains(secret));
        assert!(!warnings.contains(secret));
    }
}

#[test]
fn skip_precedence_is_explicit_and_replacement_warnings_are_static() {
    for (provider, auth, key, prefix, expected) in [
        ("kilo", "none", "", "", "add a Kilo API key"),
        ("kilocode", "no-auth", "", "", "add a Kilo API key"),
        ("custom-kilo", "none", "", "kilo", "add a Kilo API key"),
        (
            "cloudflare-playground",
            "no-auth",
            "",
            "",
            "replace with a Cloudflare Workers AI account API token",
        ),
        (
            "custom-cf",
            "apikey",
            "",
            "cloudflare-direct",
            "and a free-eligible model",
        ),
        (
            "duckduckgo-web",
            "apikey",
            "",
            "",
            "no official replacement; dropped",
        ),
        (
            "custom-deepai",
            "apikey",
            "test-key",
            "deepai",
            "no official replacement; dropped",
        ),
        (
            "custom-hms",
            "apikey",
            "",
            "hms",
            "custom headers unsupported in llmux2",
        ),
        (
            "aihorde",
            "apikey",
            "",
            "",
            "missing, encrypted or invalid API key",
        ),
        ("aihorde", "none", "", "", "loopback/localhost upstream"),
    ] {
        let mut root = fixture();
        root["providerConnections"]
            .as_array_mut()
            .unwrap()
            .push(json!({
                "id":"skipped", "provider":provider, "authType":auth, "apiKey":key,
                "providerSpecificData":{"baseUrl":"http://127.0.0.1:9999/sensitive-path"}
            }));
        root["providerNodes"] = json!([{
            "id":provider, "prefix":prefix, "type":"openai-compatible",
            "baseUrl":"http://127.0.0.1:9999/sensitive-path", "customHeaders":{"X-Test":"sensitive-header"}
        }]);
        let dir = tempfile::tempdir().unwrap();
        let report =
            llmux_import::import(&root.to_string(), &dir.path().join("out.toml"), None).unwrap();
        assert_eq!(report.credentials, 1);
        let warning = report
            .warnings
            .iter()
            .find(|w| w.contains("(skipped): skipped"))
            .unwrap();
        assert!(warning.contains(expected), "{warning}");
        assert!(!warning.contains("sensitive"));
        assert!(!dir.path().join("secrets/upstream-skipped").exists());
    }
    for (field, expected) in [("usable", "usable: false"), ("isActive", "inactive")] {
        let mut root = fixture();
        root["providerConnections"][0]["provider"] = json!("cloudflare-playground");
        root["providerConnections"][0][field] = json!(false);
        root["providerConnections"]
            .as_array_mut()
            .unwrap()
            .push(json!({
                "id":"good", "provider":"openai", "authType":"apikey", "apiKey":"test-good-key"
            }));
        let dir = tempfile::tempdir().unwrap();
        let report =
            llmux_import::import(&root.to_string(), &dir.path().join("out.toml"), None).unwrap();
        let warning = report
            .warnings
            .iter()
            .find(|w| w.contains("(baseline): skipped"))
            .unwrap();
        assert!(warning.ends_with(expected));
    }
}
