use llmux_config::RuntimeConfig;
use llmux_import::ImportOptions;
use serde_json::{json, Value};

const XPLABS: &str = "https://api.experientiallabs.ai/v1";

fn fixture(name: &str, legs: Value) -> Value {
    json!({
        "providerConnections":[
            {"id":"paid","provider":"paid-node","authType":"apikey","apiKey":"synthetic-paid"},
            {"id":"xplabs","provider":"experlabs-node","authType":"apikey","apiKey":"synthetic-xplabs"},
            {"id":"morph","provider":"morph","authType":"apikey","apiKey":"synthetic-morph"}
        ],
        "providerNodes":[
            {"id":"paid-node","prefix":"paid","type":"openai-compatible","baseUrl":"https://paid.example/v1"},
            {"id":"experlabs-node","prefix":"experlabs","type":"openai-compatible","baseUrl":"https://api.experientiallabs.ai/v1/"}
        ],
        "combos":[{"name":name,"models":legs}],
        "apiKeys":[{"key":"synthetic-client"}]
    })
}

fn routes(root: &Value) -> (Vec<(String, String)>, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.toml");
    let report =
        llmux_import::import_with_options(&root.to_string(), &out, None, &ImportOptions::default())
            .unwrap();
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(!text.contains("synthetic-"));
    assert!(report.warnings.iter().all(|w| !w.contains("synthetic-")));
    let config = RuntimeConfig::load(&out).unwrap();
    let name = root["combos"][0]["name"].as_str().unwrap();
    let model = &config.models[*config.model_names.get(name).unwrap()];
    let mut actual = Vec::new();
    for (priority, i) in model.deployments.iter().enumerate() {
        let dep = &config.deployments[i.0 as usize];
        if root["combos"][0]["strategy"] != "round-robin" {
            assert_eq!(dep.priority, priority as i32);
        }
        actual.push((dep.base_url.to_string(), dep.upstream_model.to_string()));
    }
    (actual, report.warnings)
}

fn pair(base: &str, model: &str) -> (String, String) {
    (base.into(), model.into())
}

#[test]
fn reviewed_free_xplabs_model_is_prepended_ahead_of_paid_legs() {
    for (name, model) in [
        ("gpt-6-luna", "gpt-6-luna"),
        ("deepseek-v4.1-flash", "deepseek-v4.1-flash"),
        ("deepseek-v4.1-flash-fast", "deepseek-v4.1-flash"),
        (
            "deepseek-v4.1-flash-1017-uncensored",
            "deepseek-v4.1-flash-uncensored-fast:free",
        ),
    ] {
        let (actual, warnings) = routes(&fixture(name, json!(["paid/a", "paid/b"])));
        assert_eq!(
            actual,
            [
                pair(XPLABS, model),
                pair("https://paid.example/v1", "a"),
                pair("https://paid.example/v1", "b"),
            ],
            "{name}"
        );
        assert!(warnings
            .iter()
            .any(|w| w == &format!("combo {name}: prioritized reviewed free XPLabs route")));
    }
}

#[test]
fn declared_xplabs_leg_moves_first_without_duplicating() {
    let (actual, _) = routes(&fixture(
        "deepseek-v4.1-flash",
        json!(["paid/a", "experlabs/deepseek-v4.1-flash", "paid/b"]),
    ));
    assert_eq!(
        actual,
        [
            pair(XPLABS, "deepseek-v4.1-flash"),
            pair("https://paid.example/v1", "a"),
            pair("https://paid.example/v1", "b"),
        ]
    );
    let (actual, warnings) = routes(&fixture(
        "gpt-6-luna",
        json!(["experlabs/gpt-6-luna", "paid/a"]),
    ));
    assert_eq!(
        actual,
        [
            pair(XPLABS, "gpt-6-luna"),
            pair("https://paid.example/v1", "a")
        ]
    );
    assert!(warnings.iter().all(|w| !w.contains("XPLabs")));
}

