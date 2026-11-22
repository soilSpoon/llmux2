use llmux_config::RuntimeConfig;
use llmux_import::ImportOptions;
use serde_json::json;
use std::fs;

const LISTING: &str = "Available models\n\nSWE-2 (swe-2)\n  swe-2-high  SWE-2 High  [262K context, Free]\n  gpt-6-luna-high  Luna  [1M context, $1 / 1M Input]\n";

fn root() -> serde_json::Value {
    json!({
        "providerConnections":[
            {"id":"dev","provider":"devin-cli-agentic","authType":"none"},
            {"id":"cf","provider":"openai","authType":"apikey","apiKey":"test-key"},
            {"id":"aug","provider":"auggie","authType":"oauth"},
            {"id":"zc","provider":"zcode","authType":"oauth"},
            {"id":"fa","provider":"factory","authType":"apikey","apiKey":"test-key"}
        ],
        "combos":[
            {"name":"luna","models":["dva/gpt-6-luna-high","openai/gpt-6-luna"]},
            {"name":"swe","models":["dva/swe-2-high","aug/swe-2-high","zc/glm-5-3"]}
        ],
        "apiKeys":[{"key":"test-client-key"}]
    })
}

#[test]
fn devin_bridge_is_imported_free_first_and_excluded_providers_are_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let listing = dir.path().join("models.txt");
    fs::write(&listing, LISTING).unwrap();
    let output = dir.path().join("out.toml");
    let report = llmux_import::import_with_options(
        &root().to_string(),
        &output,
        None,
        &ImportOptions {
            devin_models: Some(&listing),
            ..Default::default()
        },
    )
    .unwrap();
    for excluded in ["Augment", "ZCode", "Factory"] {
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("skipped") && w.contains(excluded)),
            "{excluded}: {:?}",
            report.warnings
        );
    }
    let config = RuntimeConfig::load(&output).unwrap();
    assert!(config
        .deployments
        .iter()
        .all(|d| !d.upstream_model.contains("aug")));
    let luna: Vec<_> = config
        .deployments
        .iter()
        .filter(|d| d.id.starts_with("luna-"))
        .collect();
    assert_eq!(luna.len(), 2);
    assert_eq!(&*luna[0].provider.0, "openai");
    assert_eq!(&*luna[1].provider.0, "devin_acp");
    assert_eq!(&*luna[1].upstream_model, "gpt-6-luna-high");
    let swe: Vec<_> = config
        .deployments
        .iter()
        .filter(|d| d.id.starts_with("swe-"))
        .collect();
    assert_eq!(swe.len(), 1);
    assert_eq!(&*swe[0].provider.0, "devin_acp");
}

#[test]
fn without_a_listing_every_devin_model_is_treated_as_paid() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = root();
    root["combos"] = json!([{"name":"mixed","models":["dva/swe-2-high","openai/swe-2-high"]}]);
    let output = dir.path().join("out.toml");
    llmux_import::import(&root.to_string(), &output, None).unwrap();
    let config = RuntimeConfig::load(&output).unwrap();
    let kinds: Vec<_> = config
        .deployments
        .iter()
        .filter(|d| d.id.starts_with("mixed-"))
        .map(|d| d.provider.0.to_string())
        .collect();
    assert_eq!(kinds, ["openai", "devin_acp"]);
}
