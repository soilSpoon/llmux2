use crate::{string, upstream, Connection};
use serde_json::Value;

pub(super) const OFFICIAL_URL: &str = "https://api.morphllm.com/v1";

// OCI verified the existing Morph key lists morph-dsv41flash-1017 and returns logprobs:
// https://api.morphllm.com/v1/models
// Mistral rejects logprobs for deepseek-v4-1-flash-1017. Reviewed exported combo names only.
const LOGPROBS_COMBOS: &[&str] = &[
    "deepseek-v4.1-flash",
    "deepseek-v4.1-flash-fast",
    "deepseek-v4.1-flash-1017",
    "deepseek-v4.1-flash-1017-fast",
    "deepseek-v4.1-flash-1017-ultrafast",
];

pub(super) fn order(
    name: &str,
    deployments: &mut Vec<Value>,
    connections: &[Connection],
    nodes: &[Value],
    warnings: &mut Vec<String>,
) {
    if !LOGPROBS_COMBOS.contains(&name) {
        return;
    }
    let connection = |dep: &Value| {
        connections.iter().find(|c| {
            c.credential
                .as_deref()
                .is_some_and(|id| id == string(dep, "credential"))
        })
    };
    let vibe = upstream::canonical_provider("vibe", nodes);
    let Some(first) = deployments.iter().position(|dep| {
        string(dep, "upstream_model") == "deepseek-v4-1-flash-1017"
            && connection(dep).is_some_and(|c| {
                c.provider == vibe
                    && matches!(c.kind.as_str(), "openai" | "openai_compat")
                    && c.base_url == "https://api.mistral.ai/v1"
            })
    }) else {
        return;
    };
    let mut insert = first;
    let mut index = first + 1;
    while index < deployments.len() {
        let dep = &deployments[index];
        if string(dep, "upstream_model") == "morph-dsv41flash-1017"
            && connection(dep)
                .is_some_and(|c| c.provider.is_builtin("morph") && c.base_url == OFFICIAL_URL)
        {
            let dep = deployments.remove(index);
            deployments.insert(insert, dep);
            insert += 1;
        }
        index += 1;
    }
    if insert != first {
        warnings.push(format!("combo {name}: reviewed Morph logprobs order places Morph ahead of Vibe/Mistral; other legs retain declared order"));
    }
}
