#![cfg(unix)]

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

struct ServerProcess(Child);

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn sigterm_exits_successfully() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        r#"
[server]
listen = "127.0.0.1:0"
client_keys = ["env:LLMUX_SIGNAL_TEST_KEY"]
drain_grace_ms = 100
shutdown_timeout_ms = 1000
[[models]]
name = "test"
[[models.deployments]]
id = "test"
auth = "none"
provider = "openai_compat"
base_url = "http://127.0.0.1:1"
upstream_model = "test"
"#,
    )
    .unwrap();
    let mut child = ServerProcess(
        Command::new(env!("CARGO_BIN_EXE_llmux-server"))
            .arg(path)
            .env("LLMUX_SIGNAL_TEST_KEY", "synthetic-key")
            .env_remove("NOTIFY_SOCKET")
            .env_remove("WATCHDOG_USEC")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stderr = child.0.stderr.take().unwrap();
    let (ready, address) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let line = line.unwrap();
            if let Some(address) = line.strip_prefix("llmux-server listening on ") {
                ready.send(address.to_owned()).unwrap();
                break;
            }
        }
    });
    let address = address
        .recv_timeout(Duration::from_secs(5))
        .expect("server must bind an ephemeral port");
    let mut health = TcpStream::connect(address).unwrap();
    health
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    health
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    health.read_to_string(&mut response).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "server must be serving before SIGTERM: {response}"
    );
    assert!(Command::new("kill")
        .arg("-TERM")
        .arg(child.0.id().to_string())
        .status()
        .unwrap()
        .success());
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(
                status.success(),
                "SIGTERM must exit successfully, got {status}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "SIGTERM must exit within the shutdown budget"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
