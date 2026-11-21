use llmux_config::RuntimeConfig;
use llmux_import::ImportOptions;
use llmux_types::NousState;
use serde_json::json;
use std::{fs, path::Path};

fn state(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("state.json");
    NousState {
        portal_base_url: "https://portal.nousresearch.com".into(),
        client_id: "hermes-cli".into(),
        scope: "inference:invoke".into(),
        access_token: "synthetic-access".into(),
        refresh_token: "synthetic-refresh".into(),
        expires_at: 4_000_000_000,
    }
    .save(&path)
    .unwrap();
    path
}

fn root() -> String {
    json!({
        "providerConnections":[{"id":"cf","provider":"openai","authType":"apikey","apiKey":"test-key"}],
        "combos":[{"name":"gpt","models":["openai/gpt-6-luna"]}],
        "apiKeys":[{"key":"test-client-key"}]
    })
    .to_string()
}

fn import(dir: &Path, listing: serde_json::Value, state: &Path) -> Result<RuntimeConfig, String> {
    let models = dir.join("models.json");
    fs::write(&models, listing.to_string()).unwrap();
    let output = dir.join("out.toml");
    llmux_import::import_with_options(
        &root(),
        &output,
        None,
        &ImportOptions {
            nous_state: Some(state),
            nous_models: Some(&models),
            ..Default::default()
        },
    )?;
    Ok(RuntimeConfig::load(&output).unwrap())
}

#[test]
fn free_models_rank_ahead_of_paid_and_each_gets_one_nous_deployment() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(dir.path());
    let listing = json!({"data":[
        {"id":"vendor/paid-big","pricing":{"prompt":"0.000003","completion":"0.000015"}},
        {"id":"tencent/hy4-flash:free","pricing":{"prompt":"0.000001","completion":"0.000001"}},
        {"id":"vendor/zero-priced","pricing":{"prompt":"0","completion":"0.00"}},
        {"id":"vendor/half-free","pricing":{"prompt":"0","completion":"0.000002"}},
        {"id":"vendor/paid-big","pricing":{"prompt":"0","completion":"0"}},
        {"id":"vendor/no-pricing"}
    ]});
    let config = import(dir.path(), listing, &state).unwrap();
    let nous: Vec<_> = config
        .models
        .iter()
        .filter(|m| m.name.starts_with("nous/"))
        .map(|m| m.name.to_string())
        .collect();
    assert_eq!(
        nous,
        [
            "nous/tencent/hy4-flash:free",
            "nous/vendor/zero-priced",
            "nous/vendor/paid-big",
            "nous/vendor/half-free",
            "nous/vendor/no-pricing",
        ]
    );
    for model in config.models.iter().filter(|m| m.name.starts_with("nous/")) {
        assert_eq!(model.deployments.len(), 1);
        let deployment = &config.deployments[model.deployments[0].0 as usize];
        assert_eq!(
            deployment.base_url.to_string(),
            "https://inference-api.nousresearch.com/v1"
        );
        assert_eq!(&*deployment.provider.0, "openai_compat");
        assert!(
            deployment.headers.is_empty(),
            "the adapter's default User-Agent makes a per-deployment header unnecessary"
        );
        assert_eq!(
            format!("nous/{}", deployment.upstream_model),
            model.name.as_ref()
        );
    }
    assert_eq!(config.nous.len(), 1);
    let gpt = config.models.iter().find(|m| &*m.name == "gpt").unwrap();
    assert!(gpt
        .deployments
        .iter()
        .all(|d| &*config.deployments[d.0 as usize].credential != "nous"));
}

#[test]
fn missing_state_or_listing_is_rejected_with_a_login_hint() {
    let dir = tempfile::tempdir().unwrap();
    let listing = json!({"data":[{"id":"a:free"}]});
    let error = import(dir.path(), listing, &dir.path().join("none.json"))
        .err()
        .unwrap();
    assert!(error.contains("llmux-login nous"));
    let state = state(dir.path());
    assert!(import(dir.path(), json!({"data":[]}), &state).is_err());
    let output = dir.path().join("again.toml");
    let error = llmux_import::import_with_options(
        &root(),
        &output,
        None,
        &ImportOptions {
            nous_state: Some(&state),
            ..Default::default()
        },
    )
    .err()
    .unwrap();
    assert!(error.contains("together"));
}
