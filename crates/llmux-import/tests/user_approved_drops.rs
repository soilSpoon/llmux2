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
    "gpt-5.6-luna",
    "duckduckgo-web/ddgw/gpt-5.6-luna",
];

fn fixture() -> Value {
    json!({
        "_meta": {"version": "omniroute-v3-legacy-export"},
        "providerNodes": [
            {"id":"cfpd","prefix":"cfpd","type":"openai-compatible","apiType":"chat","baseUrl":"https://cfpd.example/v1"},
            {"id":"ddgw","prefix":"ddgw","type":"openai-compatible","apiType":"chat","baseUrl":"https://ddgw.example/v1"},
        ],
        "providerConnections": [
            {"id":"cfpd","provider":"cfpd","authType":"apikey","apiKey":"synthetic-cfpd"},
            {"id":"ddgw","provider":"ddgw","authType":"apikey","apiKey":"synthetic-ddgw"}
        ],
        "apiKeys": [{"key":"synthetic-client"},{"key":"synthetic-buyer","isActive":true,"modelAccessMode":"restricted","allowedModels":["gpt-5.6-luna","cfpd/*"],"blockedModels":[]},{"key":"synthetic-drop-only","isActive":true,"modelAccessMode":"restricted","allowedModels":["gpt-5.6-luna"],"blockedModels":[]}],
        "combos": [
            {"id":"kimi","name":"kimi-k2.6","models":["cfpd/moonshotai/kimi-k2.6"]},
            {"id":"ddgw","name":"ddgw/gpt-5.4-mini","models":["ddgw/gpt-5.4-mini"]},
            {"id":"duck","name":"duckduckgo-web/ddgw/gpt-5.4-mini","models":["ddgw/gpt-5.4-mini"]},
            {"id":"luna","name":"gpt-5.6-luna","models":["ddgw/gpt-5.6-luna"]},
            {"id":"luna-ddgw","name":"duckduckgo-web/ddgw/gpt-5.6-luna","models":["ddgw/gpt-5.6-luna"]},
            {"id":"near-kimi","name":"kimi-k2.7","models":["cfpd/moonshotai/kimi-k2.7"]},
            {"id":"near-mini","name":"ddgw/gpt-5.4-nano","models":["ddgw/gpt-5.4-nano"]},
            {"id":"near-deepseek","name":"deepseek-v4.1-flash","models":["ddgw/deepseek-v4.1-flash"]},
            {"id":"near-gpt","name":"gpt-6-luna","models":["ddgw/gpt-6-luna"]},
            {"id":"near-luna","name":"gpt-5.6-nano","models":["ddgw/gpt-5.6-nano"]}
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
        "gpt-5.6-luna",
        "duckduckgo-web/ddgw/gpt-5.6-luna",
    ] {
        assert!(
            report.warnings.iter().any(|warning| {
                warning.contains(subject)
                    && warning.contains("user-approved")
                    && warning.contains("free exact native lane")
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
        "omniroute-fallback-duckduckgo-web-ddgw-gpt-5.6-luna",
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
        "gpt-5.6-nano",
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
        "gpt-5.6-luna",
        "duckduckgo-web/ddgw/gpt-5.6-luna",
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

    let buyer_idx = cfg
        .server
        .client_keys
        .iter()
        .position(|key| key.as_ref() == "synthetic-buyer")
        .expect("restricted buyer key remains");
    let buyer_policy = cfg.server.key_policies[buyer_idx]
        .as_ref()
        .expect("restricted buyer key keeps its portable policy");
    let interrupted = "gpt-5.6-luna";
    assert!(
        !buyer_policy.models.iter().any(|model| model == interrupted),
        "buyer key policy still advertises {interrupted}"
    );
    assert!(
        buyer_policy.models.iter().any(|model| model == "cfpd/*"),
        "unrelated buyer key policy wildcard was removed"
    );

    assert!(
        report.warnings.iter().any(|warning| {
            warning.contains("client key 2") && warning.contains("left no allowed models")
        }),
        "drop-only restricted key did not fail closed: {:?}",
        report.warnings
    );
    assert!(
        !cfg.server
            .client_keys
            .iter()
            .any(|key| key.as_ref().contains("client-2")),
        "drop-only restricted key was still emitted"
    );
    let secret_leak = fs::read_dir(dir.path().join("secrets"))
        .unwrap()
        .any(|entry| {
            fs::read_to_string(entry.unwrap().path())
                .is_ok_and(|secret| secret.contains("synthetic-drop-only"))
        });
    assert!(!secret_leak, "skipped drop-only key wrote a secret file");

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
                    && decision["approval"].as_str().is_some_and(|approval| {
                        if spelling.contains("gpt-5.6-luna") {
                            approval.contains("WO-")
                        } else {
                            approval.contains("WO-189")
                        }
                    })
            }),
            "missing user-approved audit decision for {spelling}: {audit}"
        );
    }
    for (spelling, marker) in [
        ("gpt-5.6-luna", "WO-188"),
        ("duckduckgo-web/ddgw/gpt-5.6-luna", "WO-188"),
    ] {
        assert!(
            decisions.iter().any(|decision| {
                decision["spelling"] == json!(spelling)
                    && decision["approval"]
                        .as_str()
                        .is_some_and(|approval| approval.contains(marker))
                    && decision["reason"].as_str().is_some_and(|reason| {
                        reason.contains("no adoptable free exact native lane")
                            && reason.contains("parked/rejected under current evidence")
                    })
            }),
            "missing free-lane-constraint parking decision for {spelling}: {audit}"
        );
    }
}

fn write_overlay(dir: &tempfile::TempDir, models_toml: &str) -> std::path::PathBuf {
    unsafe { std::env::set_var("WO189_OVERLAY_KEY", "synthetic-overlay") };
    let overlay = dir.path().join("site.toml");
    fs::write(
        &overlay,
        format!(
            "[[credentials]]\nid = \"overlay-omni\"\nkind = \"api_key\"\nprovider = \"openai_compat\"\nkey = \"env:WO189_OVERLAY_KEY\"\n{models_toml}"
        ),
    )
    .unwrap();
    overlay
}

fn import_with_overlay(input: &Value, overlay: &std::path::Path, output: &std::path::Path) {
    llmux_import::import_with_options(
        &input.to_string(),
        output,
        None,
        &llmux_import::ImportOptions {
            overlay: Some(overlay),
            ..Default::default()
        },
    )
    .unwrap();
}

#[test]
fn overlay_alias_to_interrupted_spelling_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let overlay = write_overlay(
        &dir,
        "[[models]]\nname = \"overlay-survivor\"\naliases = [\"kimi-k2.6\", \"overlay-alias\"]\n[[models.deployments]]\nid = \"overlay-survivor-0\"\ncredential = \"overlay-omni\"\nupstream_model = \"overlay-survivor-upstream\"\nbase_url = \"https://overlay.example/v1\"\n",
    );
    import_with_overlay(&fixture(), &overlay, &output);
    let cfg = RuntimeConfig::load(&output).unwrap();
    assert!(cfg.model_names.contains_key("overlay-survivor"));
    assert!(
        cfg.model_names.contains_key("overlay-alias"),
        "unrelated overlay alias was removed"
    );
    assert!(
        !cfg.model_names.contains_key("kimi-k2.6"),
        "overlay alias makes interrupted spelling resolve to a servable model"
    );
}

#[test]
fn mixed_combo_drops_only_interrupted_direct_upstream_leg() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let mut root = fixture();
    root["combos"].as_array_mut().unwrap().push(json!({
        "id":"mixed","name":"mixed","models":["cfpd/moonshotai/kimi-k2.6","cfpd/moonshotai/kimi-k2.7"]
    }));
    llmux_import::import_with_options(
        &root.to_string(),
        &output,
        None,
        &llmux_import::ImportOptions::default(),
    )
    .unwrap();
    let cfg = RuntimeConfig::load(&output).unwrap();
    assert!(
        cfg.model_names.contains_key("mixed"),
        "mixed should survive through its unrelated leg"
    );
    let mixed_idx = cfg.model_names["mixed"];
    let upstreams: Vec<_> = cfg.models[mixed_idx]
        .deployments
        .iter()
        .map(|i| cfg.deployments[i.0 as usize].upstream_model.to_string())
        .collect();
    assert!(
        upstreams.iter().any(|u| u == "moonshotai/kimi-k2.7"),
        "unrelated mixed leg missing: {upstreams:?}"
    );
    assert!(
        upstreams.iter().all(|u| u != "moonshotai/kimi-k2.6"),
        "interrupted direct upstream leg survives in mixed: {upstreams:?}"
    );
    assert!(
        cfg.deployments
            .iter()
            .all(|deployment| deployment.upstream_model.as_ref() != "moonshotai/kimi-k2.6"),
        "interrupted moonshotai/kimi-k2.6 still has a servable deployment"
    );
}

