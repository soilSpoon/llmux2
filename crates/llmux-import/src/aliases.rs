use super::{array, string};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

fn exact(name: &str) -> bool {
    !name.is_empty() && !name.contains(['*', '?'])
}

fn target(value: &Value) -> Option<String> {
    if let Some(value) = value.as_str() {
        return Some(value.to_owned());
    }
    let provider = value.get("provider")?.as_str()?;
    let model = value.get("model")?.as_str()?;
    Some(format!("{provider}/{model}"))
}

fn alias_map(root: &Value, warnings: &mut Vec<String>) -> BTreeMap<String, String> {
    let mut result = BTreeMap::new();
    if let Some(aliases) = root.get("modelAliases") {
        if let Some(aliases) = aliases.as_object() {
            for (name, value) in aliases {
                if let Some(target) = target(value) {
                    result.insert(name.clone(), target);
                } else {
                    warnings.push(format!("alias {name}: skipped malformed alias target"));
                }
            }
        } else {
            warnings.push("ignored malformed modelAliases".into());
        }
    }
    result
}

fn matches(pattern: &str, name: &str) -> bool {
    let pattern: Vec<_> = pattern.chars().collect();
    let name: Vec<_> = name.chars().collect();
    let (mut p, mut n, mut star) = (0, 0, None);
    while n < name.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, n));
            p += 1;
        } else if let Some((s, consumed)) = star {
            n = consumed + 1;
            p = s + 1;
            star = Some((s, n));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

pub(super) fn prepare(
    root: &Value,
    combos: &mut HashMap<String, Value>,
    combo_ids: &HashMap<String, String>,
    order: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> BTreeMap<String, String> {
    let mut aliases = alias_map(root, warnings);
    let mut mappings: Vec<_> = array(root, "modelComboMappings")
        .iter()
        .filter(|row| row["enabled"] == true || row["enabled"] == 1)
        .collect();
    mappings.sort_by(|a, b| {
        b["priority"]
            .as_i64()
            .unwrap_or(0)
            .cmp(&a["priority"].as_i64().unwrap_or(0))
            .then_with(|| string(a, "createdAt").cmp(string(b, "createdAt")))
    });
    let mut names: BTreeSet<_> = aliases.keys().cloned().collect();
    for row in &mappings {
        let pattern = string(row, "pattern");
        if exact(pattern) {
            names.insert(pattern.into());
        } else {
            warnings.push(format!(
                "mapping {pattern}: wildcard limited to known exact alias names"
            ));
        }
    }
    let mut unavailable = HashSet::new();
    for name in names {
        if combos.contains_key(&name) {
            warnings.push(format!("alias {name}: collision; existing combo wins"));
            aliases.remove(&name);
            continue;
        }
        if let Some(mapping) = mappings
            .iter()
            .find(|row| matches(string(row, "pattern"), &name))
        {
            let target = combo_ids.get(string(mapping, "comboId")).filter(|name| {
                combos
                    .get(*name)
                    .is_some_and(|combo| combo["isActive"] != false)
            });
            if let Some(target) = target {
                aliases.insert(name, target.clone());
            } else {
                warnings.push(format!(
                    "alias {name}: target unavailable (winning mapping)"
                ));
                aliases.remove(&name);
                unavailable.insert(name);
            }
        }
    }
    let mut resolved = BTreeMap::new();
    let mut direct_targets = BTreeSet::new();
    for name in aliases.keys() {
        if !exact(name) {
            warnings.push(format!("alias {name}: skipped wildcard or empty alias"));
            continue;
        }
        let mut seen = HashSet::new();
        let mut current = name.as_str();
        let terminal = loop {
            if unavailable.contains(current) {
                warnings.push(format!(
                    "alias {name}: target unavailable (winning mapping)"
                ));
                break None;
            }
            if combos.contains_key(current) {
                break Some(current.to_owned());
            }
            if let Some(combo) = current
                .strip_prefix("combo/")
                .filter(|name| combos.contains_key(*name))
            {
                break Some(combo.to_owned());
            }
            if !seen.insert(current) || seen.len() > 128 {
                warnings.push(format!("alias {name}: skipped cycle or chain safety limit"));
                break None;
            }
            match aliases.get(current) {
                Some(next) => current = next,
                None => break Some(current.to_owned()),
            }
        };
        let Some(terminal) = terminal else { continue };
        if !combos.contains_key(&terminal) {
            let direct = terminal
                .split_once('/')
                .is_some_and(|(provider, model)| exact(provider) && exact(model));
            if !direct {
                warnings.push(format!("alias {name}: target unavailable"));
                continue;
            }
            direct_targets.insert(terminal.clone());
        }
        resolved.insert(name.clone(), terminal);
    }
    for terminal in direct_targets {
        combos.insert(
            terminal.clone(),
            json!({"name":terminal,"models":[{"kind":"model","model":terminal}]}),
        );
        order.push(terminal);
    }
    resolved
}

pub(super) fn attach(
    models: &mut [Value],
    aliases: BTreeMap<String, String>,
    warnings: &mut Vec<String>,
) {
    for (name, target) in aliases {
        if models.iter().any(|model| string(model, "name") == name) {
            warnings.push(format!("alias {name}: collision; existing model wins"));
            continue;
        }
        if let Some(model) = models
            .iter_mut()
            .find(|model| string(model, "name") == target)
        {
            model["aliases"].as_array_mut().unwrap().push(name.into());
        } else {
            warnings.push(format!(
                "alias {name}: target unavailable (no imported deployments)"
            ));
        }
    }
}
