use super::{array, string};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

fn exact(name: &str) -> bool {
    !name.is_empty() && !name.contains(['*', '?'])
}

// Suffixes that change what a request actually gets: effort levels
// (`-low`/`-medium`/`-high`/`-xhigh`), `-fast`, and `no-think` spellings.
// They stack (e.g. `-fast-low`), so strip greedily from the leaf.
const MEANINGFUL_SUFFIXES: &[&str] = &[
    "low", "medium", "high", "xhigh", "fast", "no-think", "nothink", "thinking",
];

/// Ordered meaningful suffixes on a name's leaf (last `/` segment), matched
/// outermost-first — `vendor/x-fast-low` yields `["low", "fast"]`.
pub(super) fn meaningful_suffixes(name: &str) -> Vec<&'static str> {
    let mut leaf = name.rsplit('/').next().unwrap_or_default().to_lowercase();
    let mut suffixes = Vec::new();
    while let Some(suffix) = MEANINGFUL_SUFFIXES
        .iter()
        .find(|suffix| leaf.len() > suffix.len() + 1 && leaf.ends_with(&format!("-{suffix}")))
    {
        suffixes.push(*suffix);
        leaf.truncate(leaf.len() - suffix.len() - 1);
    }
    suffixes
}

fn target(value: &Value) -> Option<String> {
    if let Some(value) = value.as_str() {
        return Some(value.to_owned());
    }
    let provider = value.get("provider")?.as_str()?;
    let model = value.get("model")?.as_str()?;
    Some(format!("{provider}/{model}"))
}

pub(super) fn alias_map(root: &Value, warnings: &mut Vec<String>) -> BTreeMap<String, String> {
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

pub(super) fn matches(pattern: &str, name: &str) -> bool {
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

pub(super) fn sorted_mappings(root: &Value) -> Vec<&Value> {
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
    mappings
}

pub(super) fn mapping_target<'a>(
    mapping: &Value,
    combos: &HashMap<String, Value>,
    combo_ids: &'a HashMap<String, String>,
    unavailable_combos: Option<&HashSet<String>>,
) -> Option<&'a String> {
    combo_ids.get(string(mapping, "comboId")).filter(|name| {
        combos
            .get(*name)
            .is_some_and(|combo| combo["isActive"] != false)
            && !unavailable_combos.is_some_and(|unavailable| unavailable.contains(*name))
    })
}

pub(super) struct Prepared {
    pub resolved: BTreeMap<String, String>,
    pub synthetic: HashMap<String, Value>,
    pub unavailable: HashSet<String>,
    pub dropped_suffixed: Vec<String>,
}

pub(super) fn prepare(
    root: &Value,
    combos: &HashMap<String, Value>,
    combo_ids: &HashMap<String, String>,
    direct: &BTreeMap<String, Value>,
    order: &mut Vec<String>,
    warnings: &mut Vec<String>,
    unavailable_combos: Option<&HashSet<String>>,
) -> Prepared {
    let official_replacements = unavailable_combos.is_some();
    let mut aliases = alias_map(root, warnings);
    let mappings = sorted_mappings(root);
    let mut names: BTreeSet<_> = aliases.keys().cloned().collect();
    names.extend(direct.keys().cloned());
    if official_replacements {
        names.extend(
            combos
                .keys()
                .filter(|name| name.starts_with("openrouter/"))
                .cloned(),
        );
    }
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
        let mapping = mappings
            .iter()
            .find(|row| matches(string(row, "pattern"), &name));
        let target = mapping
            .and_then(|mapping| mapping_target(mapping, combos, combo_ids, unavailable_combos));
        if official_replacements
            && name.starts_with("openrouter/")
            && mapping.is_some()
            && target.is_none()
        {
            warnings.push(format!(
                "alias {name}: target unavailable (winning mapping)"
            ));
            aliases.remove(&name);
            unavailable.insert(name);
            continue;
        }
        if combos.contains_key(&name) {
            warnings.push(format!("alias {name}: collision; existing combo wins"));
            aliases.remove(&name);
            continue;
        }
        if mapping.is_some() {
            if let Some(target) = target {
                if official_replacements && name.starts_with("openrouter/") {
                    aliases.remove(&name);
                    if super::replacements::find(&name["openrouter/".len()..]).is_none() {
                        unavailable.insert(name.clone());
                    }
                    warnings.push(format!(
                        "alias {name}: source mapping replaced by reviewed official-only policy"
                    ));
                    continue;
                }
                aliases.insert(name, target.clone());
            } else {
                warnings.push(format!(
                    "alias {name}: target unavailable (winning mapping)"
                ));
                aliases.remove(&name);
                unavailable.insert(name);
            }
        } else if official_replacements && name.starts_with("openrouter/") {
            aliases.remove(&name);
            warnings.push(format!(
                "alias {name}: source alias replaced by reviewed official-only policy"
            ));
        }
    }
    let mut resolved = BTreeMap::new();
    let mut direct_targets = BTreeSet::new();
    let mut dropped_suffixed = Vec::new();
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
            match aliases.get(current) {
                Some(next) => {
                    if !seen.insert(current) || seen.len() > 128 {
                        warnings.push(format!("alias {name}: skipped cycle or chain safety limit"));
                        break None;
                    }
                    current = next;
                }
                None => break Some(current.to_owned()),
            }
        };
        let Some(terminal) = terminal else { continue };
        // A suffixed spelling must not collapse onto a differently-suffixed
        // target: the suffix is part of the model id, so the name stays
        // distinct and falls through to the catch-all, which forwards it to
        // the real suffixed upstream. Suffix-free renames (a plain alias to
        // a suffixed model) still resolve.
        let name_suffixes = meaningful_suffixes(name);
        if !name_suffixes.is_empty() && name_suffixes != meaningful_suffixes(&terminal) {
            warnings.push(format!(
                "alias {name}: suffix differs from target {terminal}; left unmerged"
            ));
            dropped_suffixed.push(name.clone());
            continue;
        }
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
    let mut synthetic = HashMap::new();
    for terminal in direct_targets {
        synthetic.insert(
            terminal.clone(),
            json!({"name":terminal,"models":[{"kind":"model","model":terminal}]}),
        );
        order.push(terminal);
    }
    Prepared {
        resolved,
        synthetic,
        unavailable,
        dropped_suffixed,
    }
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
