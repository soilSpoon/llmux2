//! Import OmniRoute JSON exports and SQLite table dumps into a static llmux configuration.

mod aliases;
mod connection;
mod context;
mod db_dump;
mod direct;
mod fallback;
mod morph;
mod policy;
mod replacements;
mod secondary;
mod upstream;
mod validation;

use llmux_types::provider::{
    ANTHROPIC as KIND_ANTHROPIC, OPENAI as KIND_OPENAI, OPENAI_COMPAT as KIND_OPENAI_COMPAT,
};
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

#[derive(Default)]
pub struct ImportOptions<'a> {
    pub cloudflare_account_id_file: Option<&'a Path>,
    pub cloudflare_token_file: Option<&'a Path>,
    pub workers_ai_models: Option<&'a Path>,
    pub kilo_free_models: Option<&'a Path>,
    pub kilo_key_file: Option<&'a Path>,
    pub openrouter_replacements: bool,
    pub omniroute_fallback: Option<OmniRouteFallback<'a>>,
}

#[derive(Clone, Copy)]
pub struct OmniRouteFallback<'a> {
    pub base_url: &'a str,
    pub key: &'a str,
}

struct Connection {
    headers: Value,
    source_id: String,
    provider: upstream::ProviderIdentity,
    credential: Option<String>,
    kind: String,
    base_url: String,
    priority: i64,
    policy: policy::ModelPolicy,
}

// OmniRoute registry/<id>/index.ts; Gemini uses Google's documented OpenAI compatibility API.
pub const PROVIDERS: &[(&str, &str, &str)] = &[
    ("openai", KIND_OPENAI, "https://api.openai.com/v1"),
    ("anthropic", KIND_ANTHROPIC, "https://api.anthropic.com/v1"),
    (
        "openrouter",
        KIND_OPENAI_COMPAT,
        "https://openrouter.ai/api/v1",
    ),
    ("groq", KIND_OPENAI_COMPAT, "https://api.groq.com/openai/v1"),
    ("deepseek", KIND_OPENAI_COMPAT, "https://api.deepseek.com"),
    ("mistral", KIND_OPENAI_COMPAT, "https://api.mistral.ai/v1"),
    ("xai", KIND_OPENAI_COMPAT, "https://api.x.ai/v1"),
    (
        "together",
        KIND_OPENAI_COMPAT,
        "https://api.together.xyz/v1",
    ),
    (
        "fireworks",
        KIND_OPENAI_COMPAT,
        "https://api.fireworks.ai/inference/v1",
    ),
    ("cerebras", KIND_OPENAI_COMPAT, "https://api.cerebras.ai/v1"),
    (
        "gemini",
        KIND_OPENAI_COMPAT,
        "https://generativelanguage.googleapis.com/v1beta/openai",
    ),
    ("morph", KIND_OPENAI_COMPAT, "https://api.morphllm.com/v1"),
    ("aihorde", KIND_OPENAI_COMPAT, "https://oai.aihorde.net/v1"),
    (
        "kilo",
        KIND_OPENAI_COMPAT,
        "https://ai-gateway.kilo.ai/api/v1",
    ),
    (
        "kilocode",
        KIND_OPENAI_COMPAT,
        "https://ai-gateway.kilo.ai/api/v1",
    ),
];

fn string<'a>(value: &'a Value, field: &str) -> &'a str {
    value[field].as_str().unwrap_or("").trim()
}

fn array<'a>(value: &'a Value, field: &str) -> &'a [Value] {
    value[field].as_array().map(Vec::as_slice).unwrap_or(&[])
}

