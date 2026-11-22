use llmux_config::RuntimeConfig;
use serde_json::{json, Value};

fn import(root: &Value) -> (llmux_import::ImportReport, RuntimeConfig) {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let report = llmux_import::import(&root.to_string(), &output, None).unwrap();
    let config = RuntimeConfig::load(&output).unwrap();
    (report, config)
}

fn legs<'a>(config: &'a RuntimeConfig, name: &str) -> Vec<&'a llmux_types::Deployment> {
    config.models[config.model_names[name]]
        .deployments
        .iter()
        .map(|index| &config.deployments[index.0 as usize])
        .collect()
}

#[test]
fn repeated_connection_steps_collapse_to_one_deployment() {
    let root = json!({
        "providerConnections":[{
            "id":"same",
            "provider":"openai",
            "authType":"apikey",
            "apiKey":"synthetic-same",
            "priority":1
        }],
        "combos":[{
            "name":"openai/gemini-3-flash",
            "models":[
                {"providerId":"openai","model":"gemini-3-flash","connectionId":"same"},
                {"providerId":"openai","model":"gemini-3-flash","connectionId":"same"},
                {"providerId":"openai","model":"gemini-3-flash","connectionId":"same"}
            ]
        }],
        "apiKeys":[{"key":"synthetic-client"}]
    });
    let (report, config) = import(&root);
    assert_eq!(config.models.len(), 1);
    assert_eq!(report.deployments, 1);
    let deployments = legs(&config, "openai/gemini-3-flash");
    assert_eq!(deployments.len(), 1);
    assert_eq!(deployments[0].id.as_ref(), "openai-gemini-3-flash-0");
    assert_eq!(deployments[0].priority, 0);
    assert_eq!(deployments[0].credential.as_ref(), "upstream-same");
    assert_eq!(deployments[0].upstream_model.as_ref(), "gemini-3-flash");
    assert!(report
        .warnings
        .iter()
        .any(|warning| { warning == "openai/gemini-3-flash: dropped 2 duplicate deployments" }));
}

#[test]
fn same_base_url_different_credentials_are_kept() {
    let root = json!({
        "providerConnections":[
            {"id":"alpha","provider":"openai","authType":"apikey","apiKey":"synthetic-alpha","priority":1},
            {"id":"beta","provider":"openai","authType":"apikey","apiKey":"synthetic-beta","priority":2}
        ],
        "combos":[{
            "name":"shared-url",
            "models":[
                {"model":"openai/vendor/model","connectionId":"alpha"},
                {"model":"openai/vendor/model","connectionId":"beta"},
                {"model":"openai/vendor/model","connectionId":"alpha"},
                {"model":"openai/vendor/model","connectionId":"beta"}
            ]
        }],
        "apiKeys":[{"key":"synthetic-client"}]
    });
    let (report, config) = import(&root);
    let deployments = legs(&config, "shared-url");
    assert_eq!(deployments.len(), 2);
    assert_eq!(
        deployments[0].base_url.to_string(),
        deployments[1].base_url.to_string()
    );
    assert_ne!(deployments[0].credential, deployments[1].credential);
    assert_eq!(
        deployments
            .iter()
            .map(|dep| dep.credential.as_ref())
            .collect::<Vec<_>>(),
        vec!["upstream-alpha", "upstream-beta"]
    );
    assert_eq!(
        deployments
            .iter()
            .map(|dep| (dep.id.as_ref(), dep.priority))
            .collect::<Vec<_>>(),
        vec![("shared-url-0", 0), ("shared-url-1", 1)]
    );
    assert!(report
        .warnings
        .iter()
        .any(|warning| warning == "shared-url: dropped 2 duplicate deployments"));
}

