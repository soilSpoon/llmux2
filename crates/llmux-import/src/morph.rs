use crate::{string, upstream, Connection};
use serde_json::Value;

pub(super) const OFFICIAL_URL: &str = "https://api.morphllm.com/v1";

// OCI verified the existing Morph key lists morph-dsv41flash-1017 and returns logprobs:
// https://api.morphllm.com/v1/models
// Mistral rejects logprobs for deepseek-v4-1-flash-1017. Reviewed exported combo names only.
// OCI could not verify Numes logprobs (503): Morph precedes bridge replacements;
// fast combos start with Morph, while base combos retain declared Numes first.
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
    let is_morph = |dep: &Value| {
        string(dep, "upstream_model") == "morph-dsv41flash-1017"
            && connection(dep)
                .is_some_and(|c| c.provider.is_builtin("morph") && c.base_url == OFFICIAL_URL)
    };
    if !deployments.iter().any(is_morph) {
        return;
    }
    let original = deployments.clone();
    let first = deployments.iter().position(|dep| {
        string(dep, "upstream_model") == "deepseek-v4-1-flash-1017"
            && connection(dep).is_some_and(|c| {
                c.provider == vibe
                    && matches!(c.kind.as_str(), "openai" | "openai_compat")
                    && c.base_url == "https://api.mistral.ai/v1"
            })
    });
    if name.ends_with("fast") {
        let (morph, other): (Vec<_>, Vec<_>) = deployments.drain(..).partition(is_morph);
        deployments.extend(morph);
        deployments.extend(other);
    } else {
        if let Some(first) = first {
            let mut insert = first;
            let mut index = first + 1;
            while index < deployments.len() {
                if is_morph(&deployments[index]) {
                    let dep = deployments.remove(index);
                    deployments.insert(insert, dep);
                    insert += 1;
                }
                index += 1;
            }
        }
        let mut last_morph = deployments.iter().rposition(is_morph).unwrap();
        let mut bridges = Vec::new();
        let mut index = 0;
        while index < last_morph {
            if deployments[index]["_bridge_replacement"] == true {
                bridges.push(deployments.remove(index));
                last_morph -= 1;
            } else {
                index += 1;
            }
        }
        deployments.splice(last_morph + 1..last_morph + 1, bridges);
    }
    if *deployments != original {
        warnings.push(format!("combo {name}: reviewed Morph logprobs order places Morph ahead of Vibe/Mistral and bridge replacements; fast combos start with Morph, base combos retain declared Numes priority"));
    }
}
