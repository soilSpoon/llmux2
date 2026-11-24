//! Import OmniRoute JSON exports and SQLite table dumps into a static llmux configuration.

mod aliases;
mod bridge;
mod connection;
mod context;
mod db_dump;
mod devin;
mod direct;
mod fallback;
mod morph;
mod nous;
mod policy;
mod replacements;
mod secondary;
mod upstream;
mod validation;
mod xplabs;

use llmux_types::provider::{
    ANTHROPIC as KIND_ANTHROPIC, DEEPAI as KIND_DEEPAI, DEVIN_ACP as KIND_DEVIN_ACP,
    OPENAI as KIND_OPENAI, OPENAI_COMPAT as KIND_OPENAI_COMPAT,
    OPENAI_RESPONSES as KIND_OPENAI_RESPONSES,
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
    /// Optional per-minute request cap for imported Workers AI deployments;
    /// defaults to `policy::WORKERS_DEFAULT_RPM` and requires the Cloudflare
    /// opt-in. The hard daily token cap `policy::WORKERS_TPD` always applies.
    pub workers_ai_rpm: Option<u32>,
    pub kilo_free_models: Option<&'a Path>,
    pub kilo_key_file: Option<&'a Path>,
    pub openrouter_replacements: bool,
    /// Saved `devin models` output; its `Free` rows rank ahead of Devin-plan models.
    pub devin_models: Option<&'a Path>,
    /// Nous Portal login state (`llmux-login nous`) and a saved `GET /v1/models`
    /// listing; together they add `nous/<id>` models, free ones first.
    pub nous_state: Option<&'a Path>,
    pub nous_models: Option<&'a Path>,
    pub omniroute_fallback: Option<OmniRouteFallback<'a>>,
    /// Emitted `[server].listen` address; defaults to `127.0.0.1:8080`.
    /// Scheduled re-imports must pass the deployed address — deploy rejects a
    /// regenerated config whose listen differs from the installed one.
    pub listen: Option<&'a str>,
    /// Site overlay TOML merged into the emitted config before validation:
    /// `[server]`/`[routing]` keys and extra `[[credentials]]`/`[[models]]`
    /// the export does not carry. Scheduled re-imports must pass the site's
    /// overlay or the regenerated config drops its local settings.
    pub overlay: Option<&'a Path>,
    /// JSON list of dead route names (e.g. `llmux2-probe --dead-file` output).
    /// Listed models are dropped from the import instead of being advertised,
    /// their aliases are pruned, and with `--omniroute-fallback` they land in
    /// the catch-all's `exclude` so the wildcard never serves them again.
    pub exclude_models: Option<&'a Path>,
    /// JSON list of `{"provider": ..., "evidence": ...}` entries marking
    /// custom provider legs free: `provider` matches the provider-node id or
    /// the connection's base-url host, and `evidence` must record how the
    /// operator confirmed the provider is free (a plan or pricing page, or
    /// the account usage API). Custom legs bill unless listed.
    pub free_providers: Option<&'a Path>,
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
    ("deepai", KIND_DEEPAI, upstream::DEEPAI_BASE),
    ("deepai-code", KIND_OPENAI_COMPAT, ""),
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

/// Anthropic rejects `temperature` and `top_p` unless they equal 1 on the
/// Claude 5 generation, Fable 5 and Opus 4.8.
fn anthropic_rejects_sampling(model: &str) -> bool {
    let leaf = model.rsplit('/').next().unwrap_or_default();
    let normalized = leaf.to_ascii_lowercase().replace('.', "-");
    let parts: Vec<_> = normalized.split('-').collect();
    matches!(parts.as_slice(), ["claude", family, "5", ..] if family.chars().all(|c| c.is_ascii_alphabetic()))
        || matches!(parts.as_slice(), ["claude", "opus", "4", "8", ..])
}

fn deployment_identity(
    dep: &Value,
) -> (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
) {
    (
        string(dep, "credential").to_owned(),
        string(dep, "upstream_model").to_owned(),
        string(dep, "base_url").to_owned(),
        header_identity(&dep["headers"]),
        identity_field(&dep["strip_request_fields"]),
        identity_field(&dep["max_context"]),
        identity_field(&dep["quota"]),
        identity_field(&dep["timeout_ms"]),
    )
}

fn identity_field(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Object(map) if map.is_empty() => String::new(),
        Value::Array(items) if items.is_empty() => String::new(),
        other => other.to_string(),
    }
}