#[test]
fn duplicates_are_not_collapsed_across_models() {
    let root = json!({
        "providerConnections":[{
            "id":"same",
            "provider":"openai",
            "authType":"apikey",
            "apiKey":"synthetic-same"
        }],
        "combos":[
            {"name":"left","models":["openai/shared-model","openai/shared-model"]},
            {"name":"right","models":["openai/shared-model"]}
        ],
        "apiKeys":[{"key":"synthetic-client"}]
    });
    let (report, config) = import(&root);
    assert_eq!(legs(&config, "left").len(), 1);
    assert_eq!(legs(&config, "right").len(), 1);
    assert_eq!(
        legs(&config, "left")[0].credential,
        legs(&config, "right")[0].credential
    );
    assert!(report
        .warnings
        .iter()
        .any(|warning| warning == "left: dropped 1 duplicate deployment"));
    assert!(!report
        .warnings
        .iter()
        .any(|warning| warning.starts_with("right: dropped")));
}

#[test]
fn round_robin_keeps_equal_priorities_after_dedupe() {
    let root = json!({
        "providerConnections":[
            {"id":"alpha","provider":"openai","authType":"apikey","apiKey":"synthetic-alpha","priority":1},
            {"id":"beta","provider":"openai","authType":"apikey","apiKey":"synthetic-beta","priority":2}
        ],
        "combos":[{
            "name":"rr",
            "strategy":"round-robin",
            "models":[
                {"model":"openai/m","connectionId":"alpha"},
                {"model":"openai/m","connectionId":"beta"},
                {"model":"openai/m","connectionId":"alpha"},
                {"model":"openai/m","connectionId":"beta"}
            ]
        }],
        "apiKeys":[{"key":"synthetic-client"}]
    });
    let (report, config) = import(&root);
    let deployments = legs(&config, "rr");
    assert_eq!(deployments.len(), 2);
    assert!(deployments.iter().all(|dep| dep.priority == 0));
    assert_eq!(
        deployments
            .iter()
            .map(|dep| dep.id.as_ref())
            .collect::<Vec<_>>(),
        vec!["rr-0", "rr-1"]
    );
    assert!(report
        .warnings
        .iter()
        .any(|warning| warning == "rr: dropped 2 duplicate deployments"));
}

#[test]
fn fallback_leg_survives_native_dedupe_unless_it_is_itself_a_duplicate() {
    let root = json!({
        "providerConnections":[{
            "id":"same",
            "provider":"openai",
            "authType":"apikey",
            "apiKey":"synthetic-same"
        }],
        "combos":[{
            "name":"gemini-3-flash",
            "models":["openai/gemini-3-flash","openai/gemini-3-flash","openai/gemini-3-flash"]
        }],
        "apiKeys":[{"key":"synthetic-client"}]
    });
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let key = dir.path().join("fallback-key");
    std::fs::write(&key, "synthetic-fallback").unwrap();
    let reference = format!("file:{}", key.display());
    let report = llmux_import::import_with_fallback(
        &root.to_string(),
        &output,
        None,
        Some(llmux_import::OmniRouteFallback {
            base_url: "http://127.0.0.1:20128/v1",
            key: &reference,
        }),
    )
    .unwrap();
    let text = std::fs::read_to_string(&output).unwrap();
    assert!(!text.contains("synthetic-fallback"));
    assert!(!text.contains("synthetic-same"));
    let config = RuntimeConfig::load(&output).unwrap();
    let deployments = legs(&config, "gemini-3-flash");
    assert_eq!(deployments.len(), 2);
    assert!(!deployments[0].fallback);
    assert_eq!(deployments[0].id.as_ref(), "gemini-3-flash-0");
    assert_eq!(deployments[0].priority, 0);
    assert!(deployments[1].fallback);
    assert!(deployments[1].id.as_ref().starts_with("omniroute-fallback"));
    assert!(deployments[1].priority > deployments[0].priority);
    assert_eq!(deployments[1].priority, 1);
    assert!(report
        .warnings
        .iter()
        .any(|warning| { warning == "gemini-3-flash: dropped 2 duplicate deployments" }));
    let catch_all = &config.models[config.catch_all.unwrap()];
    assert_eq!(catch_all.deployments.len(), 1);
    assert!(config.deployments[catch_all.deployments[0].0 as usize].fallback);
}
