use llmux2_ops::{Deployment, Result, Runtime};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    io::{self, Write},
    net::{IpAddr, TcpListener},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let p = std::env::temp_dir().join(format!(
            "llmux2-ops-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct Fake {
    active: BTreeSet<String>,
    calls: Vec<Vec<String>>,
    healthy: bool,
    fail_start: bool,
    fail_check: bool,
    states: VecDeque<Value>,
    waits: Vec<f64>,
    failures: BTreeMap<(String, String), String>,
    listens: VecDeque<String>,
    health_calls: usize,
}
impl Default for Fake {
    fn default() -> Self {
        Self {
            active: BTreeSet::from(["a".into()]),
            calls: vec![],
            healthy: true,
            fail_start: false,
            fail_check: false,
            states: VecDeque::new(),
            waits: vec![],
            failures: BTreeMap::new(),
            listens: VecDeque::new(),
            health_calls: 0,
        }
    }
}
fn stable() -> Value {
    json!({"ActiveState":"active","NRestarts":"0","MainPID":"1234"})
}
impl Runtime for Fake {
    fn systemctl(&mut self, action: &str, slot: &str) -> Result<Value> {
        self.calls.push(vec![action.into(), slot.into()]);
        if let Some(error) = self.failures.get(&(action.into(), slot.into())) {
            return Err(error.clone().into());
        }
        match action {
            "is-active" => return Ok(json!(self.active.contains(slot))),
            "show" => return Ok(self.states.pop_front().unwrap_or_else(stable)),
            "start" => {
                if self.fail_start {
                    return Err("start failed".into());
                }
                self.active.insert(slot.into());
            }
            "stop" => {
                self.active.remove(slot);
            }
            _ => {}
        }
        Ok(json!(true))
    }
    fn check(&mut self, binary: &Path, config: &Path) -> Result<String> {
        self.calls.push(vec![
            "check".into(),
            binary.display().to_string(),
            config.display().to_string(),
        ]);
        if self.fail_check {
            Err("invalid config".into())
        } else {
            Ok(self.listens.pop_front().unwrap_or("127.0.0.1:8080".into()))
        }
    }
    fn health(&mut self, _: &str) -> Result<bool> {
        self.health_calls += 1;
        Ok(self.healthy)
    }
    fn sleep(&mut self, seconds: f64) {
        self.waits.push(seconds);
    }
}
struct Fixture {
    temp: Temp,
    ops: Deployment<Fake>,
    binary: PathBuf,
    config: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = Temp::new();
        let binary = temp.0.join("source-bin");
        let config = temp.0.join("source.toml");
        fs::write(&binary, b"binary").unwrap();
        fs::write(&config, "config").unwrap();
        let ops = Deployment::new(temp.0.clone(), Fake::default());
        Self {
            temp,
            ops,
            binary,
            config,
        }
    }
    fn deploy(&mut self) -> Result<String> {
        self.ops.deploy(&self.binary, &self.config)
    }
    fn called(&self, action: &str, slot: &str) -> bool {
        self.ops
            .runtime
            .calls
            .contains(&vec![action.into(), slot.into()])
    }
}

