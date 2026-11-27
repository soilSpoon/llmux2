use llmux_config::RuntimeConfig;
use llmux_import::ImportOptions;
use serde_json::{json, Value};
use std::process::Command;

fn fixture() -> Value {
    json!({
        "providerConnections": [
            {"id":"morph", "provider":"morph", "authType":"apikey", "apiKey":"synthetic-morph", "providerSpecificData":{"baseUrl":"http://127.0.0.1:8790/morph/v1"}},
            {"id":"vibe", "provider":"vibe-node", "authType":"apikey", "apiKey":"synthetic-vibe"},
            {"id":"other", "provider":"other-node", "authType":"apikey", "apiKey":"synthetic-other"}
        ],
        "providerNodes": [
            {"id":"vibe-node", "prefix":"vibe", "type":"openai-compatible", "baseUrl":"https://api.mistral.ai/v1"},
            {"id":"other-node", "prefix":"other", "type":"openai-compatible", "baseUrl":"https://example.com/v1"}
        ],
        "combos":[{"name":"plain", "models":["other/first", "vibe/deepseek-v4-1-flash-1017", "other/middle", "morph/morph-dsv41flash-1017", "other/last"]}],
        "apiKeys":[{"key":"synthetic-client"}]
    })
}

#[test]
fn reviewed_morph_precedes_bridge_replacements_but_base_keeps_declared_numes_first() {
    for name in [
        "deepseek-v4.1-flash-fast",
        "deepseek-v4.1-flash-1017-fast",
        "deepseek-v4.1-flash-1017-ultrafast",
        "deepseek-v4.1-flash",
        "deepseek-v4.1-flash-1017",
        "plain",
    ] {
        let mut root = fixture();
        root["providerNodes"].as_array_mut().unwrap().extend([
            json!({"id":"deepai-node","prefix":"deepai","type":"openai-compatible","baseUrl":"http://127.0.0.1:8790/deepai/v1"}),
            json!({"id":"numes-node","prefix":"numes","type":"openai-compatible","baseUrl":"https://api.numes.ai/api/v1"}),
        ]);
        root["providerConnections"].as_array_mut().unwrap().push(json!({"id":"numes","provider":"numes-node","authType":"apikey","apiKey":"synthetic-numes"}));
        root["combos"][0] = json!({"name":name,"models":["deepai/deepseek-v4.1-flash", "numes/declared-numes", "vibe/deepseek-v4-1-flash-1017", "morph/morph-dsv41flash-1017", "other/last"]});
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.toml");
        llmux_import::import_with_options(
            &root.to_string(),
            &out,
            None,
            &ImportOptions {
                openrouter_replacements: true,
                ..Default::default()
            },
        )
        .unwrap();
        let config = RuntimeConfig::load(&out).unwrap();
        let model = &config.models[*config.model_names.get(name).unwrap()];
        let actual: Vec<_> = model
            .deployments
            .iter()
            .map(|i| config.deployments[i.0 as usize].upstream_model.as_ref())
            .collect();
        let expected = if name.ends_with("fast") {
            vec![
                "morph-dsv41flash-1017",
                "deepseek/deepseek-v4.1-flash",
                "declared-numes",
                "deepseek-v4-1-flash-1017",
                "last",
            ]
        } else if name == "plain" {
            vec![
                "deepseek/deepseek-v4.1-flash",
                "declared-numes",
                "deepseek-v4-1-flash-1017",
                "morph-dsv41flash-1017",
                "last",
            ]
        } else {
            vec![
                "declared-numes",
                "morph-dsv41flash-1017",
                "deepseek/deepseek-v4.1-flash",
                "deepseek-v4-1-flash-1017",
                "last",
            ]
        };
        assert_eq!(actual, expected, "{name}");
        assert!(!std::fs::read_to_string(out)
            .unwrap()
            .contains("_bridge_replacement"));
    }
}

#[test]
fn morph_official_loopback_policy_is_builtin_only_and_preserves_other_rejections() {
    for (provider, url, expected) in [
        ("morph", "http://127.0.0.1:8790/morph/v1", true),
        ("morph", "http://LOCALHOST.:8790/morph/v1", true),
        ("morph", "http://[::1]:8790/morph/v1", true),
        ("morph", "https://api.morphllm.com/v1", true),
        ("morph", "http://0.0.0.0:8790/morph/v1", false),
        ("morph", "https://unofficial.example/v1", false),
        ("other-node", "http://127.0.0.1:8790/morph/v1", false),
    ] {
        let mut root = fixture();
        root["providerConnections"][0]["provider"] = json!(provider);
        root["providerConnections"][0]["providerSpecificData"]["baseUrl"] = json!(url);
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.toml");
        let report = llmux_import::import(&root.to_string(), &output, None).unwrap();
        let config = RuntimeConfig::load(&output).unwrap();
        let imported: Vec<_> = config
            .deployments
            .iter()
            .filter(|d| d.credential.as_ref() == "upstream-morph")
            .collect();
        assert_eq!(!imported.is_empty(), expected, "{provider}: {url}");
        for dep in imported {
            assert_eq!(dep.base_url.to_string(), "https://api.morphllm.com/v1");
            assert!(dep.strip_request_fields.is_empty());
        }
        assert_eq!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("reviewed Morph loopback")),
            expected && url != "https://api.morphllm.com/v1"
        );
        assert!(!report.warnings.join("\n").contains("synthetic-morph"));
    }
}

