use llmux_config::RuntimeConfig;
use serde_json::{json, Value};
use std::{fs, process::Command};

const FIXTURE: &str = include_str!("fixtures/export.json");

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
        (7, 16, 5, 1)
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
        "legacy-alias",
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
    assert!(stderr.contains("imported 7 models"));
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
fn skips_malformed_steps_and_unsupported_custom_protocols() {
    let mut root: Value = serde_json::from_str(FIXTURE).unwrap();
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
    assert_eq!(config.models.len(), 1);
    assert_eq!(config.models[0].name.as_ref(), "safe");
    assert_eq!(config.deployments.len(), 1);
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
    let root = json!({
        "providerConnections": llmux_import::PROVIDERS.iter().enumerate().map(|(i,(id,_,_))| json!({"id":format!("key {i}"),"provider":id,"authType":"apikey","apiKey":format!("test-key-{i}")})).collect::<Vec<_>>(),
        "combos":[{"name":"all / models", "models":llmux_import::PROVIDERS.iter().map(|(id,_,_)|format!("{id}/org/model")).collect::<Vec<_>>()}, {"name":"all---models", "models":["openai/other"]}],
        "apiKeys":[{"key":"test-gateway"}]
    });
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    llmux_import::import(&root.to_string(), &output, None).unwrap();
    let config = RuntimeConfig::load(output).unwrap();
    for (dep, (_, kind, url)) in config.deployments.iter().zip(llmux_import::PROVIDERS) {
        assert_eq!(dep.provider.0.as_ref(), *kind);
        assert_eq!(dep.base_url.to_string().trim_end_matches('/'), *url);
        assert_eq!(dep.upstream_model.as_ref(), "org/model");
    }
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
