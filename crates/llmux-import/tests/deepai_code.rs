use llmux_config::RuntimeConfig;
use llmux_import::ImportOptions;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const BRIDGE: &str = "http://127.0.0.1:49759/v1";

fn fixture() -> Value {
    json!({
        "providerConnections":[
            {"id":"paid","provider":"paid-node","authType":"apikey","apiKey":"synthetic-paid"},
            {"id":"dac","provider":"deepai-code","authType":"apikey","apiKey":"synthetic-dac",
             "providerSpecificData":{"baseUrl":"http://127.0.0.1:8790/deepai/v1"}}
        ],
        "providerNodes":[
            {"id":"paid-node","prefix":"paid","type":"openai-compatible","baseUrl":"https://paid.example/v1"}
        ],
        "combos":[{"name":"claude-haiku-5.5","models":["paid/a"]}],
        "apiKeys":[{"key":"synthetic-client"}]
    })
}

fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, content).unwrap();
    path
}

fn run(root: &Value, options: &ImportOptions) -> (RuntimeConfig, llmux_import::ImportReport) {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.toml");
    let report = llmux_import::import_with_options(&root.to_string(), &out, None, options).unwrap();
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(!text.contains("synthetic-"));
    assert!(report.warnings.iter().all(|w| !w.contains("synthetic-")));
    (RuntimeConfig::load(&out).unwrap(), report)
}

fn legs(config: &RuntimeConfig, name: &str) -> Vec<(String, String)> {
    config.models[*config.model_names.get(name).unwrap()]
        .deployments
        .iter()
        .map(|index| {
            let dep = &config.deployments[index.0 as usize];
            (dep.base_url.to_string(), dep.upstream_model.to_string())
        })
        .collect()
}

fn options<'a>(state: &'a Path, token: &'a Path) -> ImportOptions<'a> {
    ImportOptions {
        deepai_code_state: Some(state),
        deepai_code_token_file: Some(token),
        ..Default::default()
    }
}

#[test]
fn discovered_free_leg_leads_the_mapped_canonical_model() {
    let dir = tempfile::tempdir().unwrap();
    let state = write(
        dir.path(),
        "state.json",
        &json!({"base_url":BRIDGE,"free":["claude-haiku-5-5","deepseek-v4.1-flash"],
            "scope":["claude-haiku-5-5"]})
        .to_string(),
    );
    let token = write(dir.path(), "token", "synthetic-session");
    let (config, report) = run(&fixture(), &options(&state, &token));
    assert_eq!(
        legs(&config, "claude-haiku-5.5"),
        [
            (BRIDGE.to_string(), "claude-haiku-5-5".to_string()),
            ("https://paid.example/v1".to_string(), "a".to_string()),
        ]
    );
    assert!(report
        .warnings
        .iter()
        .any(|w| w.contains("free DeepAI Code leg")));
}

#[test]
fn metered_or_out_of_scope_models_emit_no_leg() {
    let dir = tempfile::tempdir().unwrap();
    // claude-haiku-5-5 absent from scope -> no leg even though it is free.
    let state = write(
        dir.path(),
        "state.json",
        &json!({"base_url":BRIDGE,"free":["claude-haiku-5-5"],"scope":["deepseek-v4.1-flash"]})
            .to_string(),
    );
    let token = write(dir.path(), "token", "synthetic-session");
    let (config, report) = run(&fixture(), &options(&state, &token));
    assert_eq!(
        legs(&config, "claude-haiku-5.5"),
        [("https://paid.example/v1".to_string(), "a".to_string())]
    );
    assert!(report
        .warnings
        .iter()
        .any(|w| w.contains("outside the session token's model scope")));

    // Metered (theosPerRequest > 0) models never leave the free list.
    let state = write(
        dir.path(),
        "state.json",
        &json!({"base_url":BRIDGE,"free":[],"scope":["claude-haiku-5-5"]}).to_string(),
    );
    let (config, _) = run(&fixture(), &options(&state, &token));
    assert_eq!(
        legs(&config, "claude-haiku-5.5"),
        [("https://paid.example/v1".to_string(), "a".to_string())]
    );
}

#[test]
fn non_loopback_state_url_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let state = write(
        dir.path(),
        "state.json",
        &json!({"base_url":"https://upstream.example/v1","free":["claude-haiku-5-5"]}).to_string(),
    );
    let token = write(dir.path(), "token", "synthetic-session");
    let out = tempfile::tempdir().unwrap().path().join("out.toml");
    assert!(llmux_import::import_with_options(
        &fixture().to_string(),
        &out,
        None,
        &options(&state, &token),
    )
    .is_err());
}

#[test]
fn state_without_token_keeps_the_exported_credential() {
    let dir = tempfile::tempdir().unwrap();
    let state = write(
        dir.path(),
        "state.json",
        &json!({"base_url":BRIDGE,"free":["claude-haiku-5-5"]}).to_string(),
    );
    let (config, _) = run(
        &fixture(),
        &ImportOptions {
            deepai_code_state: Some(&state),
            ..Default::default()
        },
    );
    assert_eq!(
        legs(&config, "claude-haiku-5.5"),
        [
            (BRIDGE.to_string(), "claude-haiku-5-5".to_string()),
            ("https://paid.example/v1".to_string(), "a".to_string()),
        ]
    );
}

#[test]
fn unmapped_free_catalog_id_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let state = write(
        dir.path(),
        "state.json",
        &json!({"base_url":BRIDGE,"free":["claude-haiku-5-5","muse-ember"]}).to_string(),
    );
    let token = write(dir.path(), "token", "synthetic-session");
    let (_, report) = run(&fixture(), &options(&state, &token));
    assert!(report
        .warnings
        .iter()
        .any(|w| w.contains("unmapped free DeepAI catalog id muse-ember")));
}
