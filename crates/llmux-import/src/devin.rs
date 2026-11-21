//! Devin CLI bridge (`devin-cli-agentic`, prefix `dva/`) and the providers the
//! owner excluded: connection handling and free-first deployment order.

use crate::{string, Connection};
use llmux_types::{parse_devin_models, provider::DEVIN_ACP};
use serde_json::Value;
use std::{collections::HashSet, fs, path::Path};

pub(super) const PROVIDER: &str = "devin-cli-agentic";

pub(super) fn bridge(provider: &str) -> bool {
    matches!(provider, PROVIDER | "dva")
}

/// Providers dropped from llmux2: Augment and Z.AI have no balance to spend,
/// Factory was removed, and lmarena/uncloseai stay inactive.
pub(super) fn excluded(provider: &str) -> Option<&'static str> {
    match provider {
        "auggie" | "aug" => Some("Augment (auggie) is excluded: no recharge, free models only"),
        "zcode" | "zc" => Some("ZCode (Z.AI) is excluded: no recharge, free models only"),
        "factory" | "droid" => Some("Factory is removed from llmux2"),
        "lmarena" | "arena" => Some("lmarena is inactive"),
        "uncloseai" => Some("uncloseai is inactive"),
        _ => None,
    }
}

/// Model ids `devin models` marks Free, read from a saved listing.
pub(super) fn free_models(path: &Path) -> Result<HashSet<String>, String> {
    let listing = fs::read_to_string(path).map_err(|_| "could not read Devin model listing")?;
    let models = parse_devin_models(&listing);
    if models.is_empty() {
        return Err("Devin model listing has no models; save the output of `devin models`".into());
    }
    Ok(models
        .into_iter()
        .filter(|model| model.free)
        .map(|model| model.id)
        .collect())
}

/// Moves Devin-plan (paid) deployments behind every other deployment, keeping
/// relative order. Without a listing every Devin model counts as paid, so an
/// unverified model never precedes a free route.
pub(super) fn free_first(
    deployments: &mut Vec<Value>,
    connections: &[Connection],
    free: Option<&HashSet<String>>,
) {
    let paid = |dep: &Value| {
        connections
            .iter()
            .any(|c| c.kind == DEVIN_ACP && c.base_url == string(dep, "base_url"))
            && string(dep, "provider") == DEVIN_ACP
            && !free.is_some_and(|free| free.contains(string(dep, "upstream_model")))
    };
    let (paid, rest): (Vec<_>, Vec<_>) = deployments.drain(..).partition(paid);
    deployments.extend(rest);
    deployments.extend(paid);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exclusions_cover_names_and_prefixes() {
        for id in [
            "auggie",
            "aug",
            "zcode",
            "zc",
            "factory",
            "lmarena",
            "uncloseai",
        ] {
            assert!(excluded(id).is_some(), "{id}");
        }
        assert!(excluded(PROVIDER).is_none());
    }

    #[test]
    fn paid_devin_deployments_go_last_in_stable_order() {
        let connection = |kind: &str| Connection {
            headers: json!({}),
            source_id: "c".into(),
            provider: crate::upstream::ProviderIdentity::Builtin("x".into()),
            credential: None,
            kind: kind.into(),
            base_url: llmux_types::provider::DEVIN_ACP_BASE_URL.into(),
            priority: 0,
            policy: crate::policy::ModelPolicy::Any,
        };
        let dep = |model: &str, provider: &str| json!({"base_url":llmux_types::provider::DEVIN_ACP_BASE_URL,"provider":provider,"upstream_model":model});
        let mut deployments = vec![
            dep("gpt-6-luna-high", DEVIN_ACP),
            dep("gpt-6-luna", "openai_compat"),
            dep("swe-2-high", DEVIN_ACP),
        ];
        let free = HashSet::from(["swe-2-high".to_owned()]);
        free_first(&mut deployments, &[connection(DEVIN_ACP)], Some(&free));
        let order: Vec<_> = deployments
            .iter()
            .map(|d| string(d, "upstream_model"))
            .collect();
        assert_eq!(order, ["gpt-6-luna", "swe-2-high", "gpt-6-luna-high"]);
    }
}
