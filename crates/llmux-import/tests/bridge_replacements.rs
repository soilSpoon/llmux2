use llmux_config::RuntimeConfig;
use llmux_import::{ImportOptions, OmniRouteFallback};
use serde_json::{json, Value};

fn fixture() -> Value {
    json!({
        "apiKeys":[{"key":"synthetic-client"}],
        "providerNodes":[
            {"id":"openai-compatible-deepai","prefix":"deepai","type":"openai-compatible","baseUrl":"http://127.0.0.1:8790/deepai/v1"},
            {"id":"openai-compatible-numes","prefix":"numes","type":"openai-compatible","baseUrl":"https://api.numes.ai/api/v1"},
            {"id":"openai-compatible-vibe","prefix":"vibe","type":"openai-compatible","baseUrl":"https://api.mistral.ai/v1"}
        ],
        "providerConnections":[
            {"id":"ddg","provider":"duckduckgo-web","authType":"apikey"},
            {"id":"deepai","provider":"openai-compatible-deepai","authType":"apikey"},
            {"id":"numes","provider":"openai-compatible-numes","authType":"apikey","apiKey":"synthetic-numes"},
            {"id":"mistral-slow","provider":"openai-compatible-vibe","authType":"apikey","apiKey":"synthetic-slow","priority":9},
            {"id":"mistral-fast","provider":"openai-compatible-vibe","authType":"apikey","apiKey":"synthetic-fast","priority":1},
            {"id":"claude","provider":"anthropic","authType":"apikey","apiKey":"synthetic-claude"},
            {"id":"gemini","provider":"gemini","authType":"apikey","apiKey":"synthetic-gemini"},
            {"id":"baseline","provider":"openai","authType":"apikey","apiKey":"synthetic-baseline"},
            {"id":"codex","provider":"codex","authType":"oauth","accessToken":"synthetic-codex"}
        ],
        "combos":[
            {"id":"mixed-id","name":"mixed","strategy":"priority","models":[
                "openai/first",
                {"providerId":"duckduckgo-web","model":"mistral-small-2603","connectionId":"ddg"},
                "codex/gpt-5.6-luna",
                {"providerId":"deepai","model":"mimo-2.6-flash","allowedConnectionIds":["deepai"]},
                "openai/last"
            ]},
            {"name":"nested","models":[{"kind":"combo-ref","comboName":"mixed"}]},
            {"name":"unknown","models":["duckduckgo-web/gpt-6-luna","deepai/glyph-cluster"]},
            {"name":"baseline","models":["openai/test"]}
        ],
        "modelAliases":{"bridge-alias":"deepai/mimo-2.6-flash","nested-alias":"nested"},
        "modelComboMappings":[{"pattern":"public-mixed","comboId":"mixed-id","enabled":true}]
    })
}

