use llmux_config::RuntimeConfig;
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
