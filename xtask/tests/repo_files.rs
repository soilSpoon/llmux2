use serde_json::Value;
use std::{collections::BTreeSet, fs, path::Path};
fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}
fn load(path: &str) -> Value {
    serde_json::from_str(&fs::read_to_string(root().join(path)).unwrap()).unwrap()
}
fn rules() -> Value {
    let mut rules = serde_json::Map::new();
    for rule in load(".github/rulesets/main.json")["rules"]
        .as_array()
        .unwrap()
    {
        rules.insert(
            rule["type"].as_str().unwrap().into(),
            rule.get("parameters")
                .cloned()
                .unwrap_or(serde_json::json!({})),
        );
    }
    Value::Object(rules)
}
#[test]
fn targets_the_default_branch_and_is_active() {
    let r = load(".github/rulesets/main.json");
    assert_eq!(r["target"], "branch");
    assert_eq!(r["enforcement"], "active");
    assert_eq!(
        r["conditions"]["ref_name"]["include"],
        serde_json::json!(["~DEFAULT_BRANCH"])
    );
    assert_eq!(
        r.get("bypass_actors")
            .cloned()
            .unwrap_or(serde_json::json!([])),
        serde_json::json!([])
    );
}
#[test]
fn blocks_force_push_and_deletion() {
    let r = rules();
    assert!(r.get("non_fast_forward").is_some());
    assert!(r.get("deletion").is_some());
}
#[test]
fn changes_go_through_a_squash_merged_pull_request() {
    assert_eq!(
        rules()["pull_request"]["allowed_merge_methods"],
        serde_json::json!(["squash"])
    );
}
#[test]
fn review_threads_must_be_resolved() {
    assert_eq!(
        rules()["pull_request"]["required_review_thread_resolution"],
        Value::Bool(true)
    );
}

#[test]
fn every_pull_request_job_is_required() {
    let r = rules();
    let required: BTreeSet<_> = r["required_status_checks"]["required_status_checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["context"].as_str().unwrap().to_owned())
        .collect();
    let mut jobs = BTreeSet::new();
    for entry in fs::read_dir(root().join(".github/workflows")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|s| s.to_str()) != Some("yml") {
            continue;
        }
        let text = fs::read_to_string(path).unwrap();
        if !text.lines().any(|l| l.starts_with("  pull_request:")) {
            continue;
        }
        for line in text.split_once("\njobs:\n").unwrap().1.lines() {
            if let Some(name) = line
                .strip_prefix("  ")
                .and_then(|l| l.trim_end().strip_suffix(':'))
            {
                if !name.is_empty()
                    && name
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
                {
                    jobs.insert(name.to_owned());
                }
            }
        }
    }
    assert_eq!(required, jobs);
}
#[test]
fn requires_engineering_workflow_plugins() {
    let config = load(".devin/config.json");
    let entries = config["requiredPlugins"].as_array().unwrap();
    for (repo, path) in [
        ("https://github.com/cursor/plugins.git", Some("pstack")),
        ("EveryInc/compound-engineering-plugin", None),
    ] {
        assert!(entries.iter().any(|e| e
            .get("repo")
            .or_else(|| e.get("url"))
            .and_then(Value::as_str)
            == Some(repo)
            && e.get("path").and_then(Value::as_str) == path));
    }
}
#[test]
fn required_plugins_are_pinned_to_commits() {
    for entry in load(".devin/config.json")["requiredPlugins"]
        .as_array()
        .unwrap()
    {
        assert!(entry.is_object());
        let sha = entry["sha"].as_str().unwrap();
        assert_eq!(sha.len(), 40);
        assert!(sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        assert!(entry.get("ref").is_none());
    }
}
