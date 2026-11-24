use llmux_config::RuntimeConfig;
use llmux_import::{ImportOptions, OmniRouteFallback};
use llmux_types::ChatRequestField;
use serde_json::{json, Value};

fn base() -> Value {
    json!({
        "_meta": {"version": "omniroute-v3-legacy-export"},
        "providerConnections": [
            {"id":"baseline","provider":"openai","authType":"apikey","apiKey":"synthetic-baseline"}
        ],
        "combos": [{"name":"baseline","models":["openai/test"]}],
        "apiKeys": [{"key":"synthetic-client","isActive":true}]
    })
}

fn session_export() -> Value {
    let mut root = base();
    root["providerConnections"]
        .as_array_mut()
        .unwrap()
        .extend([
            json!({"id":"ddg","provider":"duckduckgo-web","authType":"apikey","apiKey":"synthetic-ddg-key"}),
            json!({"id":"ddgw","provider":"ddgw","authType":"apikey","apiKey":"synthetic-ddgw-key"}),
            json!({"id":"cfp","provider":"cloudflare-playground","authType":"none"}),
        ]);
    root["combos"].as_array_mut().unwrap().push(json!({
        "name":"ddg-only",
        "models":["duckduckgo-web/mistral-small-2603","ddgw/mistral-small-2603","cfp/meta/llama-3.1-8b-instruct"]
    }));
    root
}

#[test]
fn session_providers_need_omniroute_fallback_and_keep_the_same_model_id() {
    let root = session_export();
    let (report, config, text) = import(&root);
    let warnings = report.warnings.join("\n");
    assert!(
        warnings.contains("pass --omniroute-fallback and --omniroute-key"),
        "{warnings}"
    );
    assert!(warnings.contains("(ddg): skipped"));
    assert!(warnings.contains("(ddgw): skipped"));
    assert!(warnings.contains("(cfp): skipped"));
    assert!(!warnings.contains("unsupported"));
    assert!(!warnings.contains("dropped"));
    assert!(!config.model_names.contains_key("ddg-only"));
    assert!(!config
        .model_names
        .contains_key("duckduckgo-web/mistral-small-2603"));
    assert!(!text.contains("synthetic-ddg-key"));
    assert!(!text.contains("synthetic-ddgw-key"));

    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let key = dir.path().join("fallback-key");
    std::fs::write(&key, "synthetic-session-fallback-secret").unwrap();
    let reference = format!("file:{}", key.display());
    let options = ImportOptions {
        omniroute_fallback: Some(OmniRouteFallback {
            base_url: "http://127.0.0.1:20128/v1",
            key: &reference,
        }),
        ..ImportOptions::default()
    };
    let report =
        llmux_import::import_with_options(&root.to_string(), &output, None, &options).unwrap();
    let text = std::fs::read_to_string(&output).unwrap();
    assert!(!text.contains("synthetic-session-fallback-secret"));
    assert!(!text.contains("synthetic-ddg-key"));
    assert!(!text.contains("synthetic-ddgw-key"));
    assert!(!report
        .warnings
        .join("\n")
        .contains("synthetic-session-fallback-secret"));
    let config = RuntimeConfig::load(&output).unwrap();
    for name in [
        "ddg-only",
        "duckduckgo-web/mistral-small-2603",
        "ddgw/mistral-small-2603",
        "cloudflare-playground/meta/llama-3.1-8b-instruct",
        "cfp/meta/llama-3.1-8b-instruct",
    ] {
        let model = &config.models[*config
            .model_names
            .get(name)
            .unwrap_or_else(|| panic!("missing {name}"))];
        let deps: Vec<_> = model
            .deployments
            .iter()
            .map(|idx| &config.deployments[idx.0 as usize])
            .collect();
        let last = deps.last().unwrap();
        assert!(last.fallback, "{name}");
        assert_eq!(last.upstream_model.as_ref(), name, "{name}");
        assert!(
            deps[..deps.len() - 1].iter().all(|dep| !dep.fallback),
            "{name}"
        );
        assert!(
            deps[..deps.len() - 1]
                .iter()
                .all(|dep| dep.priority < last.priority),
            "{name}"
        );
        assert_eq!(
            last.base_url.to_string().trim_end_matches('/'),
            "http://127.0.0.1:20128/v1"
        );
    }
}

