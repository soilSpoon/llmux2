use std::{
    collections::VecDeque,
    fs,
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
    thread,
};

struct Home(PathBuf);
impl Home {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "llmux2-ops-alert-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_llmux2-alert"))
            .args(args)
            .env("HOME", &self.0)
            .output()
            .unwrap()
    }
    fn root(&self) -> PathBuf {
        let root = self.0.join("llmux2");
        fs::create_dir_all(&root).unwrap();
        root
    }
    fn record(&self, listen: &str) {
        fs::write(
            self.root().join("active.json"),
            format!("{{\"slot\": \"a\", \"listen\": \"{listen}\"}}"),
        )
        .unwrap();
    }
    fn config(&self, text: &str) -> PathBuf {
        let path = self.root().join("alerts.toml");
        fs::write(&path, text).unwrap();
        path
    }
    fn state(&self, text: &str) {
        fs::write(self.root().join("alert-state.json"), text).unwrap();
    }
    fn saved_state(&self) -> String {
        fs::read_to_string(self.0.join("llmux2/alert-state.json")).unwrap()
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Minimal HTTP/1.0 server: routes /healthz and /admin/deployments, requires the
/// bearer token when `key` is set, and repeats the last deployments body.
struct Gateway {
    listen: String,
}
impl Gateway {
    fn start(key: Option<&str>, bodies: Vec<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let listen = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
        let key = key.map(str::to_owned);
        thread::spawn(move || {
            let mut bodies: VecDeque<String> = bodies.into();
            let mut last = "{\"deployments\":[]}".to_string();
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                let mut authorized = key.is_none();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 {
                        break;
                    }
                    if line.trim().is_empty() {
                        break;
                    }
                    request.push_str(&line);
                    if let Some((name, value)) = line.split_once(':') {
                        if name.trim().eq_ignore_ascii_case("authorization") {
                            authorized =
                                value.trim() == format!("Bearer {}", key.as_deref().unwrap());
                        }
                    }
                }
                if let Some(body) = bodies.pop_front() {
                    last = body;
                }
                let path = request.split_whitespace().nth(1).unwrap_or_default();
                let (status, body) = match path {
                    "/healthz" => ("200 OK", "ok".to_string()),
                    "/admin/deployments" if authorized => ("200 OK", last.clone()),
                    "/admin/deployments" => ("401 Unauthorized", "".to_string()),
                    _ => ("404 Not Found", "".to_string()),
                };
                write!(
                    stream,
                    "HTTP/1.0 {status}\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
                stream.flush().unwrap();
            }
        });
        Self { listen }
    }
}

fn deployments(entries: &[(&str, u64, u64, u64, bool, bool)]) -> String {
    let entries: Vec<serde_json::Value> = entries
        .iter()
        .map(|(id, routed, failed, stream_errors, disabled, cooling)| {
            serde_json::json!({
                "model": "m", "id": id, "provider": "p", "priority": 0,
                "fallback": false, "disabled": disabled,
                "cooldown_ms": if *cooling { serde_json::json!(1000) } else { serde_json::Value::Null },
                "in_flight": 0, "routed": routed,
                "failed_attempts": failed, "stream_errors": stream_errors,
            })
        })
        .collect();
    serde_json::json!({"deployments": entries}).to_string()
}

#[test]
fn cli_help_and_argument_errors_do_not_touch_the_host() {
    let home = Home::new();
    for args in [vec!["--help"], vec!["-h"]] {
        let output = home.run(&args);
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("usage: llmux2-alert"));
    }
    for (args, expected) in [
        (vec!["--config"], "expected one argument"),
        (vec!["extra"], "unrecognized arguments"),
        (vec!["--other"], "unrecognized arguments"),
    ] {
        let output = home.run(&args);
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains(expected));
    }
    assert!(!home.0.join("llmux2").exists());
}

#[test]
fn cli_malformed_config_exits_two() {
    let home = Home::new();
    let config = home.config("not = [toml");
    let output = home.run(&["--config", config.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("config"));
}

#[test]
fn unreachable_gateway_alerts_and_exits_one() {
    let home = Home::new();
    // Reserve then release a port so nothing listens there.
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    home.record(&format!("127.0.0.1:{port}"));
    let output = home.run(&[]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("ALERT"), "{stderr}");
    assert!(stderr.contains("health"), "{stderr}");
}

#[test]
fn healthy_gateway_without_thresholds_exits_zero() {
    let home = Home::new();
    let gateway = Gateway::start(None, vec![deployments(&[("d1", 10, 0, 0, false, false)])]);
    home.record(&gateway.listen);
    let output = home.run(&[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // First run persists a baseline snapshot for delta checks.
    assert!(home.saved_state().contains("\"routed\":10"));
}

#[test]
fn failure_ratio_alerts_on_delta_window_only() {
    let home = Home::new();
    let gateway = Gateway::start(None, vec![deployments(&[("d1", 107, 3, 0, false, false)])]);
    home.record(&gateway.listen);
    let config = home.config("failure_ratio = 0.1\nmin_attempts = 5\n");
    home.state(
        "{\"deployments\":{\"d1\":{\"routed\":100,\"failed_attempts\":0,\"stream_errors\":0}}}",
    );
    // 3 of 10 new attempts failed (30% > 10%).
    let output = home.run(&["--config", config.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("ALERT"), "{stderr}");
    assert!(stderr.contains("failure"), "{stderr}");
    assert!(stderr.contains("d1"), "{stderr}");
    // Second run: no new attempts; the window is empty so nothing alerts.
    let output = home.run(&["--config", config.to_str().unwrap()]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn counter_reset_skips_ratio_but_updates_baseline() {
    let home = Home::new();
    let gateway = Gateway::start(None, vec![deployments(&[("d1", 5, 0, 0, false, false)])]);
    home.record(&gateway.listen);
    let config = home.config("failure_ratio = 0.01\nmin_attempts = 0\n");
    // Previous snapshot has larger counters (a reload resets them).
    home.state(
        "{\"deployments\":{\"d1\":{\"routed\":100,\"failed_attempts\":50,\"stream_errors\":0}}}",
    );
    let output = home.run(&["--config", config.to_str().unwrap()]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(home.saved_state().contains("\"routed\":5"));
}

#[test]
fn cooldown_and_disabled_thresholds_alert() {
    let home = Home::new();
    let gateway = Gateway::start(
        None,
        vec![deployments(&[
            ("d1", 0, 0, 0, false, true),
            ("d2", 0, 0, 0, false, true),
            ("d3", 0, 0, 0, true, false),
        ])],
    );
    home.record(&gateway.listen);
    let config = home.config("max_cooldowns = 1\nmax_disabled = 0\n");
    let output = home.run(&["--config", config.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cooldown"), "{stderr}");
    assert!(stderr.contains("disabled"), "{stderr}");
}

#[test]
fn admin_bearer_key_is_sent_from_key_file() {
    let home = Home::new();
    let gateway = Gateway::start(Some("s3cret"), vec![]);
    home.record(&gateway.listen);
    let key = home.0.join("dashboard.key");
    fs::write(&key, "s3cret\n").unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    let config = home.config(&format!(
        "key_file = \"{}\"\nmax_disabled = 0\n",
        key.display()
    ));
    let output = home.run(&["--config", config.to_str().unwrap()]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Without the key configured, the 401 surfaces as an admin alert.
    let config = home.config("max_disabled = 0\n");
    let output = home.run(&["--config", config.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("ALERT"));
}
