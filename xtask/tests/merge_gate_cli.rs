use serde_json::{json, Value};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};
struct Api(PathBuf);
impl Api {
    fn new(data: &Value, graphql: Option<Value>) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "llmux-gate-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let mut script = String::from("#!/bin/sh\n[ \"$1\" = api ] || exit 9\ncase \"$2\" in\n");
        for (pattern,value) in [
            ("'repos/{owner}/{repo}'",json!({"full_name":"owner/repo","default_branch":"main"})),
            ("repos/owner/repo/pulls/83",data["pr"].clone()),
            ("graphql",graphql.unwrap_or_else(||json!({"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":data["threads"],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}}))),
            ("*check-runs*",json!({"check_runs":data["checks"]})),
            ("*reviews*",data["reviews"].clone()),("*statuses*",data["statuses"].clone()),("*comments*",data["comments"].clone())
        ] {script.push_str(&format!("{pattern}) printf '%s' '{}' ;;\n",value.to_string().replace('\'',"'\\''")));}
        script.push_str("*) exit 8;;\nesac\n");
        let api = Self(path);
        api.script(&script);
        api
    }
    fn script(&self, script: &str) {
        let path = self.0.join("gh");
        fs::write(&path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fn run(&self, number: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_xtask"))
            .args(["merge-gate", number])
            .current_dir(&self.0)
            .env("PATH", &self.0)
            .output()
            .unwrap()
    }
}
impl Drop for Api {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn fixture() -> Value {
    let sha = "a".repeat(40);
    let checks = ["check", "pr-policy", "advisories", "coverage"]
        .iter()
        .map(|name| json!({"name":name,"status":"completed","conclusion":"success","head_sha":sha}))
        .collect::<Vec<_>>();
    json!({"pr":{"state":"open","draft":false,"base":{"ref":"main"},"mergeable":true,"head":{"sha":sha},"body":"Author-Session: author"},"checks":checks,"threads":[],"reviews":[{"id":1,"state":"COMMENTED","user":{"login":"devin-ai-integration"},"body":"<!-- devin-review-summary -->","commit_id":sha}],"statuses":[],"comments":[{"id":1,"user":{"login":"soilSpoon"},"body":format!("<!-- llmux-verify:v1 sha={sha} verdict=pass reviewer=independent -->")}]})
}
#[test]
fn merge_gate_pass_and_failure_output_and_exit_codes() {
    let data = fixture();
    let api = Api::new(&data, None);
    let output = api.run("83");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("Merge gate passed for PR #83 at {}.\n", "a".repeat(40))
    );
    assert!(output.stderr.is_empty());
    let mut data = fixture();
    data["pr"]["draft"] = json!(true);
    let output = Api::new(&data, None).run("83");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"FAIL: PR is a draft.\n");
    assert!(output.stderr.is_empty());
    let mut data = fixture();
    data["checks"] = json!([]);
    let output = Api::new(&data, None).run("83");
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 4);
    assert!(stdout.contains("Required check 'coverage' has no completed successful run"));
}
#[test]
fn merge_gate_api_json_graphql_and_argument_errors_exit_closed() {
    let api = Api::new(&fixture(), None);
    for script in ["#!/bin/sh\nexit 1\n", "#!/bin/sh\nprintf '{'\n"] {
        api.script(script);
        let output = api.run("83");
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8(output.stderr)
            .unwrap()
            .starts_with("FAIL: Unable to evaluate merge gate: "));
    }
    let output = Api::new(&fixture(), Some(json!({"errors":[{"message":"denied"}]}))).run("83");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        output.stderr,
        b"FAIL: Unable to evaluate merge gate: GitHub GraphQL returned errors\n"
    );
    let output = api.run("not-a-number");
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("invalid int value"));
}
