use crate::{
    replacements, string,
    upstream::{self, ProviderIdentity},
    Connection,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub(super) fn source(provider: &ProviderIdentity, nodes: &[Value]) -> Option<&'static str> {
    if provider.is_builtin("duckduckgo-web") {
        Some("duckduckgo-web")
    } else if provider.is_builtin("cloudflare-direct") {
        Some("cfpd")
    } else {
        let node = nodes.iter().find(|node| {
            matches!(provider, ProviderIdentity::Custom(id) if id == string(node, "id"))
                && string(node, "type") == "openai-compatible"
        })?;
        match string(node, "prefix") {
            "deepai" => Some("deepai"),
            "cloudflare-direct" => Some("cfpd"),
            _ => None,
        }
    }
}

pub(super) fn find(source: &str, model: &str) -> Option<&'static replacements::Route> {
    routes(source).iter().find(|route| route.0 == model)
}

pub(super) fn matches(route: &replacements::Route, connection: &Connection) -> bool {
    connection.credential.is_some()
        && if route.1 == "numes" {
            connection.kind == "openai_compat"
                && connection.base_url == "https://api.numes.ai/api/v1"
        } else {
            replacements::matches(route, connection)
        }
}

pub(super) fn seed(
    direct: &mut BTreeMap<String, Value>,
    nodes: &[Value],
    connections: &[Connection],
) {
    let providers = std::iter::once(ProviderIdentity::Builtin("duckduckgo-web".into()))
        .chain(std::iter::once(ProviderIdentity::Builtin(
            "cloudflare-direct".into(),
        )))
        .chain(
            nodes
                .iter()
                .map(|node| ProviderIdentity::Custom(string(node, "id").into())),
        );
    for provider in providers {
        let Some(source) = source(&provider, nodes) else {
            continue;
        };
        let canonical = upstream::canonical_provider(source, nodes) == provider;
        if matches!(provider, ProviderIdentity::Builtin(_)) && !canonical {
            continue;
        }
        let mut prefixes = vec![provider.as_str()];
        if canonical && provider.as_str() != source {
            prefixes.push(source);
        }
        for route in routes(source) {
            if connections
                .iter()
                .any(|connection| matches(route, connection))
            {
                for prefix in &prefixes {
                    let name = format!("{prefix}/{}", route.0);
                    direct
                        .entry(name.clone())
                        .or_insert_with(|| json!({"name":name,"models":[provider.step(route.0)]}));
                }
            }
        }
    }
}

fn routes(source: &str) -> &'static [replacements::Route] {
    match source {
        "duckduckgo-web" => DUCKDUCKGO,
        "cfpd" => CFPD,
        "deepai" => DEEPAI,
        _ => &[],
    }
}

// The cfpd sidecar is a pod-internal Cloudflare bridge the llmux2 host
// cannot reach, and Workers policy gates it to free-eligible models.
// OCI verified Morph's morph-kimik31 serves this traffic on the
// already-imported Morph key.
const CFPD: &[replacements::Route] = &[("moonshotai/kimi-k3.1-pro", "morph", "morph-kimik31")];

// OmniRoute open-sse/executors/duckduckgo-web/models.ts and
// open-sse/config/freeModelCatalog.data.ts identify these exact models.
const DUCKDUCKGO: &[replacements::Route] = &[
    // registry/duckduckgo-web + registry/anthropic: Claude Haiku 4.5, not 5.5.
    ("claude-haiku-4-5", "anthropic", "claude-haiku-4-5"),
    // freeModelCatalog: Mistral Small 4 (2603); same imported api.mistral.ai ID.
    ("mistral-small-2603", "mistral", "mistral-small-2603"),
    // freeModelCatalog: gpt-oss 120B; Workers registry + existing imported deployment.
    ("tinfoil/gpt-oss-120b", "workers", "@cf/openai/gpt-oss-120b"),
];

// DeepAI's built-in registry is image-only. These are the export's custom
// DeepAI Code node's declared chat IDs, not an integration with api.deepai.org.
const DEEPAI: &[replacements::Route] = &[
    // Export modelAliases deepai/claude-fable-5-2; existing Anthropic deployment.
    ("claude-fable-5-2", "anthropic", "claude-fable-5-2"),
    // Export modelAliases deepai/claude-opus-5-5; existing Anthropic deployment.
    ("claude-opus-5-5", "anthropic", "claude-opus-5-5"),
    // Export modelAliases deepai/claude-sonnet-5-5; existing Anthropic deployment.
    ("claude-sonnet-5-5", "anthropic", "claude-sonnet-5-5"),
    // Export modelAliases deepai/gemini-4-argon; existing Gemini OpenAI deployment.
    ("gemini-4-argon", "gemini", "gemini-4-argon"),
    // Export modelAliases deepai/gemini-4-carbon; existing Gemini OpenAI deployment.
    ("gemini-4-carbon", "gemini", "gemini-4-carbon"),
    // Export modelAliases deepai/gemini-4-hydrogen; existing Gemini OpenAI deployment.
    ("gemini-4-hydrogen", "gemini", "gemini-4-hydrogen"),
    // Export deepseek-v4.1-flash combo: same version/Flash on Numes, without a dated upgrade.
    (
        "deepseek-v4.1-flash",
        "numes",
        "deepseek/deepseek-v4.1-flash",
    ),
    // Export glm-5.3-flash combo: Numes z-ai/glm-5.3-flash:free; imported unsuffixed ID too.
    ("glm-5.3-flash", "numes", "z-ai/glm-5.3-flash"),
    // Export mimo-v2.6-flash combo declares these two IDs for the same Flash model.
    // FLAGGED: Numes 404s `xiaomi/mimo-v2.6-flash` (only the unofficial
    // deepai-code pod bridge served it, and unofficial bridges are excluded);
    // the deployment emits but the upstream does not serve it.
    ("mimo-2.6-flash", "numes", "xiaomi/mimo-v2.6-flash"),
    // Export mimo-v2.6-pro combo declares these two IDs for the same Pro model.
    // FLAGGED: same dead upstream as mimo-v2.6-flash.
    ("mimo-2.6-pro", "numes", "xiaomi/mimo-v2.6-pro"),
    // Export minimax-m3.1-flash combo; same M3.1 Flash existing Numes deployment.
    ("minimax-m3.1-flash", "numes", "minimax/minimax-m3.1-flash"),
    // Export qwen3.8-flash combo; not Qwen3.8 27B/Max or a different Flash version.
    ("qwen-3.8-flash", "numes", "qwen/qwen3.8-flash"),
];
