use llmux_config::RuntimeConfig;
use serde_json::{json, Value};
use std::{fs, path::Path, process::Command};

fn fixture() -> Value {
    json!({
        "providerConnections":[{"id":"baseline","provider":"openai","authType":"apikey","apiKey":"synthetic-baseline-key"}],
        "combos":[{"name":"baseline","models":["openai/test"]}],
        "apiKeys":[{"key":"synthetic-client-key"}]
    })
}

fn run(root: &Value, dir: &Path, flags: &[(&str, &Path)]) -> std::process::Output {
    let input = dir.join("input.json");
    fs::write(&input, root.to_string()).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_llmux-import"));
    command.args([
        "--input",
        input.to_str().unwrap(),
        "--output",
        dir.join("out.toml").to_str().unwrap(),
    ]);
    for (flag, path) in flags {
        command.arg(flag);
        if *flag != "--openrouter-replacements" {
            command.arg(path);
        }
    }
    command.output().unwrap()
}

fn success(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "{stderr}");
    stderr
}

#[test]
fn chained_mapping_availability_reaches_a_fixed_point_before_aliases() {
    let mut root = fixture();
    let names = [
        "openrouter/anthropic/claude-fable-5.2",
        "openrouter/anthropic/claude-fable-5.1:batch",
        "openrouter/anthropic/claude-fable-5:batch",
    ];
    root["providerConnections"].as_array_mut().unwrap().push(json!({"id":"anthropic","provider":"anthropic","authType":"apikey","apiKey":"synthetic-anthropic-key"}));
    for (i, name) in names.iter().enumerate() {
        root["combos"].as_array_mut().unwrap().extend([
            json!({"id":format!("original-{i}"),"name":name,"models":["openai/unverified"]}),
            json!({"id":format!("nested-{i}"),"name":format!("nested-{i}"),"models":[{"kind":"combo-ref","comboName":name}]}),
        ]);
    }
    root["modelComboMappings"] = json!([
        {"pattern":names[0],"comboId":"missing","enabled":true},
        {"pattern":names[1],"comboId":"nested-0","enabled":true},
        {"pattern":names[2],"comboId":"nested-1","enabled":true}
    ]);
    root["modelAliases"] =
        json!({"dependent":names[2],"chain":"dependent", "prefixed":format!("combo/{}", names[2])});
    let dir = tempfile::tempdir().unwrap();
    let warnings = success(&run(
        &root,
        dir.path(),
        &[("--openrouter-replacements", dir.path())],
    ));
    let config = RuntimeConfig::load(dir.path().join("out.toml")).unwrap();
    for name in names.into_iter().chain([
        "nested-0",
        "nested-1",
        "nested-2",
        "dependent",
        "chain",
        "prefixed",
    ]) {
        assert!(!config.model_names.contains_key(name), "unexpected {name}");
    }
    assert!(warnings.contains("target unavailable (winning mapping)"));
}

#[test]
fn custom_node_id_ds_keeps_its_identity_without_opt_ins() {
    let mut root = fixture();
    root["providerNodes"] = json!([{"id":"ds","prefix":"gateway","type":"openai-compatible","baseUrl":"https://custom.example/v1"}]);
    root["providerConnections"] = json!([{"id":"custom","provider":"ds","authType":"apikey","apiKey":"synthetic-custom-key"}]);
    root["combos"] =
        json!([{"name":"custom","models":["gateway/vendor/model", "ds/vendor/model"]}]);
    let dir = tempfile::tempdir().unwrap();
    success(&run(&root, dir.path(), &[]));
    let config = RuntimeConfig::load(dir.path().join("out.toml")).unwrap();
    for name in ["custom", "gateway/vendor/model", "ds/vendor/model"] {
        let model = &config.models[config.model_names[name]];
        for dep in &model.deployments {
            let dep = &config.deployments[dep.0 as usize];
            assert_eq!(dep.base_url.to_string(), "https://custom.example/v1");
            assert_eq!(dep.upstream_model.as_ref(), "vendor/model");
        }
    }
    assert!(!config.model_names.contains_key("deepseek/vendor/model"));
}

