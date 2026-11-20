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
            .any(|w| w == &format!("combo {name}: reviewed free XPLabs route placed first")));
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
fn billed_xplabs_models_keep_their_declared_position() {
    for (name, model) in [
        ("claude-haiku-5.5", "claude-haiku-5.5"),
        ("qwen3.8-27b", "qwen3.8-27b"),
        ("deepseek-v4.1-flash-1017", "deepseek-v4.1-flash-1017"),
    ] {
        let declared = format!("experlabs/{model}");
        let (actual, warnings) = routes(&fixture(name, json!(["paid/a", declared])));
        assert_eq!(
            actual,
            [pair("https://paid.example/v1", "a"), pair(XPLABS, model)],
            "{name}"
        );
        assert!(warnings.iter().all(|w| !w.contains("XPLabs")));
    }
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
        assert!(warnings.iter().all(|w| !w.contains("placed first")));
    }
}