#[test]
fn overlay_catchall_without_exclude_is_closed_to_interruptions() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let overlay = write_overlay(
        &dir,
        "[[models]]\nname = \"*\"\n[[models.deployments]]\nid = \"overlay-catchall\"\ncredential = \"overlay-omni\"\nupstream_model = \"*\"\nbase_url = \"https://overlay.example/v1\"\npriority = 0\nfallback = true\nbilling = \"billed\"\n",
    );
    import_with_overlay(&fixture(), &overlay, &output);
    let cfg = RuntimeConfig::load(&output).unwrap();
    assert!(cfg.catch_all.is_some(), "overlay catch-all missing");
    for spelling in REMOVED {
        assert!(
            cfg.catch_all_exclude
                .iter()
                .any(|excluded| excluded == spelling),
            "overlay catch-all without exclude can serve {spelling}: {:?}",
            cfg.catch_all_exclude
        );
    }
}

#[test]
fn interrupted_only_combo_collapses_and_cannot_fall_through() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let key = dir.path().join("omniroute-key");
    fs::write(&key, "synthetic-omniroute").unwrap();
    let key_ref = format!("file:{}", key.display());
    let mut root = fixture();
    root["combos"].as_array_mut().unwrap().push(json!({
        "id":"interrupted-only","name":"interrupted-only","models":["cfpd/moonshotai/kimi-k2.6"]
    }));
    llmux_import::import_with_options(
        &root.to_string(),
        &output,
        None,
        &llmux_import::ImportOptions {
            omniroute_fallback: Some(llmux_import::OmniRouteFallback {
                base_url: "http://127.0.0.1:20128/v1",
                key: &key_ref,
            }),
            ..Default::default()
        },
    )
    .unwrap();
    let cfg = RuntimeConfig::load(&output).unwrap();
    assert!(
        !cfg.model_names.contains_key("interrupted-only"),
        "combo with only an interrupted direct upstream leg stayed servable"
    );
    assert!(
        cfg.catch_all_exclude
            .iter()
            .any(|excluded| excluded == "interrupted-only"),
        "collapsed interrupted-only combo can fall through to the catch-all: {:?}",
        cfg.catch_all_exclude
    );
}

