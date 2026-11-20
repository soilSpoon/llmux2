use crate::{array, morph, string, upstream::ProviderIdentity, Connection};
use serde_json::{json, Value};

pub(super) const BASE_URL: &str = "https://api.experientiallabs.ai/v1";
const PREFIX: &str = "experlabs";

// https://www.experientiallabs.ai/pricing: Luna and DeepSeek V4.1 Flash are on the standing
// Free tier (10 RPM, 1M tokens/day shared); Haiku 5.5 and Qwen3.8 27B are $0 promotions.
// Reviewed exported combo name -> XPLabs model id.
const FREE: &[(&str, &str)] = &[
    ("gpt-6-luna", "gpt-6-luna"),
    ("deepseek-v4.1-flash", "deepseek-v4.1-flash"),
    ("deepseek-v4.1-flash-fast", "deepseek-v4.1-flash"),
    ("deepseek-v4.1-flash-1017", "deepseek-v4.1-flash-1017"),
    ("deepseek-v4.1-flash-1017-fast", "deepseek-v4.1-flash-1017"),
    (
        "deepseek-v4.1-flash-1017-ultrafast",
        "deepseek-v4.1-flash-1017",
    ),
    ("claude-haiku-5.5", "claude-haiku-5.5"),
    ("qwen3.8-27b", "qwen3.8-27b"),
];

/// The OmniRoute step for the reviewed free XPLabs model of `name`, pinned to an eligible
/// API-key connection, or `None` when the combo is unreviewed or has no such connection.
pub(super) fn step(
    name: &str,
    strategy: &str,
    deployments: &[Value],
    connections: &[Connection],
    root: &Value,
    warnings: &mut Vec<String>,
) -> Option<Value> {
    let (_, model) = FREE.iter().find(|(combo, _)| *combo == name)?;
    if deployments.is_empty() {
        return None;
    }
    let connection = connections.iter().find(|c| {
        let ProviderIdentity::Custom(id) = &c.provider else {
            return false;
        };
        c.kind == "openai_compat"
            && c.credential.is_some()
            && c.base_url == BASE_URL
            && array(root, "providerNodes")
                .iter()
                .any(|node| string(node, "id") == id && string(node, "prefix") == PREFIX)
            && array(root, "providerConnections").iter().any(|source| {
                string(source, "id") == c.source_id
                    && string(source, "provider") == id
                    && matches!(string(source, "authType"), "apikey" | "api_key" | "api-key")
                    && source["isActive"] != false
                    && source["usable"] != false
            })
    })?;
    if strategy == "round-robin" {
        warnings.push(format!(
            "combo {name}: skipped reviewed free XPLabs route for round-robin strategy"
        ));
        return None;
    }
    let mut step = connection.provider.step(model);
    step["connectionId"] = json!(connection.source_id);
    Some(step)
}

/// Places `free` first, after any verified Morph logprobs route, dropping duplicate legs.
pub(super) fn place(
    name: &str,
    deployments: &mut Vec<Value>,
    free: Vec<Value>,
    connections: &[Connection],
    warnings: &mut Vec<String>,
) {
    let original: Vec<_> = deployments
        .iter()
        .map(|dep| (dep["base_url"].clone(), dep["upstream_model"].clone()))
        .collect();
    let same = |a: &Value, b: &Value| {
        a["base_url"] == b["base_url"] && a["upstream_model"] == b["upstream_model"]
    };
    deployments.retain(|dep| !free.iter().any(|f| same(f, dep)));
    let at = if morph::LOGPROBS_COMBOS.contains(&name) {
        deployments
            .iter()
            .rposition(|dep| morph::is_verified(dep, connections))
            .map_or(0, |i| i + 1)
    } else {
        0
    };
    deployments.splice(at..at, free);
    let placed: Vec<_> = deployments
        .iter()
        .map(|dep| (dep["base_url"].clone(), dep["upstream_model"].clone()))
        .collect();
    if placed != original {
        warnings.push(format!(
            "combo {name}: reviewed free XPLabs route placed first"
        ));
    }
}