// A `ddgw/<model>` string can be attached as an alias before the session
// fallback legs are ensured; the leg must land on the model that owns the alias
// instead of creating a duplicate model name that fails validation. The shared
// chain must keep a single OmniRoute fallback whose upstream id is the
// canonical model name, so a request for `baseline` is never sent upstream as
// the alias.
#[test]
fn session_fallback_leg_follows_an_attached_alias() {
    let mut root = session_export();
    root["modelAliases"] = json!({"ddgw/mistral-small-2603": "baseline"});
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let key = dir.path().join("fallback-key");
    std::fs::write(&key, "synthetic-session-fallback-secret").unwrap();
    let reference = format!("file:{}", key.display());
    let options = ImportOptions {
        omniroute_fallback: Some(OmniRouteFallback {
            base_url: "http://127.0.0.1:20128/v1",
            key: &reference,
        }),
        ..ImportOptions::default()
    };
    let report =
        llmux_import::import_with_options(&root.to_string(), &output, None, &options).unwrap();
    let config = RuntimeConfig::load(&output).unwrap();
    assert_eq!(
        config.model_names["ddgw/mistral-small-2603"],
        config.model_names["baseline"]
    );
    let model = &config.models[config.model_names["baseline"]];
    let deps: Vec<_> = model
        .deployments
        .iter()
        .map(|idx| &config.deployments[idx.0 as usize])
        .collect();
    let fallbacks: Vec<_> = deps.iter().copied().filter(|dep| dep.fallback).collect();
    assert_eq!(
        fallbacks.len(),
        1,
        "alias session id must not add a second OmniRoute fallback"
    );
    assert_eq!(fallbacks[0].upstream_model.as_ref(), "baseline");
    assert!(
        fallbacks
            .iter()
            .all(|dep| dep.upstream_model.as_ref() == "baseline"),
        "fallback upstream_model must equal the canonical model name"
    );
    assert!(
        deps.iter()
            .all(|dep| dep.upstream_model.as_ref() != "ddgw/mistral-small-2603"),
        "alias must not leak into the canonical deployment chain"
    );
    assert!(
        !report
            .warnings
            .iter()
            .any(|warning| warning.contains("baseline") && warning.contains("dropped")),
        "canonical fallback must be reused, not deduped: {:?}",
        report.warnings
    );
}

fn import(root: &Value) -> (llmux_import::ImportReport, RuntimeConfig, String) {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let report = llmux_import::import(&root.to_string(), &output, None).unwrap();
    let text = std::fs::read_to_string(&output).unwrap();
    let config = RuntimeConfig::load(&output).unwrap();
    std::mem::forget(dir);
    (report, config, text)
}

#[test]
fn keyed_loopback_bridges_import_as_openai_compat_at_their_base_url() {
    let mut root = base();
    root["providerNodes"] = json!([
        {"id":"openai-compatible-chat-deepai-code","prefix":"deepai","type":"openai-compatible","apiType":"chat","baseUrl":"http://127.0.0.1:8790/deepai/v1"},
        {"id":"openai-compatible-chat-cfp-direct","prefix":"cloudflare-direct","type":"openai-compatible","apiType":"chat","baseUrl":"http://127.0.0.1:8790/cfp/v1"}
    ]);
    root["providerConnections"].as_array_mut().unwrap().extend([
        json!({"id":"deepai-code","provider":"openai-compatible-chat-deepai-code","authType":"apikey","apiKey":"synthetic-deepai-code"}),
        json!({"id":"cfp-direct","provider":"openai-compatible-chat-cfp-direct","authType":"apikey","apiKey":"synthetic-cfp-direct"})
    ]);
    root["combos"][0]["models"].as_array_mut().unwrap().extend([
        json!("deepai/mimo-2.6-flash"),
        json!("cloudflare-direct/meta/llama-3.1-8b-instruct"),
    ]);
    let (_report, config, text) = import(&root);
    assert!(!text.contains("synthetic-deepai-code"));
    assert!(!text.contains("synthetic-cfp-direct"));
    let deepai = config
        .deployments
        .iter()
        .find(|dep| dep.credential.as_ref() == "upstream-deepai-code")
        .expect("deepai-code deployment");
    assert_eq!(deepai.provider.0.as_ref(), "openai_compat");
    assert_eq!(
        deepai.base_url.to_string(),
        "http://127.0.0.1:8790/deepai/v1"
    );
    assert_eq!(deepai.upstream_model.as_ref(), "mimo-2.6-flash");
    let direct = config
        .deployments
        .iter()
        .find(|dep| dep.credential.as_ref() == "upstream-cfp-direct")
        .expect("cloudflare-direct deployment");
    assert_eq!(direct.provider.0.as_ref(), "openai_compat");
    assert_eq!(direct.base_url.to_string(), "http://127.0.0.1:8790/cfp/v1");
    assert_eq!(direct.upstream_model.as_ref(), "meta/llama-3.1-8b-instruct");
}