fn deepseek_model(model: &str) -> bool {
    let mut parts = model.rsplit('/');
    let leaf = parts.next().unwrap_or_default();
    !leaf.is_empty()
        && ((leaf.len() > "deepseek-".len() && leaf.to_ascii_lowercase().starts_with("deepseek-"))
            || parts.any(|part| {
                part.eq_ignore_ascii_case("deepseek") || part.eq_ignore_ascii_case("deepseek-ai")
            }))
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

fn connection_headers(
    connection: &Value,
    nodes: &[Value],
    anonymous: bool,
    kind: &str,
) -> Result<HashMap<String, String>, &'static str> {
    let metadata = upstream::metadata(connection)?;
    let node = nodes
        .iter()
        .find(|node| string(node, "id") == string(connection, "provider"));
    let raw = metadata
        .get("customHeaders")
        .filter(|v| !v.is_null())
        .or_else(|| node.and_then(|node| node.get("customHeaders")));
    let Some(raw) = raw.filter(|v| !v.is_null()) else {
        return Ok(HashMap::new());
    };
    let parsed;
    let raw = if let Some(text) = raw.as_str() {
        parsed = serde_json::from_str::<Value>(text).map_err(|_| "invalid custom headers")?;
        &parsed
    } else {
        raw
    };
    let raw = raw.as_object().ok_or("invalid custom headers")?;
    let mut headers = HashMap::new();
    for (name, value) in raw {
        let value = value.as_str().ok_or("invalid custom headers")?;
        llmux_config::validate_deployment_header(name, value, anonymous, kind)
            .map_err(|_| "unsupported or forbidden custom headers")?;
        if headers
            .insert(name.to_ascii_lowercase(), value.to_owned())
            .is_some()
        {
            return Err("duplicate custom headers");
        }
    }
    Ok(headers)
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
    context: &'a mut context::Overrides,
    combos: &'a HashMap<String, Value>,
    connections: &'a [Connection],
    nodes: &'a [Value],
    warnings: &'a mut Vec<String>,
    remaining_steps: usize,
    exhausted: bool,
    policy: &'a policy::Policy,
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
            let Some((p, model)) = upstream::model_target(step, self.nodes) else {
                self.warnings.push(format!(
                    "combo {name} step {index}: skipped invalid or wildcard model"
                ));
                continue;
            };
            let pinned = string(step, "connectionId");
            let allowed = array(step, "allowedConnectionIds");
            if !connection::valid_restrictions(step) {
                self.warnings.push(format!(
                    "combo {name} step {index}: skipped malformed connection restriction"
                ));
                continue;
            }
            let antigravity =
                p.is_builtin("antigravity") && replacements::antigravity(model).is_some();
            let replacement = if antigravity {
                if !pinned.is_empty() || !allowed.is_empty() {
                    self.warnings.push(format!("combo {name} step {index}: reviewed Antigravity replacement uses eligible Gemini connections instead of source connection restrictions"));
                }
                replacements::antigravity(model)
            } else if p.is_builtin("openrouter") && self.policy.openrouter_replacements {
                let replacement = replacements::find(model);
                if replacement.is_none() || !pinned.is_empty() || !allowed.is_empty() {
                    self.warnings.push(format!("combo {name} step {index}: no verified connected OpenRouter replacement (or source connection restriction)"));
                    continue;
                }
                replacement
            } else {
                None
            };
            let mut candidates: Vec<_> = self
                .connections
                .iter()
                .filter(|c| {
                    replacement.map_or(c.provider == p, |r| {
                        if antigravity {
                            replacements::antigravity_connection(r, c)
                        } else {
                            replacements::matches(r, c)
                        }
                    }) && if antigravity {
                        true
                    } else if !pinned.is_empty() {
                        c.source_id == pinned
                    } else {
                        allowed.is_empty()
                            || allowed.iter().any(|id| id.as_str() == Some(&c.source_id))
                    }
                })
                .collect();
            if replacement.is_some() {
                let mut seen = HashSet::new();
                candidates.retain(|c| seen.insert((&c.base_url, &c.credential)));
                if model.contains(":batch") {
                    self.warnings.push(format!("combo {name} step {index}: synchronous same-model replacement does not preserve batch scheduling or discounts"));
                }
            }
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
            for id in allowed
                .iter()
                .filter(|_| !antigravity)
                .filter_map(Value::as_str)
            {
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
                let target = replacement.map_or(model, |r| r.2);
                let model = match self.policy.model(c.policy, target) {
                    Ok(model) => model,
                    Err(reason) => {
                        self.warnings
                            .push(format!("combo {name} step {index}: skipped {reason}"));
                        continue;
                    }
                };
                let mut deployment = json!({"base_url":c.base_url,"upstream_model":model});
                if let Some(limit) = self.context.limit(&c.provider, &model) {
                    deployment["max_context"] = limit.into();
                }
                if c.headers
                    .as_object()
                    .is_some_and(|headers| !headers.is_empty())
                {
                    deployment["headers"] = c.headers.clone();
                }
                if c.policy == policy::ModelPolicy::Workers {
                    deployment["quota"] = json!({"rpm":1});
                }
                if let Some(credential) = &c.credential {
                    deployment["credential"] = credential.clone().into();
                } else {
                    deployment["auth"] = "none".into();
                    deployment["provider"] = c.kind.clone().into();
                }
                out.push(deployment);
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
    import_with_options(input, output, secrets_dir, &ImportOptions::default())
}

pub fn import_with_fallback(
    input: &str,
    output: &Path,
    secrets_dir: Option<&Path>,
    fallback: Option<OmniRouteFallback<'_>>,
) -> Result<ImportReport, String> {
    import_with_options(
        input,
        output,
        secrets_dir,
        &ImportOptions {
            omniroute_fallback: fallback,
            ..Default::default()
        },
    )
}

pub fn import_with_options(
    input: &str,
    output: &Path,
    secrets_dir: Option<&Path>,
    options: &ImportOptions<'_>,
) -> Result<ImportReport, String> {
    let policy = policy::Policy::load(options)?;
    let fallback = options.omniroute_fallback;
    if let Some(options) = &fallback {
        if !["env:", "file:"].iter().any(|prefix| {
            options
                .key
                .strip_prefix(prefix)
                .is_some_and(|value| !value.is_empty())
        }) {
            return Err("--omniroute-key must be a nonempty env: or file: secret reference".into());
        }
    }
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
    let workers_credential = policy.workers.as_ref().map(|workers| {
        let id = unique("workers-ai", &mut ids);
        credentials.push(json!({"id":id,"kind":"api_key","provider":"openai_compat","key":workers.reference}));
        warnings.push("Workers AI uses rpm=1 per deployment, NOT an account-wide neuron/daily spending cap; enforce free-only billing at Cloudflare".into());
        connections.push(Connection {headers:json!({}),source_id:"workers-ai-opt-in".into(),provider:upstream::ProviderIdentity::Builtin("workers-ai".into()),credential:Some(id.clone()),kind:"openai_compat".into(),base_url:workers.base_url.clone(),priority:i64::MAX,policy:policy::ModelPolicy::Workers});
        id
    });
    for (index, c) in array(&root, "providerConnections").iter().enumerate() {
        let label = format!("connection {index} ({})", string(c, "id"));
        let identity = connection::identity(c, array(&root, "providerNodes"));
        if policy.openrouter_replacements && identity == "openrouter" {
            warnings.push(format!(
                "{label}: skipped OpenRouter; only verified connected replacements enabled"
            ));
            continue;
        }
        let auth = match connection::select_auth(
            c,
            array(&root, "providerNodes"),
            policy.workers.is_some(),
        ) {
            Ok(key) => key,
            Err(reason) => {
                warnings.push(format!("{label}: skipped {reason}"));
                continue;
            }
        };
        let (kind, base_url) = match if matches!(auth, connection::SelectedAuth::Workers) {
            Ok((
                "openai_compat".into(),
                policy.workers.as_ref().unwrap().base_url.clone(),
            ))
        } else {
            upstream::resolve(c, array(&root, "providerNodes"), &mut warnings)
        } {
            Ok(provider) => provider,
            Err(reason) => {
                warnings.push(format!("{label}: skipped {reason}"));
                continue;
            }
        };
        let headers = match connection_headers(
            c,
            array(&root, "providerNodes"),
            matches!(auth, connection::SelectedAuth::None),
            &kind,
        ) {
            Ok(headers) => headers,
            Err(reason) => {
                warnings.push(format!("{label}: skipped {reason}"));
                continue;
            }
        };
        let mut header_refs = serde_json::Map::new();
        for (name, value) in headers {
            let id = unique("upstream-header", &mut ids);
            header_refs.insert(name, write_secret(&dir, &id, &value)?.into());
        }
        let source_id = string(c, "id");
        if source_id.is_empty() || !source_ids.insert(source_id.to_owned()) {
            warnings.push(format!("{label}: skipped missing/duplicate connection id"));
            continue;
        }
        let credential = match auth {
            connection::SelectedAuth::Workers => workers_credential.clone(),
            connection::SelectedAuth::Key(value) => {
                if policy::kilo(identity) && policy.kilo_free.is_none() {
                    warnings.push(format!(
                        "{label}: keyed Kilo requires --kilo-free-models; skipped to avoid cost"
                    ));
                    continue;
                }
                let id = unique(&format!("upstream-{source_id}"), &mut ids);
                let key = write_secret(&dir, &id, value)?;
                credentials.push(json!({"id":id,"kind":"api_key","provider":kind,"key":key}));
                Some(id)
            }
            connection::SelectedAuth::None => {
                if kind != KIND_OPENAI_COMPAT {
                    warnings.push(format!(
                        "{label}: skipped anonymous auth requires an OpenAI-compatible provider"
                    ));
                    continue;
                }
                let provider = connection::identity(c, array(&root, "providerNodes"));
                if provider == "aihorde" {
                    warnings.push(format!("{label}: AI Horde anonymous use may be restricted under load; imported without Authorization"));
                } else {
                    warnings.push(format!("{label}: Kilo anonymous access uses per-IP limits; imported without Authorization"));
                }
                None
            }
        };
        connections.push(Connection {
            headers: Value::Object(header_refs),
            source_id: source_id.into(),
            provider: upstream::ProviderIdentity::connection(
                string(c, "provider"),
                array(&root, "providerNodes"),
            ),
            credential,
            kind,
            base_url,
            priority: c["priority"].as_i64().unwrap_or(0),
            policy: if policy::cloudflare(identity) {
                policy::ModelPolicy::Workers
            } else if policy::kilo(identity) && matches!(auth, connection::SelectedAuth::Key(_)) {
                policy::ModelPolicy::KiloFree
            } else {
                policy::ModelPolicy::Any
            },
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
    if let Some(reference) = &policy.kilo_key {
        if policy.kilo_free.is_none() {
            warnings.push(
                "keyed Kilo requires --kilo-free-models; external key skipped to avoid cost".into(),
            );
        } else {
            let id = unique("kilo-external", &mut ids);
            credentials
                .push(json!({"id":id,"kind":"api_key","provider":"openai_compat","key":reference}));
            let mut providers: Vec<_> = connections
                .iter()
                .filter(|c| c.base_url == "https://ai-gateway.kilo.ai/api/v1")
                .map(|c| c.provider.clone())
                .collect();
            providers.extend(
                ["kilo", "kilocode"].map(|id| upstream::ProviderIdentity::Builtin(id.into())),
            );
            providers.sort();
            providers.dedup();
            let mut external_ids: HashSet<_> = array(&root, "providerConnections")
                .iter()
                .map(|c| string(c, "id").to_owned())
                .collect();
            for provider in providers {
                connections.push(Connection {
                    headers: json!({}),
                    source_id: unique(&format!("kilo-external-{provider}"), &mut external_ids),
                    provider,
                    credential: Some(id.clone()),
                    kind: "openai_compat".into(),
                    base_url: "https://ai-gateway.kilo.ai/api/v1".into(),
                    priority: i64::MAX,
                    policy: policy::ModelPolicy::KiloFree,
                });
            }
        }
    }
    connections.sort_by_key(|c| c.priority);
    let mut combos = HashMap::new();
    let mut combo_ids = HashMap::new();
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
        let id = string(entry, "id");
        if !id.is_empty() {
            combo_ids
                .entry(id.to_owned())
                .or_insert_with(|| name.clone());
        }
        combos.insert(name, combo);
    }
    let fallback_exclusions = fallback
        .map(|_| {
            let mut excluded = fallback::exclusions(&root, &combos, &combo_ids, "codex")?;
            if policy.openrouter_replacements {
                excluded.extend(fallback::exclusions(
                    &root,
                    &combos,
                    &combo_ids,
                    "openrouter",
                )?);
            }
            Ok::<_, String>(excluded)
        })
        .transpose()?;
    if policy.openrouter_replacements {
        for name in &order {
            if let Some(model) = name.strip_prefix("openrouter/") {
                if let Some(combo) = combos.get_mut(name) {
                    combo["models"] =
                        json!([
                            upstream::ProviderIdentity::Builtin("openrouter".into()).step(model)
                        ]);
                    warnings.push(format!(
                        "combo {name}: source combo replaced by reviewed official-only policy"
                    ));
                }
            }
        }
    }
    let nodes = array(&root, "providerNodes");
    let mut context = context::Overrides::load(&root, &mut warnings);
    let mut direct = direct::collect(&combos, nodes, &connections);
    replacements::seed_antigravity(&mut direct, &connections);
    if policy.openrouter_replacements {
        replacements::seed(&mut direct, &connections);
    }
    let mut unavailable_combos = HashSet::new();
    if policy.openrouter_replacements {
        let mappings = aliases::sorted_mappings(&root);
        let targets: HashSet<_> = mappings
            .iter()
            .filter_map(|row| combo_ids.get(string(row, "comboId")))
            .collect();
        loop {
            unavailable_combos.clear();
            for target in &targets {
                let mut deployments = Vec::new();
                Expander {
                    context: &mut context,
                    combos: &combos,
                    connections: &connections,
                    nodes,
                    warnings: &mut Vec::new(),
                    remaining_steps: 1024,
                    exhausted: false,
                    policy: &policy,
                }
                .expand(target, &mut Vec::new(), &mut deployments);
                if deployments.is_empty() {
                    unavailable_combos.insert((*target).clone());
                }
            }
            let blocked: Vec<_> = combos
                .iter()
                .filter(|(name, combo)| {
                    name.starts_with("openrouter/") && !array(combo, "models").is_empty()
                })
                .filter(|(name, _)| {
                    mappings
                        .iter()
                        .find(|row| aliases::matches(string(row, "pattern"), name))
                        .is_some_and(|mapping| {
                            aliases::mapping_target(
                                mapping,
                                &combos,
                                &combo_ids,
                                Some(&unavailable_combos),
                            )
                            .is_none()
                        })
                })
                .map(|(name, _)| name.clone())
                .collect();
            if blocked.is_empty() {
                break;
            }
            for name in blocked {
                if let Some(combo) = combos.get_mut(&name) {
                    combo["models"] = json!([]);
                }
            }
        }
    }
    let (aliases, mut synthetic, unavailable_aliases) = aliases::prepare(
        &root,
        &combos,
        &combo_ids,
        &direct,
        &mut order,
        &mut warnings,
        policy
            .openrouter_replacements
            .then_some(&unavailable_combos),
    );
    direct.extend(direct::collect(&synthetic, nodes, &connections));
    for (name, model) in direct {
        let official = policy.openrouter_replacements
            && name
                .strip_prefix("openrouter/")
                .is_some_and(|model| replacements::find(model).is_some())
            && !unavailable_aliases.contains(&name);
        if combos.contains_key(&name) || (!official && direct::reserved(&root, &name)) {
            warnings.push(format!(
                "direct {name}: collision; existing combo/alias wins"
            ));
        } else if let std::collections::hash_map::Entry::Vacant(entry) =
            synthetic.entry(name.clone())
        {
            order.push(name);
            entry.insert(model);
        }
    }
    let mut models = Vec::new();
    context.reset_usage();
    let mut deployment_ids = HashSet::new();
    let mut deployment_count = 0;
    for name in order {
        let lookup = if synthetic.contains_key(&name) {
            &synthetic
        } else {
            &combos
        };
        let combo = &lookup[&name];
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
            context: &mut context,
            combos: lookup,
            connections: &connections,
            nodes: array(&root, "providerNodes"),
            warnings: &mut warnings,
            remaining_steps: 1024,
            exhausted: false,
            policy: &policy,
        }
        .expand(&name, &mut Vec::new(), &mut deployments);
        morph::order(&name, &mut deployments, &connections, nodes, &mut warnings);
        if combos.contains_key(&name) {
            let steps = secondary::steps(
                &name,
                strategy,
                &deployments,
                &connections,
                &root,
                &mut warnings,
            );
            if !steps.is_empty() {
                let extra = HashMap::from([(name.clone(), json!({"models":steps}))]);
                Expander {
                    context: &mut context,
                    combos: &extra,
                    connections: &connections,
                    nodes,
                    warnings: &mut warnings,
                    remaining_steps: 1024,
                    exhausted: false,
                    policy: &policy,
                }
                .expand(&name, &mut Vec::new(), &mut deployments);
            }
        }
        if deployments.is_empty() {
            warnings.push(format!("combo {name}: skipped: no deployments"));
            continue;
        }
        for (i, dep) in deployments.iter_mut().enumerate() {
            dep["id"] = unique(&format!("{name}-{i}"), &mut deployment_ids).into();
            dep["priority"] = if strategy == "round-robin" { 0 } else { i }.into();
        }
        deployment_count += deployments.len();
        models.push(json!({"name":name,"aliases":[],"deployments":deployments}));
    }
    aliases::attach(&mut models, aliases, &mut warnings);
    context.report(&mut warnings);
    if let (Some(options), Some(exclusions)) = (fallback, fallback_exclusions) {
        let credential = unique("omniroute-fallback", &mut ids);
        credentials.push(
            json!({"id":credential,"kind":"api_key","provider":"openai_compat","key":options.key}),
        );
        fallback::append(
            &mut models,
            &options,
            &credential,
            &mut deployment_ids,
            exclusions,
        )?;
        deployment_count = models
            .iter()
            .map(|model| array(model, "deployments").len())
            .sum();
    }
    let credential_providers: HashMap<_, _> = credentials
        .iter()
        .map(|credential| (string(credential, "id"), string(credential, "provider")))
        .collect();
    for model in &mut models {
        if let Some(deployments) = model["deployments"].as_array_mut() {
            for deployment in deployments {
                let provider = credential_providers
                    .get(string(deployment, "credential"))
                    .copied()
                    .unwrap_or_else(|| string(deployment, "provider"));
                if matches!(provider, KIND_OPENAI | KIND_OPENAI_COMPAT) {
                    if deepseek_model(string(deployment, "upstream_model")) {
                        deployment["strip_request_fields"] =
                            json!(["seed", "user", "safety_identifier", "reasoning.effort"]);
                    } else if string(deployment, "base_url")
                        .parse::<http::Uri>()
                        .is_ok_and(|url| {
                            url.host().is_some_and(|host| {
                                host.eq_ignore_ascii_case("generativelanguage.googleapis.com")
                            })
                        })
                    {
                        deployment["strip_request_fields"] = json!(["seed", "user"]);
                    }
                }
            }
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
    validation::validate(
        &config,
        client_count == 0,
        &dir,
        &policy.external_references(),
    )?;
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
