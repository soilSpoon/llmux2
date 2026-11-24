use llmux_config::RuntimeConfig;
use llmux_import::ImportOptions;
use llmux_types::Billing;
use serde_json::{json, Value};
use std::{collections::HashMap, fs, process::Command};

const FIXTURE: &str = include_str!("fixtures/export.json");
const DB_FIXTURE: &str = include_str!("fixtures/db-dump.json");

#[test]
fn raw_dump_custom_header_json_loads_as_private_references() {
    let mut root: Value = serde_json::from_str(DB_FIXTURE).unwrap();
    root["provider_nodes"][0]["custom_headers_json"] =
        json!("{\"X-Node\":\"synthetic-header-secret\"}");
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("llmux.toml");
    llmux_import::import(&root.to_string(), &output, None).unwrap();
    let config = RuntimeConfig::load(&output).unwrap();
    let custom = config
        .deployments
        .iter()
        .find(|dep| dep.credential.as_ref() == "upstream-custom")
        .unwrap();
    assert_eq!(custom.headers["x-node"], "synthetic-header-secret");
    assert!(!fs::read_to_string(output)
        .unwrap()
        .contains("synthetic-header-secret"));
}

#[test]
fn emitted_deployments_carry_billing_from_provider_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("llmux.toml");
    llmux_import::import(DB_FIXTURE, &output, None).unwrap();
    let config = RuntimeConfig::load(&output).unwrap();
    // Builtin provider legs are billed; custom provider-node legs bill by
    // default until --free-providers marks them free on evidence.
    let by_credential: HashMap<_, _> = config
        .deployments
        .iter()
        .map(|dep| (dep.credential.as_ref(), dep.billing))
        .collect();
    assert_eq!(by_credential["upstream-fast"], Billing::Billed);
    assert_eq!(by_credential["upstream-slow"], Billing::Billed);
    assert_eq!(by_credential["upstream-custom"], Billing::Billed);
}

#[test]
fn free_providers_file_marks_a_custom_provider_free_on_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("llmux.toml");
    let free = dir.path().join("free-providers.json");
    fs::write(&free, json!([{"provider":"node-test","evidence":"operator-confirmed free plan https://provider.example/pricing"}]).to_string()).unwrap();
    llmux_import::import_with_options(
        DB_FIXTURE,
        &output,
        None,
        &ImportOptions {
            free_providers: Some(&free),
            ..ImportOptions::default()
        },
    )
    .unwrap();
    let config = RuntimeConfig::load(&output).unwrap();
    let by_credential: HashMap<_, _> = config
        .deployments
        .iter()
        .map(|dep| (dep.credential.as_ref(), dep.billing))
        .collect();
    assert_eq!(by_credential["upstream-custom"], Billing::Free);
    assert_eq!(by_credential["upstream-fast"], Billing::Billed);
}

