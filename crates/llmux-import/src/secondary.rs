use crate::{array, string, upstream::ProviderIdentity, Connection};
use serde_json::{json, Value};

struct ReviewedSecondary {
    combo: &'static str,
    prefix: &'static str,
    base_url: &'static str,
    models: &'static [&'static str],
}

// OCI verified Responses JSON and SSE on the direct Respan route.
const ROUTES: &[ReviewedSecondary] = &[ReviewedSecondary {
    combo: "gpt-6-luna",
    prefix: "respan",
    base_url: "https://api.respan.ai/api/v1",
    models: &["gpt-6-luna", "openai/gpt-6-luna", "perplexity/gpt-6-luna"],
}];

pub(super) fn steps(
    name: &str,
    strategy: &str,
    deployments: &[Value],
    connections: &[Connection],
    root: &Value,
    warnings: &mut Vec<String>,
) -> Vec<Value> {
    let Some(route) = ROUTES.iter().find(|route| route.combo == name) else {
        return Vec::new();
    };
    if deployments.is_empty() {
        return Vec::new();
    }
    if strategy == "round-robin" {
        warnings.push(format!(
            "combo {name}: skipped reviewed secondary for round-robin strategy"
        ));
        return Vec::new();
    }
    let Some(connection) = connections.iter().find(|c| {
        let ProviderIdentity::Custom(id) = &c.provider else {
            return false;
        };
        id != "codex"
            && c.kind == "openai_compat"
            && c.credential.is_some()
            && c.base_url == route.base_url
            && array(root, "providerNodes")
                .iter()
                .any(|node| string(node, "id") == id && string(node, "prefix") == route.prefix)
            && array(root, "providerConnections").iter().any(|source| {
                string(source, "id") == c.source_id
                    && string(source, "provider") == id
                    && matches!(string(source, "authType"), "apikey" | "api_key" | "api-key")
                    && source["isActive"] != false
                    && source["usable"] != false
            })
    }) else {
        return Vec::new();
    };
    let steps: Vec<_> = route
        .models
        .iter()
        .filter(|model| {
            !deployments.iter().any(|dep| {
                string(dep, "base_url") == route.base_url
                    && string(dep, "upstream_model") == **model
            })
        })
        .map(|model| {
            let mut step = connection.provider.step(model);
            step["connectionId"] = json!(connection.source_id);
            step
        })
        .collect();
    if !steps.is_empty() {
        warnings.push(format!(
            "combo {name}: appended reviewed secondary routes after source legs"
        ));
    }
    steps
}
