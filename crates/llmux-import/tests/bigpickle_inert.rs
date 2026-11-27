use llmux_config::RuntimeConfig;
use llmux_types::{provider, Billing};
use serde_json::json;
use std::{fs, process::Command, sync::Mutex};
use toml::{Table, Value as TomlValue};

const BUYER_REF: &str = "env:LLMUX_SYNTHETIC_BIGPICKLE_BUYER_KEY";
const NONBUYER_REF: &str = "env:LLMUX_SYNTHETIC_BIGPICKLE_NONBUYER_KEY";
const OVERLAY: &str = include_str!("fixtures/bigpickle-overlay.proposed.toml");
const ACTIVE: &str = include_str!("fixtures/bigpickle-active.proposed.toml");
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn parse(text: &str) -> Result<TomlValue, String> {
    toml::from_str::<TomlValue>(text).map_err(|error| format!("invalid TOML: {error}"))
}

fn table<'a>(value: &'a TomlValue, label: &str) -> Result<&'a Table, String> {
    value
        .as_table()
        .ok_or_else(|| format!("{label} must be a table"))
}

fn array<'a>(value: &'a TomlValue, label: &str) -> Result<&'a [TomlValue], String> {
    value
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| format!("{label} must be an array"))
}

fn string_list(value: &TomlValue, label: &str) -> Result<Vec<String>, String> {
    array(value, label)?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{label} entries must be strings"))
        })
        .collect()
}

fn exact_keys(table: &Table, expected: &[&str], label: &str) -> Result<(), String> {
    for key in table.keys() {
        if !expected.contains(&key.as_str()) {
            return Err(format!("{label} has unsupported key {key}"));
        }
    }
    for key in expected {
        if !table.contains_key(*key) {
            return Err(format!("{label} missing key {key}"));
        }
    }
    Ok(())
}

fn exact_string(table: &Table, key: &str, expected: &str, label: &str) -> Result<(), String> {
    match table.get(key).and_then(TomlValue::as_str) {
        Some(actual) if actual == expected => Ok(()),
        Some(actual) => Err(format!(
            "{label}.{key} is {actual:?}, expected {expected:?}"
        )),
        None => Err(format!("{label}.{key} must be a string")),
    }
}

fn exact_bool(table: &Table, key: &str, expected: bool, label: &str) -> Result<(), String> {
    match table.get(key).and_then(TomlValue::as_bool) {
        Some(actual) if actual == expected => Ok(()),
        _ => Err(format!("{label}.{key} must be {expected}")),
    }
}

fn exact_integer(table: &Table, key: &str, expected: i64, label: &str) -> Result<(), String> {
    match table.get(key).and_then(TomlValue::as_integer) {
        Some(actual) if actual == expected => Ok(()),
        _ => Err(format!("{label}.{key} must be {expected}")),
    }
}

fn bigpickle_models(doc: &TomlValue) -> Result<Vec<&Table>, String> {
    let models = doc
        .get("models")
        .ok_or_else(|| "config must contain [[models]]".to_owned())?;
    Ok(array(models, "models")?
        .iter()
        .filter_map(TomlValue::as_table)
        .filter(|model| model.get("name").and_then(TomlValue::as_str) == Some("big-pickle"))
        .collect())
}