#[test]
fn free_providers_matches_host_and_requires_evidence() {
    let dir = tempfile::tempdir().unwrap();
    // Host matching: the file can name a custom provider by its base-url host.
    let root: Value = serde_json::from_str(DB_FIXTURE).unwrap();
    let node = root["provider_nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["id"].as_str().unwrap() == "node-test")
        .unwrap();
    let host = node["base_url"]
        .as_str()
        .unwrap()
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap()
        .to_owned();
    let free = dir.path().join("free.json");
    fs::write(
        &free,
        json!([{"provider":host,"evidence":"usage API shows $0"}]).to_string(),
    )
    .unwrap();
    let output = dir.path().join("llmux.toml");
    llmux_import::import_with_options(
        &root.to_string(),
        &output,
        None,
        &ImportOptions {
            free_providers: Some(&free),
            ..ImportOptions::default()
        },
    )
    .unwrap();
    let config = RuntimeConfig::load(&output).unwrap();
    assert_eq!(
        config
            .deployments
            .iter()
            .find(|dep| dep.credential.as_ref() == "upstream-custom")
            .unwrap()
            .billing,
        Billing::Free
    );
    // An entry without evidence is rejected rather than silently marking free.
    fs::write(&free, json!([{"provider":host}]).to_string()).unwrap();
    let error = llmux_import::import_with_options(
        &root.to_string(),
        &dir.path().join("bad.toml"),
        None,
        &ImportOptions {
            free_providers: Some(&free),
            ..ImportOptions::default()
        },
    )
    .err()
    .unwrap();
    assert!(error.contains("evidence"), "{error}");
}

#[test]
fn free_providers_model_entry_marks_only_the_listed_model() {
    let dir = tempfile::tempdir().unwrap();
    let mut root: Value = serde_json::from_str(DB_FIXTURE).unwrap();
    // Give node-test a second upstream model so the node is mixed: the entry
    // names only one of them free.
    root["combos"][0]["data"]["models"]
        .as_array_mut()
        .unwrap()
        .insert(
            2,
            json!({
                "kind": "model",
                "providerId": "node-test",
                "model": "node-test/model-2",
                "connectionId": "custom"
            }),
        );
    let free = dir.path().join("free.json");
    fs::write(
        &free,
        json!([{"provider":"node-test","model":"model","evidence":"catalog lists this id at $0"}])
            .to_string(),
    )
    .unwrap();
    let output = dir.path().join("llmux.toml");
    llmux_import::import_with_options(
        &root.to_string(),
        &output,
        None,
        &ImportOptions {
            free_providers: Some(&free),
            ..ImportOptions::default()
        },
    )
    .unwrap();
    let config = RuntimeConfig::load(&output).unwrap();
    let billing: HashMap<_, _> = config
        .deployments
        .iter()
        .map(|dep| (dep.upstream_model.as_ref(), dep.billing))
        .collect();
    assert_eq!(billing["model"], Billing::Free);
    assert_eq!(billing["model-2"], Billing::Billed);

    // An empty model entry is rejected rather than widening to the whole node.
    fs::write(
        &free,
        json!([{"provider":"node-test","model":"","evidence":"x"}]).to_string(),
    )
    .unwrap();
    let error = llmux_import::import_with_options(
        &root.to_string(),
        &dir.path().join("bad.toml"),
        None,
        &ImportOptions {
            free_providers: Some(&free),
            ..ImportOptions::default()
        },
    )
    .err()
    .unwrap();
    assert!(error.contains("model"), "{error}");
}

#[test]
fn free_providers_host_node_level_entry_still_frees_when_id_is_model_scoped() {
    let dir = tempfile::tempdir().unwrap();
    let mut root: Value = serde_json::from_str(DB_FIXTURE).unwrap();
    root["combos"][0]["data"]["models"]
        .as_array_mut()
        .unwrap()
        .insert(
            2,
            json!({
                "kind": "model",
                "providerId": "node-test",
                "model": "node-test/model-2",
                "connectionId": "custom"
            }),
        );
    let host = "example.com";
    // The id-scoped entry lists one model; the host entry marks the whole
    // node. Both keys must be consulted or the host marking is lost.
    let free = dir.path().join("free.json");
    fs::write(
        &free,
        json!([
            {"provider":"node-test","model":"model","evidence":"catalog $0"},
            {"provider":host,"evidence":"operator-confirmed free plan"}
        ])
        .to_string(),
    )
    .unwrap();
    let output = dir.path().join("llmux.toml");
    llmux_import::import_with_options(
        &root.to_string(),
        &output,
        None,
        &ImportOptions {
            free_providers: Some(&free),
            ..ImportOptions::default()
        },
    )
    .unwrap();
    let config = RuntimeConfig::load(&output).unwrap();
    let billing: HashMap<_, _> = config
        .deployments
        .iter()
        .map(|dep| (dep.upstream_model.as_ref(), dep.billing))
        .collect();
    assert_eq!(billing["model"], Billing::Free);
    assert_eq!(billing["model-2"], Billing::Free);
}

#[test]
fn free_providers_matches_ipv6_hosts_bare_and_bracketed() {
    let dir = tempfile::tempdir().unwrap();
    let mut root: Value = serde_json::from_str(DB_FIXTURE).unwrap();
    root["provider_connections"][2]["provider_specific_data"] =
        json!("{\"baseUrl\":\"https://[2001:db8::1]:8443/v1\"}");
    for (index, provider) in ["2001:db8::1", "[2001:db8::1]"].iter().enumerate() {
        let free = dir.path().join(format!("free-v6-{index}.json"));
        fs::write(
            &free,
            json!([{"provider":provider,"evidence":"usage API shows $0"}]).to_string(),
        )
        .unwrap();
        let output = dir.path().join(format!("llmux-{index}.toml"));
        llmux_import::import_with_options(
            &root.to_string(),
            &output,
            None,
            &ImportOptions {
                free_providers: Some(&free),
                ..ImportOptions::default()
            },
        )
        .unwrap();
        let config = RuntimeConfig::load(&output).unwrap();
        assert_eq!(
            config
                .deployments
                .iter()
                .find(|dep| dep.credential.as_ref() == "upstream-custom")
                .unwrap()
                .billing,
            Billing::Free,
            "provider {provider}"
        );
    }
}

#[test]
fn free_legs_emit_before_billed_ones_in_stable_order() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("llmux.toml");
    let free = dir.path().join("free.json");
    fs::write(
        &free,
        json!([{"provider":"node-test","evidence":"operator-confirmed free plan"}]).to_string(),
    )
    .unwrap();
    llmux_import::import_with_options(
        DB_FIXTURE,
        &output,
        None,
        &ImportOptions {
            free_providers: Some(&free),
            ..ImportOptions::default()
        },
    )
    .unwrap();
    let config = RuntimeConfig::load(&output).unwrap();
    let order: Vec<_> = config.models[config.model_names["fallback"]]
        .deployments
        .iter()
        .map(|index| {
            let dep = &config.deployments[index.0 as usize];
            (dep.credential.as_ref(), dep.billing)
        })
        .collect();
    let split = order
        .iter()
        .position(|(_, billing)| *billing == Billing::Billed);
    assert_eq!(split, Some(1), "{order:?}");
    assert_eq!(order[0].0, "upstream-custom");
}

#[test]
fn cli_free_providers_flag_marks_emitted_billing() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("export.json");
    fs::write(&input, DB_FIXTURE).unwrap();
    let free = dir.path().join("free.json");
    fs::write(
        &free,
        json!([{"provider":"node-test","evidence":"operator-confirmed free plan"}]).to_string(),
    )
    .unwrap();
    let output = dir.path().join("llmux.toml");
    let result = Command::new(env!("CARGO_BIN_EXE_llmux-import"))
        .args(["--input"])
        .arg(&input)
        .arg("--output")
        .arg(&output)
        .arg("--free-providers")
        .arg(&free)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let config = RuntimeConfig::load(&output).unwrap();
    assert_eq!(
        config
            .deployments
            .iter()
            .find(|dep| dep.credential.as_ref() == "upstream-custom")
            .unwrap()
            .billing,
        Billing::Free
    );
}