fn header_identity(headers: &Value) -> String {
    let Some(map) = headers.as_object() else {
        return String::new();
    };
    if map.is_empty() {
        return String::new();
    }
    let mut parts: Vec<_> = map
        .iter()
        .map(|(name, value)| format!("{name}={}", header_material(value.as_str().unwrap_or(""))))
        .collect();
    parts.sort();
    parts.join("\n")
}

/// A `file:` header reference is compared by the file bytes when that file can
/// be read at import time. If it cannot be read, the reference string itself
/// is the identity, so two unreadable paths are not treated as the same header.
fn header_material(reference: &str) -> String {
    let Some(path) = reference.strip_prefix("file:") else {
        return reference.to_owned();
    };
    if path.is_empty() {
        return reference.to_owned();
    }
    match std::fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => reference.to_owned(),
    }
}

fn fallback_leg(dep: &Value) -> bool {
    dep["fallback"] == true || string(dep, "id").starts_with("omniroute-fallback")
}

fn dedupe_deployments(models: &mut [Value], ids: &mut HashSet<String>, warnings: &mut Vec<String>) {
    for model in models.iter_mut() {
        let name = string(model, "name").to_owned();
        let Some(deployments) = model.get_mut("deployments").and_then(Value::as_array_mut) else {
            continue;
        };
        let mut order: Vec<usize> = (0..deployments.len()).collect();
        order.sort_by_key(|&index| (deployments[index]["priority"].as_i64().unwrap_or(0), index));
        let mut seen = HashSet::new();
        let mut keep = Vec::new();
        for index in order {
            if seen.insert(deployment_identity(&deployments[index])) {
                keep.push(index);
            }
        }
        let dropped = deployments.len() - keep.len();
        if dropped == 0 {
            continue;
        }
        let noun = if dropped == 1 {
            "deployment"
        } else {
            "deployments"
        };
        warnings.push(format!("{name}: dropped {dropped} duplicate {noun}"));
        *deployments = keep
            .into_iter()
            .map(|index| deployments[index].clone())
            .collect();
        renumber_deployments(deployments, &name, ids);
    }
}

fn renumber_deployments(deployments: &mut [Value], name: &str, ids: &mut HashSet<String>) {
    for dep in deployments.iter() {
        if !fallback_leg(dep) {
            ids.remove(string(dep, "id"));
        }
    }
    let native_priorities: Vec<i64> = deployments
        .iter()
        .filter(|dep| !fallback_leg(dep))
        .map(|dep| dep["priority"].as_i64().unwrap_or(0))
        .collect();
    let uniform = native_priorities.windows(2).all(|pair| pair[0] == pair[1]);
    let shared = native_priorities.first().copied().unwrap_or(0);
    let mut native_index = 0usize;
    let mut sequential = 0i64;
    let mut fallback_priority = if native_priorities.is_empty() {
        0
    } else if uniform {
        shared + 1
    } else {
        i64::try_from(native_priorities.len()).unwrap_or(i64::MAX)
    };
    for dep in deployments.iter_mut() {
        if fallback_leg(dep) {
            dep["priority"] = json!(fallback_priority);
            fallback_priority = fallback_priority.saturating_add(1);
            continue;
        }
        dep["id"] = unique(&format!("{name}-{native_index}"), ids).into();
        native_index += 1;
        if !uniform {
            dep["priority"] = json!(sequential);
            sequential += 1;
        }
    }
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
    // Scheduled re-imports regenerate the same credentials into a shared
    // secrets directory: an identical existing file is reused; a differing
    // one (rotated by hand or a changed export) still fails closed.
    if let Ok(existing) = fs::read_to_string(&path) {
        if existing == secret {
            return Ok(format!("file:{}", path.display()));
        }
        return Err(
            "could not create secret file (existing files are never overwritten)".to_owned(),
        );
    }
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
    dead: &'a HashSet<String>,
}

