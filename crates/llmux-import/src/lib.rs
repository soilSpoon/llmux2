//! Import OmniRoute JSON exports and SQLite table dumps into a static llmux configuration.

mod db_dump;
mod upstream;

use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

pub struct ImportReport {
    pub warnings: Vec<String>,
    pub models: usize,
    pub deployments: usize,
    pub credentials: usize,
    pub client_keys: usize,
}

struct Connection {
    source_id: String,
    provider: String,
    credential: String,
    base_url: String,
    priority: i64,
}

// OmniRoute registry/<id>/index.ts; Gemini uses Google's documented OpenAI compatibility API.
pub const PROVIDERS: &[(&str, &str, &str)] = &[
    ("openai", "openai", "https://api.openai.com/v1"),
    ("anthropic", "anthropic", "https://api.anthropic.com/v1"),
    (
        "openrouter",
        "openai_compat",
        "https://openrouter.ai/api/v1",
    ),
    ("groq", "openai_compat", "https://api.groq.com/openai/v1"),
    ("deepseek", "openai_compat", "https://api.deepseek.com"),
    ("mistral", "openai_compat", "https://api.mistral.ai/v1"),
    ("xai", "openai_compat", "https://api.x.ai/v1"),
    ("together", "openai_compat", "https://api.together.xyz/v1"),
    (
        "fireworks",
        "openai_compat",
        "https://api.fireworks.ai/inference/v1",
    ),
    ("cerebras", "openai_compat", "https://api.cerebras.ai/v1"),
    (
        "gemini",
        "openai_compat",
        "https://generativelanguage.googleapis.com/v1beta/openai",
    ),
    ("morph", "openai_compat", "https://api.morphllm.com/v1"),
];

fn string<'a>(value: &'a Value, field: &str) -> &'a str {
    value[field].as_str().unwrap_or("").trim()
}

fn array<'a>(value: &'a Value, field: &str) -> &'a [Value] {
    value[field].as_array().map(Vec::as_slice).unwrap_or(&[])
}

fn unique(raw: &str, used: &mut HashSet<String>) -> String {
    let base: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect();
    let base = if base.is_empty() { "imported" } else { &base };
    let mut name = base.to_owned();
    let mut n = 2;
    while !used.insert(name.clone()) {
        name = format!("{base}-{n}");
        n += 1;
    }
    name
}

fn valid_secret(secret: &str) -> bool {
    !secret.is_empty()
        && !secret.starts_with("enc:v1:")
        && secret.bytes().all(|b| b.is_ascii_graphic())
}

fn unrestricted_client_key(key: &Value) -> bool {
    let Some(fields) = key.as_object() else {
        return false;
    };
    fields.iter().all(|(field, value)| {
        let field = field.replace('_', "").to_ascii_lowercase();
        if value.is_null() {
            return true;
        }
        match field.as_str() {
            "allowedmodels" | "blockedmodels" | "allowedcombos" | "allowedconnections"
            | "allowedquotas" | "ipallowlist" | "scopes" | "allowedendpoints" | "ratelimits" => {
                value.as_array().is_some_and(Vec::is_empty)
            }
            "modelaccessmode" | "catalogscope" => value.as_str() == Some("all"),
            "isactive" | "allowautocombos" | "allowccdiscoveryaliases" => {
                value == true || value.as_u64() == Some(1)
            }
            "isbanned" | "disablenonpublicmodels" | "usagelimitenabled" => {
                value == false || value.as_u64() == Some(0)
            }
            "maxrequestsperday"
            | "maxrequestsperminute"
            | "throttledelayms"
            | "maxsessions"
            | "dailyusagelimitusd"
            | "weeklyusagelimitusd"
            | "fivehourusagelimitusd" => value.as_f64() == Some(0.0),
            "accessschedule" | "revokedat" | "expiresat" | "forcedmodel" | "machineid"
            | "teamid" | "groupid" => value.as_str() == Some(""),
            "source" => value.as_str().is_some_and(|s| s != "sso"),
            _ => true,
        }
    })
}

fn write_secret(dir: &Path, id: &str, secret: &str) -> Result<String, String> {
    let path = dir.join(id);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path).map_err(|_| {
        "could not create secret file (existing files are never overwritten)".to_owned()
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| "could not secure secret file")?;
    }
    file.write_all(secret.as_bytes())
        .map_err(|_| "could not write secret file".to_owned())?;
    Ok(format!("file:{}", path.display()))
}

struct Expander<'a> {
    combos: &'a HashMap<String, Value>,
    connections: &'a [Connection],
    nodes: &'a [Value],
    warnings: &'a mut Vec<String>,
    remaining_steps: usize,
    exhausted: bool,
}

