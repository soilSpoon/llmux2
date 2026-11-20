use crate::{aliases, array, string, unique, upstream, OmniRouteFallback};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

struct CodexRoutes<'a> {
    root: &'a Value,
    combos: &'a HashMap<String, Value>,
    combo_ids: &'a HashMap<String, String>,
    aliases: BTreeMap<String, String>,
}

impl CodexRoutes<'_> {
    fn provider(&self, provider: &str) -> bool {
        let nodes = array(self.root, "providerNodes");
        let canonical = upstream::canonical_provider(provider, nodes);
        canonical == "codex"
            || nodes
                .iter()
                .any(|node| string(node, "id") == canonical && string(node, "prefix") == "codex")
    }

    fn reaches(
        &self,
        name: &str,
        stack: &mut HashSet<String>,
        budget: &mut usize,
    ) -> Result<bool, String> {
        if *budget == 0 || stack.len() >= 128 {
            return Err("Codex exclusion discovery exceeded its safety budget".into());
        }
        *budget -= 1;
        if !stack.insert(name.to_owned()) {
            return Ok(false);
        }
        let result = self.visit(name, stack, budget);
        stack.remove(name);
        result
    }

    fn visit(
        &self,
        name: &str,
        stack: &mut HashSet<String>,
        budget: &mut usize,
    ) -> Result<bool, String> {
        if let Some(combo) = self.combos.get(name) {
            for step in array(combo, "models") {
                let raw = step
                    .as_str()
                    .unwrap_or_else(|| string(step, "model"))
                    .trim();
                if string(step, "kind") == "combo-ref" {
                    if self.reaches(string(step, "comboName"), stack, budget)? {
                        return Ok(true);
                    }
                } else {
                    let provider = match string(step, "providerId") {
                        "" => string(step, "provider"),
                        id => id,
                    };
                    if (!provider.is_empty() && self.provider(provider))
                        || (provider.is_empty() && self.reaches(raw, stack, budget)?)
                    {
                        return Ok(true);
                    }
                }
            }
            return Ok(false);
        }
        if let Some(combo) = name
            .strip_prefix("combo/")
            .filter(|name| self.combos.contains_key(*name))
        {
            return self.reaches(combo, stack, budget);
        }
        for mapping in array(self.root, "modelComboMappings")
            .iter()
            .filter(|row| row["enabled"] == true || row["enabled"] == 1)
        {
            if aliases::matches(string(mapping, "pattern"), name) {
                if let Some(target) = self.combo_ids.get(string(mapping, "comboId")) {
                    if self.reaches(target, stack, budget)? {
                        return Ok(true);
                    }
                }
            }
        }
        if let Some(target) = self.aliases.get(name) {
            if self.reaches(target, stack, budget)? {
                return Ok(true);
            }
        }
        Ok(name
            .split_once('/')
            .is_some_and(|(provider, _)| self.provider(provider)))
    }

    fn contains(&self, name: &str) -> Result<bool, String> {
        self.reaches(name, &mut HashSet::new(), &mut 10_000)
    }
}

fn insert(exclusions: &mut BTreeSet<String>, pattern: &str) -> Result<(), String> {
    if pattern.is_empty()
        || pattern.contains('?')
        || pattern.strip_suffix('*').unwrap_or(pattern).contains('*')
    {
        return Err(format!("Codex exclusion pattern cannot be represented as an exact ID or a single trailing * prefix: {pattern:?}"));
    }
    exclusions.insert(pattern.to_owned());
    Ok(())
}

pub(super) fn exclusions(
    root: &Value,
    combos: &HashMap<String, Value>,
    combo_ids: &HashMap<String, String>,
) -> Result<BTreeSet<String>, String> {
    let routes = CodexRoutes {
        root,
        combos,
        combo_ids,
        aliases: aliases::alias_map(root, &mut Vec::new()),
    };
    let mut excluded = BTreeSet::from(["codex/*".to_owned()]);
    for (alias, provider) in upstream::PROVIDER_ALIASES {
        if routes.provider(provider) {
            insert(&mut excluded, &format!("{alias}/*"))?;
        }
    }
    for node in array(root, "providerNodes") {
        if routes.provider(string(node, "id")) || routes.provider(string(node, "prefix")) {
            for field in ["id", "prefix"] {
                let prefix = string(node, field);
                if !prefix.is_empty() {
                    insert(&mut excluded, &format!("{prefix}/*"))?;
                }
            }
        }
    }
    for name in combos.keys().chain(routes.aliases.keys()) {
        if routes.contains(name)? {
            insert(&mut excluded, name)?;
        }
    }
    for mapping in array(root, "modelComboMappings")
        .iter()
        .filter(|row| row["enabled"] == true || row["enabled"] == 1)
    {
        if let Some(target) = combo_ids.get(string(mapping, "comboId")) {
            if routes.contains(target)? {
                insert(&mut excluded, string(mapping, "pattern"))?;
            }
        }
    }
    Ok(excluded)
}

