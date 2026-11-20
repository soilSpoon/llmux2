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
fn anonymous_only_import_needs_no_credentials_or_secret_files() {
    static ENV: std::sync::Once = std::sync::Once::new();
    ENV.call_once(|| std::env::set_var("LLMUX_CLIENT_KEY", "synthetic-client-key"));
    for provider in ["aihorde", "kilo", "kilocode", "custom-kilo"] {
        for auth in ["no-auth", "none"] {
            let mut root = fixture();
            root["apiKeys"] = json!([]);
            root["providerConnections"][0] =
                json!({"id":"anon","provider":provider,"authType":auth});
            root["combos"] = json!([{"name":"anonymous","models":[{"providerId":provider,"model":"vendor/free"}]}]);
            if provider == "custom-kilo" {
                root["providerNodes"] = json!([{"id":"custom-kilo","prefix":"kilo","type":"openai-compatible","baseUrl":"https://ai-gateway.kilo.ai/api/v1"}]);
            }
            let dir = tempfile::tempdir().unwrap();
            let output = dir.path().join("out.toml");
            let report = llmux_import::import(&root.to_string(), &output, None).unwrap();
            let models = if matches!(provider, "kilocode" | "custom-kilo") {
                3
            } else {
                2
            };
            assert_eq!(
                (report.models, report.deployments, report.credentials),
                (models, models, 0)
            );
            let config = RuntimeConfig::load(&output).unwrap();
            assert!(config.api_keys.is_empty());
            assert!(config.oauth.is_empty());
            assert!(config.deployments[0].credential.is_empty());
            assert_eq!(fs::read_dir(dir.path().join("secrets")).unwrap().count(), 0);
            let raw: toml::Value = toml::from_str(&fs::read_to_string(output).unwrap()).unwrap();
            let dep = &raw["models"][0]["deployments"][0];
            assert_eq!(dep["auth"].as_str(), Some("none"));
            assert_eq!(dep["provider"].as_str(), Some("openai_compat"));
            assert!(dep.get("credential").is_none());
        }
    }
}

#[test]
fn concurrent_anonymous_imports_share_a_secrets_directory() {
    let root = json!({
        "providerConnections":[{"id":"anonymous","provider":"kilo","authType":"none"}],
        "combos": (0..64).map(|i| json!({"name":format!("model-{i}"),"models":["kilo/vendor/free"]})).collect::<Vec<_>>()
    }).to_string();
    let dir = tempfile::tempdir().unwrap();
    let secrets = dir.path().join("secrets");
    let start = std::sync::Barrier::new(2);
    let results = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..2)
            .map(|i| {
                let root = &root;
                let secrets = &secrets;
                let start = &start;
                let output = dir.path().join(format!("output-{i}.toml"));
                scope.spawn(move || {
                    start.wait();
                    llmux_import::import(root, &output, Some(secrets))
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Vec<_>>()
    });
    for result in results {
        let report = result.unwrap();
        assert_eq!(
            (report.models, report.deployments, report.credentials),
            (65, 65, 0)
        );
    }
    assert_eq!(fs::read_dir(secrets).unwrap().count(), 0);
    let client_key = dir.path().join("client-key");
    fs::write(&client_key, "synthetic-client-key").unwrap();
    for i in 0..2 {
        let mut config: toml::Value = toml::from_str(
            &fs::read_to_string(dir.path().join(format!("output-{i}.toml"))).unwrap(),
        )
        .unwrap();
        config["server"]["client_keys"] =
            toml::Value::Array(vec![format!("file:{}", client_key.display()).into()]);
        let config = RuntimeConfig::parse(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(config.models.len(), 65);
        assert!(config.api_keys.is_empty());
        assert!(config.deployments.iter().all(|d| d.credential.is_empty()));
    }
}

#[test]
fn one_model_can_mix_anonymous_and_keyed_kilo_deployments() {
    let mut root = fixture();
    root["providerConnections"] = json!([
        {"id":"anonymous","provider":"kilo","authType":"none","priority":0},
        {"id":"keyed","provider":"kilo","authType":"apikey","apiKey":"synthetic-kilo-key","priority":10}
    ]);
    root["combos"] = json!([{"name":"mixed","models":["kilo/vendor/free"]}]);
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let report = llmux_import::import(&root.to_string(), &output, None).unwrap();
    assert_eq!(
        (report.models, report.deployments, report.credentials),
        (2, 4, 1)
    );
    let config = RuntimeConfig::load(output).unwrap();
    assert!(config.deployments[0].credential.is_empty());
    assert_eq!(config.deployments[1].credential.as_ref(), "upstream-keyed");
    assert!(!dir.path().join("secrets/upstream-anonymous").exists());
}

#[test]
fn anonymous_kilo_rejects_nonofficial_urls_before_writing_secrets() {
    for (provider, prefix) in [("kilo", ""), ("kilocode", ""), ("custom-kilo", "kilo")] {
        let mut root = fixture();
        root["providerConnections"].as_array_mut().unwrap().push(json!({"id":"bad","provider":provider,"authType":"none","providerSpecificData":{"baseUrl":"https://unsupported.example/private"}}));
        root["providerNodes"] = json!([{"id":"custom-kilo","prefix":prefix,"type":"openai-compatible","baseUrl":"https://ai-gateway.kilo.ai/api/v1"}]);
        let dir = tempfile::tempdir().unwrap();
        let report =
            llmux_import::import(&root.to_string(), &dir.path().join("out.toml"), None).unwrap();
        assert_eq!(report.credentials, 1);
        assert!(report
            .warnings
            .iter()
            .any(|w| w.contains("Kilo requires its public official")));
        assert!(!report.warnings.join("\n").contains("unsupported.example"));
        assert!(!dir.path().join("secrets/upstream-bad").exists());
    }
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
        ("kilo-anon", "kilo", "no-auth", "", "vendor/free"),
        ("kilocode-anon", "kilocode", "none", "", "openrouter/free"),
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
        (16, 26, 4)
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
    for name in ["horde-anon", "horde-none", "kilo-anon", "kilocode-anon"] {
        let model = &config.models[config.model_names[name]];
        let deployment = &config.deployments[model.deployments[0].0 as usize];
        assert!(deployment.credential.is_empty());
        assert_eq!(deployment.provider.0.as_ref(), "openai_compat");
        assert!(!dir.path().join(format!("secrets/upstream-{name}")).exists());
    }
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
        ("kilo", "none", "", "", "loopback/localhost upstream"),
        ("kilocode", "no-auth", "", "", "loopback/localhost upstream"),
        (
            "kilo",
            "apikey",
            "",
            "",
            "missing, encrypted or invalid API key",
        ),
        (
            "kilocode",
            "apikey",
            "",
            "",
            "missing, encrypted or invalid API key",
        ),
        (
            "custom-kilo",
            "none",
            "",
            "kilo",
            "unknown/non-compatible provider",
        ),
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
