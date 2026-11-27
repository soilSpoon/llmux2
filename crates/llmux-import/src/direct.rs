use super::{array, string, upstream, Connection};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};

pub(super) fn collect(
    combos: &HashMap<String, Value>,
    nodes: &[Value],
    connections: &[Connection],
) -> BTreeMap<String, Value> {
    let mut direct = BTreeMap::new();
    for combo in combos.values() {
        for step in array(combo, "models") {
            let raw = step
                .as_str()
                .unwrap_or_else(|| string(step, "model"))
                .trim();
            if step["usable"] == false
                || !super::connection::valid_restrictions(step)
                || !matches!(string(step, "kind"), "" | "model")
                || (string(step, "kind") != "model"
                    && string(step, "providerId").is_empty()
                    && string(step, "provider").is_empty()
                    && combos.contains_key(raw))
            {
                continue;
            }
            let Some((provider, model)) = upstream::model_target(step, nodes) else {
                continue;
            };
            let node = nodes.iter().find(|node| matches!(&provider, upstream::ProviderIdentity::Custom(id) if id == string(node, "id")));
            if model.contains(['*', '?'])
                || model.chars().any(char::is_whitespace)
                || !connections
                    .iter()
                    .any(|connection| connection.provider == provider)
            {
                continue;
            }
            let mut prefixes = vec![provider.as_str()];
            if let Some(node) = node {
                let prefix = string(node, "prefix");
                if !prefix.is_empty() && upstream::canonical_provider(prefix, nodes) == provider {
                    prefixes.push(prefix);
                }
            } else {
                prefixes.extend(upstream::PROVIDER_ALIASES.iter().filter_map(|(alias, id)| {
                    (provider.is_builtin(id)
                        && upstream::canonical_provider(alias, nodes) == provider)
                        .then_some(*alias)
                }));
            }
            let mut names: Vec<_> = prefixes
                .into_iter()
                .map(|prefix| format!("{prefix}/{model}"))
                .collect();
            if raw
                .split_once('/')
                .is_some_and(|(prefix, _)| upstream::canonical_provider(prefix, nodes) == provider)
            {
                names.push(raw.to_owned());
            }
            for name in names {
                direct.insert(
                    name.clone(),
                    json!({"name":name,"models":[provider.step(model)]}),
                );
            }
        }
    }
    direct
}

pub(super) fn reserved(root: &Value, name: &str) -> bool {
    root["modelAliases"].get(name).is_some()
        || super::aliases::REVIEWED_EXACT_ALIASES
            .iter()
            .any(|(alias, _)| *alias == name)
        || array(root, "modelComboMappings").iter().any(|row| {
            (row["enabled"] == true || row["enabled"] == 1)
                && super::aliases::matches(string(row, "pattern"), name)
        })
}