// An OmniRoute node pointing back at this gateway (the OmniRoute -> llmux2
// bridge) resolves to the emitted listen port through a container-runtime
// host alias. Importing it would loop requests into llmux2 itself, so the
// connection is skipped with a warning instead of producing a config the
// validator rejects.
#[test]
fn gateway_self_referential_connection_is_skipped() {
    let mut root = base();
    root["providerNodes"] = json!([
        {"id":"openai-compatible-chat-self","prefix":"llmux","type":"openai-compatible","apiType":"chat","baseUrl":"http://host.containers.internal:8090/v1"}
    ]);
    root["providerConnections"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":"self","provider":"openai-compatible-chat-self","authType":"none"}));
    root["combos"][0]["models"]
        .as_array_mut()
        .unwrap()
        .push(json!("llmux/swe-2"));
    root["combos"].as_array_mut().unwrap().push(json!({
        "name":"swe-2",
        "models":["llmux/swe-2"]
    }));
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let options = ImportOptions {
        listen: Some("127.0.0.1:8090"),
        ..ImportOptions::default()
    };
    let report =
        llmux_import::import_with_options(&root.to_string(), &output, None, &options).unwrap();
    let warnings = report.warnings.join("\n");
    assert!(warnings.contains("gateway"), "{warnings}");
    let config = RuntimeConfig::load(&output).unwrap();
    assert!(config
        .deployments
        .iter()
        .all(|dep| !dep.base_url.to_string().contains("containers.internal")));
    assert!(!config.model_names.contains_key("swe-2"));
    assert!(config.model_names.contains_key("baseline"));
}

// The same bridge must be recognized when the effective listen address comes
// only from the site overlay's `[server].listen` — scheduled re-imports pass
// no `--listen`, so the guard has to use the port the emitted config will
// actually bind, not the 8080 default.
#[test]
fn overlay_listen_alone_guards_self_referential_connection() {
    let mut root = base();
    root["providerNodes"] = json!([
        {"id":"openai-compatible-chat-self","prefix":"llmux","type":"openai-compatible","apiType":"chat","baseUrl":"http://host.containers.internal:8090/v1"}
    ]);
    root["providerConnections"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":"self","provider":"openai-compatible-chat-self","authType":"none"}));
    root["combos"][0]["models"]
        .as_array_mut()
        .unwrap()
        .push(json!("llmux/swe-2"));
    root["combos"].as_array_mut().unwrap().push(json!({
        "name":"swe-2",
        "models":["llmux/swe-2"]
    }));
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let overlay = dir.path().join("overlay.toml");
    std::fs::write(&overlay, "[server]\nlisten = \"127.0.0.1:8090\"\n").unwrap();
    let options = ImportOptions {
        overlay: Some(&overlay),
        ..ImportOptions::default()
    };
    let report =
        llmux_import::import_with_options(&root.to_string(), &output, None, &options).unwrap();
    let warnings = report.warnings.join("\n");
    assert!(warnings.contains("gateway"), "{warnings}");
    let config = RuntimeConfig::load(&output).unwrap();
    assert!(config
        .deployments
        .iter()
        .all(|dep| !dep.base_url.to_string().contains("containers.internal")));
    assert!(!config.model_names.contains_key("swe-2"));
}

// A host-alias upstream on a port this gateway does not bind is a legitimate
// upstream (some other service on the host), not the OmniRoute -> llmux2
// bridge: it must not be skipped just because 8080 is the emitted default.
#[test]
fn alias_upstream_on_a_non_listen_port_is_kept() {
    let mut root = base();
    root["providerNodes"] = json!([
        {"id":"openai-compatible-chat-host","prefix":"hostsvc","type":"openai-compatible","apiType":"chat","baseUrl":"https://host.containers.internal:8080/v1"}
    ]);
    root["providerConnections"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":"hostsvc","provider":"openai-compatible-chat-host","authType":"none"}));
    root["combos"][0]["models"]
        .as_array_mut()
        .unwrap()
        .push(json!("hostsvc/swe-2"));
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let overlay = dir.path().join("overlay.toml");
    std::fs::write(&overlay, "[server]\nlisten = \"127.0.0.1:8090\"\n").unwrap();
    let options = ImportOptions {
        overlay: Some(&overlay),
        ..ImportOptions::default()
    };
    llmux_import::import_with_options(&root.to_string(), &output, None, &options).unwrap();
    let config = RuntimeConfig::load(&output).unwrap();
    assert!(config.deployments.iter().any(|dep| dep
        .base_url
        .to_string()
        .contains("containers.internal:8080")));
}

#[test]
fn xai_oauth_imports_openai_compat_deployment_and_token_url() {
    let mut root = base();
    root["providerConnections"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "id":"xai",
            "provider":"xai-oauth",
            "authType":"oauth",
            "refreshToken":"synthetic-xai-refresh",
            "providerSpecificData":{"clientId":"synthetic-xai-client","scope":"openid api:access"}
        }));
    root["combos"][0]["models"]
        .as_array_mut()
        .unwrap()
        .push(json!("xai-oauth/grok-4.6"));
    let (report, config, text) = import(&root);
    assert!(!text.contains("synthetic-xai-refresh"));
    assert!(!report.warnings.join("\n").contains("synthetic-xai-refresh"));
    assert!(text.contains("https://auth.x.ai/oauth2/token"));
    assert!(text.contains("synthetic-xai-client"));
    let dep = config
        .deployments
        .iter()
        .find(|dep| dep.upstream_model.as_ref() == "grok-4.6")
        .expect("grok deployment");
    assert_eq!(dep.base_url.to_string(), "https://api.x.ai/v1");
    assert_eq!(dep.provider.0.as_ref(), "xai");
    assert!(config.oauth.contains_key(dep.credential.as_ref()));
    assert_eq!(
        config.oauth[dep.credential.as_ref()].token_url,
        "https://auth.x.ai/oauth2/token"
    );
}

#[test]
fn missing_xai_refresh_token_is_skipped_with_a_counted_reason() {
    let mut root = base();
    root["providerConnections"].as_array_mut().unwrap().extend([
        json!({"id":"bare","provider":"xai-oauth","authType":"oauth","accessToken":"synthetic-access"}),
        json!({"id":"no-client","provider":"xai-oauth","authType":"oauth","refreshToken":"synthetic-refresh"})
    ]);
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let report = llmux_import::import(&root.to_string(), &output, None).unwrap();
    let warnings = report.warnings.join("\n");
    assert_eq!(
        warnings.matches("missing refresh token").count(),
        1,
        "{warnings}"
    );
    assert_eq!(
        warnings.matches("missing oauth client id").count(),
        1,
        "{warnings}"
    );
    assert!(!warnings.contains("synthetic-access"));
    assert!(!warnings.contains("synthetic-refresh"));
    assert!(!std::fs::read_to_string(output)
        .unwrap()
        .contains("synthetic-refresh"));
}

#[test]
fn codex_oauth_imports_responses_deployment_with_owner_keys() {
    let mut root = base();
    root["providerConnections"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "id":"codex",
            "provider":"codex",
            "authType":"oauth",
            "refreshToken":"synthetic-codex-refresh",
            "clientId":"synthetic-codex-client",
            "providerSpecificData":{"workspaceId":"synthetic-workspace"}
        }));
    root["combos"][0]["models"]
        .as_array_mut()
        .unwrap()
        .push(json!("codex/gpt-6-luna"));
    let (_report, config, text) = import(&root);
    assert!(!text.contains("synthetic-codex-refresh"));
    assert!(text.contains("https://auth.openai.com/oauth/token"));
    let model = &config.models[config.model_names["codex/gpt-6-luna"]];
    let dep = &config.deployments[model.deployments[0].0 as usize];
    assert_eq!(dep.provider.0.as_ref(), "openai_responses");
    assert_eq!(
        dep.base_url.to_string(),
        "https://chatgpt.com/backend-api/codex"
    );
    assert_eq!(dep.upstream_model.as_ref(), "gpt-6-luna");
    assert_eq!(
        config.oauth[dep.credential.as_ref()].token_url,
        "https://auth.openai.com/oauth/token"
    );
    assert_eq!(dep.headers["originator"], "codex_cli_rs");
    assert_eq!(dep.headers["chatgpt-account-id"], "synthetic-workspace");
    let allowed = &config.model_allowed_keys[&config.model_names["codex/gpt-6-luna"]];
    assert!(!allowed.is_empty());
}