#[test]
fn bridge_replacements_preserve_leg_order_aliases_and_optional_fallback() {
    for fallback in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.toml");
        let key = dir.path().join("key");
        std::fs::write(&key, "synthetic-fallback").unwrap();
        let reference = format!("file:{}", key.display());
        let report = llmux_import::import_with_options(
            &fixture().to_string(),
            &output,
            Some(&dir.path().join("secrets")),
            &ImportOptions {
                openrouter_replacements: true,
                omniroute_fallback: fallback.then_some(OmniRouteFallback {
                    base_url: "http://127.0.0.1:20128/v1",
                    key: &reference,
                }),
                ..Default::default()
            },
        )
        .unwrap();
        let config = RuntimeConfig::load(output).unwrap();
        for name in ["mixed", "nested", "nested-alias", "public-mixed"] {
            let model = &config.models[*config.model_names.get(name).unwrap()];
            let deps: Vec<_> = model
                .deployments
                .iter()
                .map(|i| &config.deployments[i.0 as usize])
                .collect();
            let ids: Vec<_> = deps
                .iter()
                .filter(|d| !d.fallback)
                .map(|d| d.upstream_model.as_ref())
                .collect();
            assert_eq!(
                ids,
                [
                    "first",
                    "mistral-small-2603",
                    "mistral-small-2603",
                    "xiaomi/mimo-v2.6-flash",
                    "last"
                ],
                "{name}"
            );
            assert_eq!(deps[1].credential.as_ref(), "upstream-mistral-fast");
            assert_eq!(deps[2].credential.as_ref(), "upstream-mistral-slow");
            assert!(!deps.iter().any(|d| d.fallback), "Codex ancestry: {name}");
        }
        for name in [
            "deepai/mimo-2.6-flash",
            "openai-compatible-deepai/mimo-2.6-flash",
            "bridge-alias",
        ] {
            let model = &config.models[*config
                .model_names
                .get(name)
                .unwrap_or_else(|| panic!("missing {name}"))];
            assert_eq!(
                config.deployments[model.deployments[0].0 as usize]
                    .upstream_model
                    .as_ref(),
                "xiaomi/mimo-v2.6-flash"
            );
            let last = &config.deployments[model.deployments.last().unwrap().0 as usize];
            assert_eq!(last.fallback, fallback);
            if fallback {
                assert_eq!(last.upstream_model, model.name);
            }
        }
        if fallback {
            for name in ["unknown", "duckduckgo-web/gpt-6-luna"] {
                let model = &config.models[*config
                    .model_names
                    .get(name)
                    .unwrap_or_else(|| panic!("missing {name}"))];
                let last = &config.deployments[model.deployments.last().unwrap().0 as usize];
                assert!(last.fallback, "{name}");
                assert_eq!(last.upstream_model.as_ref(), name, "{name}");
            }
        } else {
            assert!(!config.model_names.contains_key("unknown"));
            assert!(!config.model_names.contains_key("duckduckgo-web/gpt-6-luna"));
        }
        assert!(!config
            .api_keys
            .keys()
            .any(|k| k.as_ref() == "upstream-codex"));
        assert!(report
            .warnings
            .iter()
            .any(|w| w.contains("reviewed bridge replacement")
                && w.contains("source connection restrictions")));
    }
}

#[test]
fn bridge_replacements_are_opt_in_fail_closed_and_respect_source_reservations() {
    for enabled in [false, true] {
        let mut root = fixture();
        if enabled {
            root["providerConnections"]
                .as_array_mut()
                .unwrap()
                .retain(|c| c["id"] == "baseline");
        }
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.toml");
        llmux_import::import_with_options(
            &root.to_string(),
            &out,
            Some(&dir.path().join("secrets")),
            &ImportOptions {
                openrouter_replacements: enabled,
                ..Default::default()
            },
        )
        .unwrap();
        let config = RuntimeConfig::load(out).unwrap();
        assert!(!config.model_names.contains_key("deepai/mimo-2.6-flash"));
        assert!(!config
            .model_names
            .contains_key("duckduckgo-web/mistral-small-2603"));
    }
    let mut root = fixture();
    root["combos"].as_array_mut().unwrap().extend([
        json!({"name":"openai-compatible-deepai/mimo-2.6-pro","models":["openai/collision-winner"]}),
        json!({"name":"disabled","models":[{"providerId":"duckduckgo-web","model":"mistral-small-2603","usable":false}]}),
        json!({"name":"malformed","models":[{"providerId":"deepai","model":"mimo-2.6-flash","allowedConnectionIds":"deepai"}]}),
    ]);
    root["modelComboMappings"].as_array_mut().unwrap().push(json!({"pattern":"deepai/mimo-2.6-flash","comboId":"missing","priority":100,"enabled":true}));
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.toml");
    llmux_import::import_with_options(
        &root.to_string(),
        &out,
        Some(&dir.path().join("secrets")),
        &ImportOptions {
            openrouter_replacements: true,
            ..Default::default()
        },
    )
    .unwrap();
    let config = RuntimeConfig::load(out).unwrap();
    let winner = &config.models[*config
        .model_names
        .get("openai-compatible-deepai/mimo-2.6-pro")
        .unwrap()];
    assert_eq!(
        config.deployments[winner.deployments[0].0 as usize]
            .upstream_model
            .as_ref(),
        "collision-winner"
    );
    for name in [
        "disabled",
        "malformed",
        "deepai/mimo-2.6-flash",
        "bridge-alias",
    ] {
        assert!(!config.model_names.contains_key(name), "{name}");
    }
}

