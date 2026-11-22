use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};
struct Home(PathBuf);
impl Home {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "llmux2-ops-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn run(&self, guard: bool, args: &[&str]) -> Output {
        Command::new(if guard {
            env!("CARGO_BIN_EXE_llmux2-guard")
        } else {
            env!("CARGO_BIN_EXE_llmux2-deploy")
        })
        .args(args)
        .env("HOME", &self.0)
        .env("PATH", &self.0)
        .output()
        .unwrap()
    }
    fn systemctl(&self, text: &str) {
        executable(&self.0.join("systemctl"), text);
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn executable(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
#[test]
fn cli_help_and_argument_errors_do_not_touch_the_host() {
    let home = Home::new();
    for (guard, args) in [
        (false, vec!["--help"]),
        (false, vec!["deploy", "--help"]),
        (false, vec!["status", "-h"]),
        (true, vec!["-h"]),
    ] {
        let output = home.run(guard, &args);
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("usage:"));
    }
    for (guard, args, expected) in [
        (false, vec![], "required: command"),
        (false, vec!["other"], "invalid choice"),
        (false, vec!["deploy", "--binary"], "expected one argument"),
        (false, vec!["deploy", "--config"], "expected one argument"),
        (false, vec!["deploy", "--canary"], "expected one argument"),
        (false, vec!["deploy", "--canary=x"], "invalid value"),
        (false, vec!["deploy", "--canary=-1"], "invalid value"),
        (false, vec!["deploy", "--canary=nan"], "invalid value"),
        (false, vec!["deploy", "--canary=inf"], "invalid value"),
        (false, vec!["deploy", "--other"], "unrecognized arguments"),
        (false, vec!["status", "extra"], "unrecognized arguments"),
        (true, vec!["status"], "unrecognized arguments"),
    ] {
        let output = home.run(guard, &args);
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains(expected));
    }
    assert!(!home.0.join("llmux2").exists());
}
#[test]
fn cli_status_treats_restarting_as_active_and_creates_private_root_and_lock() {
    let home = Home::new();
    home.systemctl("#!/bin/sh\n[ \"$1\" = --user ] && [ \"$2\" = is-active ] || exit 99\ncase \"$3\" in llmux2@a.service) echo activating;; llmux2@b.service) echo inactive;; *) exit 98;; esac\nexit 3\n");
    let output = home.run(false, &["status"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&text).unwrap(),
        serde_json::json!({"active":["a"],"healthy":false,"recorded":null})
    );
    assert!(text.contains("\n  \"active\": [\n    \"a\""));
    for (path, mode) in [("llmux2", 0o700), ("llmux2/deploy.lock", 0o600)] {
        assert_eq!(
            fs::metadata(home.0.join(path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            mode
        );
    }
}
#[test]
fn cli_runtime_errors_have_prefix_and_exit_one() {
    for (script, expected) in [
        (
            "#!/bin/sh\necho denied >&2\nexit 4\n",
            "llmux2: systemctl is-active a failed: denied\n",
        ),
        (
            "#!/bin/sh\necho unexpected\nexit 3\n",
            "llmux2: unexpected unit state: unexpected\n",
        ),
    ] {
        let home = Home::new();
        home.systemctl(script);
        let output = home.run(false, &["status"]);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(String::from_utf8(output.stderr).unwrap(), expected);
        assert!(output.stdout.is_empty());
    }
}
#[test]
fn cli_guard_without_a_record_is_silent() {
    let home = Home::new();
    home.systemctl("#!/bin/sh\necho inactive\nexit 3\n");
    let output = home.run(true, &[]);
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}
#[test]
fn cli_deploy_options_and_defaults_reach_the_checker_in_root() {
    let home = Home::new();
    let root = home.0.join("llmux2");
    fs::create_dir(&root).unwrap();
    fs::create_dir(root.join("bin")).unwrap();
    let script = "#!/bin/sh\nprintf '%s|%s|%s' \"$PWD\" \"$1\" \"$2\" >&2\nexit 1\n";
    executable(&root.join("bin/llmux-server"), script);
    let output = home.run(false, &["deploy"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!(
            "llmux2: configuration validation failed: {}|--check|{}\n",
            root.display(),
            root.join("llmux.toml").display()
        )
    );
    let binary = home.0.join("custom server");
    executable(&binary, script);
    let config = home.0.join("custom config");
    let output = home.run(
        false,
        &[
            "deploy",
            "--binary",
            binary.to_str().unwrap(),
            &format!("--config={}", config.display()),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!(
            "llmux2: configuration validation failed: {}|--check|{}\n",
            root.display(),
            config.display()
        )
    );
}

#[test]
fn cli_deploy_checks_installed_copies_and_sends_show_properties_to_systemctl() {
    let home = Home::new();
    home.systemctl("#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$HOME/commands\"\ncase \"$2\" in\nis-active) if [ \"$3\" = llmux2@a.service ]; then echo active; exit 0; else echo inactive; exit 3; fi;;\nshow) printf 'ActiveState=failed\\nNRestarts=0\\nMainPID=4321\\n';;\nesac\n");
    let binary = home.0.join("source-bin");
    executable(
        &binary,
        "#!/bin/sh\n[ \"$1\" = --check ] || exit 9\necho 127.0.0.1:8080\n",
    );
    let config = home.0.join("source.toml");
    fs::write(&config, "config").unwrap();
    let output = home.run(
        false,
        &[
            "deploy",
            "--binary",
            binary.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(String::from_utf8(output.stderr).unwrap(),"llmux2: deployment failed; old slot unchanged: new slot did not remain active without restarts during settle window\n");
    let calls = fs::read_to_string(home.0.join("commands")).unwrap();
    assert!(calls
        .lines()
        .any(|line| line == "--user show -p ActiveState,NRestarts,MainPID llmux2@b.service"));
    assert!(calls
        .lines()
        .any(|line| line == "--user stop llmux2@b.service"));
    assert!(!calls
        .lines()
        .any(|line| line == "--user stop llmux2@a.service"));
    assert_eq!(
        fs::read(home.0.join("llmux2/bin/llmux-server-b")).unwrap(),
        fs::read(binary).unwrap()
    );
}

#[test]
fn cli_preserves_non_utf8_binary_and_config_paths() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let home = Home::new();
    let binary = home.0.join(OsString::from_vec(b"binary-\xff".to_vec()));
    let config = home
        .0
        .join(OsString::from_vec(b"config-\xfe.toml".to_vec()));
    fs::write(&config, b"non-utf8-path-config").unwrap();
    executable(&binary,"#!/bin/sh\n[ \"$1\" = --check ] || exit 9\n[ -f \"$2\" ] || exit 8\necho non-utf8-path-checker >&2\nexit 1\n");
    for inline in [false, true] {
        let mut args = vec![OsString::from("deploy")];
        for (flag, path) in [("--binary", &binary), ("--config", &config)] {
            if inline {
                let mut arg = OsString::from(format!("{flag}="));
                arg.push(path);
                args.push(arg);
            } else {
                args.extend([flag.into(), path.as_os_str().into()]);
            }
        }
        let output = Command::new(env!("CARGO_BIN_EXE_llmux2-deploy"))
            .args(args)
            .env("HOME", &home.0)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            "llmux2: configuration validation failed: non-utf8-path-checker\n"
        );
    }
}
