use llmux_config::RuntimeConfig;
use serde_json::{json, Value};
use std::fs;

const REMOVED: &[&str] = &[
    "kimi-k2.6",
    "moonshotai/kimi-k2.6",
    "cloudflare-direct/moonshotai/kimi-k2.6",
    "cfp/moonshotai/kimi-k2.6",
    "cfpd/moonshotai/kimi-k2.6",
    "cloudflare-playground/moonshotai/kimi-k2.6",
    "ddgw/gpt-5.4-mini",
    "duckduckgo-web/ddgw/gpt-5.4-mini",
];

fn fixture() -> Value {
    json!({
        "_meta": {"version": "omniroute-v3-legacy-export"},
        "providerNodes": [
            {"id":"cfpd","prefix":"cfpd","type":"openai-compatible","apiType":"chat","baseUrl":"https://cfpd.example/v1"},
            {"id":"ddgw","prefix":"ddgw","type":"openai-compatible","apiType":"chat","baseUrl":"https://ddgw.example/v1"}
        ],
        "providerConnections": [
            {"id":"cfpd","provider":"cfpd","authType":"apikey","apiKey":"synthetic-cfpd"},
            {"id":"ddgw","provider":"ddgw","authType":"apikey","apiKey":"synthetic-ddgw"}
        ],
        "apiKeys": [{"key":"synthetic-client"}],
        "combos": [
            {"id":"kimi","name":"kimi-k2.6","models":["cfpd/moonshotai/kimi-k2.6"]},
            {"id":"ddgw","name":"ddgw/gpt-5.4-mini","models":["ddgw/gpt-5.4-mini"]},
            {"id":"duck","name":"duckduckgo-web/ddgw/gpt-5.4-mini","models":["ddgw/gpt-5.4-mini"]},
            {"id":"near-kimi","name":"kimi-k2.7","models":["cfpd/moonshotai/kimi-k2.7"]},
            {"id":"near-mini","name":"ddgw/gpt-5.4-nano","models":["ddgw/gpt-5.4-nano"]},
            {"id":"near-deepseek","name":"deepseek-v4.1-flash","models":["ddgw/deepseek-v4.1-flash"]},
            {"id":"near-gpt","name":"gpt-6-luna","models":["ddgw/gpt-6-luna"]}
        ],
        "modelAliases": {
            "moonshotai/kimi-k2.6":"kimi-k2.6",
            "cfp/moonshotai/kimi-k2.6":"kimi-k2.6",
            "cfpd/moonshotai/kimi-k2.6":"kimi-k2.6",
            "cloudflare-playground/moonshotai/kimi-k2.6":"kimi-k2.6"
        }
    })
}

