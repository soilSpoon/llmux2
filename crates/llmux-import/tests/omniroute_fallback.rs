use llmux_config::RuntimeConfig;
use serde_json::{json, Value};
use std::{fs, process::Command};

fn fixture() -> Value {
    json!({
        "providerConnections":[{"id":"openai","provider":"openai","authType":"apikey","apiKey":"synthetic-upstream"}],
        "providerNodes":[{"id":"codex","prefix":"cx"}],
        "apiKeys":[{"key":"synthetic-client"}],
        "combos":[
            {"id":"safe","name":"public","models":["openai/first","openai/second"]},
            {"id":"private","name":"owner","models":["cx/gpt","openai/first"]},
            {"id":"nested","name":"nested-owner","models":[{"kind":"combo-ref","comboName":"owner"}]},
            {"id":"only","name":"codex-only","models":[{"providerId":"codex","model":"gpt"}]}
        ],
        "modelAliases":{"owner-alias":"nested-owner","chain":"owner-alias","direct-owner":{"provider":"cx","model":"gpt"},"public-alias":"public"},
        "modelComboMappings":[
            {"pattern":"private-*","comboId":"nested","enabled":true},
            {"pattern":"mapped-owner","comboId":"private","enabled":1},
            {"pattern":"ignored?","comboId":"private","enabled":false}
        ]
    })
}

fn run(root: &Value, args: &[&str]) -> (tempfile::TempDir, std::process::Output) {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("input.json"), root.to_string()).unwrap();
    fs::write(dir.path().join("key"), "synthetic-omniroute").unwrap();
    let args: Vec<_> = args
        .iter()
        .map(|arg| {
            arg.replace(
                "KEY_REF",
                &format!("file:{}", dir.path().join("key").display()),
            )
        })
        .collect();
    let output = Command::new(env!("CARGO_BIN_EXE_llmux-import"))
        .args(["--input", "input.json", "--output", "out.toml"])
        .args(args)
        .env("LLMUX_IMPORT_TEST_OMNI_KEY", "synthetic-omniroute")
        .current_dir(dir.path())
        .output()
        .unwrap();
    (dir, output)
}

#[test]
fn fallback_is_last_and_codex_routes_are_excluded() {
    let (dir, result) = run(
        &fixture(),
        &[
            "--omniroute-fallback",
            "http://127.0.0.1:20128/v1",
            "--omniroute-key",
            "KEY_REF",
        ],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let text = fs::read_to_string(dir.path().join("out.toml")).unwrap();
    assert!(!text.contains("synthetic-omniroute"));
    let cfg = RuntimeConfig::parse(&text).unwrap();
    let public = &cfg.models[cfg.model_names["public"]];
    let deps: Vec<_> = public
        .deployments
        .iter()
        .map(|idx| &cfg.deployments[idx.0 as usize])
        .collect();
    let fallback = deps.last().unwrap();
    assert!(deps[..deps.len() - 1]
        .iter()
        .all(|dep| dep.priority < fallback.priority));
    assert_eq!(fallback.upstream_model.as_ref(), "public");
    assert_eq!(fallback.provider.0.as_ref(), "openai_compat");
    assert_eq!(fallback.base_url.to_string(), "http://127.0.0.1:20128/v1");
    assert_eq!(
        cfg.api_keys[&fallback.credential].key.as_ref(),
        "synthetic-omniroute"
    );
    for name in ["owner", "nested-owner"] {
        let model = &cfg.models[cfg.model_names[name]];
        assert!(model
            .deployments
            .iter()
            .all(|idx| cfg.deployments[idx.0 as usize].credential != fallback.credential));
    }
    assert!(!cfg.model_names.contains_key("*"));
    let catch_all = &cfg.models[cfg.catch_all.unwrap()];
    assert_eq!(catch_all.deployments.len(), 1);
    assert_eq!(
        cfg.deployments[catch_all.deployments[0].0 as usize]
            .upstream_model
            .as_ref(),
        "*"
    );
    assert_eq!(
        cfg.catch_all_exclude,
        [
            "chain",
            "codex-only",
            "codex/*",
            "cx/*",
            "direct-owner",
            "mapped-owner",
            "nested-owner",
            "owner",
            "owner-alias",
            "private-*"
        ]
    );
}

#[test]
fn rejects_unrepresentable_codex_mapping_without_writing_config() {
    for pattern in ["private-?", "private-*-suffix", "private-**"] {
        let mut root = fixture();
        root["modelComboMappings"][0]["pattern"] = pattern.into();
        let (dir, result) = run(
            &root,
            &[
                "--omniroute-fallback",
                "https://example.com/v1",
                "--omniroute-key",
                "KEY_REF",
            ],
        );
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr)
            .contains("Codex exclusion pattern cannot be represented"));
        assert!(!dir.path().join("out.toml").exists());
    }
}

#[test]
fn validates_fallback_flags_without_exposing_key_values() {
    for args in [
        vec!["--omniroute-fallback", "http://127.0.0.1:20128/v1"],
        vec!["--omniroute-key", "KEY_REF"],
        vec![
            "--omniroute-fallback",
            "https://example.com/v1",
            "--omniroute-key",
            "sensitive-literal",
        ],
        vec![
            "--omniroute-fallback",
            "http://example.com/v1",
            "--omniroute-key",
            "KEY_REF",
        ],
        vec![
            "--omniroute-fallback",
            "https://user:pass@example.com/v1",
            "--omniroute-key",
            "KEY_REF",
        ],
        vec![
            "--omniroute-fallback",
            "https://example.com/v1?query=1",
            "--omniroute-key",
            "KEY_REF",
        ],
        vec![
            "--omniroute-fallback",
            "https://example.com/v1",
            "--omniroute-key",
            "env:",
        ],
    ] {
        let (dir, result) = run(&fixture(), &args);
        assert!(!result.status.success());
        assert!(!String::from_utf8_lossy(&result.stderr).contains("sensitive-literal"));
        assert!(!dir.path().join("out.toml").exists());
    }
}

#[test]
fn no_flags_preserve_existing_output() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("baseline.toml");
    let secrets = dir.path().join("secrets");
    llmux_import::import(&fixture().to_string(), &out, Some(&secrets)).unwrap();
    let before = fs::read(&out).unwrap();
    fs::remove_file(&out).unwrap();
    fs::remove_dir_all(&secrets).unwrap();
    fs::write(dir.path().join("input.json"), fixture().to_string()).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_llmux-import"))
        .args(["--input", "input.json", "--output", "baseline.toml"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(result.status.success());
    assert_eq!(before, fs::read(&out).unwrap());
}

#[test]
fn fallback_only_import_preserves_env_reference() {
    let (dir, result) = run(
        &json!({}),
        &[
            "--omniroute-fallback",
            "https://example.com/v1",
            "--omniroute-key",
            "env:LLMUX_IMPORT_TEST_OMNI_KEY",
        ],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let text = fs::read_to_string(dir.path().join("out.toml")).unwrap();
    assert!(!text.contains("synthetic-omniroute"));
    let config: toml::Value = toml::from_str(&text).unwrap();
    assert_eq!(
        config["credentials"][0]["key"].as_str(),
        Some("env:LLMUX_IMPORT_TEST_OMNI_KEY")
    );
    assert_eq!(config["models"].as_array().unwrap().len(), 1);
    assert_eq!(config["models"][0]["name"].as_str(), Some("*"));
    assert_eq!(
        config["models"][0]["exclude"].as_array().unwrap(),
        &[toml::Value::String("codex/*".into())]
    );
}
