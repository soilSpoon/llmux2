use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};

static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Repo(PathBuf);
impl Repo {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "llmux-xtask-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(path.join(".github")).unwrap();
        fs::write(path.join(".github/coverage-floor"), "90.0\n").unwrap();
        let repo = Self(path);
        repo.git(&["init", "-q"]);
        repo.git(&["add", ".github/coverage-floor"]);
        repo.git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ]);
        repo
    }
    fn git(&self, args: &[&str]) {
        assert!(Command::new("git")
            .args(args)
            .current_dir(&self.0)
            .output()
            .unwrap()
            .status
            .success());
    }
    fn run(&self, args: &[&str], body: &str, base: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_xtask"))
            .args(args)
            .current_dir(&self.0)
            .env("PR_BODY", body)
            .env("BASE_SHA", base)
            .env("HEAD_SHA", "HEAD")
            .output()
            .unwrap()
    }
}
impl Drop for Repo {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn pr_policy_exit_codes_and_git_errors() {
    let repo = Repo::new();
    let body="## Summary\nWorks\n## Verification\nTests\n## Reliability and compatibility\nUnchanged\n## Workflow and learning\nNative\n";
    let valid = repo.run(&["pr-policy"], body, "HEAD");
    assert!(valid.status.success());
    assert_eq!(
        String::from_utf8(valid.stdout).unwrap(),
        "PR policy satisfied.\n"
    );
    let empty = repo.run(&["pr-policy"], "", "HEAD");
    assert_eq!(empty.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(empty.stdout)
            .unwrap()
            .matches("::error::")
            .count(),
        4
    );
    let invalid = repo.run(&["pr-policy"], body, "missing-ref");
    assert_eq!(invalid.status.code(), Some(1));
    assert!(!invalid.stderr.is_empty());
}

#[test]
fn coverage_ratchet_base_floor_and_reports() {
    let repo = Repo::new();
    fs::write(
        repo.0.join("coverage.json"),
        r#"{"data":[{"totals":{"lines":{"percent":93.5}}}]}"#,
    )
    .unwrap();
    let valid = repo.run(&["coverage-ratchet", "coverage.json"], "", "HEAD");
    assert!(valid.status.success());
    assert_eq!(String::from_utf8(valid.stdout).unwrap(), "Line coverage 93.50% meets the 90.0% floor.\n::notice::Coverage is 3.50 points above the floor; raise .github/coverage-floor.\n");
    for base in ["", "missing-ref"] {
        assert!(repo
            .run(&["coverage-ratchet", "coverage.json"], "", base)
            .status
            .success());
    }
    fs::write(repo.0.join(".github/coverage-floor"), "89.0").unwrap();
    let lowered = repo.run(&["coverage-ratchet", "coverage.json"], "", "HEAD");
    assert_eq!(lowered.status.code(), Some(1));
    assert!(String::from_utf8(lowered.stdout)
        .unwrap()
        .contains("lowered from 90.0 to 89.0"));
    fs::write(repo.0.join(".github/coverage-floor"), "94.0").unwrap();
    assert_eq!(
        repo.run(&["coverage-ratchet", "coverage.json"], "", "HEAD")
            .status
            .code(),
        Some(1)
    );
    for report in ["", "{}", "{bad json}"] {
        fs::write(repo.0.join("coverage.json"), report).unwrap();
        assert_eq!(
            repo.run(&["coverage-ratchet", "coverage.json"], "", "HEAD")
                .status
                .code(),
            Some(1)
        );
    }
    assert_eq!(
        repo.run(&["coverage-ratchet", "absent.json"], "", "HEAD")
            .status
            .code(),
        Some(1)
    );
    assert_eq!(repo.run(&[], "", "").status.code(), Some(1));
}