#[test]
fn imports_raw_sqlite_dump_through_shared_mapping() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("llmux.toml");
    let report = llmux_import::import(DB_FIXTURE, &output, None).unwrap();
    assert_eq!(
        (
            report.models,
            report.deployments,
            report.credentials,
            report.client_keys
        ),
        (7, 12, 3, 1)
    );
    let config = RuntimeConfig::load(&output).unwrap();
    assert_eq!(
        config.model_names.get("ignored"),
        config.model_names.get("openai/test")
    );
    let deployments = config.models[config.model_names["fallback"]]
        .deployments
        .iter()
        .map(|index| {
            let deployment = &config.deployments[index.0 as usize];
            (
                deployment.credential.to_string(),
                deployment.upstream_model.to_string(),
                deployment.priority,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        deployments,
        vec![
            ("upstream-fast".into(), "test".into(), 0),
            ("upstream-slow".into(), "test".into(), 1),
            ("upstream-custom".into(), "model".into(), 2),
        ]
    );
    let pinned = &config.models[config.model_names["pinned"]].deployments;
    assert_eq!(
        config.deployments[pinned[0].0 as usize]
            .credential
            .to_string(),
        "upstream-slow"
    );
    assert_eq!(
        config.deployments[2].base_url.to_string(),
        "https://example.com/v1"
    );
    let warnings = report.warnings.join("\n");
    assert!(warnings.contains("unusable): skipped usable: false"));
    assert!(warnings.contains("step 2: skipped usable: false"));
    assert!(warnings.contains("inactive): skipped inactive"));
    let text = fs::read_to_string(output).unwrap();
    for secret in [
        "test-db-slow-secret",
        "test-db-fast-secret",
        "test-db-client-secret",
        "<REDACTED:synthetic:api_key>",
    ] {
        assert!(!text.contains(secret));
        assert!(!warnings.contains(secret));
    }
    assert_eq!(
        fs::read_to_string(dir.path().join("secrets/upstream-custom")).unwrap(),
        "<REDACTED:synthetic:api_key>"
    );
    assert!(!dir.path().join("secrets/upstream-unusable").exists());
    assert!(!dir.path().join("secrets/upstream-inactive").exists());
}

#[test]
fn rejects_malformed_dump_columns_without_logging_values() {
    for (field, value) in [
        ("provider_connections", json!({})),
        ("provider_nodes", json!(false)),
        ("api_keys", json!("sensitive-invalid-data")),
    ] {
        let mut root: Value = serde_json::from_str(DB_FIXTURE).unwrap();
        root[field] = value;
        let dir = tempfile::tempdir().unwrap();
        let error = llmux_import::import(&root.to_string(), &dir.path().join("out.toml"), None)
            .err()
            .unwrap();
        assert!(error.contains("must be an array"));
        assert!(!error.contains("sensitive-invalid-data"));
    }
    for value in [json!(2), json!("0")] {
        let mut root: Value = serde_json::from_str(DB_FIXTURE).unwrap();
        root["provider_connections"][0]["is_active"] = value;
        let dir = tempfile::tempdir().unwrap();
        assert!(
            llmux_import::import(&root.to_string(), &dir.path().join("out.toml"), None).is_err()
        );
    }
}

#[test]
fn dump_json_columns_fail_closed_and_empty_dump_is_rejected() {
    for value in [json!("sensitive-invalid-data"), json!([])] {
        let mut root: Value = serde_json::from_str(DB_FIXTURE).unwrap();
        root["provider_nodes"][0]["custom_headers_json"] = value;
        let dir = tempfile::tempdir().unwrap();
        let error = llmux_import::import(&root.to_string(), &dir.path().join("out.toml"), None)
            .err()
            .unwrap();
        assert_eq!(
            error,
            "provider_nodes.custom_headers_json must contain a JSON object"
        );
        assert!(!dir.path().join("secrets").exists());
    }
    let dir = tempfile::tempdir().unwrap();
    assert!(llmux_import::import(
        r#"{"provider_connections":[],"combos":[]}"#,
        &dir.path().join("out.toml"),
        None
    )
    .is_err());
}

#[test]
fn listen_overrides_the_emitted_server_address() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("llmux.toml");
    llmux_import::import_with_options(
        DB_FIXTURE,
        &output,
        None,
        &llmux_import::ImportOptions {
            listen: Some("127.0.0.1:8090"),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        RuntimeConfig::load(&output)
            .unwrap()
            .server
            .listen
            .to_string(),
        "127.0.0.1:8090"
    );
}

#[test]
fn imports_and_validates_real_config() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("llmux.toml");
    let report = llmux_import::import(FIXTURE, &output, None).unwrap();
    let config = RuntimeConfig::load(&output).unwrap();
    assert_eq!(
        (
            report.models,
            report.deployments,
            report.credentials,
            report.client_keys
        ),
        (15, 26, 5, 1)
    );
    assert_eq!(
        config.model_names.get("legacy-alias"),
        config.model_names.get("openai/gpt-test")
    );
    assert_eq!(config.server.listen.to_string(), "127.0.0.1:8080");
    let deployments = |name: &str| {
        config.models[config.model_names[name]]
            .deployments
            .iter()
            .map(|idx| {
                let d = &config.deployments[idx.0 as usize];
                (
                    d.credential.to_string(),
                    d.upstream_model.to_string(),
                    d.priority,
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        deployments("priority"),
        vec![
            ("upstream-fast-key".into(), "gpt-test".into(), 0),
            ("upstream-slow-key".into(), "gpt-test".into(), 1),
            ("upstream-groq".into(), "org/model".into(), 2),
            ("upstream-custom".into(), "vendor/model".into(), 3),
        ]
    );
    assert!(deployments("rotate").iter().all(|d| d.2 == 0));
    assert_eq!(
        deployments("pinned"),
        vec![("upstream-slow-key".into(), "pin".into(), 0)]
    );
    assert_eq!(deployments("allowed")[0].0, "upstream-fast-key");
    assert_eq!(
        deployments("nested")
            .iter()
            .map(|d| d.2)
            .collect::<Vec<_>>(),
        vec![0, 1, 2, 3]
    );
    assert_eq!(deployments("nested")[0].1, "pin");
    assert!(!config.model_names.contains_key("empty"));
    let text = fs::read_to_string(&output).unwrap();
    let root: Value = serde_json::from_str(FIXTURE).unwrap();
    for c in root["providerConnections"].as_array().unwrap() {
        for field in ["apiKey", "accessToken", "refreshToken"] {
            if let Some(secret) = c[field].as_str() {
                assert!(!text.contains(secret));
            }
        }
    }
    for key in root["apiKeys"].as_array().unwrap() {
        assert!(!text.contains(key["key"].as_str().unwrap()));
    }
    let secrets = fs::read_dir(dir.path().join("secrets"))
        .unwrap()
        .map(|e| {
            let path = e.unwrap().path();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
            fs::read_to_string(path).unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(secrets.len(), 6);
    assert!(!secrets.iter().any(|s| s == "test-restricted-client-secret"));
    assert!(report
        .warnings
        .iter()
        .any(|w| w.contains("client key 2: skipped restricted")));
    for secret in [
        "test-openai-slow-secret",
        "test-openai-fast-secret",
        "test-custom-secret",
        "test-claude-secret",
        "test-groq-secret",
        "test-client-secret",
    ] {
        assert!(secrets.iter().any(|s| s == secret));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(dir.path().join("secrets"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    let warnings = report.warnings.join("\n");
    for expected in [
        "oauth",
        "inactive",
        "encrypted",
        "unknown",
        "cyclic",
        "no deployments",
        "weighted",
        "client key 1",
        "nested routing policy",
    ] {
        assert!(
            warnings.contains(expected),
            "missing {expected}: {warnings}"
        );
    }
    assert!(!warnings.contains("test-oauth-secret"));
}

#[test]
fn fallback_key_empty_and_invalid_inputs() {
    let mut fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    fixture["apiKeys"] = json!([]);
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let report = llmux_import::import(&fixture.to_string(), &output, None).unwrap();
    let value: toml::Value = toml::from_str(&fs::read_to_string(output).unwrap()).unwrap();
    assert_eq!(
        value["server"]["client_keys"][0].as_str(),
        Some("env:LLMUX_CLIENT_KEY")
    );
    assert!(report
        .warnings
        .iter()
        .any(|w| w.contains("LLMUX_CLIENT_KEY")));
    for input in [
        "not JSON",
        "[]",
        "{}",
        r#"{"combos":false}"#,
        r#"{"_meta":{"version":"unsupported"}}"#,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.toml");
        assert!(llmux_import::import(input, &output, None).is_err());
        assert!(!output.exists());
    }
}

#[test]
fn refuses_overwrites_and_invalid_urls() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    fs::write(&output, "keep").unwrap();
    assert!(llmux_import::import(FIXTURE, &output, None).is_err());
    assert_eq!(fs::read_to_string(output).unwrap(), "keep");
    let dir = tempfile::tempdir().unwrap();
    let secrets = dir.path().join("secrets");
    fs::create_dir(&secrets).unwrap();
    fs::write(secrets.join("upstream-slow-key"), "keep-secret").unwrap();
    assert!(llmux_import::import(FIXTURE, &dir.path().join("out.toml"), None).is_err());
    assert_eq!(
        fs::read_to_string(secrets.join("upstream-slow-key")).unwrap(),
        "keep-secret"
    );
    let dir = tempfile::tempdir().unwrap();
    let mut fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    fixture["providerNodes"][0]["baseUrl"] = "https://user:password@example.com/v1".into();
    assert!(
        llmux_import::import(&fixture.to_string(), &dir.path().join("out.toml"), None).is_err()
    );
    assert!(!dir.path().join("out.toml").exists());
}

#[test]
fn identical_secrets_are_reused_on_re_import() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = dir.path().join("secrets");
    llmux_import::import(FIXTURE, &dir.path().join("one.toml"), Some(&secrets)).unwrap();
    // A scheduled re-import regenerates the same export's credentials; an
    // identical existing file is reused rather than aborting.
    llmux_import::import(FIXTURE, &dir.path().join("two.toml"), Some(&secrets)).unwrap();
    let differing = secrets.join("upstream-slow-key");
    fs::write(&differing, "rotated-by-hand").unwrap();
    assert!(llmux_import::import(FIXTURE, &dir.path().join("three.toml"), Some(&secrets)).is_err());
    assert_eq!(fs::read_to_string(&differing).unwrap(), "rotated-by-hand");
}

#[test]
fn cli_reports_without_leaking_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("export.json");
    fs::write(&input, FIXTURE).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_llmux-import"))
        .args(["--input"])
        .arg(input)
        .arg("--output")
        .arg(dir.path().join("out.toml"))
        .arg("--secrets-dir")
        .arg(dir.path().join("private"))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let stderr = String::from_utf8(result.stderr).unwrap();
    assert!(stderr.contains("imported 15 models"));
    assert!(stderr.contains("warning:"));
    assert!(!stderr.contains("-secret"));
    assert!(result.stdout.is_empty());
    let result = Command::new(env!("CARGO_BIN_EXE_llmux-import"))
        .arg("--unknown")
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("usage:"));
}

#[test]
fn header_secret_names_are_stable_across_imports() {
    let mut root: Value = serde_json::from_str(DB_FIXTURE).unwrap();
    root["provider_nodes"][0]["custom_headers_json"] = json!(
        "{\"X-Node\":\"synthetic-one\",\"X-Other\":\"synthetic-two\",\"X-Third\":\"synthetic-three\"}"
    );
    let dir = tempfile::tempdir().unwrap();
    let secrets = dir.path().join("secrets");
    llmux_import::import(
        &root.to_string(),
        &dir.path().join("one.toml"),
        Some(&secrets),
    )
    .unwrap();
    // Header secret filenames derive from connection+header name, so a
    // re-import resolves the same files regardless of map iteration order.
    llmux_import::import(
        &root.to_string(),
        &dir.path().join("two.toml"),
        Some(&secrets),
    )
    .unwrap();
    let names: Vec<String> = fs::read_dir(&secrets)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    for header in ["x-node", "x-other", "x-third"] {
        assert!(
            names.iter().any(|n| n.ends_with(header)),
            "no secret named for {header}: {names:?}"
        );
    }
}

#[test]
fn cli_listen_reaches_the_emitted_config() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("export.json");
    let output = dir.path().join("out.toml");
    fs::write(&input, FIXTURE).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_llmux-import"))
        .args(["--input"])
        .arg(input)
        .arg("--output")
        .arg(&output)
        .args(["--listen", "127.0.0.1:8090"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        RuntimeConfig::load(&output)
            .unwrap()
            .server
            .listen
            .to_string(),
        "127.0.0.1:8090"
    );
}

#[test]
fn skips_malformed_steps_and_unsupported_custom_protocols() {
    let mut root: Value = serde_json::from_str(FIXTURE).unwrap();
    root["modelAliases"] = json!({});
    root["combos"] = json!([
        {"data":"not-json"},
        {"name":"bad-models", "models":{}},
        {"name":" safe ", "models":[
            {"model":"openai/no", "connectionId":123},
            {"model":"openai/no", "allowedConnectionIds":"bad"},
            {"model":"openai/no", "allowedConnectionIds":[null]},
            {"kind":"combo-ref", "comboName":"missing"},
            "openai/*",
            {"model":"openai/yes", "allowedConnectionIds":["fast key","missing"]},
            "local/no"
        ]}
    ]);
    root["providerNodes"][0]["apiType"] = "responses".into();
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let report = llmux_import::import(&root.to_string(), &output, None).unwrap();
    let config = RuntimeConfig::load(output).unwrap();
    assert_eq!(config.models.len(), 2);
    assert_eq!(config.models[0].name.as_ref(), "safe");
    assert_eq!(config.deployments.len(), 3);
    assert_eq!(config.models[0].deployments.len(), 1);
    assert!(!config.model_names.contains_key("openai/no"));
    assert_eq!(config.deployments[0].upstream_model.as_ref(), "yes");
    let warnings = report.warnings.join("\n");
    for expected in [
        "invalid data JSON",
        "malformed model steps",
        "malformed connection restriction",
        "missing reference",
        "wildcard",
        "unavailable allowed connection",
        "trimmed whitespace",
        "custom protocol settings",
    ] {
        assert!(warnings.contains(expected), "missing {expected}");
    }
}

#[test]
fn provider_table_uses_adapter_base_urls_and_unique_ids() {
    let provider_table: Vec<_> = llmux_import::PROVIDERS
        .iter()
        .map(|&(id, kind, url)| match id {
            "deepai" => (id, kind, url, "text-generator"),
            "deepai-code" => (id, kind, "http://127.0.0.1:8790/deepai/v1", "org/model"),
            _ => (id, kind, url, "org/model"),
        })
        .collect();
    let root = json!({
        "providerConnections": provider_table.iter().enumerate().map(|(i,(id,_,url,_))| {
            let mut connection = json!({"id":format!("key {i}"),"provider":id,"authType":"apikey",
                "apiKey":format!("test-key-{i}")});
            if *id == "deepai-code" {
                connection["providerSpecificData"] = json!({"baseUrl":url});
            }
            connection
        }).collect::<Vec<_>>(),
        "combos":[{"name":"all / models", "models":provider_table.iter()
            .map(|(id,_,_,model)|format!("{id}/{model}")).collect::<Vec<_>>()},
            {"name":"all---models", "models":["openai/other"]}],
        "apiKeys":[{"key":"test-gateway"}]
    });
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let catalog = dir.path().join("kilo-models.json");
    fs::write(
        &catalog,
        r#"[{"id":"org/model","pricing":{"prompt":"0","completion":"0"}}]"#,
    )
    .unwrap();
    llmux_import::import_with_options(
        &root.to_string(),
        &output,
        None,
        &llmux_import::ImportOptions {
            kilo_free_models: Some(&catalog),
            ..Default::default()
        },
    )
    .unwrap();
    let config = RuntimeConfig::load(output).unwrap();
    assert_eq!(config.models[0].deployments.len(), provider_table.len());
    let mut emitted: Vec<_> = config.models[0]
        .deployments
        .iter()
        .map(|index| {
            let dep = &config.deployments[index.0 as usize];
            (
                dep.provider.0.as_ref(),
                dep.base_url.to_string().trim_end_matches('/').to_owned(),
                dep.upstream_model.as_ref(),
            )
        })
        .collect();
    let mut expected: Vec<_> = provider_table
        .iter()
        .map(|&(_, kind, url, model)| (kind, url.to_owned(), model))
        .collect();
    emitted.sort();
    expected.sort();
    assert_eq!(emitted, expected);
    let ids = config
        .deployments
        .iter()
        .map(|d| d.id.as_ref())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(ids.len(), config.deployments.len());
    assert_eq!(config.models[0].name.as_ref(), "all / models");
}

#[cfg(unix)]
#[test]
fn refuses_symlink_secret_directories() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    fs::create_dir(&target).unwrap();
    std::os::unix::fs::symlink(target, dir.path().join("secrets")).unwrap();
    assert!(llmux_import::import(FIXTURE, &dir.path().join("out.toml"), None).is_err());
}

#[test]
fn restricted_client_policies_fail_closed() {
    for policy in [
        json!({"allowedModels":["one"]}),
        json!({"blockedModels":["one"]}),
        json!({"modelAccessMode":"restricted"}),
        json!({"allowedCombos":["one"]}),
        json!({"allowedConnections":["one"]}),
        json!({"allowedQuotas":["one"]}),
        json!({"ipAllowlist":["127.0.0.1"]}),
        json!({"scopes":["read"]}),
        json!({"allowedEndpoints":["chat"]}),
        json!({"accessSchedule":{}}),
        json!({"rateLimits":[{}]}),
        json!({"maxRequestsPerDay":1}),
        json!({"maxRequestsPerMinute":1}),
        json!({"maxSessions":1}),
        json!({"throttleDelayMs":1}),
        json!({"expiresAt":"2099-01-01"}),
        json!({"revokedAt":"2026-01-01"}),
        json!({"isBanned":true}),
        json!({"usageLimitEnabled":true}),
        json!({"dailyUsageLimitUsd":1}),
        json!({"weeklyUsageLimitUsd":1}),
        json!({"fiveHourUsageLimitUsd":1}),
        json!({"disableNonPublicModels":true}),
        json!({"catalogScope":"combos"}),
        json!({"forcedModel":"one"}),
        json!({"allowAutoCombos":false}),
        json!({"allowCcDiscoveryAliases":false}),
        json!({"allowedModels":"malformed"}),
        json!({"allowed_models":["one"]}),
        json!({"is_active":0}),
        json!({"source":"sso"}),
    ] {
        let mut root: Value = serde_json::from_str(FIXTURE).unwrap();
        let mut key = policy;
        key["key"] = "restricted-secret".into();
        root["apiKeys"] = json!([key]);
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.toml");
        let report = llmux_import::import(&root.to_string(), &output, None).unwrap();
        assert_eq!(report.client_keys, 0, "policy: {key}");
        assert!(report
            .warnings
            .iter()
            .any(|w| w.contains("skipped restricted")));
        assert!(!dir.path().join("secrets/client-0").exists());
        assert!(fs::read_to_string(output)
            .unwrap()
            .contains("env:LLMUX_CLIENT_KEY"));
    }
    let mut root: Value = serde_json::from_str(FIXTURE).unwrap();
    root["apiKeys"] = json!([{"key":"unrestricted-secret", "isActive":true,
        "allowedModels":[], "blockedModels":[], "allowedCombos":[], "allowedConnections":[],
        "allowedQuotas":[], "scopes":[], "ipAllowlist":[], "allowedEndpoints":[],
        "modelAccessMode":"all", "catalogScope":"all", "maxSessions":0,
        "expiresAt":null, "isBanned":false, "usageLimitEnabled":0,
        "allowAutoCombos":true, "allowCcDiscoveryAliases":true}]);
    let dir = tempfile::tempdir().unwrap();
    let report =
        llmux_import::import(&root.to_string(), &dir.path().join("out.toml"), None).unwrap();
    assert_eq!(report.client_keys, 1);
}

#[test]
fn shared_empty_combo_diamonds_have_bounded_work() {
    let mut root: Value = serde_json::from_str(FIXTURE).unwrap();
    root["modelAliases"] = json!({});
    let mut combos = vec![json!({"name":"usable", "models":["openai/ok"]})];
    for i in 0..31 {
        let child = if i == 30 {
            "unknown/empty".to_owned()
        } else {
            format!("diamond-{}", i + 1)
        };
        combos.push(json!({"name":format!("diamond-{i}"), "models":[child,child]}));
    }
    root["combos"] = combos.into();
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let started = std::time::Instant::now();
    let report = llmux_import::import(&root.to_string(), &output, None).unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert_eq!(report.models, 2);
    assert!(report.warnings.len() < 70_000);
    assert_eq!(
        report
            .warnings
            .iter()
            .filter(|w| w.contains("combo diamond-0: stopped expansion at 1024-step work budget"))
            .count(),
        1
    );
    assert!(RuntimeConfig::load(output).is_ok());
}

#[test]
fn overlay_merges_site_settings_into_the_emitted_config() {
    // SAFETY: single-threaded test binary; no other test reads OVERLAY_KEY.
    unsafe { std::env::set_var("OVERLAY_KEY", "synthetic") };
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("llmux.toml");
    let overlay = dir.path().join("site.toml");
    fs::write(
        &overlay,
        "[server]\nrequest_log = \"/var/log/llmux/requests.jsonl\"\n\
         [routing]\nmax_attempts = 6\n\
         [[credentials]]\nid = \"overlay-cred\"\nkind = \"api_key\"\nprovider = \"openai_compat\"\n\
         key = \"env:OVERLAY_KEY\"\n\
         [[models]]\nname = \"swe-2\"\n[[models.deployments]]\nid = \"overlay-swe2\"\ncredential = \"overlay-cred\"\n\
         upstream_model = \"swe-2\"\nbase_url = \"https://overlay.example.com/v1\"\n",
    )
    .unwrap();
    let options = llmux_import::ImportOptions {
        overlay: Some(&overlay),
        ..Default::default()
    };
    llmux_import::import_with_options(FIXTURE, &output, None, &options).unwrap();
    let config = RuntimeConfig::load(&output).unwrap();
    assert_eq!(config.routing.max_attempts, 6);
    assert_eq!(
        config
            .server
            .request_log
            .as_deref()
            .unwrap()
            .to_str()
            .unwrap(),
        "/var/log/llmux/requests.jsonl"
    );
    assert!(config.models.iter().any(|m| m.name.as_ref() == "swe-2"));
}

#[test]
fn overlay_expires_at_stays_a_bare_toml_datetime() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("llmux.toml");
    let overlay = dir.path().join("site.toml");
    fs::write(
        &overlay,
        "[[credentials]]\nid = \"overlay-cred\"\nkind = \"api_key\"\nprovider = \"openai_compat\"\n\
         key = \"env:OVERLAY_KEY\"\n\
         [[models]]\nname = \"expiring\"\n[[models.deployments]]\nid = \"overlay-expiring\"\n\
         credential = \"overlay-cred\"\nupstream_model = \"expiring\"\n\
         base_url = \"https://overlay.example.com/v1\"\nexpires_at = 2030-01-01T00:00:00Z\n",
    )
    .unwrap();
    unsafe { std::env::set_var("OVERLAY_KEY", "synthetic") };
    llmux_import::import_with_options(
        DB_FIXTURE,
        &output,
        None,
        &ImportOptions {
            overlay: Some(&overlay),
            ..ImportOptions::default()
        },
    )
    .unwrap();
    // The emitted field must be a bare TOML datetime — a quoted string fails
    // llmux-config's `expires_at` parse entirely.
    assert!(fs::read_to_string(&output)
        .unwrap()
        .contains("expires_at = 2030-01-01T00:00:00Z"));
    let config = RuntimeConfig::load(&output).unwrap();
    assert!(config
        .deployments
        .iter()
        .find(|dep| dep.id.as_ref() == "overlay-expiring")
        .unwrap()
        .expires_at
        .is_some());
}

#[test]
fn overlay_header_named_like_a_datetime_field_stays_a_string() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("llmux.toml");
    let overlay = dir.path().join("site.toml");
    fs::write(
        &overlay,
        "[[credentials]]\nid = \"overlay-cred\"\nkind = \"api_key\"\nprovider = \"openai_compat\"\n\
         key = \"env:OVERLAY_KEY\"\n\
         [[models]]\nname = \"with-header\"\n[[models.deployments]]\nid = \"overlay-header\"\n\
         credential = \"overlay-cred\"\nupstream_model = \"h\"\n\
         base_url = \"https://overlay.example.com/v1\"\n\
         headers = { expires_at = \"2030-01-01T00:00:00Z\" }\n",
    )
    .unwrap();
    unsafe { std::env::set_var("OVERLAY_KEY", "synthetic") };
    llmux_import::import_with_options(
        DB_FIXTURE,
        &output,
        None,
        &ImportOptions {
            overlay: Some(&overlay),
            ..ImportOptions::default()
        },
    )
    .unwrap();
    // The header is a user string inside a free-form map: it must stay
    // quoted. A bare datetime breaks HashMap<String, String> on load.
    assert!(fs::read_to_string(&output)
        .unwrap()
        .contains("expires_at = \"2030-01-01T00:00:00Z\""));
    RuntimeConfig::load(&output).unwrap();
}

#[test]
fn overlay_disabled_marks_an_emitted_credential_disabled() {
    unsafe { std::env::set_var("OVERLAY_KEY", "synthetic") };
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("llmux.toml");
    let overlay = dir.path().join("site.toml");
    // A site decision: upstream-fast is parked but stays configured — the
    // overlay row names an emitted credential id and only adds the flag.
    fs::write(
        &overlay,
        "[[credentials]]\nid = \"upstream-fast\"\ndisabled = true\n",
    )
    .unwrap();
    llmux_import::import_with_options(
        DB_FIXTURE,
        &output,
        None,
        &ImportOptions {
            overlay: Some(&overlay),
            ..ImportOptions::default()
        },
    )
    .unwrap();
    let emitted = fs::read_to_string(&output).unwrap();
    assert_eq!(emitted.matches("id = \"upstream-fast\"").count(), 1);
    let config = RuntimeConfig::load(&output).unwrap();
    assert!(config.credential_disabled.contains("upstream-fast"));
    // An unknown id still appends as a new row, and ids are unique either way.
    let overlay_extra = dir.path().join("site-extra.toml");
    fs::write(
        &overlay_extra,
        "[[credentials]]\nid = \"upstream-fast\"\ndisabled = true\n\
         [[credentials]]\nid = \"overlay-only\"\nkind = \"api_key\"\n\
         provider = \"openai_compat\"\nkey = \"env:OVERLAY_KEY\"\ndisabled = true\n",
    )
    .unwrap();
    let output_extra = dir.path().join("llmux-extra.toml");
    llmux_import::import_with_options(
        DB_FIXTURE,
        &output_extra,
        None,
        &ImportOptions {
            overlay: Some(&overlay_extra),
            ..ImportOptions::default()
        },
    )
    .unwrap();
    let config = RuntimeConfig::load(&output_extra).unwrap();
    assert!(config.credential_disabled.contains("upstream-fast"));
    assert!(config.credential_disabled.contains("overlay-only"));
}

#[test]
fn cli_overlay_reaches_the_emitted_config() {
    unsafe { std::env::set_var("OVERLAY_KEY", "synthetic") };
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("export.json");
    let output = dir.path().join("out.toml");
    let overlay = dir.path().join("site.toml");
    fs::write(&input, FIXTURE).unwrap();
    fs::write(&overlay, "[routing]\nmax_attempts = 6\n").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_llmux-import"))
        .args(["--input"])
        .arg(input)
        .arg("--output")
        .arg(&output)
        .args(["--overlay"])
        .arg(&overlay)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        RuntimeConfig::load(&output).unwrap().routing.max_attempts,
        6
    );
}

