use llmux_config::RuntimeConfig;
use serde_json::{json, Value};

fn fixture() -> Value {
    json!({
        "providerConnections":[
            {"id":"primary","provider":"experlabs-node","authType":"apikey","apiKey":"synthetic-primary"},
            {"id":"secondary","provider":"respan-node","authType":"apikey","apiKey":"synthetic-secondary"}
        ],
        "providerNodes":[
            {"id":"experlabs-node","prefix":"experlabs","type":"openai-compatible","baseUrl":"https://primary.example/v1"},
            {"id":"respan-node","prefix":"respan","type":"openai-compatible","baseUrl":"https://api.respan.ai/api/v1/"}
        ],
        "combos":[{"name":"gpt-6-luna","models":["experlabs/gpt-6-luna"]}],
        "modelAliases":{"luna":"gpt-6-luna"},
        "apiKeys":[{"key":"synthetic-client"}]
    })
}

fn check(root: &Value, expected: &[&str], fallback: bool) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let fallback_key = dir.path().join("fallback-key");
    std::fs::write(&fallback_key, "synthetic-fallback").unwrap();
    let fallback_reference = format!("file:{}", fallback_key.display());
    let report = llmux_import::import_with_fallback(
        &root.to_string(),
        &output,
        None,
        fallback.then_some(llmux_import::OmniRouteFallback {
            base_url: "https://fallback.example/v1",
            key: &fallback_reference,
        }),
    );
    if expected.is_empty() {
        assert_eq!(
            report.err().as_deref(),
            Some("no importable models; no configuration written")
        );
        assert!(!output.exists());
        return Vec::new();
    }
    let report = report.unwrap();
    let config = RuntimeConfig::load(&output).unwrap();
    let name = root["combos"][0]["name"].as_str().unwrap();
    let model = config.models.iter().find(|m| m.name.as_ref() == name);
    let actual: Vec<_> = model
        .into_iter()
        .flat_map(|m| m.deployments.iter())
        .map(|i| config.deployments[i.0 as usize].base_url.to_string())
        .collect();
    assert_eq!(actual, expected);
    if let Some(model) = model {
        for (priority, i) in model.deployments.iter().enumerate() {
            let dep = &config.deployments[i.0 as usize];
            if root["combos"][0]["strategy"] != "round-robin" {
                assert_eq!(dep.priority, priority as i32);
            }
            assert!(MODELS.contains(&dep.upstream_model.as_ref()));
        }
        let actual_secondary: Vec<_> = model
            .deployments
            .iter()
            .map(|i| &config.deployments[i.0 as usize])
            .filter(|d| d.base_url == SECONDARY)
            .collect();
        if actual_secondary.len() == 3 {
            let existing: Vec<_> = root["combos"][0]["models"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|m| m.as_str().and_then(|s| s.strip_prefix("respan/")))
                .collect();
            let mut expected_models = existing.clone();
            expected_models.extend(MODELS.iter().copied().filter(|m| !existing.contains(m)));
            assert_eq!(
                actual_secondary
                    .iter()
                    .map(|d| d.upstream_model.as_ref())
                    .collect::<Vec<_>>(),
                expected_models
            );
            assert!(actual_secondary
                .iter()
                .all(|d| d.credential.as_ref() == "upstream-secondary"));
        }
        if name == "gpt-6-luna" {
            assert_eq!(config.model_names.get("luna"), config.model_names.get(name));
        }
    }
    let text = std::fs::read_to_string(output).unwrap();
    for secret in [
        "synthetic-primary",
        "synthetic-secondary",
        "synthetic-client",
        "synthetic-second-key",
        "synthetic-fallback",
    ] {
        assert!(!text.contains(secret));
        assert!(!report.warnings.join("\n").contains(secret));
    }
    report.warnings
}

const PRIMARY: &str = "https://primary.example/v1";
const SECONDARY: &str = "https://api.respan.ai/api/v1";
const MODELS: &[&str] = &["gpt-6-luna", "openai/gpt-6-luna", "perplexity/gpt-6-luna"];