fn references_dead(
    combos: &HashMap<String, Value>,
    name: &str,
    dead: &HashSet<String>,
    visited: &mut HashSet<String>,
) -> bool {
    if !visited.insert(name.to_owned()) {
        return false;
    }
    let Some(combo) = combos.get(name) else {
        return false;
    };
    for step in array(combo, "models") {
        if step["usable"] == false {
            continue;
        }
        let raw = step
            .as_str()
            .unwrap_or_else(|| string(step, "model"))
            .trim();
        let target = if string(step, "kind") == "combo-ref" {
            string(step, "comboName")
        } else if combos.contains_key(raw) {
            raw
        } else {
            ""
        };
        if !target.is_empty()
            && (dead.contains(target) || references_dead(combos, target, dead, visited))
        {
            return true;
        }
    }
    false
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
                if self.dead.contains(target) {
                    self.warnings.push(format!(
                        "combo {name} step {index}: skipped dead reference {target}"
                    ));
                    continue;
                }
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
            let native_antigravity = self.connections.iter().any(|connection| {
                connection.provider.is_builtin("antigravity") && connection.kind == "antigravity"
            });
            let antigravity = p.is_builtin("antigravity")
                && replacements::antigravity(model).is_some()
                && !native_antigravity;
            let bridge = self
                .policy
                .openrouter_replacements
                .then(|| bridge::source(&p, self.nodes))
                .flatten();
            let redirected = antigravity || bridge.is_some();
            let replacement = if antigravity {
                if !pinned.is_empty() || !allowed.is_empty() {
                    self.warnings.push(format!("combo {name} step {index}: reviewed Antigravity replacement uses eligible Gemini connections instead of source connection restrictions"));
                }
                replacements::antigravity(model)
            } else if let Some(source) = bridge {
                let Some(replacement) = bridge::find(source, model) else {
                    self.warnings.push(format!("combo {name} step {index}: no reviewed bridge replacement for {source}/{model}"));
                    continue;
                };
                if !pinned.is_empty() || !allowed.is_empty() {
                    self.warnings.push(format!("combo {name} step {index}: reviewed bridge replacement uses eligible official connections instead of source connection restrictions"));
                }
                Some(replacement)
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
                        } else if bridge.is_some() {
                            bridge::matches(r, c)
                        } else {
                            replacements::matches(r, c)
                        }
                    }) && if redirected {
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
                .filter(|_| !redirected)
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
                if c.base_url == xplabs::BASE_URL && !xplabs::free_model(&model) {
                    self.warnings.push(format!(
                        "combo {name} step {index}: dropped billed XPLabs route {model}"
                    ));
                    continue;
                }
                let mut deployment = json!({"base_url":c.base_url,"upstream_model":model});
                deployment["billing"] = if policy::billed(c, &model, self.policy) {
                    "billed".into()
                } else {
                    "free".into()
                };
                if bridge.is_some() {
                    deployment["_bridge_replacement"] = true.into();
                }
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
                    deployment["quota"] = json!({"rpm":self.policy.workers_rpm});
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

fn overlay_value(value: toml::Value) -> Result<Value, String> {
    Ok(match value {
        toml::Value::String(s) => Value::String(s),
        toml::Value::Integer(n) => json!(n),
        toml::Value::Float(f) => json!(f),
        toml::Value::Boolean(b) => Value::Bool(b),
        toml::Value::Datetime(d) => Value::String(d.to_string()),
        toml::Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(overlay_value)
                .collect::<Result<_, _>>()?,
        ),
        toml::Value::Table(table) => {
            let mut object = serde_json::Map::new();
            for (key, value) in table {
                object.insert(key, overlay_value(value)?);
            }
            Value::Object(object)
        }
    })
}