#[test]
fn unavailable_mapping_blocks_retained_openrouter_combo_and_dependents() {
    for target in ["missing", "empty"] {
        let mut root = fixture();
        let name = "openrouter/anthropic/claude-fable-5.2";
        root["providerConnections"].as_array_mut().unwrap().push(json!({"id":"anthropic","provider":"anthropic","authType":"apikey","apiKey":"synthetic-anthropic-key"}));
        root["combos"].as_array_mut().unwrap().extend([
            json!({"id":"original","name":name,"models":["openai/unverified"]}),
            json!({"id":"empty","name":"empty","models":[]}),
            json!({"name":"nested","models":[{"kind":"combo-ref","comboName":name}]}),
            json!({"name":"bare-nested","models":[name]}),
        ]);
        root["modelAliases"] =
            json!({"dependent":name,"chain":"dependent","prefixed":format!("combo/{name}")});
        root["modelComboMappings"] =
            json!([{"pattern":name,"comboId":target,"priority":100,"enabled":true}]);
        let dir = tempfile::tempdir().unwrap();
        let warnings = success(&run(
            &root,
            dir.path(),
            &[("--openrouter-replacements", dir.path())],
        ));
        let config = RuntimeConfig::load(dir.path().join("out.toml")).unwrap();
        for name in [
            name,
            "dependent",
            "chain",
            "prefixed",
            "nested",
            "bare-nested",
        ] {
            assert!(
                !config.model_names.contains_key(name),
                "unexpected {name}: target {target}"
            );
        }
        assert!(warnings.contains("target unavailable (winning mapping)"));
    }
}

#[test]
fn workers_shorthand_only_connections_match_canonical_and_alias_legs() {
    for (alias, canonical) in [
        ("cfpd", "cloudflare-direct"),
        ("cfp", "cloudflare-playground"),
    ] {
        let mut root = fixture();
        root["providerConnections"] = json!([{"id":"only","provider":alias,"authType":"none"}]);
        root["combos"] = json!([
            {"name":"canonical","models":[format!("{canonical}/openai/gpt-oss-20b")]},
            {"name":"alias","models":[format!("{alias}/openai/gpt-oss-20b")]}
        ]);
        let dir = tempfile::tempdir().unwrap();
        let account = dir.path().join("account");
        let token = dir.path().join("token");
        fs::write(&account, "0123456789abcdef0123456789abcdef").unwrap();
        fs::write(&token, "synthetic-token").unwrap();
        success(&run(
            &root,
            dir.path(),
            &[
                ("--cloudflare-account-id-file", &account),
                ("--cloudflare-token-file", &token),
            ],
        ));
        let config = RuntimeConfig::load(dir.path().join("out.toml")).unwrap();
        for name in [
            "canonical",
            "alias",
            &format!("{canonical}/openai/gpt-oss-20b"),
            &format!("{alias}/openai/gpt-oss-20b"),
        ] {
            assert!(config.model_names.contains_key(name), "missing {name}");
            let model = &config.models[config.model_names[name]];
            assert_eq!(model.deployments.len(), 1);
            assert_eq!(
                config.deployments[model.deployments[0].0 as usize]
                    .upstream_model
                    .as_ref(),
                "@cf/openai/gpt-oss-20b"
            );
        }
    }
}

