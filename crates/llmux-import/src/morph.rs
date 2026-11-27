use crate::{string, upstream, Connection};
use serde_json::Value;

pub(super) const OFFICIAL_URL: &str = "https://api.morphllm.com/v1";

/// Upstream ids the reviewed Morph Ultra scope allows without per-token
/// list-price billing: Kimi K3.1 plus the DeepSeek V4.1 Flash variants. The
/// abliterated DeepSeek row still requires Morph privileged access, but it
/// is never list-price billed when that access is approved. Kimi K3, Kimi K3
/// Fast, and Kimi Abliterated are deliberately absent: Morph's Ultra prose
/// calls morph-kimik3 a K3.1 alias, but Morph's machine-readable model
/// catalog identifies it as moonshotai/Kimi-K3 rather than
/// moonshotai/Kimi-K3.1-Pro. Exact whitelisting uses the canonical K3.1 id
/// only; Kimi Abliterated is billed per token even then.
pub(super) const ULTRA_MODELS: &[&str] = &[
    "morph-kimik31",
    "morph-dsv41flash",
    "morph-dsv41flash-1017",
    "morph-dsv41flash-abliterated",
];

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

fn connection<'a>(dep: &Value, connections: &'a [Connection]) -> Option<&'a Connection> {
    connections.iter().find(|c| {
        c.credential
            .as_deref()
            .is_some_and(|id| id == string(dep, "credential"))
    })
}

pub(super) fn is_verified(dep: &Value, connections: &[Connection]) -> bool {
    string(dep, "upstream_model") == "morph-dsv41flash-1017"
        && connection(dep, connections)
            .is_some_and(|c| c.provider.is_builtin("morph") && c.base_url == OFFICIAL_URL)
}

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
    let connection = |dep: &Value| connection(dep, connections);
    let vibe = upstream::canonical_provider("vibe", nodes);
    let is_morph = |dep: &Value| is_verified(dep, connections);
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
        let Some(mut last_morph) = deployments.iter().rposition(is_morph) else {
            *deployments = original;
            return;
        };
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

#[cfg(test)]
mod tests {
    use super::order;
    use serde_json::json;

    #[test]
    fn missing_morph_position_preserves_empty_and_nonempty_orders() {
        for name in ["deepseek-v4.1-flash", "deepseek-v4.1-flash-fast"] {
            for mut deployments in [
                vec![],
                vec![
                    json!({"upstream_model":"deepseek/deepseek-v4.1-flash", "_bridge_replacement":true}),
                    json!({"upstream_model":"deepseek-v4-1-flash-1017"}),
                ],
            ] {
                let expected = deployments.clone();
                let mut warnings = Vec::new();
                order(name, &mut deployments, &[], &[], &mut warnings);
                assert_eq!(deployments, expected);
                assert!(warnings.is_empty());
            }
        }
    }
}