impl Expander<'_> {
    fn expand(&mut self, name: &str, stack: &mut Vec<String>, out: &mut Vec<Value>) {
        if stack.iter().any(|n| n == name) || stack.len() >= 64 {
            self.warnings.push(format!(
                "combo {name}: skipped cyclic or overly deep reference"
            ));
            return;
        }
        let Some(combo) = self.combos.get(name) else {
            self.warnings
                .push(format!("combo {name}: skipped missing reference"));
            return;
        };
        stack.push(name.to_owned());
        for (index, step) in array(combo, "models").iter().enumerate() {
            if self.exhausted {
                break;
            }
            if self.remaining_steps == 0 {
                self.exhausted = true;
                self.warnings.push(format!(
                    "combo {}: stopped expansion at 1024-step work budget",
                    stack[0]
                ));
                break;
            }
            self.remaining_steps -= 1;
            if step["usable"] == false {
                self.warnings
                    .push(format!("combo {name} step {index}: skipped usable: false"));
                continue;
            }
            if out.len() >= 10_000 {
                self.warnings.push(format!(
                    "combo {name}: skipped remaining steps at 10000-deployment safety limit"
                ));
                break;
            }
            let raw = step
                .as_str()
                .unwrap_or_else(|| string(step, "model"))
                .trim();
            let provider_id = match string(step, "providerId") {
                "" => string(step, "provider"),
                id => id,
            };
            let reference = if string(step, "kind") == "combo-ref" {
                Some(string(step, "comboName"))
            } else if string(step, "kind") != "model"
                && provider_id.is_empty()
                && self.combos.contains_key(raw)
            {
                Some(raw)
            } else {
                None
            };
            if let Some(target) = reference {
                self.warnings.push(format!("combo {name} step {index}: expanded nested combo {target} inline; nested routing policy is not preserved"));
                self.expand(target, stack, out);
                continue;
            }
            let (p, model) = if provider_id.is_empty() {
                raw.split_once('/').unwrap_or(("", raw))
            } else {
                (provider_id, raw)
            };
            let canonical = upstream::canonical_provider(p, self.nodes);
            let model = if provider_id.is_empty() {
                // The routing segment was already consumed by split_once.
                if self
                    .nodes
                    .iter()
                    .any(|node| string(node, "id") == canonical)
                {
                    upstream::model_id(model, canonical, self.nodes)
                } else {
                    model
                }
            } else {
                upstream::model_id(model, canonical, self.nodes)
            };
            if p.is_empty()
                || model.is_empty()
                || model.contains('*')
                || string(step, "kind") == "provider-wildcard"
            {
                self.warnings.push(format!(
                    "combo {name} step {index}: skipped invalid or wildcard model"
                ));
                continue;
            }
            let p = canonical;
            let pinned = string(step, "connectionId");
            let allowed = array(step, "allowedConnectionIds");
            if step
                .get("connectionId")
                .is_some_and(|v| !v.is_null() && !v.is_string())
                || step
                    .get("allowedConnectionIds")
                    .is_some_and(|v| !v.is_null() && !v.is_array())
                || allowed
                    .iter()
                    .any(|v| v.as_str().is_none_or(|s| s.trim().is_empty()))
            {
                self.warnings.push(format!(
                    "combo {name} step {index}: skipped malformed connection restriction"
                ));
                continue;
            }
            let candidates: Vec<_> = self
                .connections
                .iter()
                .filter(|c| {
                    c.provider == p
                        && if !pinned.is_empty() {
                            c.source_id == pinned
                        } else {
                            allowed.is_empty()
                                || allowed.iter().any(|id| id.as_str() == Some(&c.source_id))
                        }
                })
                .collect();
            if candidates.is_empty() {
                self.warnings.push(format!("combo {name} step {index}: skipped provider {p}: no eligible imported connection"));
            }
            if step.get("weight").is_some() || step.get("config").is_some() {
                self.warnings.push(format!(
                    "combo {name} step {index}: ignored weight/config routing metadata"
                ));
            }
            if !pinned.is_empty() && !allowed.is_empty() {
                self.warnings.push(format!("combo {name} step {index}: connectionId takes precedence over allowedConnectionIds"));
            }
            for id in allowed.iter().filter_map(Value::as_str) {
                if !self
                    .connections
                    .iter()
                    .any(|c| c.provider == p && c.source_id == id)
                {
                    self.warnings.push(format!(
                        "combo {name} step {index}: skipped unavailable allowed connection {id}"
                    ));
                }
            }
            if candidates.len() > 10_000 - out.len() {
                self.warnings.push(format!("combo {name} step {index}: truncated connections at 10000-deployment safety limit"));
            }
            for c in candidates.into_iter().take(10_000 - out.len()) {
                out.push(
                    json!({"credential":c.credential,"base_url":c.base_url,"upstream_model":model}),
                );
            }
        }
        stack.pop();
    }
}

