use llmux_config::RuntimeConfig;
use serde_json::{json, Value};
use std::{fs, process::Command};

fn fixture() -> Value {
    json!({
        "_meta": {"version": "omniroute-v3-legacy-export"},
        "providerNodes": [
            {"id":"cfpd-node","prefix":"cfpd","type":"openai-compatible","baseUrl":"https://cfpd.example/v1"}
        ],
        "providerConnections": [
            {"id":"openai","provider":"openai","authType":"apikey","apiKey":"synthetic-upstream"},
            {"id":"cfpd","provider":"cfpd-node","authType":"apikey","apiKey":"synthetic-cfpd"},
            {"id":"morph-connection","provider":"morph","authType":"apikey","apiKey":"synthetic-morph"}
        ],
        "apiKeys": [{"key":"synthetic-client"}],
        "combos": [
            {"id":"morph","name":"morph-kimi-k3-abliterated","models":["openai/morph-native-upstream"]},
            {"id":"deepseek","name":"deepseek-v4.1-flash","models":["openai/deepseek-native-upstream"]},
            {"id":"cfpd-direct","name":"cfpd-direct-source","models":["cfpd/deepseek-v4.1-flash"]},
            {"id":"unrelated-cfpd","name":"unrelated-cfpd-source","models":["cfpd/deepseek-ai/deepseek-v4.1-flash","cfpd/other-model"]},
            {"id":"morph-base","name":"morph-kimik3","models":["morph/morph-kimik3"]},
            {"id":"morph-fast","name":"morph-kimik3-fast","models":["morph/morph-kimik3-fast"]},
            {"id":"prefixed-morph","name":"unrelated-prefixed-morph-source","models":["morph/morph-kimik3-abliterated"]}
        ],
        "modelAliases": {
            "public-cfpd":"cfpd/deepseek-v4.1-flash",
            "public-morph":"morph-kimik3-abliterated"
        }
    })
}