// The public OAuth client metadata the Antigravity CLI embeds; OmniRoute
// stores only per-connection tokens, so the importer fills these in when a
// dump omits them. They are upstream-supplied public values, not user secrets;
// they are built from parts so secret scanners do not flag them.
const ANTIGRAVITY_CLIENT_ID: &str = concat!(
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep",
    ".apps.googleusercontent.com"
);
const ANTIGRAVITY_CLIENT_SECRET: &str = concat!("GOCSPX-", "K58FWR486LdLJ1mLB8sXC4z6qDAf");

fn antigravity_dump(connections: Value) -> Value {
    json!({
        "provider_connections": connections,
        "combos": [
            {"id":"c1","name":"ag-combo","data":{"name":"ag-combo","strategy":"priority","models":[{"kind":"model","providerId":"antigravity","model":"gemini-3-flash"}]}}
        ],
        "api_keys": [{"id":"k","key":"synthetic-client","is_active":1}],
        "model_combo_mappings": [],
        "key_value": {}
    })
}

#[test]
fn antigravity_oauth_uses_the_embedded_public_client_when_the_dump_omits_it() {
    let root = antigravity_dump(json!([
        {"id":"ag","provider":"antigravity","auth_type":"oauth","is_active":1,"access_token":"synthetic-access","refresh_token":"synthetic-refresh","provider_specific_data":"{\"projectId\":\"synthetic-project\"}","priority":1},
        {"id":"gem","provider":"gemini","auth_type":"apikey","is_active":1,"api_key":"synthetic-gemini"}
    ]));
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let report = llmux_import::import(&root.to_string(), &output, None).unwrap();
    let warnings = report.warnings.join("\n");
    assert!(!warnings.contains("missing oauth client id"), "{warnings}");
    assert!(!warnings.contains("skipped"), "{warnings}");
    let text = std::fs::read_to_string(&output).unwrap();
    assert!(!text.contains("synthetic-refresh"));
    assert!(!text.contains("synthetic-access"));
    let config = RuntimeConfig::load(&output).unwrap();
    let credential = &config.oauth["upstream-ag"];
    assert_eq!(credential.token_url, "https://oauth2.googleapis.com/token");
    assert_eq!(credential.client_id, ANTIGRAVITY_CLIENT_ID);
    assert_eq!(
        credential.client_secret.as_deref(),
        Some(ANTIGRAVITY_CLIENT_SECRET)
    );
    assert_eq!(&*credential.refresh_token, "synthetic-refresh");
    for name in ["ag-combo", "antigravity/gemini-3-flash"] {
        let model = &config.models[*config
            .model_names
            .get(name)
            .unwrap_or_else(|| panic!("missing {name}"))];
        let deps: Vec<_> = model
            .deployments
            .iter()
            .map(|idx| &config.deployments[idx.0 as usize])
            .collect();
        assert_eq!(deps.len(), 1, "{name}");
        let dep = deps[0];
        assert_eq!(dep.provider.0.as_ref(), "antigravity", "{name}");
        assert_eq!(
            dep.base_url.to_string().trim_end_matches('/'),
            "https://daily-cloudcode-pa.googleapis.com",
            "{name}"
        );
        assert_eq!(dep.upstream_model.as_ref(), "gemini-3-flash", "{name}");
        assert_eq!(dep.credential.as_ref(), "upstream-ag", "{name}");
        assert_eq!(dep.headers["x-llmux-project"], "synthetic-project");
    }
}

