use crate::{array, string, upstream::ProviderIdentity, Connection};
use serde_json::{json, Value};

struct ReviewedSecondary {
    combo: &'static str,
    prefix: &'static str,
    base_url: &'static str,
    models: &'static [&'static str],
    builtin: bool,
}

// OCI verified Responses JSON and SSE on the direct Respan route.
const ROUTES: &[ReviewedSecondary] = &[
    ReviewedSecondary {
        combo: "gpt-6-luna",
        prefix: "respan",
        base_url: "https://api.respan.ai/api/v1",
        models: &["gpt-6-luna", "openai/gpt-6-luna", "perplexity/gpt-6-luna"],
        builtin: false,
    },
    // kimi-k3.1-pro's cfpd leg resolves to a pod-internal Cloudflare sidecar
    // the llmux2 host cannot reach (Workers policy also gates the model).
    // OCI verified Morph's morph-kimik31 serves this traffic; appended last so
    // free-first routing keeps the declared numes leg primary.
    ReviewedSecondary {
        combo: "kimi-k3.1-pro",
        prefix: "morph",
        base_url: "https://api.morphllm.com/v1",
        models: &["morph-kimik31"],
        builtin: true,
    },
];

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
        if c.kind != "openai_compat" || c.credential.is_none() || c.base_url != route.base_url {
            return false;
        }
        let id: &str = match &c.provider {
            // Builtin providers (morph) have no provider node: match on the
            // provider name when it is the route's prefix.
            ProviderIdentity::Builtin(name) => {
                if name.as_str() == route.prefix {
                    name.as_str()
                } else {
                    return false;
                }
            }
            // Routes with a builtin provider name never fall through to a
            // custom node carrying the same prefix, which would substitute an
            // unreviewed credential for the reviewed builtin key.
            ProviderIdentity::Custom(_) if route.builtin => return false,
            ProviderIdentity::Custom(id) => {
                if id == "codex"
                    || !array(root, "providerNodes").iter().any(|node| {
                        string(node, "id") == id.as_str() && string(node, "prefix") == route.prefix
                    })
                {
                    return false;
                }
                id.as_str()
            }
        };
        array(root, "providerConnections").iter().any(|source| {
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