#[test]
fn handoff_installs_idle_slot_and_records_reboot_target() {
    let mut f = Fixture::new();
    assert_eq!(f.deploy().unwrap(), "b");
    assert_eq!(f.ops.runtime.active, BTreeSet::from(["b".into()]));
    assert_eq!(
        fs::read(f.temp.0.join("bin/llmux-server-b")).unwrap(),
        b"binary"
    );
    assert_eq!(
        fs::read_to_string(f.temp.0.join("llmux-b.toml")).unwrap(),
        "config"
    );
    let record: Value =
        serde_json::from_slice(&fs::read(f.temp.0.join("active.json")).unwrap()).unwrap();
    assert_eq!(record["slot"], "b");
    let calls = &f.ops.runtime.calls;
    assert!(
        calls.iter().position(|v| v == &["start", "b"]).unwrap()
            < calls.iter().position(|v| v == &["stop", "a"]).unwrap()
    );
    assert!(f.called("enable", "b"));
    assert!(f.called("disable", "a"));
    assert_eq!(f.deploy().unwrap(), "a");
    assert_eq!(f.ops.runtime.active, BTreeSet::from(["a".into()]));
}
#[test]
fn first_install() {
    let mut f = Fixture::new();
    f.ops.runtime.active.clear();
    assert_eq!(f.deploy().unwrap(), "a");
    assert_eq!(f.ops.runtime.active, BTreeSet::from(["a".into()]));
}
#[test]
fn failed_health_keeps_old() {
    let mut f = Fixture::new();
    f.ops.runtime.healthy = false;
    assert!(f.deploy().unwrap_err().to_string().contains("health"));
    assert_eq!(f.ops.runtime.active, BTreeSet::from(["a".into()]));
    assert!(!f.called("stop", "a"));
    assert!(f.called("stop", "b"));
}
#[test]
fn failed_start_keeps_old() {
    let mut f = Fixture::new();
    f.ops.runtime.fail_start = true;
    assert!(f.deploy().unwrap_err().to_string().contains("start"));
    assert_eq!(f.ops.runtime.active, BTreeSet::from(["a".into()]));
    assert!(f.called("stop", "b"));
}
#[test]
fn old_health_does_not_mask_new_slot_failure() {
    for state in [
        json!({}),
        json!({"ActiveState":"failed","NRestarts":"0","MainPID":"0"}),
        json!({"ActiveState":"active","NRestarts":"1","MainPID":"1234"}),
        json!({"ActiveState":"active","NRestarts":"0","MainPID":"0"}),
    ] {
        let mut f = Fixture::new();
        f.ops.runtime.states.push_back(state);
        assert!(f.deploy().unwrap_err().to_string().contains("new slot"));
        assert_eq!(f.ops.runtime.active, BTreeSet::from(["a".into()]));
        assert!(!f.called("stop", "a"));
        assert!(f.called("stop", "b"));
    }
}
#[test]
fn new_slot_must_stay_stable_for_settle_window() {
    let mut f = Fixture::new();
    f.ops.runtime.states = VecDeque::from([
        stable(),
        stable(),
        json!({"ActiveState":"active","NRestarts":"0","MainPID":"5678"}),
    ]);
    assert!(f.deploy().unwrap_err().to_string().contains("new slot"));
    assert_eq!(f.ops.runtime.active, BTreeSet::from(["a".into()]));
    assert!(!f.called("stop", "a"));
}
#[test]
fn healthy_slot_is_observed_for_three_seconds() {
    let mut f = Fixture::new();
    f.deploy().unwrap();
    assert!(f.ops.runtime.waits.iter().sum::<f64>() >= 3.0);
    assert!(
        f.ops
            .runtime
            .calls
            .iter()
            .filter(|v| v == &&["show", "b"])
            .count()
            >= 2
    );
}
#[test]
fn cli_checks_new_unit_properties() {
    let mut f = Fixture::new();
    f.deploy().unwrap();
    let probes: Vec<_> = f
        .ops
        .runtime
        .calls
        .iter()
        .filter(|c| c[0] == "show")
        .map(|c| llmux2_ops::systemctl_args(&c[0], &c[1]))
        .collect();
    assert!(probes.len() >= 2);
    assert!(probes.iter().all(|c| c
        == &[
            "--user",
            "show",
            "-p",
            "ActiveState,NRestarts,MainPID",
            "llmux2@b.service"
        ]));
}
#[test]
fn invalid_config_does_not_touch_units() {
    let mut f = Fixture::new();
    f.ops.runtime.fail_check = true;
    assert!(f.deploy().unwrap_err().to_string().contains("invalid"));
    assert!(!f
        .ops
        .runtime
        .calls
        .iter()
        .any(|v| v[0] == "start" || v[0] == "stop"));
}
#[test]
fn guard_recovers_only_when_no_active_slot_and_unhealthy() {
    let mut f = Fixture::new();
    fs::write(
        f.temp.0.join("active.json"),
        json!({"slot":"b","listen":"127.0.0.1:8080"}).to_string(),
    )
    .unwrap();
    f.ops.runtime.healthy = false;
    f.ops.guard().unwrap();
    assert!(!f.called("start", "b"));
    f.ops.runtime.active.clear();
    f.ops.runtime.healthy = true;
    f.ops.guard().unwrap();
    assert!(!f.called("start", "b"));
    f.ops.runtime.healthy = false;
    f.ops.guard().unwrap();
    assert!(f.called("start", "b"));
}
#[test]
fn guard_without_record_is_noop() {
    let mut f = Fixture::new();
    f.ops.runtime.active.clear();
    f.ops.guard().unwrap();
    assert!(!f.ops.runtime.calls.iter().any(|v| v[0] == "start"));
}
#[test]
fn restarting_slot_is_busy() {
    assert!(llmux2_ops::unit_active(3, "activating").unwrap());
    assert!(!llmux2_ops::unit_active(3, "inactive").unwrap());
    let mut f = Fixture::new();
    assert_eq!(f.ops.status().unwrap()["active"], json!(["a"]));
}