fn merge_overlay(config: &mut Value, overlay: Value) -> Result<(), String> {
    let (config, overlay) = match (config.as_object_mut(), overlay.as_object()) {
        (Some(config), Some(overlay)) => (config, overlay),
        _ => return Err("site overlay must be a TOML table".to_owned()),
    };
    for (key, value) in overlay {
        match (config.get_mut(key), value) {
            (Some(Value::Object(existing)), Value::Object(extra)) => {
                for (inner, inner_value) in extra {
                    existing.insert(inner.clone(), inner_value.clone());
                }
            }
            (Some(Value::Array(existing)), Value::Array(extra)) => {
                existing.extend(extra.iter().cloned());
            }
            (_, value) => {
                config.insert(key.clone(), value.clone());
            }
        }
    }
    Ok(())
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
    let mut dead: HashSet<String> = match options.exclude_models {
        Some(path) => serde_json::from_str::<Vec<String>>(
            &fs::read_to_string(path)
                .map_err(|e| format!("could not read --exclude-models {}: {e}", path.display()))?,
        )
        .ok()
        .and_then(|list| {
            list.iter()
                .all(|name| !name.is_empty() && !name.contains('*'))
                .then(|| list.into_iter().collect())
        })
        .ok_or_else(|| {
            format!(
                "--exclude-models {} must be a JSON array of nonempty model names",
                path.display()
            )
        })?,
        None => HashSet::new(),
    };
    let nous = match (options.nous_state, options.nous_models) {
        (None, None) => None,
        (Some(state), Some(models)) => Some(nous::Nous::load(state, models)?),
        _ => return Err("--nous-state and --nous-models must be provided together".into()),
    };
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
        credentials.push(json!({"id":id,"kind":"api_key","provider":"openai_compat","key":workers.reference,"quota":{"tpd":policy::WORKERS_TPD}}));
        warnings.push(format!("Workers AI deployments cap at rpm={} each and share one credential tpd={} estimated-token daily limit across the account, NOT an account-wide neuron/daily spending cap; enforce free-only billing at Cloudflare", policy.workers_rpm, policy::WORKERS_TPD));
        connections.push(Connection {headers:json!({}),source_id:"workers-ai-opt-in".into(),provider:upstream::ProviderIdentity::Builtin("workers-ai".into()),credential:Some(id.clone()),kind:"openai_compat".into(),base_url:workers.base_url.clone(),priority:i64::MAX,policy:policy::ModelPolicy::Workers});
        id
    });
    // The site overlay is read once here: the self-referential guard below
    // needs the emitted listen port, which comes from --listen, else the
    // overlay's [server].listen, else the 8080 default — and the merged
    // config reuses the same parsed value later.
    let overlay = options
        .overlay
        .map(|path| {
            overlay_value(
                toml::from_str::<toml::Value>(
                    &fs::read_to_string(path)
                        .map_err(|_| "could not read site overlay TOML".to_owned())?,
                )
                .map_err(|_| "invalid site overlay TOML".to_owned())?,
            )
        })
        .transpose()?;
    // Connections resolving to this gateway's own listen port through a
    // container host alias are the OmniRoute -> llmux2 bridge; importing
    // them would loop requests. Guard the port the emitted config actually
    // binds: --listen, else the overlay's [server].listen, else 8080.
    let listen_port = match options.listen {
        Some(listen) => listen
            .parse::<std::net::SocketAddr>()
            .map_or(8080, |addr| addr.port()),
        None => overlay
            .as_ref()
            .and_then(|value| value["server"]["listen"].as_str())
            .and_then(|listen| listen.parse::<std::net::SocketAddr>().ok())
            .map_or(8080, |addr| addr.port()),
    };
    for (index, c) in array(&root, "providerConnections").iter().enumerate() {
        let label = format!("connection {index} ({})", string(c, "id"));
        let identity = connection::identity(c, array(&root, "providerNodes"));
        if let Some(reason) = devin::excluded(identity) {
            warnings.push(format!("{label}: skipped {reason}"));
            continue;
        }
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
        } else if let connection::SelectedAuth::Oauth(oauth) = &auth {
            Ok((oauth.kind.to_owned(), oauth.base_url.to_owned()))
        } else {
            upstream::resolve(
                c,
                array(&root, "providerNodes"),
                matches!(auth, connection::SelectedAuth::Key(_)),
                &mut warnings,
                listen_port,
            )
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
        let source_id = string(c, "id");
        let mut header_refs = serde_json::Map::new();
        for (name, value) in headers {
            // Deterministic per connection+header name so a scheduled
            // re-import resolves the same secret file; the `header-` prefix
            // can never collide with `upstream-{source_id}` credential names.
            let id = unique(&format!("header-{source_id}-{name}"), &mut ids);
            header_refs.insert(name, write_secret(&dir, &id, &value)?.into());
        }
        if source_id.is_empty() || !source_ids.insert(source_id.to_owned()) {
            warnings.push(format!("{label}: skipped missing/duplicate connection id"));
            continue;
        }
        let keyed = matches!(auth, connection::SelectedAuth::Key(_));
        let workers_auth = matches!(auth, connection::SelectedAuth::Workers);
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
            connection::SelectedAuth::Oauth(oauth) => {
                let id = unique(&format!("upstream-{source_id}"), &mut ids);
                let refresh = write_secret(&dir, &id, &oauth.refresh)?;
                let mut credential = json!({
                    "id": id,
                    "kind": "oauth",
                    "provider": kind,
                    "token_url": oauth.token_url,
                    "client_id": oauth.client_id,
                    "refresh_token": refresh,
                });
                if let Some(secret) = &oauth.client_secret {
                    let secret_id = unique(&format!("{id}-secret"), &mut ids);
                    credential["client_secret"] = write_secret(&dir, &secret_id, secret)?.into();
                }
                if let Some(scopes) = &oauth.scopes {
                    credential["scopes"] = json!(scopes);
                }
                credentials.push(credential);
                if kind == "openai_responses" {
                    header_refs.insert("originator".into(), "codex_cli_rs".into());
                    header_refs.insert("version".into(), "0.165.0".into());
                    header_refs.insert(
                        "user-agent".into(),
                        "codex-cli/0.165.0 (Linux; x86_64)".into(),
                    );
                    header_refs.insert("origin".into(), "https://chatgpt.com".into());
                    if let Some(workspace) = &oauth.workspace {
                        header_refs.insert("chatgpt-account-id".into(), workspace.clone().into());
                    }
                }
                if kind == "antigravity" {
                    if let Some(project) = &oauth.project {
                        header_refs.insert("x-llmux-project".into(), project.clone().into());
                    }
                }
                Some(id)
            }
            connection::SelectedAuth::None if kind == KIND_DEVIN_ACP => {
                warnings.push(format!("{label}: Devin CLI bridge uses the host's own devin credentials; none imported"));
                None
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
            base_url,
            priority: c["priority"].as_i64().unwrap_or(0),
            policy: if workers_auth {
                policy::ModelPolicy::Workers
            } else if policy::kilo(identity) && keyed {
                policy::ModelPolicy::KiloFree
            } else if kind == KIND_DEEPAI {
                policy::ModelPolicy::DeepAiText
            } else {
                policy::ModelPolicy::Any
            },
            kind,
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
        bridge::seed(&mut direct, nodes, &connections);
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
                    dead: &dead,
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
    let (mut aliases, mut synthetic, unavailable_aliases) = aliases::prepare(
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
    // Aliases into a dead route die with it and join the exclusion set so the
    // catch-all cannot resurrect them either.
    aliases.retain(|alias, target| {
        let keep = !dead.contains(alias) && !dead.contains(target);
        if !keep {
            dead.insert(alias.clone());
        }
        keep
    });
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
    for name in order {
        if dead.contains(&name) {
            warnings.push(format!(
                "model {name}: dropped dead route listed in --exclude-models"
            ));
            continue;
        }
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
            dead: &dead,
        }
        .expand(&name, &mut Vec::new(), &mut deployments);
        // Verified-free legs go ahead of billed ones; stable sort keeps each
        // group's existing relative order. Deliberate placement passes below
        // (Morph logprobs order, XPLabs placement, secondary expansion) still
        // decide the positions of the legs they manage.
        deployments.sort_by_key(|dep| usize::from(string(dep, "billing") != "free"));
        morph::order(&name, &mut deployments, &connections, nodes, &mut warnings);
        if let Some(step) = combos
            .contains_key(&name)
            .then(|| {
                xplabs::step(
                    &name,
                    strategy,
                    &deployments,
                    &connections,
                    &root,
                    &mut warnings,
                )
            })
            .flatten()
        {
            let mut free = Vec::new();
            let combo = HashMap::from([(name.clone(), json!({"models":[step]}))]);
            Expander {
                context: &mut context,
                combos: &combo,
                connections: &connections,
                nodes,
                warnings: &mut warnings,
                remaining_steps: 1,
                exhausted: false,
                policy: &policy,
                dead: &dead,
            }
            .expand(&name, &mut Vec::new(), &mut free);
            xplabs::place(&name, &mut deployments, free, &connections, &mut warnings);
        }
        devin::free_first(&mut deployments, &connections, policy.devin_free.as_ref());
        if policy.openrouter_replacements && combos.contains_key(&name) {
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
                    dead: &dead,
                }
                .expand(&name, &mut Vec::new(), &mut deployments);
            }
        }
        for deployment in &mut deployments {
            if let Some(fields) = deployment.as_object_mut() {
                fields.remove("_bridge_replacement");
            }
        }
        let mut nonchat = Vec::new();
        deployments.retain(|dep| {
            let chat = !policy::non_chat(string(dep, "upstream_model"));
            if !chat {
                nonchat.push(string(dep, "upstream_model").to_owned());
            }
            chat
        });
        for model in nonchat {
            warnings.push(format!(
                "combo {name}: dropped non-chat model {model} (no chat endpoint)"
            ));
        }
        if deployments.is_empty() {
            warnings.push(format!("combo {name}: skipped: no deployments"));
            // A combo that collapsed entirely into dead references is dead
            // itself: the catch-all must not serve its name either.
            if references_dead(lookup, &name, &dead, &mut HashSet::new()) {
                dead.insert(name.clone());
            }
            continue;
        }
        for (i, dep) in deployments.iter_mut().enumerate() {
            dep["id"] = unique(&format!("{name}-{i}"), &mut deployment_ids).into();
            dep["priority"] = if strategy == "round-robin" { 0 } else { i }.into();
        }
        models.push(json!({"name":name,"aliases":[],"deployments":deployments}));
    }
    // Combos that collapsed during emit can leave aliases pointing at them:
    // sweep again so those names also join the catch-all exclusion set.
    aliases.retain(|alias, target| {
        let keep = !dead.contains(alias) && !dead.contains(target);
        if !keep {
            dead.insert(alias.clone());
        }
        keep
    });
    aliases::attach(&mut models, aliases, &mut warnings);
    context.report(&mut warnings);
    if let (Some(options), Some(exclusions)) = (fallback, fallback_exclusions) {
        let credential = unique("omniroute-fallback", &mut ids);
        credentials.push(
            json!({"id":credential,"kind":"api_key","provider":"openai_compat","key":options.key}),
        );
        let mut exclusions = exclusions.clone();
        exclusions.extend(dead.iter().cloned());
        let session_excluded = exclusions.clone();
        fallback::append(
            &mut models,
            &options,
            &credential,
            &mut deployment_ids,
            exclusions,
        )?;
        fallback::ensure_session_models(
            &mut models,
            &fallback::session_names(&combos, nodes),
            &options,
            &credential,
            &mut deployment_ids,
            &session_excluded,
        )?;
    }
    if let Some(nous) = &nous {
        let credential = unique("nous", &mut ids);
        let taken: HashSet<String> = models
            .iter()
            .flat_map(|model| {
                std::iter::once(string(model, "name")).chain(
                    array(model, "aliases")
                        .iter()
                        .map(|alias| alias.as_str().unwrap_or("")),
                )
            })
            .map(str::to_owned)
            .collect();
        let mut added = nous.models(&credential, &taken, &mut deployment_ids, &mut warnings);
        added.retain(|model| !dead.contains(string(model, "name")));
        if !added.is_empty() {
            credentials.push(nous.credential(&credential));
            models.extend(added);
        }
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
                } else if provider == KIND_ANTHROPIC
                    && anthropic_rejects_sampling(string(deployment, "upstream_model"))
                {
                    deployment["strip_request_fields"] = json!(["temperature", "top_p"]);
                }
            }
        }
    }
    dedupe_deployments(&mut models, &mut deployment_ids, &mut warnings);
    let deployment_count: usize = models
        .iter()
        .map(|model| array(model, "deployments").len())
        .sum();
    if models.is_empty() {
        if warnings
            .iter()
            .any(|warning| warning.contains("--omniroute-fallback"))
        {
            return Err("no importable models; pass --omniroute-fallback and --omniroute-key to keep serving DuckDuckGo and Cloudflare Playground through OmniRoute".into());
        }
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
    let owner_oauth: HashSet<String> = credentials
        .iter()
        .filter(|credential| {
            string(credential, "kind") == "oauth"
                && string(credential, "provider") == KIND_OPENAI_RESPONSES
        })
        .map(|credential| string(credential, "id").to_owned())
        .collect();
    if !owner_oauth.is_empty() {
        for model in &mut models {
            if array(model, "deployments")
                .iter()
                .any(|deployment| owner_oauth.contains(string(deployment, "credential")))
            {
                model["allowed_keys"] = json!(client_keys);
            }
        }
    }
    let mut config = json!({"server":{"listen":options.listen.unwrap_or("127.0.0.1:8080"),"client_keys":client_keys},"credentials":credentials,"models":models});
    if let Some(overlay) = overlay {
        // An overlay [server].listen is a site choice when no --listen was
        // passed, but it must never silently override the deployed address a
        // scheduled re-import was told to keep.
        if let (Some(expected), Some(actual)) =
            (options.listen, overlay["server"]["listen"].as_str())
        {
            if actual != expected {
                return Err(format!(
                    "site overlay listen {actual} conflicts with --listen {expected}"
                ));
            }
        }
        merge_overlay(&mut config, overlay)?;
        // Overlay models are routes too: drop the ones the sweep marked dead
        // and shield their names from the catch-all.
        if !dead.is_empty() {
            if let Some(list) = config["models"].as_array_mut() {
                list.retain(|model| !dead.contains(model["name"].as_str().unwrap_or("")));
                for model in list.iter_mut() {
                    if model["name"].as_str() == Some("*") {
                        if let Some(exclude) = model["exclude"].as_array_mut() {
                            for name in &dead {
                                if !exclude.iter().any(|e| e.as_str() == Some(name)) {
                                    exclude.push(json!(name));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    validation::validate(&config, &dir)?;
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

#[cfg(test)]
mod dedupe_tests {
    use super::*;

    fn run(models: &mut [Value]) -> Vec<String> {
        let mut ids = models
            .iter()
            .flat_map(|model| array(model, "deployments").iter())
            .map(|dep| string(dep, "id").to_owned())
            .collect();
        let mut warnings = Vec::new();
        dedupe_deployments(models, &mut ids, &mut warnings);
        warnings
    }

    #[test]
    fn headers_and_strip_fields_keep_otherwise_identical_legs() {
        let mut models = vec![json!({
            "name": "m",
            "deployments": [
                {"id":"m-0","credential":"c","upstream_model":"u","base_url":"https://example.com/v1","priority":0,"headers":{"x-a":"1"}},
                {"id":"m-1","credential":"c","upstream_model":"u","base_url":"https://example.com/v1","priority":1,"headers":{"x-a":"2"}},
                {"id":"m-2","credential":"c","upstream_model":"u","base_url":"https://example.com/v1","priority":2,"strip_request_fields":["seed"]},
                {"id":"m-3","credential":"c","upstream_model":"u","base_url":"https://example.com/v1","priority":3},
                {"id":"m-4","credential":"c","upstream_model":"u","base_url":"https://example.com/v1","priority":4,"headers":{"x-a":"1"}},
                {"id":"m-5","credential":"c","upstream_model":"u","base_url":"https://example.com/v1","priority":5,"headers":{}}
            ]
        })];
        let warnings = run(&mut models);
        let deps = models[0]["deployments"].as_array().unwrap();
        assert_eq!(deps.len(), 4);
        assert_eq!(
            deps.iter().map(|dep| string(dep, "id")).collect::<Vec<_>>(),
            vec!["m-0", "m-1", "m-2", "m-3"]
        );
        assert_eq!(warnings, vec!["m: dropped 2 duplicate deployments"]);
    }

    #[test]
    fn duplicate_fallback_is_dropped_and_its_id_is_not_reused() {
        let mut models = vec![json!({
            "name": "m",
            "deployments": [
                {"id":"m-0","credential":"native","upstream_model":"u","base_url":"https://up.example/v1","priority":0},
                {"id":"m-2","credential":"native","upstream_model":"u","base_url":"https://up.example/v1","priority":2},
                {"id":"omniroute-fallback-m","credential":"omniroute-fallback","upstream_model":"m","base_url":"http://127.0.0.1:20128/v1","priority":3,"fallback":true},
                {"id":"omniroute-fallback-m-2","credential":"omniroute-fallback","upstream_model":"m","base_url":"http://127.0.0.1:20128/v1","priority":4,"fallback":true}
            ]
        })];
        let warnings = run(&mut models);
        let deps = models[0]["deployments"].as_array().unwrap();
        assert_eq!(deps.len(), 2);
        assert_eq!(deps[0]["id"], "m-0");
        assert_eq!(deps[0]["priority"], 0);
        assert_eq!(deps[1]["id"], "omniroute-fallback-m");
        assert_eq!(deps[1]["priority"], 1);
        assert_eq!(warnings, vec!["m: dropped 2 duplicate deployments"]);
    }

    #[test]
    fn routing_fields_and_unreadable_header_files_stay_distinct() {
        let dir = tempfile::tempdir().unwrap();
        let left = dir.path().join("left");
        let right = dir.path().join("right");
        std::fs::write(&left, "same-bytes").unwrap();
        std::fs::write(&right, "same-bytes").unwrap();
        let missing_a = dir.path().join("missing-a");
        let missing_b = dir.path().join("missing-b");
        let mut models = vec![json!({
            "name": "m",
            "deployments": [
                {"id":"m-0","credential":"","upstream_model":"u","base_url":"https://example.com/v1","priority":0,"max_context":8192,"headers":{"x-a": format!("file:{}", left.display())}},
                {"id":"m-1","credential":"","upstream_model":"u","base_url":"https://example.com/v1","priority":1,"max_context":8192,"headers":{"x-a": format!("file:{}", right.display())}},
                {"id":"m-2","credential":"","upstream_model":"u","base_url":"https://example.com/v1","priority":2,"max_context":32768},
                {"id":"m-3","credential":"","upstream_model":"u","base_url":"https://example.com/v1","priority":3,"quota":{"rpm":1}},
                {"id":"m-4","credential":"","upstream_model":"u","base_url":"https://example.com/v1","priority":4,"quota":{"rpm":2}},
                {"id":"m-5","credential":"","upstream_model":"u","base_url":"https://example.com/v1","priority":5,"timeout_ms":1000},
                {"id":"m-6","credential":"","upstream_model":"u","base_url":"https://example.com/v1","priority":6,"timeout_ms":2000},
                {"id":"m-7","credential":"","upstream_model":"u","base_url":"https://example.com/v1","priority":7,"headers":{"x-a": format!("file:{}", missing_a.display())}},
                {"id":"m-8","credential":"","upstream_model":"u","base_url":"https://example.com/v1","priority":8,"headers":{"x-a": format!("file:{}", missing_b.display())}}
            ]
        })];
        let warnings = run(&mut models);
        let deps = models[0]["deployments"].as_array().unwrap();
        assert_eq!(deps.len(), 8);
        assert!(deps.iter().any(|dep| dep["max_context"] == 8192));
        assert!(deps.iter().any(|dep| dep["max_context"] == 32768));
        assert!(deps.iter().any(|dep| dep["quota"]["rpm"] == 1));
        assert!(deps.iter().any(|dep| dep["quota"]["rpm"] == 2));
        assert!(deps.iter().any(|dep| dep["timeout_ms"] == 1000));
        assert!(deps.iter().any(|dep| dep["timeout_ms"] == 2000));
        let unread: Vec<_> = deps
            .iter()
            .filter_map(|dep| dep["headers"]["x-a"].as_str())
            .filter(|value| value.contains("missing-"))
            .collect();
        assert_eq!(unread.len(), 2);
        assert_ne!(unread[0], unread[1]);
        assert_eq!(warnings, vec!["m: dropped 1 duplicate deployment"]);
    }
}
