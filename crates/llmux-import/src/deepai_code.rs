use crate::{string, Connection};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::Path;

/// Non-secret DeepAI Code discovery output (`--deepai-code-state`): where a
/// reachable model bridge listens plus which catalog models are free
/// (`theosPerRequest == "0"`) and, when known, inside the session token's
/// model scope. Produced on the app host by the discovery tool; the bridge
/// URL must already be reachable from the llmux2 host (the app's ephemeral
/// loopback port directly, or a tunnel/omni-bridge address mapped to it).
pub(crate) struct State {
    pub base_url: String,
    pub free: HashSet<String>,
    scope: Option<HashSet<String>>,
}

impl State {
    pub(super) fn load(path: &Path) -> Result<Self, String> {
        let content = fs_err(path)?;
        let root: Value =
            serde_json::from_str(&content).map_err(|_| "invalid deepai-code state JSON")?;
        let base_url = string(&root, "base_url").to_owned();
        if !is_bridge_url(&base_url) {
            return Err(format!(
                "deepai-code state base_url must be a loopback bridge URL, got {base_url:?}"
            ));
        }
        let scope = match root.get("scope") {
            Some(_) => Some(ids(&root, "scope")?),
            None => None,
        };
        Ok(State {
            base_url,
            free: ids(&root, "free")?,
            scope,
        })
    }
}

fn fs_err(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|_| "could not read deepai-code state file".into())
}

fn ids(root: &Value, field: &str) -> Result<HashSet<String>, String> {
    let Some(value) = root.get(field) else {
        return Err(format!("deepai-code state {field} must be an array"));
    };
    let Some(items) = value.as_array() else {
        return Err(format!("deepai-code state {field} must be an array"));
    };
    let mut out = HashSet::new();
    for (i, item) in items.iter().enumerate() {
        let Some(id) = item.as_str().map(str::trim) else {
            return Err(format!("deepai-code state {field}[{i}] must be a string"));
        };
        if id.is_empty() {
            return Err(format!("deepai-code state {field}[{i}] is empty"));
        }
        out.insert(id.to_owned());
    }
    Ok(out)
}

/// The app's model bridge is a loopback OpenAI-compatible listener on an
/// ephemeral port (the app plants `…/v1`); the OmniRoute-side bridge exposes
/// the same session as `…/deepai/v1`. Both path shapes are the reviewed
/// DeepAI Code upstream — nothing else on loopback qualifies.
pub(crate) fn is_bridge_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://127.0.0.1:") else {
        return false;
    };
    let (port, path) = rest.split_once('/').unwrap_or((rest, ""));
    port.bytes().all(|b| b.is_ascii_digit())
        && port.parse::<u16>().is_ok_and(|port| port != 0)
        && matches!(path, "v1" | "deepai/v1")
}

// Canonical combo name -> DeepAI Code catalog id, reviewed against the app's
// `userdata/secrets/deepai-model-catalog.bin` (its theosPerRequest table) and
// the harness `models.json`. Free catalog ids without a reviewed canonical
// name are reported, never guessed onto a lookalike combo.
const MODEL_MAP: &[(&str, &str)] = &[
    ("claude-haiku-5.5", "claude-haiku-5-5"),
    ("deepseek-v4.1-flash", "deepseek-v4.1-flash"),
    ("glm-5.3-flash", "glm-5.3-flash"),
    ("qwen-3.8-flash", "qwen-3.8-flash"),
    ("mimo-2.6-flash", "mimo-2.6-flash"),
    ("mimo-2.6-pro", "mimo-2.6-pro"),
    ("minimax-m3.1-flash", "minimax-m3.1-flash"),
    ("gpt-6-luna", "gpt-6-luna"),
];

/// A step on the keyed DeepAI Code connection for `name`'s catalog id, or
/// `None` when the model is metered, outside the session token's scope, or
/// no keyed deepai-code connection exists.
pub(crate) fn step(
    name: &str,
    state: &State,
    connections: &[Connection],
    warnings: &mut Vec<String>,
) -> Option<Value> {
    let (_, model) = MODEL_MAP.iter().find(|(combo, _)| *combo == name)?;
    if !state.free.contains(*model) {
        return None;
    }
    if let Some(scope) = &state.scope {
        if !scope.contains(*model) {
            warnings.push(format!(
                "combo {name}: skipped DeepAI Code leg {model} — outside the session token's model scope"
            ));
            return None;
        }
    }
    let connection = connections
        .iter()
        .find(|c| c.kind == "openai_compat" && c.credential.is_some() && discovered(c))?;
    let mut step = connection.provider.step(model);
    step["connectionId"] = json!(connection.source_id);
    step["deepaiCodeDiscovery"] = json!(true);
    Some(step)
}

/// Whether `model` on the discovered bridge is cataloged free
/// (`theosPerRequest == "0"`). Metered models must never be generated — a
/// premier leg spends the account's daily Theos allowance.
pub(crate) fn free(state: &State, model: &str) -> bool {
    state.free.contains(model)
}

/// Free catalog ids the model map does not cover — surfaced so a new DeepAI
/// free model is a review decision, not silent.
pub(crate) fn unmapped(state: &State) -> impl Iterator<Item = &String> {
    state
        .free
        .iter()
        .filter(|id| !MODEL_MAP.iter().any(|(_, model)| model == *id))
}

/// Whether the raw export connection is the reviewed DeepAI Code identity:
/// the `deepai-code` builtin, or a custom node exposing the `deepai` prefix
/// through an OpenAI-compatible protocol. URL shape alone is not identity —
/// keyed Cloudflare-direct bridges may use the same loopback `/v1` shape.
pub(crate) fn reviewed_source(connection: &Value, nodes: &[Value]) -> bool {
    let provider = string(connection, "provider");
    nodes
        .iter()
        .find(|node| string(node, "id") == provider)
        .map_or(provider == "deepai-code", |node| {
            string(node, "prefix") == "deepai" && string(node, "type") == "openai-compatible"
        })
}

/// Whether `model` is outside the discovered session-token scope.
pub(crate) fn outside_scope(state: &State, model: &str) -> bool {
    state
        .scope
        .as_ref()
        .is_some_and(|scope| !scope.contains(model))
}

/// Connections the discovery stage may rewire. Marked at import time from
/// the reviewed provider identity, never inferred from a loopback URL alone.
pub(crate) fn discovered(connection: &Connection) -> bool {
    connection.policy == crate::policy::ModelPolicy::DeepAiCode
}