#[test]
fn installed_check_runs_before_start_and_sampling_is_exact() {
    let mut f = Fixture::new();
    f.deploy().unwrap();
    let calls = &f.ops.runtime.calls;
    let checks: Vec<_> = calls
        .iter()
        .enumerate()
        .filter(|(_, c)| c[0] == "check")
        .collect();
    assert_eq!(checks.len(), 2);
    assert!(checks[1].0 < calls.iter().position(|c| c == &["start", "b"]).unwrap());
    assert_eq!(
        checks[1].1[1],
        f.temp.0.join("bin/llmux-server-b").display().to_string()
    );
    assert_eq!(calls.iter().filter(|c| c == &&["show", "b"]).count(), 7);
    assert_eq!(f.ops.runtime.waits, vec![0.5; 6]);
    assert_eq!(f.ops.runtime.health_calls, 1);
    for (path, mode) in [
        ("bin/llmux-server-b", 0o700),
        ("llmux-b.toml", 0o600),
        ("active.json", 0o600),
    ] {
        assert_eq!(
            fs::metadata(f.temp.0.join(path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            mode
        );
    }
}
#[test]
fn failed_health_has_sixty_attempts_and_cleanup_wording() {
    let mut f = Fixture::new();
    f.ops.runtime.healthy = false;
    assert_eq!(
        f.deploy().unwrap_err().to_string(),
        "deployment failed; old slot unchanged: new slot health check timed out"
    );
    assert_eq!(f.ops.runtime.health_calls, 60);
    assert_eq!(f.ops.runtime.waits, vec![0.5; 66]);
    assert!(!f.temp.0.join("active.json").exists());
}
#[test]
fn conflicting_slots_and_installed_listen_fail_before_start() {
    let mut f = Fixture::new();
    f.ops.runtime.active.insert("b".into());
    assert_eq!(
        f.deploy().unwrap_err().to_string(),
        "both slots are active; inspect status and stop the unwanted slot first"
    );
    assert!(!f.temp.0.join("bin").exists());
    let mut f = Fixture::new();
    f.ops.runtime.listens = VecDeque::from(["127.0.0.1:8080".into(), "0.0.0.0:8080".into()]);
    assert_eq!(
        f.deploy().unwrap_err().to_string(),
        "installed configuration changed its listen address"
    );
    assert!(!f.called("start", "b"));
    assert!(!f.called("stop", "b"));
}
#[test]
fn cleanup_and_handoff_errors_keep_their_exact_wording() {
    let mut f = Fixture::new();
    f.ops.runtime.fail_start = true;
    f.ops
        .runtime
        .failures
        .insert(("stop".into(), "b".into()), "cleanup failed".into());
    assert_eq!(
        f.deploy().unwrap_err().to_string(),
        "deployment failed: start failed; new slot cleanup also failed: cleanup failed"
    );
    for (action, slot) in [("stop", "a"), ("enable", "b"), ("disable", "a")] {
        let mut f = Fixture::new();
        f.ops
            .runtime
            .failures
            .insert((action.into(), slot.into()), "unit failed".into());
        assert_eq!(
            f.deploy().unwrap_err().to_string(),
            "handoff reached old-slot stop; new slot remains running; inspect status: unit failed"
        );
        assert!(f.ops.runtime.active.contains("b"));
        assert!(!f.called("stop", "b"));
    }
}
#[test]
fn record_write_failure_after_handoff_leaves_new_slot_running() {
    let mut f = Fixture::new();
    fs::create_dir(f.temp.0.join("active.json")).unwrap();
    assert!(f
        .deploy()
        .unwrap_err()
        .to_string()
        .starts_with("handoff reached old-slot stop; new slot remains running; inspect status: "));
    assert_eq!(f.ops.runtime.active, BTreeSet::from(["b".into()]));
    assert!(!f.called("stop", "b"));
}
#[test]
fn atomic_write_replaces_private_files_and_cleans_failed_temporaries() {
    let temp = Temp::new();
    let path = temp.0.join("target");
    llmux2_ops::atomic_write(&path, |out| out.write_all(b"old"), 0o600).unwrap();
    let error = llmux2_ops::atomic_write(
        &path,
        |out| {
            out.write_all(b"partial")?;
            Err(io::Error::other("writer failed"))
        },
        0o700,
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "writer failed");
    assert_eq!(fs::read(&path).unwrap(), b"old");
    assert_eq!(fs::read_dir(&temp.0).unwrap().count(), 1);
    llmux2_ops::atomic_write(&path, |out| out.write_all(b"new"), 0o700).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"new");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let directory = temp.0.join("directory");
    fs::create_dir(&directory).unwrap();
    assert!(llmux2_ops::atomic_write(&directory, |_| Ok(()), 0o600).is_err());
    assert_eq!(fs::read_dir(&temp.0).unwrap().count(), 2);
}
#[test]
fn record_and_status_validate_inputs_and_guard_ignores_records_when_busy() {
    let mut f = Fixture::new();
    assert_eq!(
        f.ops.status().unwrap(),
        json!({"active":["a"],"recorded":null,"healthy":false})
    );
    let record = f.temp.0.join("active.json");
    for text in [
        "{",
        "{}",
        r#"{"slot":"c"}"#,
        r#"{"slot":"a","listen":"bad"}"#,
        r#"{"slot":"a"}"#,
    ] {
        fs::write(&record, text).unwrap();
        assert!(f.ops.record().is_err());
        f.ops.guard().unwrap();
    }
    fs::write(
        &record,
        json!({"slot":"a","listen":"[::]:8080"}).to_string(),
    )
    .unwrap();
    assert_eq!(f.ops.status().unwrap()["healthy"], true);
}
#[test]
fn address_and_unit_state_mapping() {
    for (listen, host, port) in [
        ("0.0.0.0:80", "127.0.0.1", 80),
        ("[::]:90", "::1", 90),
        ("[2001:db8::1]:443", "2001:db8::1", 443),
        ("127.0.0.2:12", "127.0.0.2", 12),
    ] {
        assert_eq!(
            llmux2_ops::address_parts(listen).unwrap(),
            (host.parse::<IpAddr>().unwrap(), port)
        );
    }
    for listen in ["bad", "localhost:12", "127.0.0.1:x"] {
        assert!(llmux2_ops::address_parts(listen).is_err());
    }
    for code in [0, 3] {
        for state in [
            "active",
            "reloading",
            "activating",
            "deactivating",
            "refreshing",
        ] {
            assert!(llmux2_ops::unit_active(code, state).unwrap());
        }
        for state in ["inactive", "failed"] {
            assert!(!llmux2_ops::unit_active(code, state).unwrap());
        }
        assert_eq!(
            llmux2_ops::unit_active(code, "unknown")
                .unwrap_err()
                .to_string(),
            "unexpected unit state: unknown"
        );
    }
    assert!(llmux2_ops::unit_active(1, "active").is_err());
}
#[test]
fn health_requires_http_200_and_ok_prefix() {
    use std::io::{BufRead, BufReader};
    for (response, expected) in [
        ("HTTP/1.0 200 OK\r\nContent-Length: 4\r\n\r\nokay", true),
        ("HTTP/1.0 503 Unavailable\r\n\r\nok", false),
        ("HTTP/1.1 200 OK\r\n\r\nno", false),
        ("garbage\r\n\r\nok", false),
        ("HTTP/1.0 200 OK\r\n", false),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(line, "GET /healthz HTTP/1.0\r\n");
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
            }
            stream.write_all(response.as_bytes()).unwrap();
        });
        assert_eq!(
            llmux2_ops::health(&format!("0.0.0.0:{}", address.port())).unwrap(),
            expected
        );
        thread.join().unwrap();
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    assert!(!llmux2_ops::health(&address.to_string()).unwrap());
    assert!(llmux2_ops::health("127.0.0.1:65536").is_err());
}