/// Writes a new configuration and exclusive secret files. Existing files are never overwritten.
/// Paths in the resulting configuration are absolute; warnings never include credential values.
pub fn import(
    input: &str,
    output: &Path,
    secrets_dir: Option<&Path>,
) -> Result<ImportReport, String> {
    let root: Value =
        serde_json::from_str(input).map_err(|_| "invalid OmniRoute JSON".to_owned())?;
    if !root.is_object() {
        return Err("export must be a JSON object".into());
    }
    let mut warnings = Vec::new();
    let root = if root.get("provider_connections").is_some() {
        warnings.push("SQLite table dump: normalized rows; unrelated roots are ignored".into());
        db_dump::normalize(&root)?
    } else {
        root
    };
    match root["_meta"]["version"].as_str() {
        Some("omniroute-v3-legacy-export") => {}
        None => warnings.push("missing export version; interpreting as legacy db.json".into()),
        _ => return Err("unsupported OmniRoute export version".into()),
    }
    for key in ["providerConnections", "providerNodes", "combos", "apiKeys"] {
        if root.get(key).is_some_and(|v| !v.is_array()) {
            return Err(format!("{key} must be an array"));
        }
    }
    let output = std::path::absolute(output).map_err(|_| "could not resolve output path")?;
    if output.exists() {
        return Err("output already exists".into());
    }
    let dir: PathBuf = secrets_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| output.parent().unwrap().join("secrets"));
    let dir = std::path::absolute(dir).map_err(|_| "could not resolve secrets path")?;
    if fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err("secrets directory must not be a symlink".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .or_else(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists && dir.is_dir() {
                    Ok(())
                } else {
                    Err(e)
                }
            })
            .map_err(|_| "could not create secrets directory")?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .map_err(|_| "could not secure secrets directory")?;
    }
    #[cfg(not(unix))]
    return Err("secure secret-file creation currently requires Unix".into());
    let mut ids = HashSet::new();
    let mut credentials = Vec::new();
    let mut connections = Vec::new();
    let mut source_ids = HashSet::new();
    for (index, c) in array(&root, "providerConnections").iter().enumerate() {
        let label = format!("connection {index} ({})", string(c, "id"));
        let reason = if c["usable"] == false {
            Some("usable: false")
        } else if c["isActive"] == false {
            Some("inactive")
        } else if !matches!(string(c, "authType"), "apikey" | "api_key" | "api-key") {
            Some("OAuth/token or unsupported auth")
        } else if !valid_secret(c["apiKey"].as_str().unwrap_or("")) {
            Some("missing, encrypted or invalid API key")
        } else {
            None
        };
        if let Some(reason) = reason {
            warnings.push(format!("{label}: skipped {reason}"));
            continue;
        }
        let (kind, base_url) = match upstream::resolve(c, array(&root, "providerNodes")) {
            Ok(provider) => provider,
            Err(reason) => {
                warnings.push(format!("{label}: skipped {reason}"));
                continue;
            }
        };
        let source_id = string(c, "id");
        if source_id.is_empty() || !source_ids.insert(source_id.to_owned()) {
            warnings.push(format!("{label}: skipped missing/duplicate connection id"));
            continue;
        }
        let id = unique(&format!("upstream-{source_id}"), &mut ids);
        let key = write_secret(&dir, &id, c["apiKey"].as_str().unwrap())?;
        credentials.push(json!({"id":id,"kind":"api_key","provider":kind,"key":key}));
        connections.push(Connection {
            source_id: source_id.into(),
            provider: string(c, "provider").into(),
            credential: id,
            base_url,
            priority: c["priority"].as_i64().unwrap_or(0),
        });
        if c.get("globalPriority").is_some()
            || c.get("maxConcurrent").is_some()
            || c.get("rateLimitOverrides").is_some()
        {
            warnings.push(format!(
                "{label}: ignored global priority and account-level limits"
            ));
        }
    }
    connections.sort_by_key(|c| c.priority);
    let mut combos = HashMap::new();
    let mut order = Vec::new();
    for (i, entry) in array(&root, "combos").iter().enumerate() {
        let combo = if let Some(data) = entry.get("data") {
            if let Some(data) = data.as_str() {
                match serde_json::from_str(data) {
                    Ok(combo) => combo,
                    Err(_) => {
                        warnings.push(format!("combo {i}: skipped invalid data JSON"));
                        continue;
                    }
                }
            } else {
                data.clone()
            }
        } else {
            entry.clone()
        };
        let name = string(&combo, "name").to_owned();
        if name.is_empty() || combos.contains_key(&name) {
            warnings.push(format!("combo {i}: skipped empty/duplicate name"));
            continue;
        }
        if combo["name"].as_str() != Some(&name) {
            warnings.push(format!("combo {i}: trimmed whitespace from model name"));
        }
        if combo.get("models").is_some_and(|v| !v.is_array()) {
            warnings.push(format!("combo {name}: skipped malformed model steps"));
            continue;
        }
        order.push(name.clone());
        combos.insert(name, combo);
    }
    let mut models = Vec::new();
    let mut deployment_ids = HashSet::new();
    let mut deployment_count = 0;
    for name in order {
        let combo = &combos[&name];
        let strategy = string(combo, "strategy");
        if !matches!(strategy, "" | "priority" | "fallback" | "round-robin") {
            warnings.push(format!(
                "combo {name}: approximated strategy {strategy} as priority"
            ));
        }
        if combo["config"].as_object().is_some_and(|c| !c.is_empty()) {
            warnings.push(format!("combo {name}: ignored combo config"));
        }
        let mut deployments = Vec::new();
        Expander {
            combos: &combos,
            connections: &connections,
            nodes: array(&root, "providerNodes"),
            warnings: &mut warnings,
            remaining_steps: 1024,
            exhausted: false,
        }
        .expand(&name, &mut Vec::new(), &mut deployments);
        if deployments.is_empty() {
            warnings.push(format!("combo {name}: skipped: no deployments"));
            continue;
        }
        for (i, dep) in deployments.iter_mut().enumerate() {
            dep["id"] = unique(&format!("{name}-{i}"), &mut deployment_ids).into();
            dep["priority"] = if strategy == "round-robin" { 0 } else { i }.into();
        }
        deployment_count += deployments.len();
        models.push(
            json!({"name":name,"strategy":"priority","aliases":[],"deployments":deployments}),
        );
    }
    if let Some(aliases) = root.get("modelAliases") {
        if let Some(aliases) = aliases.as_object() {
            for alias in aliases.keys() {
                warnings.push(format!("alias {alias}: skipped legacy model alias; only combos become canonical models"));
            }
        } else {
            warnings.push("ignored malformed modelAliases".into());
        }
    }
    if models.is_empty() {
        return Err("no importable models; no configuration written".into());
    }
    let mut client_keys = Vec::new();
    for (i, key) in array(&root, "apiKeys").iter().enumerate() {
        if key["isActive"] == false || !valid_secret(key["key"].as_str().unwrap_or("")) {
            warnings.push(format!(
                "client key {i}: skipped inactive, missing, encrypted or invalid key"
            ));
            continue;
        }
        if !unrestricted_client_key(key) {
            warnings.push(format!("client key {i}: skipped restricted or malformed per-key access policy; llmux keys grant unrestricted access"));
            continue;
        }
        let id = unique(&format!("client-{i}"), &mut ids);
        client_keys.push(write_secret(&dir, &id, key["key"].as_str().unwrap())?);
    }
    let client_count = client_keys.len();
    if client_keys.is_empty() {
        client_keys.push("env:LLMUX_CLIENT_KEY".into());
        warnings.push(
            "no active client keys; set LLMUX_CLIENT_KEY before starting llmux-server".into(),
        );
    }
    let config = json!({"server":{"listen":"127.0.0.1:8080","client_keys":client_keys},"credentials":credentials,"models":models});
    let mut validation = config.clone();
    // Validate structure and URLs without requiring the fallback environment variable yet.
    if client_count == 0 {
        validation["server"]["client_keys"] = json!([credentials[0]["key"]]);
    }
    let validation =
        toml::Value::try_from(&validation).map_err(|_| "could not encode configuration")?;
    llmux_config::RuntimeConfig::parse(
        &toml::to_string(&validation).map_err(|_| "could not encode configuration")?,
    )
    .map_err(|_| "generated configuration failed llmux validation (check upstream URLs)")?;
    let value = toml::Value::try_from(&config).map_err(|_| "could not encode configuration")?;
    let content = toml::to_string_pretty(&value).map_err(|_| "could not encode configuration")?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&output)
        .map_err(|_| "could not create output configuration")?;
    file.write_all(content.as_bytes())
        .map_err(|_| "could not write output configuration")?;
    Ok(ImportReport {
        warnings,
        models: models.len(),
        deployments: deployment_count,
        credentials: credentials.len(),
        client_keys: client_count,
    })
}