#[test]
fn overlay_deployments_drop_interrupted_upstream_but_preserve_survivors() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let overlay = write_overlay(
        &dir,
        "[[models]]\nname = \"overlay-mixed\"\naliases = [\"overlay-mixed-alias\"]\n[[models.deployments]]\nid = \"overlay-mixed-interrupted\"\ncredential = \"overlay-omni\"\nupstream_model = \"moonshotai/kimi-k2.6\"\nbase_url = \"https://overlay.example/v1\"\n[[models.deployments]]\nid = \"overlay-mixed-survivor\"\ncredential = \"overlay-omni\"\nupstream_model = \"surviving-upstream\"\nbase_url = \"https://overlay.example/v1\"\n",
    );
    import_with_overlay(&fixture(), &overlay, &output);
    let cfg = RuntimeConfig::load(&output).unwrap();
    assert!(cfg.model_names.contains_key("overlay-mixed"));
    assert!(cfg.model_names.contains_key("overlay-mixed-alias"));
    let mixed_idx = cfg.model_names["overlay-mixed"];
    let upstreams: Vec<_> = cfg.models[mixed_idx]
        .deployments
        .iter()
        .map(|i| cfg.deployments[i.0 as usize].upstream_model.to_string())
        .collect();
    assert!(
        upstreams.iter().any(|u| u == "surviving-upstream"),
        "unrelated overlay deployment was removed: {upstreams:?}"
    );
    assert!(
        upstreams.iter().all(|u| u != "moonshotai/kimi-k2.6"),
        "interrupted overlay deployment survives in overlay-mixed: {upstreams:?}"
    );
    assert!(
        cfg.deployments
            .iter()
            .all(|deployment| deployment.upstream_model.as_ref() != "moonshotai/kimi-k2.6"),
        "interrupted moonshotai/kimi-k2.6 still has a servable deployment"
    );
}

#[test]
fn overlay_model_with_only_interrupted_deployments_cannot_fall_through() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let overlay = write_overlay(
        &dir,
        "[[models]]\nname = \"overlay-collapsed\"\naliases = [\"overlay-collapsed-alias\"]\n[[models.deployments]]\nid = \"overlay-collapsed-0\"\ncredential = \"overlay-omni\"\nupstream_model = \"moonshotai/kimi-k2.6\"\nbase_url = \"https://overlay.example/v1\"\n[[models]]\nname = \"*\"\n[[models.deployments]]\nid = \"overlay-catchall\"\ncredential = \"overlay-omni\"\nupstream_model = \"*\"\nbase_url = \"https://overlay.example/v1\"\npriority = 0\nfallback = true\nbilling = \"billed\"\n",
    );
    import_with_overlay(&fixture(), &overlay, &output);
    let cfg = RuntimeConfig::load(&output).unwrap();
    assert!(cfg.catch_all.is_some(), "overlay catch-all missing");
    for spelling in ["overlay-collapsed", "overlay-collapsed-alias"] {
        assert!(
            !cfg.model_names.contains_key(spelling),
            "collapsed overlay spelling stayed servable: {spelling}"
        );
        assert!(
            cfg.catch_all_exclude
                .iter()
                .any(|excluded| excluded == spelling),
            "collapsed overlay spelling can fall through to the catch-all: {spelling}: {:?}",
            cfg.catch_all_exclude
        );
    }
}

