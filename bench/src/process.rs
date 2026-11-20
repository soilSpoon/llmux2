use crate::Result;
use std::{
    collections::HashMap,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::Duration,
};

pub fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace parent")
}

pub fn build() -> Result<HashMap<String, PathBuf>> {
    let output = Command::new("cargo")
        .args([
            "build",
            "--locked",
            "--release",
            "-p",
            "llmux-server",
            "-p",
            "llmux-testkit",
            "--bin",
            "llmux-server",
            "--bin",
            "mock_load_upstream",
            "--message-format=json-render-diagnostics",
        ])
        .current_dir(root())
        .output()?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned().into());
    }
    let mut binaries = HashMap::new();
    for line in output
        .stdout
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
    {
        let event: serde_json::Value = serde_json::from_slice(line)?;
        if event["reason"] == "compiler-artifact" {
            if let (Some(name), Some(path)) = (
                event["target"]["name"].as_str(),
                event["executable"].as_str(),
            ) {
                binaries.insert(name.to_owned(), PathBuf::from(path));
            }
        }
    }
    Ok(binaries)
}

pub struct Server {
    child: Child,
    pub address: String,
}
impl Server {
    pub fn start(binary: &Path, args: &[String], workers: usize) -> Result<Self> {
        let mut child = Command::new(binary)
            .args(args)
            .env("TOKIO_WORKER_THREADS", workers.to_string())
            .env("LLMUX_BENCH_CLIENT_KEY", "mock-gateway")
            .env("LLMUX_BENCH_UPSTREAM_KEY", "mock-backend")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let stderr = child.stderr.take().ok_or("missing stderr")?;
        let (tx, rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut lines = BufReader::new(stderr).lines();
            let first = lines.next().transpose();
            let _ = tx.send(first);
            for _ in lines {}
        });
        let mut server = Self {
            child,
            address: String::new(),
        };
        let line = rx
            .recv_timeout(Duration::from_secs(15))??
            .ok_or("server produced no startup output")?;
        let prefix = format!(
            "{} listening on ",
            binary
                .file_name()
                .ok_or("missing binary name")?
                .to_string_lossy()
        );
        server.address = line
            .strip_prefix(&prefix)
            .ok_or_else(|| format!("server failed to start: {line}"))?
            .to_owned();
        Ok(server)
    }
    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct Directory(pub PathBuf);
impl Directory {
    pub fn new() -> Result<Self> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "llmux-bench-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