#[test]
fn replacement_restrictions_exclude_legs_and_mistral_uses_mapped_model() {
    let mut root = fixture();
    root["providerNodes"] = json!([{"id":"mistral","prefix":"vibe","type":"openai-compatible","baseUrl":"https://api.mistral.ai/v1"}]);
    root["providerConnections"].as_array_mut().unwrap().push(json!({"id":"mistral","provider":"mistral","authType":"apikey","apiKey":"synthetic-mistral-key"}));
    let model = "openrouter/mistralai/mistral-large-4-0";
    root["combos"].as_array_mut().unwrap().extend([
        json!({"name":"mapped-mistral","models":[model]}),
        json!({"name":"pinned","models":[{"model":model,"connectionId":"mistral"}]}),
        json!({"name":"allowed","models":[{"model":model,"allowedConnectionIds":["mistral"]}]}),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let warnings = success(&run(
        &root,
        dir.path(),
        &[("--openrouter-replacements", dir.path())],
    ));
    let config = RuntimeConfig::load(dir.path().join("out.toml")).unwrap();
    for name in ["pinned", "allowed"] {
        assert!(!config.model_names.contains_key(name), "unexpected {name}");
    }
    let model = &config.models[config.model_names["mapped-mistral"]];
    assert_eq!(model.deployments.len(), 1);
    let dep = &config.deployments[model.deployments[0].0 as usize];
    assert_eq!(dep.upstream_model.as_ref(), "mistral-large-4");
    assert_eq!(dep.base_url.to_string(), "https://api.mistral.ai/v1");
    assert!(warnings.contains("source connection restriction"));
}

#[test]
fn empty_winning_mapping_blocks_seeded_replacement_and_dependents() {
    for models in [
        json!([]),
        json!(["openrouter/vendor/unknown"]),
        json!([{"providerId":"anthropic","model":"claude-fable-5-2","usable":false}]),
    ] {
        let mut root = fixture();
        root["providerConnections"].as_array_mut().unwrap().push(json!({"id":"anthropic","provider":"anthropic","authType":"apikey","apiKey":"synthetic-anthropic-key"}));
        root["combos"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":"empty","name":"empty","isActive":true,"models":models}));
        root["modelAliases"] =
            json!({"dependent":"openrouter/anthropic/claude-fable-5.2", "chain":"dependent"});
        root["modelComboMappings"] = json!([
            {"pattern":"openrouter/anthropic/claude-fable-5.2","comboId":"empty","priority":100,"enabled":true},
            {"pattern":"openrouter/anthropic/claude-fable-5.2","comboId":"missing","priority":0,"enabled":true}
        ]);
        let dir = tempfile::tempdir().unwrap();
        let warnings = success(&run(
            &root,
            dir.path(),
            &[("--openrouter-replacements", dir.path())],
        ));
        let config = RuntimeConfig::load(dir.path().join("out.toml")).unwrap();
        for name in [
            "openrouter/anthropic/claude-fable-5.2",
            "dependent",
            "chain",
        ] {
            assert!(!config.model_names.contains_key(name), "unexpected {name}");
        }
        assert!(warnings.contains("target unavailable (winning mapping)"));
    }
}

#[test]
fn replaced_openrouter_combos_preserve_nested_and_alias_references() {
    let mut root = fixture();
    let name = "openrouter/anthropic/claude-fable-5.2";
    root["providerConnections"].as_array_mut().unwrap().push(json!({"id":"anthropic","provider":"anthropic","authType":"apikey","apiKey":"synthetic-anthropic-key"}));
    root["combos"].as_array_mut().unwrap().extend([
        json!({"id":"original","name":name,"models":["openai/unverified"]}),
        json!({"name":"nested","models":[{"kind":"combo-ref","comboName":name}]}),
    ]);
    root["modelAliases"] = json!({"prefixed":format!("combo/{name}"),"dependent":"mapped"});
    root["modelComboMappings"] = json!([{"pattern":"mapped","comboId":"original","enabled":true}]);
    let dir = tempfile::tempdir().unwrap();
    success(&run(
        &root,
        dir.path(),
        &[("--openrouter-replacements", dir.path())],
    ));
    let config = RuntimeConfig::load(dir.path().join("out.toml")).unwrap();
    for name in [name, "nested", "prefixed", "mapped", "dependent"] {
        assert!(config.model_names.contains_key(name), "missing {name}");
        let model = &config.models[config.model_names[name]];
        assert_eq!(model.deployments.len(), 1);
        let dep = &config.deployments[model.deployments[0].0 as usize];
        assert_eq!(dep.upstream_model.as_ref(), "claude-fable-5-2");
        assert_eq!(dep.base_url.to_string(), "https://api.anthropic.com/v1");
    }
}

#[test]
fn workers_registry_shorthands_use_canonical_connections() {
    let mut root = fixture();
    for (alias, provider) in [
        ("cfpd", "cloudflare-direct"),
        ("cfp", "cloudflare-playground"),
    ] {
        root["providerConnections"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":provider,"provider":provider,"authType":"none"}));
        root["combos"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":alias,"models":[format!("{alias}/openai/gpt-oss-20b")]}));
    }
    let dir = tempfile::tempdir().unwrap();
    let account = dir.path().join("account");
    let token = dir.path().join("token");
    fs::write(&account, "0123456789abcdef0123456789abcdef").unwrap();
    fs::write(&token, "synthetic-token").unwrap();
    success(&run(
        &root,
        dir.path(),
        &[
            ("--cloudflare-account-id-file", &account),
            ("--cloudflare-token-file", &token),
        ],
    ));
    let config = RuntimeConfig::load(dir.path().join("out.toml")).unwrap();
    for alias in ["cfpd", "cfp"] {
        assert!(config.model_names.contains_key(alias), "missing {alias}");
        let model = &config.models[config.model_names[alias]];
        assert_eq!(
            config.deployments[model.deployments[0].0 as usize]
                .upstream_model
                .as_ref(),
            "@cf/openai/gpt-oss-20b"
        );
    }
}

#[test]
fn workers_opt_in_maps_bridges_and_fails_closed_for_unknown_deprecated_and_paid_models() {
    let mut root = fixture();
    for (i, provider) in [
        "cloudflare-direct",
        "cfpd",
        "cloudflare-playground",
        "cfp",
        "custom-cf",
    ]
    .iter()
    .enumerate()
    {
        root["providerConnections"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":format!("cf-{i}"),"provider":provider,"authType":"none"}));
        root["combos"].as_array_mut().unwrap().push(json!({"name":format!("cf-{i}"),"models":[{"providerId":provider,"model":"cfpd/openai/gpt-oss-20b"}]}));
    }
    root["providerNodes"] = json!([{"id":"custom-cf","prefix":"cloudflare-direct","type":"openai-compatible","baseUrl":"http://127.0.0.1:8790/v1"}]);
    for (name, model) in [
        ("unknown", "unknown/test"),
        ("deprecated", "@cf/meta/llama-3.2-1b-instruct"),
        ("paid", "zai-org/glm-5.3"),
    ] {
        root["combos"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":name,"models":[{"providerId":"cloudflare-direct","model":model}]}));
    }
    let dir = tempfile::tempdir().unwrap();
    let account = dir.path().join("account-id");
    let token = dir.path().join("external-token");
    fs::write(&account, "0123456789abcdef0123456789abcdef\n").unwrap();
    fs::write(&token, "synthetic-external-cf-token").unwrap();
    let warnings = success(&run(
        &root,
        dir.path(),
        &[
            ("--cloudflare-account-id-file", &account),
            ("--cloudflare-token-file", &token),
        ],
    ));
    let config = RuntimeConfig::load(dir.path().join("out.toml")).unwrap();
    for i in 0..5 {
        let model = &config.models[config.model_names[format!("cf-{i}").as_str()]];
        let dep = &config.deployments[model.deployments[0].0 as usize];
        assert_eq!(dep.upstream_model.as_ref(), "@cf/openai/gpt-oss-20b");
        assert_eq!(
            dep.base_url.to_string(),
            "https://api.cloudflare.com/client/v4/accounts/0123456789abcdef0123456789abcdef/ai/v1"
        );
        assert_eq!(dep.quota.rpm, Some(1));
        assert!(!dep.credential.is_empty());
    }
    for name in ["unknown", "deprecated", "paid"] {
        assert!(!config.model_names.contains_key(name));
    }
    assert!(warnings.contains("unknown or deprecated Workers AI model"));
    assert!(warnings.contains("neuron"));
    let text = fs::read_to_string(dir.path().join("out.toml")).unwrap();
    assert!(text.contains(&format!("file:{}", token.display())));
    assert!(!text.contains("synthetic-external-cf-token"));
    assert!(!warnings.contains("synthetic-external-cf-token"));
    assert_eq!(fs::read_dir(dir.path().join("secrets")).unwrap().count(), 2);
}

#[test]
fn workers_without_opt_in_keeps_replacement_warning_and_does_not_read_external_token() {
    let mut root = fixture();
    root["providerConnections"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":"cf","provider":"cloudflare-direct","authType":"none"}));
    root["combos"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name":"cf-model","models":["cloudflare-direct/openai/gpt-oss-20b"]}));
    let dir = tempfile::tempdir().unwrap();
    let warnings = success(&run(&root, dir.path(), &[]));
    assert!(warnings.contains("replace with a Cloudflare Workers AI account API token"));
    let config = RuntimeConfig::load(dir.path().join("out.toml")).unwrap();
    assert!(!config.model_names.contains_key("cf-model"));
    let dir = tempfile::tempdir().unwrap();
    let account = dir.path().join("account");
    fs::write(&account, "0123456789abcdef0123456789abcdef").unwrap();
    let nonexistent_token = dir.path().join("runtime-only-token");
    success(&run(
        &root,
        dir.path(),
        &[
            ("--cloudflare-account-id-file", &account),
            ("--cloudflare-token-file", &nonexistent_token),
        ],
    ));
    assert!(!nonexistent_token.exists());
    assert!(fs::read_to_string(dir.path().join("out.toml"))
        .unwrap()
        .contains("runtime-only-token"));
}

#[test]
fn empty_and_conflicting_kilo_catalogs_never_allow_keyed_routes() {
    for catalog in [
        json!({"data":[]}),
        json!([
            {"id":"vendor/free","pricing":{"prompt":"0","completion":"0"}},
            {"id":"vendor/free","pricing":{"prompt":"0","completion":"0.1"}}
        ]),
    ] {
        let mut root = fixture();
        root["providerConnections"].as_array_mut().unwrap().push(json!({"id":"kilo","provider":"kilo","authType":"apikey","apiKey":"synthetic-kilo-key"}));
        root["combos"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"kilo-model","models":["kilo/vendor/free"]}));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        fs::write(&path, catalog.to_string()).unwrap();
        let warnings = success(&run(&root, dir.path(), &[("--kilo-free-models", &path)]));
        let config = RuntimeConfig::load(dir.path().join("out.toml")).unwrap();
        assert!(!config.model_names.contains_key("kilo-model"));
        assert!(warnings.contains("Kilo model not verified free"));
    }
}

#[test]
fn openrouter_replacements_fail_closed_without_official_connections_or_transport() {
    let mut root = fixture();
    root["providerConnections"].as_array_mut().unwrap().extend([
        json!({"id":"or","provider":"openrouter","authType":"apikey","apiKey":"synthetic-or-key"}),
        json!({"id":"anthropic","provider":"anthropic","authType":"apikey","apiKey":"synthetic-anthropic-key","providerSpecificData":{"baseUrl":"https://unverified.example/v1"}})
    ]);
    root["modelAliases"] = json!({"fable":"openrouter/anthropic/claude-fable-5.2", "gemma-interactions":"openrouter/google/gemma-4-31b-it:free", "paid":"openrouter/x-ai/grok-4.7"});
    root["combos"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name":"paid-direct","models":["openrouter/x-ai/grok-4.7"]}));
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let report = llmux_import::import_with_options(
        &root.to_string(),
        &output,
        None,
        &llmux_import::ImportOptions {
            openrouter_replacements: true,
            ..Default::default()
        },
    )
    .unwrap();
    let config = RuntimeConfig::load(output).unwrap();
    for name in [
        "fable",
        "gemma-interactions",
        "paid",
        "paid-direct",
        "openrouter/x-ai/grok-4.7",
    ] {
        assert!(!config.model_names.contains_key(name));
    }
    assert!(config
        .deployments
        .iter()
        .all(|d| !d.base_url.to_string().contains("openrouter.ai")));
    assert!(!dir.path().join("secrets/upstream-or").exists());
    assert!(report
        .warnings
        .join("\n")
        .contains("no verified connected OpenRouter replacement"));
}

#[test]
fn official_replacements_do_not_bypass_unavailable_winning_mappings() {
    let mut root = fixture();
    root["providerConnections"].as_array_mut().unwrap().push(json!({"id":"anthropic","provider":"anthropic","authType":"apikey","apiKey":"synthetic-anthropic-key"}));
    root["modelAliases"] = json!({"fable":"openrouter/anthropic/claude-fable-5.2","openrouter/anthropic/claude-fable-5.2":"baseline"});
    root["modelComboMappings"] = json!([{"pattern":"openrouter/anthropic/claude-fable-5.2","comboId":"missing","priority":100,"enabled":true}]);
    let dir = tempfile::tempdir().unwrap();
    let report = llmux_import::import_with_options(
        &root.to_string(),
        &dir.path().join("out.toml"),
        None,
        &llmux_import::ImportOptions {
            openrouter_replacements: true,
            ..Default::default()
        },
    )
    .unwrap();
    let config = RuntimeConfig::load(dir.path().join("out.toml")).unwrap();
    assert!(!config
        .model_names
        .contains_key("openrouter/anthropic/claude-fable-5.2"));
    assert!(!config.model_names.contains_key("fable"));
    assert!(report
        .warnings
        .join("\n")
        .contains("target unavailable (winning mapping)"));
}

#[test]
fn keyed_kilo_requires_two_zero_prices_while_anonymous_is_unchanged() {
    for provider in ["kilo", "kilocode", "custom-kilo"] {
        let mut root = fixture();
        if provider == "custom-kilo" {
            root["providerNodes"] = json!([{"id":"custom-kilo","prefix":"kilo","type":"openai-compatible","baseUrl":"https://ai-gateway.kilo.ai/api/v1"}]);
        }
        root["providerConnections"].as_array_mut().unwrap().push(json!({"id":"keyed","provider":provider,"authType":"apikey","apiKey":"synthetic-kilo-key"}));
        root["providerConnections"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":"anon","provider":provider,"authType":"none"}));
        for model in [
            "vendor/free",
            "vendor/paid",
            "vendor/missing",
            "vendor/false-free",
        ] {
            root["combos"]
                .as_array_mut()
                .unwrap()
                .push(json!({"name":model,"models":[{"providerId":provider,"model":model}]}));
        }
        let dir = tempfile::tempdir().unwrap();
        let catalog = dir.path().join("models.json");
        fs::write(
            &catalog,
            json!({"data":[
                {"id":"vendor/free","pricing":{"prompt":"0.000","completion":0}},
                {"id":"vendor/paid","pricing":{"prompt":0,"completion":"0.1"}},
                {"id":"vendor/false-free","isFree":true,"pricing":{"prompt":"0.2","completion":"0"}}
            ]})
            .to_string(),
        )
        .unwrap();
        let warnings = success(&run(&root, dir.path(), &[("--kilo-free-models", &catalog)]));
        let config = RuntimeConfig::load(dir.path().join("out.toml")).unwrap();
        for name in [
            "vendor/free",
            "vendor/paid",
            "vendor/missing",
            "vendor/false-free",
        ] {
            let model = &config.models[config.model_names[name]];
            assert_eq!(
                model.deployments.len(),
                if name == "vendor/free" { 2 } else { 1 }
            );
        }
        assert!(warnings.contains("Kilo model not verified free"));
        let dir = tempfile::tempdir().unwrap();
        let warnings = success(&run(&root, dir.path(), &[]));
        assert!(warnings.contains("keyed Kilo requires --kilo-free-models"));
        assert!(!dir.path().join("secrets/upstream-keyed").exists());
    }
}

#[test]
fn external_kilo_key_and_verified_openrouter_replacements_use_real_loader() {
    let mut root = fixture();
    root["providerConnections"].as_array_mut().unwrap().extend([
        json!({"id":"anthropic","provider":"anthropic","authType":"apikey","apiKey":"synthetic-anthropic-key"}),
        json!({"id":"gemini","provider":"gemini","authType":"apikey","apiKey":"synthetic-gemini-key"})
    ]);
    root["modelAliases"] = json!({"public-fable":"openrouter/anthropic/claude-fable-5.2", "public-gemma":"openrouter/google/gemma-4-26b-a4b-it:free", "public-kilo":"openrouter/baidu/ernie-5.5-flash-preview:free", "public-paid":"openrouter/x-ai/grok-4.7", "public-unknown":"openrouter/vendor/unknown"});
    root["providerNodes"] = json!([{"id":"mirror","prefix":"mirror","type":"openai-compatible","baseUrl":"https://unverified.example/v1"}]);
    root["providerConnections"].as_array_mut().unwrap().push(json!({"id":"mirror","provider":"mirror","authType":"apikey","apiKey":"synthetic-mirror-key"}));
    root["combos"].as_array_mut().unwrap().push(json!({"id":"mixed-id","name":"mixed-original","models":["anthropic/claude-fable-5-2","mirror/claude-fable-5.2"]}));
    root["modelAliases"]["openrouter/anthropic/claude-fable-5.2"] = json!("mixed-original");
    root["modelAliases"]["openrouter/x-ai/grok-4.7"] = json!("mixed-original");
    root["modelComboMappings"] = json!([{"pattern":"openrouter/anthropic/claude-fable-5.2","comboId":"mixed-id","priority":100,"enabled":true}]);
    root["combos"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name":"direct-seed","models":["openrouter/google/gemini-4-hydrogen"]}));
    let dir = tempfile::tempdir().unwrap();
    let account = dir.path().join("account");
    let token = dir.path().join("cf-token");
    let kilo = dir.path().join("kilo-key");
    let catalog = dir.path().join("models.json");
    fs::write(&account, "0123456789abcdef0123456789abcdef").unwrap();
    fs::write(&token, "synthetic-cf-token").unwrap();
    fs::write(&kilo, "synthetic-kilo-external-key").unwrap();
    fs::write(&catalog, json!({"data":[{"id":"baidu/ernie-5.5-flash-preview:free","pricing":{"prompt":"0","completion":"0"}}]}).to_string()).unwrap();
    let warnings = success(&run(
        &root,
        dir.path(),
        &[
            ("--cloudflare-account-id-file", &account),
            ("--cloudflare-token-file", &token),
            ("--kilo-key-file", &kilo),
            ("--kilo-free-models", &catalog),
            ("--openrouter-replacements", dir.path()),
        ],
    ));
    let config = RuntimeConfig::load(dir.path().join("out.toml")).unwrap();
    for (name, upstream) in [
        ("public-fable", "claude-fable-5-2"),
        ("public-gemma", "@cf/google/gemma-4-26b-a4b-it"),
        ("public-kilo", "baidu/ernie-5.5-flash-preview:free"),
        ("openrouter/anthropic/claude-fable-5.2", "claude-fable-5-2"),
        ("openrouter/google/gemini-4-hydrogen", "gemini-4-hydrogen"),
    ] {
        let model = &config.models[config.model_names[name]];
        assert_eq!(
            model.deployments.len(),
            1,
            "{name} must not retain unverified fallback legs"
        );
        let dep = &config.deployments[model.deployments[0].0 as usize];
        assert_eq!(dep.upstream_model.as_ref(), upstream);
        assert!(!dep.base_url.to_string().contains("openrouter.ai"));
    }
    assert_eq!(
        config.models[config.model_names["mixed-original"]]
            .deployments
            .len(),
        2
    );
    for name in [
        "public-paid",
        "public-unknown",
        "openrouter/x-ai/grok-4.7",
        "openrouter/vendor/unknown",
    ] {
        assert!(!config.model_names.contains_key(name));
    }
    assert!(warnings.contains("no verified connected OpenRouter replacement"));
    let text = fs::read_to_string(dir.path().join("out.toml")).unwrap();
    assert!(text.contains(&format!("file:{}", kilo.display())));
    assert!(!text.contains("synthetic-kilo-external-key"));
}

#[test]
fn malformed_cost_policy_inputs_fail_without_echoing_contents() {
    for (flag, content) in [
        ("--kilo-free-models", "sensitive-malformed-json"),
        ("--cloudflare-account-id-file", "sensitive-invalid-account"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("option");
        fs::write(&file, content).unwrap();
        let result = run(&fixture(), dir.path(), &[(flag, &file)]);
        assert!(!result.status.success());
        assert!(!String::from_utf8_lossy(&result.stderr).contains(content));
        assert!(!dir.path().join("out.toml").exists());
    }
}