#[test]
fn reviewed_secondary_appends_once_after_existing_legs_and_before_fallback() {
    for fallback in [false, true] {
        let mut root = fixture();
        root["providerConnections"].as_array_mut().unwrap().push(json!({
            "id":"secondary-two","provider":"respan-node","authType":"apikey","apiKey":"synthetic-second-key"
        }));
        let mut expected = vec![PRIMARY, SECONDARY, SECONDARY, SECONDARY];
        if fallback {
            expected.push("https://fallback.example/v1");
        }
        let warnings = check(&root, &expected, fallback);
        assert!(warnings.iter().any(|w| w.contains("reviewed secondary")));
    }
    let mut root = fixture();
    root["combos"][0]["models"] = json!(["experlabs/gpt-6-luna", "experlabs/gpt-6-luna"]);
    let warnings = check(&root, &[PRIMARY, SECONDARY, SECONDARY, SECONDARY], false);
    assert!(warnings
        .iter()
        .any(|warning| warning == "gpt-6-luna: dropped 1 duplicate deployment"));
}

#[test]
fn reviewed_secondary_normalizes_endpoint_like_other_upstreams() {
    for url in [
        SECONDARY.to_owned(),
        format!("{SECONDARY}/"),
        format!("{SECONDARY}/chat/completions"),
    ] {
        let mut root = fixture();
        root["providerConnections"][1]["providerSpecificData"] = json!({"baseUrl":url});
        check(&root, &[PRIMARY, SECONDARY, SECONDARY, SECONDARY], false);
    }
}

#[test]
fn reviewed_secondary_requires_active_reviewed_api_key_connection() {
    for (field, value) in [
        ("isActive", json!(false)),
        ("usable", json!(false)),
        ("apiKey", json!("")),
        ("authType", json!("oauth")),
        ("authType", json!("token")),
        ("authType", json!("none")),
        ("provider", json!("codex")),
    ] {
        let mut root = fixture();
        root["providerConnections"][1][field] = value;
        check(&root, &[PRIMARY], false);
    }
    for (field, value) in [
        ("prefix", "other"),
        ("prefix", "codex"),
        ("type", "anthropic-compatible"),
        ("baseUrl", "https://api.respan.ai/api/v1/azure_openai"),
        ("baseUrl", "https://api.respan.ai/api/v1/azure_foundry"),
        ("baseUrl", "https://api.respan.ai/api/v1/bedrock"),
        ("baseUrl", "https://other.example/api/v1"),
    ] {
        let mut root = fixture();
        root["providerNodes"][1][field] = json!(value);
        check(&root, &[PRIMARY], false);
    }
    let mut root = fixture();
    root["providerConnections"].as_array_mut().unwrap().pop();
    check(&root, &[PRIMARY], false);
}

#[test]
fn reviewed_secondary_does_not_duplicate_or_create_primary_or_unrelated_routes() {
    for model in MODELS {
        let mut root = fixture();
        root["combos"][0]["models"] = json!(["experlabs/gpt-6-luna", format!("respan/{model}")]);
        check(&root, &[PRIMARY, SECONDARY, SECONDARY, SECONDARY], false);
    }
    let mut root = fixture();
    root["combos"][0]["models"] = json!([
        "experlabs/gpt-6-luna",
        "respan/gpt-6-luna",
        "respan/openai/gpt-6-luna",
        "respan/perplexity/gpt-6-luna"
    ]);
    let warnings = check(&root, &[PRIMARY, SECONDARY, SECONDARY, SECONDARY], false);
    assert!(!warnings
        .iter()
        .any(|w| w.contains("appended reviewed secondary")));
    let mut root = fixture();
    root["combos"][0]["name"] = json!("other");
    check(&root, &[PRIMARY], false);
    let mut root = fixture();
    root["combos"][0]["models"] = json!([]);
    check(&root, &[], false);
    let mut root = fixture();
    root["combos"] = json!([]);
    let dir = tempfile::tempdir().unwrap();
    let result = llmux_import::import(&root.to_string(), &dir.path().join("out.toml"), None);
    assert_eq!(
        result.err().as_deref(),
        Some("no importable models; no configuration written")
    );
    assert!(!dir.path().join("out.toml").exists());
}

#[test]
fn reviewed_secondary_skips_round_robin_with_warning() {
    let mut root = fixture();
    root["combos"][0]["strategy"] = json!("round-robin");
    let warnings = check(&root, &[PRIMARY], false);
    assert!(warnings
        .iter()
        .any(|w| w.contains("reviewed secondary") && w.contains("round-robin")));
}