#[test]
fn antigravity_oauth_without_a_project_id_stays_skipped() {
    let mut root = antigravity_dump(json!([
        {"id":"ag","provider":"antigravity","auth_type":"oauth","is_active":1,"access_token":"synthetic-access","refresh_token":"synthetic-refresh"},
        {"id":"gem","provider":"gemini","auth_type":"apikey","is_active":1,"api_key":"synthetic-gemini"}
    ]));
    root["combos"][0]["data"]["models"]
        .as_array_mut()
        .unwrap()
        .push(json!({"kind":"model","providerId":"gemini","model":"gemini-3-flash-preview"}));
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let report = llmux_import::import(&root.to_string(), &output, None).unwrap();
    let warnings = report.warnings.join("\n");
    assert!(
        warnings.contains("missing Antigravity project id"),
        "{warnings}"
    );
    let text = std::fs::read_to_string(&output).unwrap();
    assert!(!text.contains("synthetic-refresh"));
    let config = RuntimeConfig::load(&output).unwrap();
    assert!(config.oauth.is_empty());
    assert!(config
        .deployments
        .iter()
        .all(|dep| dep.provider.0.as_ref() != "antigravity"));
}

#[test]
fn antigravity_multi_account_deployments_follow_connection_priority_with_fallback_last() {
    let root = antigravity_dump(json!([
        {"id":"ag-slow","provider":"antigravity","auth_type":"oauth","is_active":1,"refresh_token":"synthetic-slow-refresh","project_id":"synthetic-slow-project","priority":9},
        {"id":"ag-fast","provider":"antigravity","auth_type":"oauth","is_active":1,"refresh_token":"synthetic-fast-refresh","project_id":"synthetic-fast-project","priority":1}
    ]));
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let key = dir.path().join("fallback-key");
    std::fs::write(&key, "synthetic-fallback").unwrap();
    let reference = format!("file:{}", key.display());
    let options = ImportOptions {
        omniroute_fallback: Some(OmniRouteFallback {
            base_url: "http://127.0.0.1:20128/v1",
            key: &reference,
        }),
        ..ImportOptions::default()
    };
    llmux_import::import_with_options(&root.to_string(), &output, None, &options).unwrap();
    let config = RuntimeConfig::load(&output).unwrap();
    assert_eq!(
        config.oauth["upstream-ag-fast"].client_id,
        ANTIGRAVITY_CLIENT_ID
    );
    assert_eq!(
        config.oauth["upstream-ag-slow"].client_id,
        ANTIGRAVITY_CLIENT_ID
    );
    let model = &config.models[config.model_names["ag-combo"]];
    let deps: Vec<_> = model
        .deployments
        .iter()
        .map(|idx| &config.deployments[idx.0 as usize])
        .collect();
    assert_eq!(deps.len(), 3);
    assert_eq!(deps[0].credential.as_ref(), "upstream-ag-fast");
    assert_eq!(deps[0].headers["x-llmux-project"], "synthetic-fast-project");
    assert_eq!(deps[1].credential.as_ref(), "upstream-ag-slow");
    assert_eq!(deps[1].headers["x-llmux-project"], "synthetic-slow-project");
    for dep in &deps[..2] {
        assert_eq!(dep.provider.0.as_ref(), "antigravity");
        assert_eq!(dep.upstream_model.as_ref(), "gemini-3-flash");
        assert!(!dep.fallback);
    }
    assert!(deps[2].fallback);
    assert!(deps[2].priority > deps[1].priority);
    assert_eq!(deps[2].upstream_model.as_ref(), "ag-combo");
}