fn validate_bigpickle_model(model: &Table) -> Result<(), String> {
    exact_keys(
        model,
        &["name", "allowed_keys", "capabilities", "deployments"],
        "big-pickle model",
    )?;
    exact_string(model, "name", "big-pickle", "big-pickle model")?;
    let allowed = model
        .get("allowed_keys")
        .ok_or_else(|| "big-pickle model missing allowed_keys".to_owned())?;
    let allowed = string_list(allowed, "big-pickle allowed_keys")?;
    if allowed != [BUYER_REF.to_owned()] {
        return Err(format!(
            "big-pickle allowed_keys must be exactly the synthetic buyer reference, got {allowed:?}"
        ));
    }
    let capabilities = model
        .get("capabilities")
        .and_then(TomlValue::as_table)
        .ok_or_else(|| "big-pickle capabilities must be a table".to_owned())?;
    exact_keys(capabilities, &["stream"], "big-pickle capabilities")?;
    exact_bool(capabilities, "stream", true, "big-pickle capabilities")?;

    let deployments = model
        .get("deployments")
        .ok_or_else(|| "big-pickle model missing deployments".to_owned())?;
    let deployments = array(deployments, "big-pickle deployments")?;
    if deployments.len() != 1 {
        return Err(format!(
            "big-pickle must have exactly one deployment, got {}",
            deployments.len()
        ));
    }
    let deployment = table(&deployments[0], "big-pickle deployment")?;
    exact_keys(
        deployment,
        &[
            "id",
            "auth",
            "provider",
            "upstream_model",
            "billing",
            "priority",
            "timeout_ms",
        ],
        "big-pickle deployment",
    )?;
    exact_string(
        deployment,
        "id",
        "opencode-big-pickle",
        "big-pickle deployment",
    )?;
    exact_string(deployment, "auth", "none", "big-pickle deployment")?;
    exact_string(deployment, "provider", "opencode", "big-pickle deployment")?;
    exact_string(
        deployment,
        "upstream_model",
        "big-pickle",
        "big-pickle deployment",
    )?;
    exact_string(deployment, "billing", "free", "big-pickle deployment")?;
    exact_integer(deployment, "priority", 0, "big-pickle deployment")?;
    exact_integer(deployment, "timeout_ms", 300_000, "big-pickle deployment")?;
    Ok(())
}

fn validate_d4a_server_shape(doc: &TomlValue) -> Result<(), String> {
    let server = doc
        .get("server")
        .and_then(TomlValue::as_table)
        .ok_or_else(|| "config missing [server]".to_owned())?;
    let keys = server
        .get("client_keys")
        .ok_or_else(|| "server missing client_keys".to_owned())?;
    let keys = string_list(keys, "server.client_keys")?;
    let mut sorted_keys = keys.clone();
    sorted_keys.sort();
    let mut expected_keys = [BUYER_REF.to_owned(), NONBUYER_REF.to_owned()];
    expected_keys.sort();
    if sorted_keys != expected_keys {
        return Err(format!(
            "server.client_keys must be exactly the synthetic buyer and non-buyer references, got {keys:?}"
        ));
    }
    let policies = server
        .get("key_policies")
        .ok_or_else(|| "server missing key_policies".to_owned())?;
    let policies = array(policies, "server.key_policies")?;
    if policies.len() != 1 {
        return Err(format!(
            "server.key_policies must contain exactly the synthetic buyer policy, got {}",
            policies.len()
        ));
    }
    let policy = table(&policies[0], "buyer key policy")?;
    exact_string(policy, "key", BUYER_REF, "buyer key policy")?;
    let models = policy
        .get("models")
        .ok_or_else(|| "buyer key policy missing models".to_owned())?;
    if string_list(models, "buyer policy models")? != ["big-pickle".to_owned()] {
        return Err("buyer key policy must contain exact literal big-pickle".to_owned());
    }
    exact_bool(policy, "free_only", true, "buyer key policy")?;
    if let Some(blocked) = policy.get("blocked") {
        if string_list(blocked, "buyer policy blocked")?
            .iter()
            .any(|pattern| glob_match(pattern, "big-pickle"))
        {
            return Err("buyer key policy blocks big-pickle".to_owned());
        }
    }
    Ok(())
}

fn validate_exact_overlay_shape(text: &str) -> Result<(), String> {
    let doc = parse(text)?;
    validate_d4a_server_shape(&doc)?;
    let models = doc
        .get("models")
        .ok_or_else(|| "overlay missing [[models]]".to_owned())?;
    if array(models, "overlay models")?.len() != 1 {
        return Err("overlay must contain exactly one model row".to_owned());
    }
    let rows = bigpickle_models(&doc)?;
    if rows.len() != 1 {
        return Err(format!(
            "overlay must contain exactly one canonical big-pickle row, got {}",
            rows.len()
        ));
    }
    validate_bigpickle_model(rows[0])
}

