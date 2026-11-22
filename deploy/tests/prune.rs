//! prune.sh end-to-end: the EXIT trap must shred the plaintext credential
//! export on every exit path — success, a failed import, or an early bail —
//! and never delete the shared secrets directory.
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};

struct Fixture {
    home: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let home = std::env::temp_dir().join(format!(
            "llmux2-ops-prune-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let dir = home.join("llmux2");
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::create_dir_all(dir.join("secrets")).unwrap();
        fs::write(dir.join("export.json"), b"{\"credentials\":[]}").unwrap();
        fs::write(dir.join("dead-models.json"), b"[]").unwrap();
        fs::write(dir.join("site.toml"), b"{}").unwrap();
        fs::write(dir.join("import.env"), b"IMPORT_FLAGS=()\n").unwrap();
        fs::write(
            dir.join("active.json"),
            b"{\"config\":{\"server\":{\"listen\":\"127.0.0.1:8090\"}}}",
        )
        .unwrap();
        Self { home }
    }

    fn stub(&self, name: &str, script: &str) {
        let path = self.home.join("llmux2/bin").join(name);
        fs::write(&path, format!("#!/usr/bin/env bash\n{script}")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    fn run(&self) -> Output {
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("prune.sh");
        Command::new("bash")
            .arg(script)
            .env("HOME", &self.home)
            .output()
            .unwrap()
    }

    fn export_exists(&self) -> bool {
        self.home.join("llmux2/export.json").exists()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.home);
    }
}

#[test]
fn the_export_is_shredded_when_the_import_fails() {
    let fixture = Fixture::new();
    fixture.stub("llmux-import", "exit 1\n");
    fixture.stub("llmux2-deploy", "exit 0\n");
    fixture.stub("llmux-server", "true\n");
    let output = fixture.run();
    assert!(!output.status.success(), "failing import must fail the run");
    assert!(
        !fixture.export_exists(),
        "export.json survived a failed import: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn the_export_is_shredded_after_a_successful_run() {
    let fixture = Fixture::new();
    fixture.stub(
        "llmux-import",
        "touch \"$HOME/llmux2/llmux-next.toml\"\nexit 0\n",
    );
    fixture.stub("llmux2-deploy", "exit 0\n");
    fixture.stub("llmux-server", "true\n");
    let output = fixture.run();
    assert!(
        output.status.success(),
        "prune failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!fixture.export_exists(), "export.json survived a clean run");
    assert!(
        fixture.home.join("llmux2/secrets").exists(),
        "the shared secrets directory was removed"
    );
}

#[test]
fn the_export_is_shredded_on_the_early_bails() {
    // No site overlay -> the run bails before importing.
    let fixture = Fixture::new();
    fs::remove_file(fixture.home.join("llmux2/site.toml")).unwrap();
    let output = fixture.run();
    assert!(!output.status.success());
    assert!(
        !fixture.export_exists(),
        "export.json survived an early bail"
    );
}