#[test]
fn reviewed_flash_combos_move_only_verified_morph_ahead_of_mistral() {
    for name in [
        "deepseek-v4.1-flash",
        "deepseek-v4.1-flash-fast",
        "deepseek-v4.1-flash-1017",
        "deepseek-v4.1-flash-1017-fast",
        "deepseek-v4.1-flash-1017-ultrafast",
        "plain",
    ] {
        for morph_model in [Some("morph-dsv41flash-1017"), Some("morph-other"), None] {
            let mut root = fixture();
            root["combos"][0]["name"] = json!(name);
            if let Some(model) = morph_model {
                root["combos"][0]["models"][3] = json!(format!("morph/{model}"));
            } else {
                root["combos"][0]["models"]
                    .as_array_mut()
                    .unwrap()
                    .remove(3);
            }
            root["modelAliases"] = json!({"public-alias":name});
            let dir = tempfile::tempdir().unwrap();
            let output = dir.path().join("out.toml");
            let report = llmux_import::import(&root.to_string(), &output, None).unwrap();
            let config = RuntimeConfig::load(output).unwrap();
            let model = config
                .models
                .iter()
                .find(|m| m.name.as_ref() == name)
                .unwrap();
            let actual: Vec<_> = model
                .deployments
                .iter()
                .map(|i| config.deployments[i.0 as usize].upstream_model.as_ref())
                .collect();
            let reorder = name != "plain" && morph_model == Some("morph-dsv41flash-1017");
            let mut expected = vec!["first", "deepseek-v4-1-flash-1017", "middle"];
            if let Some(model) = morph_model {
                expected.insert(
                    if reorder && name.ends_with("fast") {
                        0
                    } else if reorder {
                        1
                    } else {
                        3
                    },
                    model,
                );
            }
            expected.push("last");
            assert_eq!(actual, expected, "{name}");
            assert_eq!(
                config.model_names.get("public-alias"),
                config.model_names.get(name)
            );
            assert_eq!(
                report
                    .warnings
                    .iter()
                    .any(|w| w.contains("reviewed Morph logprobs order")),
                reorder
            );
            for (priority, i) in model.deployments.iter().enumerate() {
                assert_eq!(config.deployments[i.0 as usize].priority, priority as i32);
            }
        }
    }
}

#[test]
fn morph_ultra_opt_in_filters_out_of_plan_models_and_marks_the_credential() {
    let mut root = fixture();
    root["combos"][0]["models"] = json!([
        "morph/morph-kimik31",
        "morph/morph-kimik3",
        "morph/morph-kimik3-fast",
        "morph/morph-dsv41flash",
        "morph/morph-dsv41flash-1017",
        "morph/morph-dsv41flash-abliterated",
        "morph/morph-v3-fast",
        "morph/morph-kimik3-abliterated"
    ]);
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let report = llmux_import::import_with_options(
        &root.to_string(),
        &output,
        None,
        &ImportOptions {
            morph_ultra: true,
            ..Default::default()
        },
    )
    .unwrap();
    let config = RuntimeConfig::load(&output).unwrap();
    let model = config
        .models
        .iter()
        .find(|model| model.name.as_ref() == "plain")
        .unwrap();
    let upstreams: Vec<_> = model
        .deployments
        .iter()
        .map(|index| &config.deployments[index.0 as usize])
        .filter(|dep| dep.credential.as_ref() == "upstream-morph")
        .map(|dep| dep.upstream_model.as_ref())
        .collect();
    assert_eq!(
        upstreams,
        [
            "morph-kimik31",
            "morph-dsv41flash",
            "morph-dsv41flash-1017",
            "morph-dsv41flash-abliterated"
        ]
    );
    let emitted = std::fs::read_to_string(&output).unwrap();
    assert!(emitted.contains("allowed_upstream_models"));
    assert!(emitted.contains("morph-kimik31"));
    assert!(!upstreams.contains(&"morph-kimik3"));
    assert!(report
        .warnings
        .iter()
        .any(|warning| warning.contains("outside the Morph Ultra plan")));

    let output = dir.path().join("default.toml");
    llmux_import::import(&root.to_string(), &output, None).unwrap();
    let config = RuntimeConfig::load(&output).unwrap();
    assert!(config
        .deployments
        .iter()
        .any(|dep| dep.upstream_model.as_ref() == "morph-v3-fast"));
}

#[test]
fn cli_morph_ultra_flag_reaches_the_emitted_config() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input.json");
    let output = dir.path().join("out.toml");
    std::fs::write(&input, fixture().to_string()).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_llmux-import"))
        .args(["--input"])
        .arg(&input)
        .args(["--output"])
        .arg(&output)
        .args(["--morph-ultra"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let emitted = std::fs::read_to_string(&output).unwrap();
    assert!(emitted.contains("allowed_upstream_models"));

    let duplicate = Command::new(env!("CARGO_BIN_EXE_llmux-import"))
        .args(["--input"])
        .arg(&input)
        .args(["--output"])
        .arg(&output)
        .args(["--morph-ultra", "--morph-ultra"])
        .output()
        .unwrap();
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("duplicate argument"));
}