#[test]
fn user_approved_no_free_native_lane_models_fail_closed_exactly() {
    unsafe { std::env::set_var("WO189_OVERLAY_KEY", "synthetic-overlay") };
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let audit_path = dir.path().join("canonical-map.json");
    let overlay = dir.path().join("site.toml");
    fs::write(
        &overlay,
        "[[credentials]]\nid = \"overlay-omni\"\nkind = \"api_key\"\nprovider = \"openai_compat\"\n\
         key = \"env:WO189_OVERLAY_KEY\"\n\
         [[models]]\nname = \"cloudflare-direct/moonshotai/kimi-k2.6\"\n[[models.deployments]]\n\
         id = \"cloudflare-direct-moonshotai-kimi-k2-6\"\ncredential = \"overlay-omni\"\n\
         upstream_model = \"cfpd/moonshotai/kimi-k2.6\"\nbase_url = \"http://127.0.0.1:20128/v1\"\n",
    )
    .unwrap();
    let key = dir.path().join("omniroute-key");
    fs::write(&key, "synthetic-omniroute").unwrap();
    let key_ref = format!("file:{}", key.display());
    let report = llmux_import::import_with_options(
        &fixture().to_string(),
        &output,
        None,
        &llmux_import::ImportOptions {
            omniroute_fallback: Some(llmux_import::OmniRouteFallback {
                base_url: "http://127.0.0.1:20128/v1",
                key: &key_ref,
            }),
            overlay: Some(&overlay),
            canonical_map: Some(&audit_path),
            ..Default::default()
        },
    )
    .unwrap();

    for subject in [
        "kimi-k2.6",
        "ddgw/gpt-5.4-mini",
        "duckduckgo-web/ddgw/gpt-5.4-mini",
    ] {
        assert!(
            report.warnings.iter().any(|warning| {
                warning.contains(subject)
                    && warning.contains("user-approved")
                    && warning.contains("WO-189")
            }),
            "missing explicit user-approved removal warning for {subject}: {:?}",
            report.warnings
        );
    }

    let text = fs::read_to_string(&output).unwrap();
    assert!(!text.contains("synthetic-cfpd"));
    assert!(!text.contains("synthetic-ddgw"));
    assert!(!text.contains("synthetic-client"));
    assert!(!text.contains("synthetic-overlay"));
    assert!(!text.contains("synthetic-omniroute"));
    assert!(!text.contains("cloudflare-direct-moonshotai-kimi-k2-6"));
    for deployment_id in [
        "omniroute-fallback-kimi-k2.6",
        "omniroute-fallback-kimi-k2-6",
        "omniroute-fallback-ddgw-gpt-5.4-mini",
        "omniroute-fallback-duckduckgo-web-ddgw-gpt-5.4-mini",
    ] {
        assert!(!text.contains(deployment_id), "{deployment_id} remains");
    }

    let cfg = RuntimeConfig::parse(&text).unwrap();
    let catch_all = cfg.catch_all.expect("global catch-all remains");
    assert_eq!(cfg.models[catch_all].name.as_ref(), "*");
    for spelling in REMOVED {
        assert!(
            !cfg.model_names.contains_key(*spelling),
            "{spelling} still resolves to a servable catalog entry"
        );
        assert!(
            cfg.catch_all_exclude
                .iter()
                .any(|excluded| excluded == spelling),
            "{spelling} can still fall through to the OmniRoute catch-all"
        );
    }
    assert!(
        !cfg.model_names.contains_key("gpt-5.4-mini"),
        "bare gpt-5.4-mini was reintroduced"
    );
    for spelling in [
        "kimi-k2.7",
        "ddgw/gpt-5.4-nano",
        "deepseek-v4.1-flash",
        "gpt-6-luna",
    ] {
        assert!(
            cfg.model_names.contains_key(spelling),
            "near-match or unrelated spelling was removed: {spelling}"
        );
    }
    for upstream in [
        "kimi-k2.6",
        "moonshotai/kimi-k2.6",
        "cfpd/moonshotai/kimi-k2.6",
        "ddgw/gpt-5.4-mini",
        "duckduckgo-web/ddgw/gpt-5.4-mini",
    ] {
        assert!(
            cfg.deployments
                .iter()
                .all(|deployment| deployment.upstream_model.as_ref() != upstream),
            "servable deployment remains for {upstream}"
        );
    }
    for deployment in cfg.deployments.iter() {
        if deployment.base_url == "http://127.0.0.1:20128/v1" {
            assert!(
                !REMOVED.contains(&deployment.upstream_model.as_ref()),
                "OmniRoute loopback deployment remains for {}",
                deployment.upstream_model
            );
        }
    }

    let audit: Value = serde_json::from_str(&fs::read_to_string(&audit_path).unwrap()).unwrap();
    let decisions = audit["drop_decisions"]
        .as_array()
        .expect("canonical map records drop decisions");
    for spelling in REMOVED {
        assert!(
            decisions.iter().any(|decision| {
                decision["spelling"] == json!(spelling)
                    && decision["action"] == json!("drop")
                    && decision["decision_source"] == json!("user-approved")
                    && decision["approval"]
                        .as_str()
                        .is_some_and(|approval| approval.contains("WO-189"))
            }),
            "missing user-approved audit decision for {spelling}: {audit}"
        );
    }
}