#[test]
fn reviewed_bridge_table_uses_exact_models_and_official_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    let account = dir.path().join("account");
    let token = dir.path().join("token");
    let snapshot = dir.path().join("workers.json");
    std::fs::write(&account, "0123456789abcdef0123456789abcdef").unwrap();
    std::fs::write(&token, "synthetic-workers-token").unwrap();
    std::fs::write(
        &snapshot,
        r#"{"success":true,"result":[{"name":"@cf/openai/gpt-oss-120b"}]}"#,
    )
    .unwrap();
    let out = dir.path().join("out.toml");
    llmux_import::import_with_options(
        &fixture().to_string(),
        &out,
        Some(&dir.path().join("secrets")),
        &ImportOptions {
            openrouter_replacements: true,
            cloudflare_account_id_file: Some(&account),
            cloudflare_token_file: Some(&token),
            workers_ai_models: Some(&snapshot),
            ..Default::default()
        },
    )
    .unwrap();
    let config = RuntimeConfig::load(out).unwrap();
    for (source, upstream, host) in [
        (
            "duckduckgo-web/claude-haiku-4-5",
            "claude-haiku-4-5",
            "api.anthropic.com",
        ),
        (
            "duckduckgo-web/mistral-small-2603",
            "mistral-small-2603",
            "api.mistral.ai",
        ),
        (
            "duckduckgo-web/tinfoil/gpt-oss-120b",
            "@cf/openai/gpt-oss-120b",
            "api.cloudflare.com",
        ),
        (
            "deepai/claude-fable-5-2",
            "claude-fable-5-2",
            "api.anthropic.com",
        ),
        (
            "deepai/claude-opus-5-5",
            "claude-opus-5-5",
            "api.anthropic.com",
        ),
        (
            "deepai/claude-sonnet-5-5",
            "claude-sonnet-5-5",
            "api.anthropic.com",
        ),
        (
            "deepai/gemini-4-argon",
            "gemini-4-argon",
            "generativelanguage.googleapis.com",
        ),
        (
            "deepai/gemini-4-carbon",
            "gemini-4-carbon",
            "generativelanguage.googleapis.com",
        ),
        (
            "deepai/gemini-4-hydrogen",
            "gemini-4-hydrogen",
            "generativelanguage.googleapis.com",
        ),
        (
            "deepai/deepseek-v4.1-flash",
            "deepseek/deepseek-v4.1-flash",
            "api.numes.ai",
        ),
        ("deepai/glm-5.3-flash", "z-ai/glm-5.3-flash", "api.numes.ai"),
        (
            "deepai/mimo-2.6-flash",
            "xiaomi/mimo-v2.6-flash",
            "api.numes.ai",
        ),
        (
            "deepai/mimo-2.6-pro",
            "xiaomi/mimo-v2.6-pro",
            "api.numes.ai",
        ),
        (
            "deepai/minimax-m3.1-flash",
            "minimax/minimax-m3.1-flash",
            "api.numes.ai",
        ),
        (
            "deepai/qwen-3.8-flash",
            "qwen/qwen3.8-flash",
            "api.numes.ai",
        ),
    ] {
        let model = &config.models[*config
            .model_names
            .get(source)
            .unwrap_or_else(|| panic!("missing {source}"))];
        for d in &model.deployments {
            let d = &config.deployments[d.0 as usize];
            assert_eq!(d.upstream_model.as_ref(), upstream, "{source}");
            assert_eq!(d.base_url.host(), Some(host), "{source}");
        }
    }
    for source in [
        "duckduckgo-web/claude-haiku-5-5-high",
        "duckduckgo-web/ddgw/mistral-small-2603",
        "duckduckgo-web/tinfoil/gemma4-31b",
        "deepai/tencent/hy4",
        "deepai/gpt-6-luna",
        "deepai/mimo-2.6-pro-high",
        "deepai/qwen-3.8-27b",
    ] {
        assert!(!config.model_names.contains_key(source), "{source}");
    }
}