fn validate_candidate_shape(text: &str) -> Result<(), String> {
    let doc = parse(text)?;
    validate_d4a_server_shape(&doc)?;
    let rows = bigpickle_models(&doc)?;
    if rows.len() != 1 {
        return Err(format!(
            "candidate must contain exactly one canonical big-pickle row, got {}",
            rows.len()
        ));
    }
    validate_bigpickle_model(rows[0])?;

    let models = array(
        doc.get("models")
            .ok_or_else(|| "candidate missing models".to_owned())?,
        "candidate models",
    )?;
    let mut upstream_matches = 0;
    for model in models.iter().filter_map(TomlValue::as_table) {
        let Some(deployments) = model.get("deployments").and_then(TomlValue::as_array) else {
            continue;
        };
        for deployment in deployments.iter().filter_map(TomlValue::as_table) {
            if deployment.get("upstream_model").and_then(TomlValue::as_str) == Some("big-pickle") {
                upstream_matches += 1;
                if deployment.get("fallback").and_then(TomlValue::as_bool) == Some(true) {
                    return Err("candidate has an OmniRoute fallback leg for big-pickle".to_owned());
                }
            }
            if deployment
                .get("id")
                .and_then(TomlValue::as_str)
                .is_some_and(|id| id.contains("omniroute-fallback-big-pickle"))
            {
                return Err("candidate has an omniroute-fallback-big-pickle deployment".to_owned());
            }
        }
    }
    if upstream_matches != 1 {
        return Err(format!(
            "candidate must have exactly one upstream big-pickle leg, got {upstream_matches}"
        ));
    }
    Ok(())
}

