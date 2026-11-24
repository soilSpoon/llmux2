use llmux_config::RuntimeConfig;
use llmux_import::ImportOptions;
use llmux_types::Billing;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::time::{Duration, SystemTime};

/// `2026-12-16T23:59:59Z` — the documented packet.ai promo end.
const PACKET_PROMO_END_S: u64 = 1_797_465_599;

fn fixture() -> Value {
    json!({
        "provider_nodes": [
            {"id":"node-freemodel","type":"openai-compatible","prefix":"fm","api_type":"chat","base_url":"https://freemodel.in/v1","chat_path":"","custom_headers_json":"{}"},
            {"id":"node-cohere","type":"openai-compatible","prefix":"coh","api_type":"chat","base_url":"https://api.cohere.ai/compatibility/v1","chat_path":"","custom_headers_json":"{}"},
            {"id":"node-packet","type":"openai-compatible","prefix":"pk","api_type":"chat","base_url":"https://api.packet.ai/v1","chat_path":"","custom_headers_json":"{}"},
            {"id":"node-samba","type":"openai-compatible","prefix":"sb","api_type":"chat","base_url":"https://api.sambanova.ai/v1","chat_path":"","custom_headers_json":"{}"},
            {"id":"node-evil","type":"openai-compatible","prefix":"ev","api_type":"chat","base_url":"https://openrouter.ai.evil.com/v1","chat_path":"","custom_headers_json":"{}"}
        ],
        "provider_connections": [
            {"id":"or","provider":"openrouter","auth_type":"apikey","is_active":1,"api_key":"test-or-secret","priority":0},
            {"id":"kiloanon","provider":"kilo","auth_type":"none","is_active":1,"priority":0},
            {"id":"fmc","provider":"node-freemodel","auth_type":"apikey","is_active":1,"api_key":"test-fm-secret","priority":0},
            {"id":"cohc","provider":"node-cohere","auth_type":"apikey","is_active":1,"api_key":"test-coh-secret","priority":0},
            {"id":"pkc","provider":"node-packet","auth_type":"apikey","is_active":1,"api_key":"test-pk-secret","priority":0},
            {"id":"sbc","provider":"node-samba","auth_type":"apikey","is_active":1,"api_key":"test-sb-secret","priority":0},
            {"id":"evc","provider":"node-evil","auth_type":"apikey","is_active":1,"api_key":"test-ev-secret","priority":0}
        ],
        "combos": [
            {"id":"c1","name":"free-mix","data":{"name":"free-mix","strategy":"priority","models":[
                {"kind":"model","providerId":"openrouter","model":"google/test-model:free","connectionId":"or"},
                {"kind":"model","providerId":"openrouter","model":"openai/paid-model","connectionId":"or"},
                {"kind":"model","providerId":"kilo","model":"minimax/minimax-m2.1","connectionId":"kiloanon"},
                {"kind":"model","providerId":"node-freemodel","model":"claude-fable-5","connectionId":"fmc"},
                {"kind":"model","providerId":"node-cohere","model":"command-a","connectionId":"cohc"},
                {"kind":"model","providerId":"node-cohere","model":"command-r","connectionId":"cohc"},
                {"kind":"model","providerId":"node-packet","model":"nvidia/DeepSeek-V4.1-Flash-NVFP4","connectionId":"pkc"},
                {"kind":"model","providerId":"node-samba","model":"gpt-oss-120b","connectionId":"sbc"},
                {"kind":"model","providerId":"node-evil","model":"x/evil-model:free","connectionId":"evc"}
            ]}}
        ],
        "api_keys": [{"id":"client","key":"test-db-client-secret","is_active":1,"allowed_models":[]}],
        "model_combo_mappings": [],
        "key_value": []
    })
}

fn import(free_providers: &[(&str, &str)]) -> RuntimeConfig {
    import_fixture(&fixture(), free_providers)
}

fn import_fixture(dump: &Value, free_providers: &[(&str, &str)]) -> RuntimeConfig {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("llmux.toml");
    let free = dir.path().join("free-providers.json");
    let entries: Vec<Value> = free_providers
        .iter()
        .map(|(provider, evidence)| json!({"provider":provider,"evidence":evidence}))
        .collect();
    fs::write(&free, serde_json::to_string(&entries).unwrap()).unwrap();
    llmux_import::import_with_options(
        &dump.to_string(),
        &output,
        None,
        &ImportOptions {
            free_providers: Some(&free),
            ..ImportOptions::default()
        },
    )
    .unwrap();
    RuntimeConfig::load(&output).unwrap()
}

fn by_model(config: &RuntimeConfig) -> HashMap<String, &llmux_types::Deployment> {
    config
        .deployments
        .iter()
        .map(|dep| (dep.upstream_model.to_string(), dep))
        .collect()
}

#[test]
fn openrouter_free_variants_get_the_documented_caps_and_free_billing() {
    let config = import(&[]);
    let deps = by_model(&config);
    let free = deps["google/test-model:free"];
    // Free-by-definition: the `:free` variant suffix is provider-documented
    // evidence, no --free-providers flag needed.
    assert_eq!(free.billing, Billing::Free);
    assert_eq!(free.quota.rpm, Some(20));
    assert_eq!(free.quota.rpd, Some(50));
    // A non-:free OpenRouter model stays billed and uncapped.
    let paid = deps["openai/paid-model"];
    assert_eq!(paid.billing, Billing::Billed);
    assert_eq!(paid.quota.rpm, None);
}