#[test]
fn bridge_targets_reject_anonymous_inactive_and_lookalike_endpoints() {
    for mode in ["anonymous", "inactive", "lookalike", "anthropic-adapter"] {
        let mut root = fixture();
        for c in root["providerConnections"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .filter(|c| c["id"] == "numes")
        {
            match mode {
                "anonymous" => {
                    c["authType"] = json!("none");
                    c["apiKey"] = Value::Null;
                }
                "inactive" => c["isActive"] = json!(false),
                _ => {}
            }
        }
        if mode == "lookalike" {
            root["providerNodes"][1]["baseUrl"] =
                json!("https://api.numes.ai.unverified.example/api/v1");
        }
        if mode == "anthropic-adapter" {
            root["providerNodes"][1]["type"] = json!("anthropic-compatible");
        }
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.toml");
        llmux_import::import_with_options(
            &root.to_string(),
            &out,
            Some(&dir.path().join("secrets")),
            &ImportOptions {
                openrouter_replacements: true,
                ..Default::default()
            },
        )
        .unwrap();
        let config = RuntimeConfig::load(out).unwrap();
        assert!(
            !config.model_names.contains_key("deepai/mimo-2.6-flash"),
            "{mode}"
        );
    }
}

fn cfpd_fixture() -> Value {
    json!({
        "apiKeys":[{"key":"synthetic-client"}],
        "providerNodes":[
            {"id":"openai-compatible-chat-cfp-direct","prefix":"cloudflare-direct","type":"openai-compatible","baseUrl":"http://127.0.0.1:8790/cfp/v1"},
            {"id":"openai-compatible-numes","prefix":"numes","type":"openai-compatible","baseUrl":"https://api.numes.ai/api/v1"}
        ],
        "providerConnections":[
            {"id":"cfp-direct","provider":"openai-compatible-chat-cfp-direct","authType":"apikey","apiKey":"synthetic-cfp-direct"},
            {"id":"numes","provider":"openai-compatible-numes","authType":"apikey","apiKey":"synthetic-numes"},
            {"id":"morph","provider":"morph","authType":"apikey","apiKey":"synthetic-morph"}
        ],
        "combos":[
            {"name":"kimi-k3.1-pro","models":[
                "cfpd/moonshotai/kimi-k3.1-pro",
                "numes/moonshotai/kimi-k3.1-pro"
            ]},
            {"name":"kimi-direct","models":["cloudflare-direct/moonshotai/kimi-k3.1-pro"]}
        ]
    })
}

#[test]
fn unreachable_cfpd_kimi_redirects_to_reviewed_morph() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.toml");
    llmux_import::import_with_options(
        &cfpd_fixture().to_string(),
        &out,
        Some(&dir.path().join("secrets")),
        &ImportOptions {
            openrouter_replacements: true,
            ..Default::default()
        },
    )
    .unwrap();
    let config = RuntimeConfig::load(out).unwrap();
    for name in ["kimi-k3.1-pro", "kimi-direct"] {
        let model = &config.models[*config.model_names.get(name).unwrap()];
        let deps: Vec<_> = model
            .deployments
            .iter()
            .map(|i| &config.deployments[i.0 as usize])
            .collect();
        assert_eq!(deps[0].upstream_model.as_ref(), "morph-kimik31", "{name}");
        assert_eq!(deps[0].base_url.host(), Some("api.morphllm.com"), "{name}");
        assert_eq!(deps[0].credential.as_ref(), "upstream-morph", "{name}");
        if name == "kimi-k3.1-pro" {
            assert_eq!(deps[1].upstream_model.as_ref(), "moonshotai/kimi-k3.1-pro");
            assert_eq!(deps[1].base_url.host(), Some("api.numes.ai"));
            assert_eq!(deps.len(), 2, "{name}");
        } else {
            assert_eq!(deps.len(), 1, "{name}");
        }
    }
    for name in [
        "cfpd/moonshotai/kimi-k3.1-pro",
        "cloudflare-direct/moonshotai/kimi-k3.1-pro",
        "openai-compatible-chat-cfp-direct/moonshotai/kimi-k3.1-pro",
    ] {
        let model = &config.models[*config
            .model_names
            .get(name)
            .unwrap_or_else(|| panic!("missing {name}"))];
        assert_eq!(
            config.deployments[model.deployments[0].0 as usize]
                .upstream_model
                .as_ref(),
            "morph-kimik31"
        );
    }
}