#[test]
fn free_xplabs_logprobs_route_leads_base_combo_and_follows_morph_in_fast_combo() {
    let legs = json!(["paid/a", "morph/morph-dsv41flash-1017"]);
    let morph = pair("https://api.morphllm.com/v1", "morph-dsv41flash-1017");
    let free = pair(XPLABS, "deepseek-v4.1-flash");
    let paid = pair("https://paid.example/v1", "a");
    let (actual, _) = routes(&fixture("deepseek-v4.1-flash", legs.clone()));
    assert_eq!(actual, [free.clone(), paid.clone(), morph.clone()]);
    let (actual, _) = routes(&fixture("deepseek-v4.1-flash-fast", legs));
    assert_eq!(actual, [morph, free, paid]);
}

#[test]
fn billed_xplabs_models_are_dropped() {
    for (name, model) in [
        ("claude-haiku-5.5", "claude-haiku-5.5"),
        ("qwen3.8-27b", "qwen3.8-27b"),
        ("deepseek-v4.1-flash-1017", "deepseek-v4.1-flash-1017"),
    ] {
        let declared = format!("experlabs/{model}");
        let (actual, warnings) = routes(&fixture(name, json!(["paid/a", declared])));
        assert_eq!(actual, [pair("https://paid.example/v1", "a")], "{name}");
        assert!(warnings
            .iter()
            .any(|w| w.contains(&format!("dropped billed XPLabs route {model}"))));
    }
}

#[test]
fn billed_xplabs_leg_drops_in_round_robin_and_only_leg_combos() {
    let mut root = fixture(
        "claude-haiku-5.5",
        json!(["experlabs/claude-haiku-5.5", "paid/a"]),
    );
    root["combos"][0]["strategy"] = "round-robin".into();
    let (actual, warnings) = routes(&root);
    assert_eq!(actual, [pair("https://paid.example/v1", "a")]);
    assert!(warnings
        .iter()
        .any(|w| w.contains("dropped billed XPLabs route claude-haiku-5.5")));
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.toml");
    let report = llmux_import::import_with_options(
        &fixture("claude-haiku-5.5", json!(["experlabs/claude-haiku-5.5"])).to_string(),
        &out,
        None,
        &ImportOptions::default(),
    );
    assert_eq!(
        report.err().as_deref(),
        Some("no importable models; no configuration written")
    );
    assert!(!out.exists());
}