#[test]
fn kilo_anonymous_legs_get_the_documented_hourly_bound() {
    let config = import(&[]);
    let deps = by_model(&config);
    let anon = deps["minimax/minimax-m2.1"];
    assert_eq!(anon.billing, Billing::Free);
    // Documented 200 requests/hour/IP is approximated as rpm=3 until hourly
    // windows exist; never exceeds the real cap.
    assert_eq!(anon.quota.rpm, Some(3));
}

#[test]
fn key_gated_free_legs_get_caps_only_with_evidence() {
    // Without --free-providers evidence a key-gated host stays billed and gets
    // no free-tier cap — the same key could be on a paid plan.
    let config = import(&[]);
    let deps = by_model(&config);
    assert_eq!(deps["claude-fable-5"].billing, Billing::Billed);
    assert_eq!(deps["claude-fable-5"].quota.rpm, None);

    let config = import(&[
        (
            "freemodel.in",
            "Free plan 1M tok/month + 60 RPM / 100K TPM, per account dashboard",
        ),
        (
            "api.cohere.ai",
            "trial 1,000 calls/month, Command A family 20 RPM per docs",
        ),
        (
            "api.packet.ai",
            "$0 promo until 2026-12-16 per tokenfactory page",
        ),
        (
            "api.sambanova.ai",
            "free tier 20 RPM / 20 RPD / 200K TPD per docs",
        ),
    ]);
    let deps = by_model(&config);
    let fm = deps["claude-fable-5"];
    assert_eq!(fm.billing, Billing::Free);
    assert_eq!(fm.quota.rpm, Some(60));
    assert_eq!(fm.quota.tpm, Some(100_000));
    let cohere = deps["command-a"];
    assert_eq!(cohere.quota.rpm, Some(20));
    // [C1] covers the Command A family only — a `command-r` leg on the same
    // host keeps no free-tier cap.
    assert_eq!(deps["command-r"].quota.rpm, None);
    let samba = deps["gpt-oss-120b"];
    assert_eq!(samba.quota.rpm, Some(20));
    assert_eq!(samba.quota.rpd, Some(20));
    assert_eq!(samba.quota.tpd, Some(200_000));
}

#[test]
fn hosts_containing_a_table_row_as_substring_do_not_match() {
    // `openrouter.ai.evil.com` contains `openrouter.ai` but is a different
    // host: no `:free` free-by-definition, no documented caps.
    let config = import(&[]);
    let deps = by_model(&config);
    let evil = deps["x/evil-model:free"];
    assert_eq!(evil.billing, Billing::Billed);
    assert_eq!(evil.quota.rpm, None);
}

#[test]
fn header_authenticated_connections_are_not_anonymous() {
    // A custom node with `authType = "none"` but an `Authorization` custom
    // header is authenticated upstream, not per-IP anonymous — neither
    // free-by-definition billing nor the anonymous cap applies.
    let mut dump = fixture();
    dump["provider_nodes"].as_array_mut().unwrap().push(json!(
        {"id":"node-kilokeyed","type":"openai-compatible","prefix":"kk","api_type":"chat","base_url":"https://ai-gateway.kilo.ai/v1","chat_path":"","custom_headers_json":"{\"Authorization\":\"Bearer test-kilo-header\"}"}
    ));
    dump["provider_connections"]
        .as_array_mut()
        .unwrap()
        .push(json!(
            {"id":"kkc","provider":"node-kilokeyed","auth_type":"none","is_active":1,"priority":0}
        ));
    dump["combos"][0]["data"]["models"]
        .as_array_mut()
        .unwrap()
        .push(json!(
            {"kind":"model","providerId":"node-kilokeyed","model":"minimax/keyed-model","connectionId":"kkc"}
        ));
    let config = import_fixture(&dump, &[]);
    let deps = by_model(&config);
    let keyed = deps["minimax/keyed-model"];
    assert_eq!(keyed.billing, Billing::Billed);
    assert_eq!(keyed.quota.rpm, None);
}

#[test]
fn time_boxed_free_legs_get_expires_at() {
    let config = import(&[(
        "api.packet.ai",
        "$0 promo until 2026-12-16 per tokenfactory page",
    )]);
    let deps = by_model(&config);
    let packet = deps["nvidia/DeepSeek-V4.1-Flash-NVFP4"];
    assert_eq!(packet.billing, Billing::Free);
    let end = SystemTime::UNIX_EPOCH + Duration::from_secs(PACKET_PROMO_END_S);
    assert_eq!(packet.expires_at, Some(end));
    // A billed leg on the same host keeps working after the promo — no stamp.
    let config = import(&[]);
    let deps = by_model(&config);
    let billed = deps["nvidia/DeepSeek-V4.1-Flash-NVFP4"];
    assert_eq!(billed.billing, Billing::Billed);
    assert_eq!(billed.expires_at, None);
}