#[test]
fn cfpd_kimi_redirect_is_opt_in_fail_closed_and_needs_reviewed_morph() {
    for replacements in [false, true] {
        let mut root = cfpd_fixture();
        if replacements {
            root["providerConnections"]
                .as_array_mut()
                .unwrap()
                .retain(|c| c["id"] != "morph");
        }
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.toml");
        llmux_import::import_with_options(
            &root.to_string(),
            &out,
            Some(&dir.path().join("secrets")),
            &ImportOptions {
                openrouter_replacements: replacements,
                ..Default::default()
            },
        )
        .unwrap();
        let config = RuntimeConfig::load(out).unwrap();
        let model = &config.models[*config.model_names.get("kimi-k3.1-pro").unwrap()];
        assert_eq!(model.deployments.len(), 1);
        let dep = &config.deployments[model.deployments[0].0 as usize];
        assert_eq!(dep.upstream_model.as_ref(), "moonshotai/kimi-k3.1-pro");
        assert_eq!(dep.base_url.host(), Some("api.numes.ai"));
        assert!(!config
            .model_names
            .contains_key("cfpd/moonshotai/kimi-k3.1-pro"));
    }
    for mode in ["anonymous", "unofficial-url"] {
        let mut root = cfpd_fixture();
        for c in root["providerConnections"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .filter(|c| c["id"] == "morph")
        {
            match mode {
                "anonymous" => {
                    c["authType"] = json!("none");
                    c["apiKey"] = Value::Null;
                }
                _ => c["providerSpecificData"] = json!({"baseUrl":"https://unofficial.example/v1"}),
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.toml");
        llmux_import::import_with_options(
            &root.to_string(),
            &out,
            Some(&dir.path().join("secrets")),
            &ImportOptions {
                openrouter_replacements: true,
                ..Default::default()
            },
        )
        .unwrap();
        let config = RuntimeConfig::load(out).unwrap();
        let model = &config.models[*config.model_names.get("kimi-k3.1-pro").unwrap()];
        assert!(
            !model.deployments.iter().any(|i| {
                config.deployments[i.0 as usize].upstream_model.as_ref() == "morph-kimik31"
            }),
            "{mode}"
        );
    }
}

#[test]
fn custom_duckduckgo_identity_does_not_inherit_builtin_replacements() {
    let mut root = fixture();
    root["providerNodes"].as_array_mut().unwrap().push(json!({"id":"duckduckgo-web","prefix":"duckduckgo-web","type":"openai-compatible","baseUrl":"http://127.0.0.1:8790/other/v1"}));
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.toml");
    llmux_import::import_with_options(
        &root.to_string(),
        &out,
        Some(&dir.path().join("secrets")),
        &ImportOptions {
            openrouter_replacements: true,
            ..Default::default()
        },
    )
    .unwrap();
    let config = RuntimeConfig::load(out).unwrap();
    assert!(!config
        .model_names
        .contains_key("duckduckgo-web/mistral-small-2603"));
    let model = &config.models[*config.model_names.get("mixed").unwrap()];
    assert!(!model
        .deployments
        .iter()
        .any(|i| config.deployments[i.0 as usize].upstream_model.as_ref() == "mistral-small-2603"));
}