#[test]
fn structured_interrupted_step_collapses_without_blocking_other_providers() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let key = dir.path().join("omniroute-key");
    fs::write(&key, "synthetic-omniroute").unwrap();
    let key_ref = format!("file:{}", key.display());
    let mut root = fixture();
    for node in root["providerNodes"].as_array_mut().unwrap() {
        if node["id"] == "ddgw" {
            node["id"] = "node-123".into();
        }
    }
    for connection in root["providerConnections"].as_array_mut().unwrap() {
        if connection["provider"] == "ddgw" {
            connection["provider"] = "node-123".into();
        }
    }
    root["combos"].as_array_mut().unwrap().extend([
        json!({"id":"structured-interrupted","name":"structured-interrupted","models":[{"kind":"model","providerId":"ddgw","model":"gpt-5.4-mini"}]}),
        json!({"id":"other-mini","name":"other-mini","models":[{"kind":"model","providerId":"cfpd","model":"gpt-5.4-mini"}]}),
        json!({"id":"collision-survivor","name":"gpt-5.4-mini","models":["cfpd/moonshotai/kimi-k2.7"]}),
    ]);
    llmux_import::import_with_options(
        &root.to_string(),
        &output,
        None,
        &llmux_import::ImportOptions {
            omniroute_fallback: Some(llmux_import::OmniRouteFallback {
                base_url: "http://127.0.0.1:20128/v1",
                key: &key_ref,
            }),
            ..Default::default()
        },
    )
    .unwrap();
    let cfg = RuntimeConfig::load(&output).unwrap();
    assert!(
        !cfg.model_names.contains_key("structured-interrupted"),
        "structured interrupted step stayed servable"
    );
    assert!(
        cfg.catch_all_exclude
            .iter()
            .any(|excluded| excluded == "structured-interrupted"),
        "structured interrupted combo can fall through: {:?}",
        cfg.catch_all_exclude
    );
    assert!(
        cfg.model_names.contains_key("gpt-5.4-mini"),
        "unrelated surviving combo with the colliding model name was removed"
    );
    assert!(
        cfg.model_names.contains_key("other-mini"),
        "unrelated provider's bare gpt-5.4-mini was globally blocked"
    );
    let other_idx = cfg.model_names["other-mini"];
    assert!(
        cfg.models[other_idx]
            .deployments
            .iter()
            .any(|i| cfg.deployments[i.0 as usize].upstream_model.as_ref() == "gpt-5.4-mini"),
        "unrelated provider leg missing"
    );
}

#[test]
fn mixed_combo_with_interrupted_leg_does_not_fall_back_to_original_combo() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let key = dir.path().join("omniroute-key");
    fs::write(&key, "synthetic-omniroute").unwrap();
    let key_ref = format!("file:{}", key.display());
    let mut root = fixture();
    root["combos"].as_array_mut().unwrap().push(json!({
        "id":"mixed","name":"mixed","models":["cfpd/moonshotai/kimi-k2.6","cfpd/moonshotai/kimi-k2.7"]
    }));
    llmux_import::import_with_options(
        &root.to_string(),
        &output,
        None,
        &llmux_import::ImportOptions {
            omniroute_fallback: Some(llmux_import::OmniRouteFallback {
                base_url: "http://127.0.0.1:20128/v1",
                key: &key_ref,
            }),
            ..Default::default()
        },
    )
    .unwrap();
    let cfg = RuntimeConfig::load(&output).unwrap();
    assert!(
        cfg.model_names.contains_key("mixed"),
        "mixed should survive through its unrelated native leg"
    );
    let mixed_idx = cfg.model_names["mixed"];
    let upstreams: Vec<_> = cfg.models[mixed_idx]
        .deployments
        .iter()
        .map(|i| cfg.deployments[i.0 as usize].upstream_model.to_string())
        .collect();
    assert!(
        upstreams.iter().any(|u| u == "moonshotai/kimi-k2.7"),
        "unrelated native leg missing: {upstreams:?}"
    );
    assert!(
        upstreams.iter().all(|u| u != "mixed"),
        "mixed falls back to the original OmniRoute combo and can reselect the interrupted leg: {upstreams:?}"
    );
    assert!(
        cfg.catch_all_exclude
            .iter()
            .any(|excluded| excluded == "mixed"),
        "mixed can fall through to the original OmniRoute combo: {:?}",
        cfg.catch_all_exclude
    );
}