fn is_excluded(excluded: &BTreeSet<String>, name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    excluded.iter().any(|pattern| {
        let pattern = pattern.to_ascii_lowercase();
        pattern
            .strip_suffix('*')
            .map_or(name == pattern, |prefix| name.starts_with(prefix))
    })
}

pub(super) fn append(
    models: &mut Vec<Value>,
    options: &OmniRouteFallback<'_>,
    credential: &str,
    ids: &mut HashSet<String>,
    mut excluded: BTreeSet<String>,
) -> Result<(), String> {
    for model in models.iter_mut() {
        let name = string(model, "name").to_owned();
        if model.get("allowed_keys").is_some() || is_excluded(&excluded, &name) {
            insert(&mut excluded, &name)?;
            for alias in array(model, "aliases").iter().filter_map(Value::as_str) {
                insert(&mut excluded, alias)?;
            }
            continue;
        }
        let priority = array(model, "deployments")
            .iter()
            .filter_map(|dep| dep["priority"].as_i64())
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .filter(|priority| i32::try_from(*priority).is_ok())
            .ok_or("no priority available for OmniRoute fallback")?;
        let id = unique(&format!("omniroute-fallback-{name}"), ids);
        model["deployments"].as_array_mut().ok_or("imported model has no deployments")?.push(json!({"id":id,"credential":credential,"base_url":options.base_url,"upstream_model":name,"priority":priority}));
    }
    let id = unique("omniroute-catch-all", ids);
    models.push(json!({"name":"*","exclude":excluded,"deployments":[{"id":id,"credential":credential,"base_url":options.base_url,"upstream_model":"*","priority":0}]}));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPTIONS: OmniRouteFallback<'_> = OmniRouteFallback {
        base_url: "http://127.0.0.1:20128/v1",
        key: "env:SYNTHETIC_KEY",
    };

    #[test]
    fn allowed_keys_models_and_aliases_never_get_a_fallback_leg() {
        let original = json!({"name":"owner","aliases":["owner-alias"],"allowed_keys":["env:SYNTHETIC_OWNER"],"deployments":[{"id":"primary","priority":3}]});
        let mut models = vec![original.clone()];
        append(
            &mut models,
            &OPTIONS,
            "fallback",
            &mut HashSet::new(),
            BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(models[0], original);
        assert_eq!(models[1]["exclude"], json!(["owner", "owner-alias"]));
    }

    #[test]
    fn models_matching_an_exclusion_prefix_get_no_fallback_leg() {
        let original = json!({"name":"OpenAI/GPT","deployments":[{"id":"primary","priority":0}]});
        let mut models = vec![original.clone()];
        append(
            &mut models,
            &OPTIONS,
            "fallback",
            &mut HashSet::new(),
            BTreeSet::from(["openai/*".to_owned()]),
        )
        .unwrap();
        assert_eq!(models[0], original);
    }

    #[test]
    fn empty_import_gets_only_a_catch_all_and_ids_are_unique() {
        let mut models = Vec::new();
        let mut ids = HashSet::from(["omniroute-catch-all".to_owned()]);
        append(&mut models, &OPTIONS, "fallback", &mut ids, BTreeSet::new()).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["deployments"][0]["id"], "omniroute-catch-all-2");
    }

    #[test]
    fn fallback_priority_must_fit_after_every_existing_priority() {
        let mut models = vec![json!({"name":"full","deployments":[{"priority":i32::MAX}]})];
        let error = append(
            &mut models,
            &OPTIONS,
            "fallback",
            &mut HashSet::new(),
            BTreeSet::new(),
        )
        .unwrap_err();
        assert_eq!(error, "no priority available for OmniRoute fallback");
    }
}