#[test]
fn exact_alias_does_not_remove_direct_route_without_canonical_target() {
    let root = json!({
        "_meta": {"version": "omniroute-v3-legacy-export"},
        "providerNodes": [
            {"id":"cfpd-node","prefix":"cfpd","type":"openai-compatible","baseUrl":"https://cfpd.example/v1"}
        ],
        "providerConnections": [
            {"id":"cfpd","provider":"cfpd-node","authType":"apikey","apiKey":"synthetic-cfpd"}
        ],
        "apiKeys": [{"key":"synthetic-client"}],
        "combos": [
            {"id":"cfpd-direct","name":"cfpd-direct-source","models":["cfpd/deepseek-v4.1-flash"]}
        ],
        "modelAliases": {
            "public-cfpd":"cfpd/deepseek-v4.1-flash"
        }
    });
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("input.json"), root.to_string()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_llmux-import"))
        .args(["--input", "input.json", "--output", "out.toml"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = fs::read_to_string(dir.path().join("out.toml")).unwrap();
    assert!(!text.contains("synthetic-cfpd"));
    let cfg = RuntimeConfig::parse(&text).unwrap();
    let index = cfg.model_names["cfpd/deepseek-v4.1-flash"];
    assert_eq!(cfg.models[index].name.as_ref(), "cfpd/deepseek-v4.1-flash");
    assert_eq!(cfg.model_names.get("public-cfpd"), Some(&index));
    assert!(!cfg.model_names.contains_key("deepseek-v4.1-flash"));
}

#[test]
fn export_aliases_win_over_reviewed_exact_defaults() {
    let mut root = fixture();
    root["modelAliases"]["morph-kimik3-abliterated"] = json!("morph-kimik3");
    root["modelAliases"]["cfpd/deepseek-v4.1-flash"] =
        json!("cfpd/deepseek-ai/deepseek-v4.1-flash");
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("input.json"), root.to_string()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_llmux-import"))
        .args(["--input", "input.json", "--output", "out.toml"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = fs::read_to_string(dir.path().join("out.toml")).unwrap();
    let cfg = RuntimeConfig::parse(&text).unwrap();
    assert_eq!(
        cfg.model_names.get("morph-kimik3-abliterated"),
        cfg.model_names.get("morph-kimik3")
    );
    assert_ne!(
        cfg.model_names.get("morph-kimik3-abliterated"),
        cfg.model_names.get("morph-kimi-k3-abliterated")
    );
    assert_eq!(
        cfg.model_names.get("cfpd/deepseek-v4.1-flash"),
        cfg.model_names.get("cfpd/deepseek-ai/deepseek-v4.1-flash")
    );
    assert_ne!(
        cfg.model_names.get("cfpd/deepseek-v4.1-flash"),
        cfg.model_names.get("deepseek-v4.1-flash")
    );
}

#[test]
fn winning_wildcard_mapping_resolves_reviewed_spelling_before_default_suppression() {
    let mut root = fixture();
    root["modelComboMappings"] = json!([
        {"enabled":true,"pattern":"morph-*","comboId":"morph","priority":100}
    ]);
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("input.json"), root.to_string()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_llmux-import"))
        .args(["--input", "input.json", "--output", "out.toml"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cfg =
        RuntimeConfig::parse(&fs::read_to_string(dir.path().join("out.toml")).unwrap()).unwrap();
    let canonical = cfg.model_names["morph-kimi-k3-abliterated"];
    assert_eq!(
        cfg.model_names.get("morph-kimik3-abliterated"),
        Some(&canonical)
    );
    assert_eq!(cfg.model_names.get("public-morph"), Some(&canonical));
}

#[test]
fn exact_evidence_backed_spellings_alias_canonical_models_without_fallback_removal() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("input.json"), fixture().to_string()).unwrap();
    fs::write(dir.path().join("omniroute-key"), "synthetic-omniroute").unwrap();
    let key_ref = format!("file:{}", dir.path().join("omniroute-key").display());
    let output = Command::new(env!("CARGO_BIN_EXE_llmux-import"))
        .args([
            "--input",
            "input.json",
            "--output",
            "out.toml",
            "--omniroute-fallback",
            "http://127.0.0.1:20128/v1",
            "--omniroute-key",
            &key_ref,
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = fs::read_to_string(dir.path().join("out.toml")).unwrap();
    assert!(!text.contains("synthetic-upstream"));
    assert!(!text.contains("synthetic-cfpd"));
    assert!(!text.contains("synthetic-morph"));
    assert!(!text.contains("synthetic-omniroute"));
    let cfg = RuntimeConfig::parse(&text).unwrap();

    for (spelling, canonical, native_upstream) in [
        (
            "morph-kimik3-abliterated",
            "morph-kimi-k3-abliterated",
            "morph-native-upstream",
        ),
        (
            "cfpd/deepseek-v4.1-flash",
            "deepseek-v4.1-flash",
            "deepseek-native-upstream",
        ),
    ] {
        let canonical_index = cfg.model_names[canonical];
        assert_eq!(
            cfg.model_names.get(spelling),
            Some(&canonical_index),
            "{spelling}"
        );
        assert!(
            cfg.models
                .iter()
                .all(|model| model.name.as_ref() != spelling),
            "{spelling} must not become a duplicate fallback-only model"
        );
        let model = &cfg.models[canonical_index];
        let deployments: Vec<_> = model
            .deployments
            .iter()
            .map(|idx| &cfg.deployments[idx.0 as usize])
            .collect();
        assert!(deployments.len() >= 2, "{canonical}");
        assert!(deployments[..deployments.len() - 1]
            .iter()
            .all(|deployment| !deployment.fallback));
        assert_eq!(
            deployments[0].upstream_model.as_ref(),
            native_upstream,
            "{canonical} native upstream spelling changed"
        );
        let fallback = deployments.last().unwrap();
        assert!(fallback.fallback, "{canonical} fallback deployment missing");
        assert_eq!(fallback.upstream_model.as_ref(), canonical);
        assert!(
            deployments
                .iter()
                .all(|deployment| deployment.upstream_model.as_ref() != spelling),
            "{spelling} must stay a public alias, not an upstream substitution"
        );
    }

    let deepseek_index = cfg.model_names["deepseek-v4.1-flash"];
    assert_eq!(cfg.model_names.get("public-cfpd"), Some(&deepseek_index));
    let morph_index = cfg.model_names["morph-kimi-k3-abliterated"];
    assert_eq!(cfg.model_names.get("public-morph"), Some(&morph_index));

    for spelling in [
        "cfpd/deepseek-ai/deepseek-v4.1-flash",
        "cfpd/other-model",
        "morph-kimik3",
        "morph-kimik3-fast",
        "morph/morph-kimik3-abliterated",
    ] {
        let index = *cfg
            .model_names
            .get(spelling)
            .unwrap_or_else(|| panic!("unrelated spelling missing: {spelling}"));
        assert_ne!(
            index, cfg.model_names["morph-kimi-k3-abliterated"],
            "unrelated spelling collapsed onto morph canonical: {spelling}"
        );
        assert_ne!(
            index, cfg.model_names["deepseek-v4.1-flash"],
            "unrelated spelling collapsed onto deepseek canonical: {spelling}"
        );
    }
}