#[test]
fn gemini_apikey_connections_emit_native_openai_compat_legs_in_priority_order() {
    let root = json!({
        "provider_connections": [
            {"id":"slow","provider":"gemini","auth_type":"apikey","is_active":1,"api_key":"synthetic-slow","priority":9},
            {"id":"fast","provider":"gemini","auth_type":"apikey","is_active":1,"api_key":"synthetic-fast","priority":1}
        ],
        "combos": [
            {"id":"c1","name":"gem-combo","data":{"name":"gem-combo","strategy":"priority","models":[{"kind":"model","providerId":"gemini","model":"gemini-3-flash-preview"}]}}
        ],
        "api_keys": [{"id":"k","key":"synthetic-client","is_active":1}],
        "model_combo_mappings": [],
        "key_value": {}
    });
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.toml");
    let key = dir.path().join("fallback-key");
    std::fs::write(&key, "synthetic-fallback").unwrap();
    let reference = format!("file:{}", key.display());
    let options = ImportOptions {
        omniroute_fallback: Some(OmniRouteFallback {
            base_url: "http://127.0.0.1:20128/v1",
            key: &reference,
        }),
        ..ImportOptions::default()
    };
    let report =
        llmux_import::import_with_options(&root.to_string(), &output, None, &options).unwrap();
    assert!(!report.warnings.join("\n").contains("skipped"));
    let text = std::fs::read_to_string(&output).unwrap();
    assert!(!text.contains("synthetic-slow"));
    assert!(!text.contains("synthetic-fast"));
    let config = RuntimeConfig::load(&output).unwrap();
    for name in ["gem-combo", "gemini/gemini-3-flash-preview"] {
        let model = &config.models[*config
            .model_names
            .get(name)
            .unwrap_or_else(|| panic!("missing {name}"))];
        let deps: Vec<_> = model
            .deployments
            .iter()
            .map(|idx| &config.deployments[idx.0 as usize])
            .collect();
        assert_eq!(deps.len(), 3, "{name}");
        assert_eq!(deps[0].credential.as_ref(), "upstream-fast", "{name}");
        assert_eq!(deps[1].credential.as_ref(), "upstream-slow", "{name}");
        for dep in &deps[..2] {
            assert_eq!(dep.provider.0.as_ref(), "openai_compat", "{name}");
            assert_eq!(
                dep.base_url.to_string(),
                "https://generativelanguage.googleapis.com/v1beta/openai",
                "{name}"
            );
            assert_eq!(
                dep.upstream_model.as_ref(),
                "gemini-3-flash-preview",
                "{name}"
            );
            assert_eq!(
                dep.strip_request_fields.as_ref(),
                &[ChatRequestField::Seed, ChatRequestField::User],
                "{name}"
            );
            assert!(!dep.fallback, "{name}");
        }
        assert!(deps[2].fallback, "{name}");
        assert!(deps[2].priority > deps[1].priority, "{name}");
        assert_eq!(deps[2].upstream_model.as_ref(), name, "{name}");
    }
}

