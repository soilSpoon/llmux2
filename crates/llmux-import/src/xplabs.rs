use crate::{array, morph, string, upstream::ProviderIdentity, Connection};
use serde_json::{json, Value};

pub(super) const BASE_URL: &str = "https://api.experientiallabs.ai/v1";
const PREFIX: &str = "experlabs";

// OCI verified usage.cost 0 on the existing key for these models, and logprobs on DeepSeek
// (Free tier: 10 RPM, 1M tokens/day shared). Haiku 5.5, Qwen3.8 27B and the standard 1017
// variants were billed, so only legs whose model is listed here survive. The
// `uncensored-fast:free` promotion ended audience-gated — on 2026-11-25 the key got
// `model_not_granted` ("selected audience") and no plan discounts the uncensored slug to
// zero — so the pair was removed; a promo that comes back must be re-verified with
// `--scan-free experlabs` before rejoining. Reviewed combo -> XPLabs model id.
pub(crate) const FREE: &[(&str, &str)] = &[
    ("gpt-6-luna", "gpt-6-luna"),
    ("deepseek-v4.1-flash", "deepseek-v4.1-flash"),
    ("deepseek-v4.1-flash-fast", "deepseek-v4.1-flash"),
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

/// Whether `model` on the XPLabs endpoint is verified free. Billed legs (Haiku 5.5, Qwen3.8
/// 27B, the 1017 model) must never be generated while the account is overdrawn, so expansion
/// drops them before limits, availability scans or ordering ever see them.
pub(super) fn free_model(model: &str) -> bool {
    FREE.iter().any(|(_, free)| *free == model)
}

/// Places `free` first (after the verified Morph route in fast combos). Declared legs for the
/// same model and endpoint are moved instead, keeping their pinned connections.
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
    let (declared, rest): (Vec<_>, Vec<_>) = deployments
        .drain(..)
        .partition(|dep| free.iter().any(|f| same(f, dep)));
    *deployments = rest;
    let free = if declared.is_empty() { free } else { declared };
    let at = if name.ends_with("fast") {
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
            "combo {name}: prioritized reviewed free XPLabs route"
        ));
    }
}