fn canonical_reference(reference: &str) -> Result<String, String> {
    if let Some(name) = reference.strip_prefix("env:") {
        if name.is_empty() {
            return Err("empty env: secret reference".to_owned());
        }
        return Ok(format!("env:{name}"));
    }
    let Some(path) = reference.strip_prefix("file:") else {
        return Err("structural proof accepts only file: and env: references".to_owned());
    };
    if path.is_empty() || !path.starts_with('/') {
        return Err("file: secret references must be absolute paths".to_owned());
    }
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err("file: secret reference escapes its root".to_owned());
                }
            }
            normal => parts.push(normal),
        }
    }
    if parts.is_empty() {
        return Err("file: secret reference must name a file".to_owned());
    }
    Ok(format!("file:/{}", parts.join("/")))
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    let (mut star, mut star_t) = (usize::MAX, 0);
    while t < text.len() {
        if p < pattern.len() && pattern[p] == '*' {
            star = p;
            star_t = t;
            p += 1;
        } else if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if star != usize::MAX {
            p = star + 1;
            star_t += 1;
            t = star_t;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

fn validate_buyer_key_proof(
    active_text: &str,
    overlay_text: &str,
    expected_buyer_ref: &str,
) -> Result<(), String> {
    let expected = canonical_reference(expected_buyer_ref)?;
    let active = parse(active_text)?;
    let overlay = parse(overlay_text)?;
    let server = active
        .get("server")
        .and_then(TomlValue::as_table)
        .ok_or_else(|| "active config missing [server]".to_owned())?;
    let client_keys = server
        .get("client_keys")
        .ok_or_else(|| "active config missing server.client_keys".to_owned())?;
    let client_keys = string_list(client_keys, "server.client_keys")?;
    let client_matches = client_keys
        .iter()
        .map(|reference| canonical_reference(reference))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|reference| *reference == expected)
        .count();
    if client_matches != 1 {
        return Err(format!(
            "buyer reference must identify exactly one server.client_keys entry, got {client_matches}"
        ));
    }

    let policies_value = server
        .get("key_policies")
        .ok_or_else(|| "active config missing server.key_policies".to_owned())?;
    let policies = array(policies_value, "server.key_policies")?;
    let mut matching_policies = Vec::new();
    for policy in policies.iter().filter_map(TomlValue::as_table) {
        let key = policy
            .get("key")
            .and_then(TomlValue::as_str)
            .ok_or_else(|| "server.key_policies entry missing key".to_owned())?;
        if canonical_reference(key)? == expected {
            matching_policies.push(policy);
        }
    }
    if matching_policies.len() != 1 {
        return Err(format!(
            "buyer reference must identify exactly one server.key_policies entry, got {}",
            matching_policies.len()
        ));
    }
    let policy = matching_policies[0];
    let models = policy
        .get("models")
        .ok_or_else(|| "buyer policy missing models".to_owned())?;
    if !string_list(models, "buyer policy models")?
        .iter()
        .any(|model| model == "big-pickle")
    {
        return Err("buyer policy does not contain exact literal big-pickle".to_owned());
    }
    if policy.get("free_only").and_then(TomlValue::as_bool) != Some(true) {
        return Err("buyer policy must set free_only = true".to_owned());
    }
    if let Some(blocked) = policy.get("blocked") {
        if string_list(blocked, "buyer policy blocked")?
            .iter()
            .any(|pattern| glob_match(pattern, "big-pickle"))
        {
            return Err("buyer policy has a blocking pattern matching big-pickle".to_owned());
        }
    }

    let rows = bigpickle_models(&overlay)?;
    if rows.len() != 1 {
        return Err(format!(
            "overlay must contain exactly one canonical big-pickle row, got {}",
            rows.len()
        ));
    }
    let allowed = rows[0]
        .get("allowed_keys")
        .ok_or_else(|| "big-pickle overlay row missing allowed_keys".to_owned())?;
    let allowed = string_list(allowed, "big-pickle allowed_keys")?;
    if allowed.len() != 1 {
        return Err("big-pickle allowed_keys must contain exactly one reference".to_owned());
    }
    if canonical_reference(&allowed[0])? != expected {
        return Err("big-pickle allowed_keys is not the intended buyer reference".to_owned());
    }
    Ok(())
}

fn mutate_candidate(text: &str, mutate: impl FnOnce(&mut TomlValue)) -> String {
    let mut doc = parse(text).unwrap();
    mutate(&mut doc);
    toml::to_string_pretty(&doc).unwrap()
}

fn bigpickle_model_mut(doc: &mut TomlValue) -> &mut Table {
    doc.get_mut("models")
        .and_then(TomlValue::as_array_mut)
        .unwrap()
        .iter_mut()
        .filter_map(TomlValue::as_table_mut)
        .find(|model| model.get("name").and_then(TomlValue::as_str) == Some("big-pickle"))
        .unwrap()
}

fn mutate_first_deployment(model: &mut Table, mutate: impl FnOnce(&mut Table)) {
    let deployment = model
        .get_mut("deployments")
        .and_then(TomlValue::as_array_mut)
        .unwrap()
        .first_mut()
        .and_then(TomlValue::as_table_mut)
        .unwrap();
    mutate(deployment);
}

fn generate_candidate() -> (String, tempfile::TempDir) {
    let _env_guard = ENV_LOCK.lock().unwrap();
    std::env::set_var("LLMUX_SYNTHETIC_BIGPICKLE_BUYER_KEY", "synthetic-buyer-key");
    std::env::set_var(
        "LLMUX_SYNTHETIC_BIGPICKLE_NONBUYER_KEY",
        "synthetic-nonbuyer-key",
    );
    let root = json!({
        "providerConnections":[{"id":"openai","provider":"openai","authType":"apikey","apiKey":"synthetic-upstream"}],
        "apiKeys":[{"key":"synthetic-client"}],
        "combos":[{"id":"public","name":"public","models":["openai/first"]}]
    });
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("input.json"), root.to_string()).unwrap();
    fs::write(dir.path().join("omniroute-key"), "synthetic-omniroute").unwrap();
    let key_ref = format!("file:{}", dir.path().join("omniroute-key").display());
    let overlay = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/bigpickle-overlay.proposed.toml"
    );
    let output = Command::new(env!("CARGO_BIN_EXE_llmux-import"))
        .args([
            "--input",
            "input.json",
            "--output",
            "out.toml",
            "--omniroute-fallback",
            "http://127.0.0.1:20128/v1",
            "--omniroute-key",
            &key_ref,
            "--overlay",
            overlay,
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = fs::read_to_string(dir.path().join("out.toml")).unwrap();
    (text, dir)
}

#[test]
fn proposed_exact_overlay_fixture_has_native_d4a_shape() {
    assert!(OVERLAY.contains("PROPOSED SYNTHETIC FIXTURE"));
    assert!(OVERLAY.contains("not approved production input"));
    assert!(OVERLAY.contains("native `opencode` adaptation"));
    assert!(OVERLAY.contains("D4-A"));
    validate_exact_overlay_shape(OVERLAY).unwrap();
    validate_buyer_key_proof(ACTIVE, OVERLAY, BUYER_REF).unwrap();
}

#[test]
fn shipped_bigpickle_examples_use_the_native_shape() {
    let docs = include_str!("../../../docs/free-providers.md");
    let example = include_str!("../../../examples/llmux.free-providers.example.toml");
    for (label, text) in [("docs", docs), ("example", example)] {
        assert!(!text.contains("zen-big-pickle"), "{label}");
        assert!(!text.contains("max_context = 200000"), "{label}");
    }
    let example_pos = example.find("name = \"big-pickle\"").unwrap();
    let example_end = example[example_pos..]
        .find("\n[[models]]")
        .map(|offset| example_pos + offset)
        .unwrap_or(example.len());
    let example_block = &example[example_pos..example_end];
    for required in [
        "id = \"opencode-big-pickle\"",
        "auth = \"none\"",
        "provider = \"opencode\"",
        "upstream_model = \"big-pickle\"",
        "billing = \"free\"",
    ] {
        assert!(
            example_block.contains(required),
            "example missing {required}"
        );
    }
    for forbidden in [
        "credential = \"zen-key\"",
        "base_url = \"https://opencode.ai/inference/openai/v1\"",
        "provider = \"openai_compat\"",
        "max_context",
    ] {
        assert!(
            !example_block.contains(forbidden),
            "example retains {forbidden}"
        );
    }

    let docs_pos = docs.find("name = \"big-pickle\"").unwrap();
    let docs_end = docs[docs_pos..]
        .find("\n```")
        .map(|offset| docs_pos + offset)
        .unwrap_or(docs.len());
    let docs_block = &docs[docs_pos..docs_end];
    for required in [
        "id = \"opencode-big-pickle\"",
        "auth = \"none\"",
        "provider = \"opencode\"",
        "billing = \"free\"",
    ] {
        assert!(docs_block.contains(required), "docs missing {required}");
    }
    for forbidden in [
        "credential = \"zen-key\"",
        "base_url = \"https://opencode.ai/inference/openai/v1\"",
        "provider = \"openai_compat\"",
        "max_context",
    ] {
        assert!(!docs_block.contains(forbidden), "docs retain {forbidden}");
    }
}

#[test]
fn exact_overlay_validator_rejects_native_shape_mutations() {
    let mut cases = vec![
        (
            "withdrawn plain provider",
            OVERLAY.replace(
                "provider = \"opencode\"",
                "provider = \"openai_compat\"",
            ),
        ),
        (
            "added credential",
            OVERLAY.replace(
                "auth = \"none\"",
                "auth = \"none\"\ncredential = \"zen-key\"",
            ),
        ),
        (
            "replaced upstream model",
            OVERLAY.replace("upstream_model = \"big-pickle\"", "upstream_model = \"other\""),
        ),
        (
            "missing free billing marker",
            OVERLAY.replace("billing = \"free\"\n", ""),
        ),
        (
            "unsupported max_context",
            OVERLAY.replace(
                "capabilities = { stream = true }",
                "capabilities = { stream = true, max_context = 200000 }",
            ),
        ),
        (
            "explicit direct base_url",
            OVERLAY.replace(
                "provider = \"opencode\"",
                "provider = \"opencode\"\nbase_url = \"https://opencode.ai/inference/openai/v1\"",
            ),
        ),
        (
            "improvised authorization header",
            OVERLAY.replace(
                "auth = \"none\"",
                "auth = \"none\"\nheaders = { Authorization = \"Bearer synthetic\" }",
            ),
        ),
        (
            "extra allowed key",
            OVERLAY.replace(
                "allowed_keys = [\"env:LLMUX_SYNTHETIC_BIGPICKLE_BUYER_KEY\"]",
                "allowed_keys = [\"env:LLMUX_SYNTHETIC_BIGPICKLE_BUYER_KEY\", \"env:LLMUX_SYNTHETIC_BIGPICKLE_NONBUYER_KEY\"]",
            ),
        ),
        (
            "catalog visibility escape hatch",
            OVERLAY.replace(
                "allowed_keys = [\"env:LLMUX_SYNTHETIC_BIGPICKLE_BUYER_KEY\"]",
                "allowed_keys = [\"env:LLMUX_SYNTHETIC_BIGPICKLE_BUYER_KEY\"]\ncatalog_visible_keys = [\"env:LLMUX_SYNTHETIC_BIGPICKLE_BUYER_KEY\"]",
            ),
        ),
    ];
    cases.push((
        "added fallback leg",
        format!(
            "{OVERLAY}\n[[models.deployments]]\nid = \"omniroute-fallback-big-pickle\"\ncredential = \"omniroute-fallback\"\nbase_url = \"http://127.0.0.1:20128/v1\"\nupstream_model = \"big-pickle\"\nfallback = true\nbilling = \"billed\"\n"
        ),
    ));
    for (name, text) in cases {
        assert!(
            validate_exact_overlay_shape(&text).is_err(),
            "{name} must be rejected"
        );
    }
}

#[test]
fn structural_buyer_key_proof_accepts_exact_file_and_env_references() {
    validate_buyer_key_proof(ACTIVE, OVERLAY, BUYER_REF).unwrap();

    let file_ref = "file:/synthetic/llmux2/secrets/client-1";
    let active_file = ACTIVE.replace(BUYER_REF, file_ref);
    let overlay_file = OVERLAY.replace(BUYER_REF, file_ref);
    validate_buyer_key_proof(&active_file, &overlay_file, file_ref).unwrap();

    let normalized = active_file.replace(
        "file:/synthetic/llmux2/secrets/client-1",
        "file:/synthetic/llmux2/secrets/../secrets/client-1",
    );
    validate_buyer_key_proof(&normalized, &overlay_file, file_ref).unwrap();
}

#[test]
fn structural_buyer_key_proof_rejects_wrong_or_ambiguous_shapes() {
    let wrong_key_overlay = OVERLAY.replace(
        "allowed_keys = [\"env:LLMUX_SYNTHETIC_BIGPICKLE_BUYER_KEY\"]",
        "allowed_keys = [\"env:LLMUX_SYNTHETIC_BIGPICKLE_NONBUYER_KEY\"]",
    );
    let missing_exact = ACTIVE.replace("models = [\"big-pickle\"]", "models = [\"other-model\"]");
    let not_free_only = ACTIVE.replace("free_only = true", "free_only = false");
    let blocked = ACTIVE.replace(
        "free_only = true",
        "free_only = true\nblocked = [\"big-*\"]",
    );
    let duplicate_client_key = ACTIVE.replace(
        "\"env:LLMUX_SYNTHETIC_BIGPICKLE_BUYER_KEY\",\n  \"env:LLMUX_SYNTHETIC_BIGPICKLE_NONBUYER_KEY\"",
        "\"env:LLMUX_SYNTHETIC_BIGPICKLE_BUYER_KEY\",\n  \"env:LLMUX_SYNTHETIC_BIGPICKLE_BUYER_KEY\",\n  \"env:LLMUX_SYNTHETIC_BIGPICKLE_NONBUYER_KEY\"",
    );
    let duplicate_policy = format!(
        "{ACTIVE}\n[[server.key_policies]]\nkey = \"env:LLMUX_SYNTHETIC_BIGPICKLE_BUYER_KEY\"\nmodels = [\"big-pickle\"]\nfree_only = true\n"
    );
    for (name, active, overlay) in [
        (
            "reference is only some configured key",
            ACTIVE,
            wrong_key_overlay.as_str(),
        ),
        (
            "buyer policy missing exact big-pickle",
            missing_exact.as_str(),
            OVERLAY,
        ),
        (
            "buyer policy free_only is false",
            not_free_only.as_str(),
            OVERLAY,
        ),
        (
            "blocking pattern matches big-pickle",
            blocked.as_str(),
            OVERLAY,
        ),
        (
            "duplicate client-key references are ambiguous",
            duplicate_client_key.as_str(),
            OVERLAY,
        ),
        (
            "duplicate buyer policies are ambiguous",
            duplicate_policy.as_str(),
            OVERLAY,
        ),
    ] {
        assert!(
            validate_buyer_key_proof(active, overlay, BUYER_REF).is_err(),
            "{name} must be rejected"
        );
    }
}

#[test]
fn synthetic_import_with_fallback_and_overlay_ends_with_only_the_overlay_free_leg() {
    let (text, _dir) = generate_candidate();
    validate_candidate_shape(&text).unwrap();

    let cfg = RuntimeConfig::parse(&text).unwrap();
    assert!(
        cfg.deployments.iter().any(|deployment| deployment.fallback),
        "--omniroute-fallback must still produce fallback coverage elsewhere"
    );
    let model = &cfg.models[cfg.model_names["big-pickle"]];
    assert_eq!(model.deployments.len(), 1);
    let deployment = &cfg.deployments[model.deployments[0].0 as usize];
    assert_eq!(deployment.id.as_ref(), "opencode-big-pickle");
    assert_eq!(deployment.provider.0.as_ref(), provider::OPENCODE);
    assert!(deployment.credential.is_empty());
    assert_eq!(deployment.base_url.to_string(), provider::OPENCODE_BASE_URL);
    assert_eq!(deployment.upstream_model.as_ref(), "big-pickle");
    assert_eq!(deployment.billing, Billing::Free);
    assert!(!deployment.fallback);
    assert!(deployment.max_context.is_none());
}

#[test]
fn candidate_validator_rejects_observable_bigpickle_mutations() {
    let (candidate, _dir) = generate_candidate();
    validate_candidate_shape(&candidate).unwrap();
    let mut cases = Vec::new();

    let fallback_added = mutate_candidate(&candidate, |doc| {
        let model = bigpickle_model_mut(doc);
        let valid = model
            .get("deployments")
            .and_then(TomlValue::as_array)
            .unwrap()[0]
            .clone();
        let mut fallback = valid.as_table().unwrap().clone();
        fallback.insert(
            "id".to_owned(),
            TomlValue::String("omniroute-fallback-big-pickle".to_owned()),
        );
        fallback.insert("fallback".to_owned(), TomlValue::Boolean(true));
        fallback.insert("billing".to_owned(), TomlValue::String("billed".to_owned()));
        model
            .get_mut("deployments")
            .and_then(TomlValue::as_array_mut)
            .unwrap()
            .push(TomlValue::Table(fallback));
    });
    cases.push(("added fallback leg", fallback_added));

    let second_deployment = mutate_candidate(&candidate, |doc| {
        let model = bigpickle_model_mut(doc);
        let mut second = model
            .get("deployments")
            .and_then(TomlValue::as_array)
            .unwrap()[0]
            .as_table()
            .unwrap()
            .clone();
        second.insert(
            "id".to_owned(),
            TomlValue::String("opencode-big-pickle-second".to_owned()),
        );
        model
            .get_mut("deployments")
            .and_then(TomlValue::as_array_mut)
            .unwrap()
            .push(TomlValue::Table(second));
    });
    cases.push(("second deployment", second_deployment));

    let missing_free = mutate_candidate(&candidate, |doc| {
        mutate_first_deployment(bigpickle_model_mut(doc), |deployment| {
            deployment.remove("billing");
        });
    });
    cases.push(("missing free billing marker", missing_free));

    let wrong_upstream = mutate_candidate(&candidate, |doc| {
        mutate_first_deployment(bigpickle_model_mut(doc), |deployment| {
            deployment.insert(
                "upstream_model".to_owned(),
                TomlValue::String("other-model".to_owned()),
            );
        });
    });
    cases.push(("replacement of exact upstream model", wrong_upstream));

    let credential_added = mutate_candidate(&candidate, |doc| {
        mutate_first_deployment(bigpickle_model_mut(doc), |deployment| {
            deployment.insert(
                "credential".to_owned(),
                TomlValue::String("zen-key".to_owned()),
            );
        });
    });
    cases.push(("added credential", credential_added));

    let wrong_provider = mutate_candidate(&candidate, |doc| {
        mutate_first_deployment(bigpickle_model_mut(doc), |deployment| {
            deployment.insert(
                "provider".to_owned(),
                TomlValue::String("openai_compat".to_owned()),
            );
        });
    });
    cases.push(("withdrawn plain openai_compat provider", wrong_provider));

    let authorization_header = mutate_candidate(&candidate, |doc| {
        mutate_first_deployment(bigpickle_model_mut(doc), |deployment| {
            let mut headers = Table::new();
            headers.insert(
                "Authorization".to_owned(),
                TomlValue::String("Bearer synthetic".to_owned()),
            );
            deployment.insert("headers".to_owned(), TomlValue::Table(headers));
        });
    });
    cases.push(("improvised authorization header", authorization_header));

    let direct_url = mutate_candidate(&candidate, |doc| {
        mutate_first_deployment(bigpickle_model_mut(doc), |deployment| {
            deployment.insert(
                "base_url".to_owned(),
                TomlValue::String("https://opencode.ai/inference/openai/v1".to_owned()),
            );
        });
    });
    cases.push(("explicit direct base_url", direct_url));

    let duplicate_row = mutate_candidate(&candidate, |doc| {
        let duplicate = TomlValue::Table(bigpickle_model_mut(doc).clone());
        doc.get_mut("models")
            .and_then(TomlValue::as_array_mut)
            .unwrap()
            .push(duplicate);
    });
    cases.push(("duplicate canonical row", duplicate_row));

    for (name, text) in cases {
        assert!(
            validate_candidate_shape(&text).is_err(),
            "{name} must be rejected"
        );
    }
}