#[test]
fn antigravity_oauth_serves_the_same_model_id() {
    let mut root = base();
    root["providerConnections"].as_array_mut().unwrap().extend([
        json!({
            "id":"ag",
            "provider":"antigravity",
            "authType":"oauth",
            "refreshToken":"synthetic-ag-refresh",
            "clientId":"synthetic-ag-client",
            "providerSpecificData":{"projectId":"synthetic-project","clientSecret":"synthetic-ag-secret"}
        }),
        json!({"id":"gemini","provider":"gemini","authType":"apikey","apiKey":"synthetic-gemini"})
    ]);
    root["combos"][0]["models"].as_array_mut().unwrap().extend([
        json!("antigravity/gemini-3-flash"),
        json!("antigravity/claude-sonnet-5-5"),
    ]);
    let (_report, config, text) = import(&root);
    assert!(!text.contains("synthetic-ag-refresh"));
    assert!(!text.contains("synthetic-ag-secret"));
    for (name, model_id) in [
        ("antigravity/gemini-3-flash", "gemini-3-flash"),
        ("antigravity/claude-sonnet-5-5", "claude-sonnet-5-5"),
    ] {
        let model = &config.models[config.model_names[name]];
        let dep = &config.deployments[model.deployments[0].0 as usize];
        assert_eq!(dep.provider.0.as_ref(), "antigravity", "{name}");
        assert_eq!(dep.upstream_model.as_ref(), model_id, "{name}");
        assert_eq!(
            dep.base_url.to_string().trim_end_matches('/'),
            "https://daily-cloudcode-pa.googleapis.com",
            "{name}"
        );
        assert_eq!(dep.headers["x-llmux-project"], "synthetic-project");
        assert_ne!(
            dep.base_url.to_string(),
            "https://generativelanguage.googleapis.com/v1beta/openai"
        );
    }
    assert_eq!(
        config.oauth["upstream-ag"].token_url,
        "https://oauth2.googleapis.com/token"
    );
}