#[test]
fn billed_only_target_blocks_mapped_openrouter_replacement() {
    let mut root = fixture("ok", json!(["paid/a"]));
    root["providerConnections"].as_array_mut().unwrap().push(
        json!({"id":"anthropic","provider":"anthropic","authType":"apikey","apiKey":"synthetic-anthropic"}),
    );
    root["combos"].as_array_mut().unwrap().extend([
        json!({"id":"billed","name":"claude-haiku-5.5","isActive":true,"models":["experlabs/claude-haiku-5.5"]}),
        json!({"name":"openrouter/anthropic/claude-fable-5.2","models":["openai/unverified"]}),
    ]);
    root["modelComboMappings"] = json!([
        {"pattern":"openrouter/anthropic/claude-fable-5.2","comboId":"billed","priority":100,"enabled":true}
    ]);
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.toml");
    let report = llmux_import::import_with_options(
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
    // the billed-only target is unavailable, so the mapped openrouter combo keeps
    // no reviewed replacement and emits nothing
    for name in ["openrouter/anthropic/claude-fable-5.2", "claude-haiku-5.5"] {
        assert!(!config.model_names.contains_key(name), "unexpected {name}");
    }
    assert!(config.model_names.contains_key("ok"));
    assert!(report
        .warnings
        .iter()
        .any(|w| { w == "combo openrouter/anthropic/claude-fable-5.2: skipped: no deployments" }));
}

#[test]
fn dropped_billed_leg_never_consumes_a_context_override() {
    let mut root = fixture(
        "claude-haiku-5.5",
        json!(["experlabs/claude-haiku-5.5", "paid/a"]),
    );
    root["modelContextOverrides"] = json!([
        {"provider":"experlabs","modelId":"claude-haiku-5.5","realContext":131072},
        {"provider":"paid","modelId":"a","realContext":65536},
    ]);
    let (_, warnings) = routes(&root);
    assert!(warnings.iter().any(
        |w| w == "context overrides: 1 applied, 1 ignored (no imported deployment), 0 invalid"
    ));
}

#[test]
fn free_xplabs_leg_in_a_billed_combo_survives() {
    let (actual, warnings) = routes(&fixture(
        "claude-haiku-5.5",
        json!([
            "experlabs/deepseek-v4.1-flash",
            "experlabs/claude-haiku-5.5"
        ]),
    ));
    assert_eq!(actual, [pair(XPLABS, "deepseek-v4.1-flash")]);
    assert!(warnings
        .iter()
        .any(|w| w.contains("dropped billed XPLabs route claude-haiku-5.5")));
}

#[test]
fn ineligible_connections_and_combos_are_unchanged() {
    let paid = vec![pair("https://paid.example/v1", "a")];
    let mut cases = Vec::new();
    for edit in [
        json!({"isActive":false}),
        json!({"usable":false}),
        json!({"authType":"oauth","accessToken":"synthetic-token"}),
        json!({"providerSpecificData":{"baseUrl":"https://other.example/v1"}}),
    ] {
        let mut root = fixture("deepseek-v4.1-flash", json!(["paid/a"]));
        for (key, value) in edit.as_object().unwrap() {
            root["providerConnections"][1][key] = value.clone();
        }
        cases.push(root);
    }
    let mut prefix = fixture("deepseek-v4.1-flash", json!(["paid/a"]));
    prefix["providerNodes"][1]["prefix"] = "other-gateway".into();
    cases.push(prefix);
    cases.push(fixture("deepseek-v4.1-flash-free", json!(["paid/a"])));
    let mut round_robin = fixture("deepseek-v4.1-flash", json!(["paid/a"]));
    round_robin["combos"][0]["strategy"] = "round-robin".into();
    cases.push(round_robin);
    for root in cases {
        let (actual, warnings) = routes(&root);
        assert_eq!(actual, paid, "{root}");
        assert!(warnings.iter().all(|w| !w.contains("prioritized")));
    }
}

fn credentials(root: &Value) -> Vec<(String, String)> {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.toml");
    llmux_import::import_with_options(&root.to_string(), &out, None, &ImportOptions::default())
        .unwrap();
    let config = RuntimeConfig::load(&out).unwrap();
    let name = root["combos"][0]["name"].as_str().unwrap();
    let model = &config.models[*config.model_names.get(name).unwrap()];
    model
        .deployments
        .iter()
        .map(|i| {
            let dep = &config.deployments[i.0 as usize];
            (dep.upstream_model.to_string(), dep.credential.to_string())
        })
        .collect()
}

#[test]
fn moved_declared_leg_keeps_its_pinned_connection() {
    let legs = json!(["paid/a", {"model":"experlabs/deepseek-v4.1-flash","connectionId":"xplabs"}]);
    let with = |name: &str| {
        let mut root = fixture(name, legs.clone());
        root["providerConnections"][1]["priority"] = 5.into();
        root["providerConnections"].as_array_mut().unwrap().push(json!({"id":"xplabs-other","provider":"experlabs-node","authType":"apikey","apiKey":"synthetic-xplabs-other","priority":1}));
        root
    };
    let declared = credentials(&with("deepseek-v4.1-flash-free"));
    let pinned = declared[0].clone();
    assert_eq!(pinned.0, "deepseek-v4.1-flash");
    let moved = credentials(&with("deepseek-v4.1-flash"));
    assert_eq!(moved, [pinned, declared[1].clone()]);
}