#[test]
fn exclude_models_drops_overlay_models_too() {
    unsafe { std::env::set_var("OVERLAY_KEY", "synthetic") };
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let overlay = dir.path().join("site.toml");
    let dead = dir.path().join("dead.json");
    fs::write(
        &overlay,
        "[[credentials]]\nid = \"overlay-cred\"\nkind = \"api_key\"\nprovider = \"openai_compat\"\n\
         key = \"env:OVERLAY_KEY\"\n\
         [[models]]\nname = \"swe-2\"\n[[models.deployments]]\nid = \"overlay-swe2\"\ncredential = \"overlay-cred\"\n\
         upstream_model = \"swe-2\"\nbase_url = \"https://overlay.example.com/v1\"\n",
    )
    .unwrap();
    fs::write(&dead, "[\"swe-2\"]").unwrap();
    llmux_import::import_with_options(
        FIXTURE,
        &output,
        None,
        &llmux_import::ImportOptions {
            overlay: Some(&overlay),
            exclude_models: Some(&dead),
            ..Default::default()
        },
    )
    .unwrap();
    let config = RuntimeConfig::load(&output).unwrap();
    assert!(!config.models.iter().any(|m| m.name.as_ref() == "swe-2"));
}

#[test]
fn overlay_client_keys_and_allowed_keys_validate_without_export_keys() {
    unsafe { std::env::set_var("OVERLAY_KEY", "synthetic") };
    unsafe { std::env::set_var("OVERLAY_GATE", "synthetic") };
    let mut fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    fixture["apiKeys"] = json!([]);
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let overlay = dir.path().join("site.toml");
    fs::write(
        &overlay,
        "[server]\nclient_keys = [\"env:OVERLAY_GATE\"]\n\
         [[credentials]]\nid = \"overlay-cred\"\nkind = \"api_key\"\nprovider = \"openai_compat\"\n\
         key = \"env:OVERLAY_KEY\"\n\
         [[models]]\nname = \"swe-2\"\nallowed_keys = [\"env:OVERLAY_GATE\"]\n[[models.deployments]]\n\
         id = \"overlay-swe2\"\ncredential = \"overlay-cred\"\n\
         upstream_model = \"swe-2\"\nbase_url = \"https://overlay.example.com/v1\"\n",
    )
    .unwrap();
    llmux_import::import_with_options(
        &fixture.to_string(),
        &output,
        None,
        &llmux_import::ImportOptions {
            overlay: Some(&overlay),
            ..Default::default()
        },
    )
    .unwrap();
    let config = RuntimeConfig::load(&output).unwrap();
    // A site-supplied key list replaces the emitted placeholder: the stale
    // env:LLMUX_CLIENT_KEY reference must not survive into the deployed config.
    assert_eq!(config.server.client_keys.len(), 1);
    assert!(config.models.iter().any(|m| m.name.as_ref() == "swe-2"));
}

#[test]
fn overlay_listen_must_match_listen() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let overlay = dir.path().join("site.toml");
    fs::write(&overlay, "[server]\nlisten = \"127.0.0.1:8099\"\n").unwrap();
    let result = llmux_import::import_with_options(
        FIXTURE,
        &output,
        None,
        &llmux_import::ImportOptions {
            listen: Some("127.0.0.1:8090"),
            overlay: Some(&overlay),
            ..Default::default()
        },
    );
    let error = match result {
        Ok(_) => panic!("expected a listen conflict error"),
        Err(error) => error,
    };
    assert!(error.contains("listen"), "{error}");
}
