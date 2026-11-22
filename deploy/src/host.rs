use crate::{scoped_address_parts, Result, Runtime};
use serde_json::{json, Value};
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    net::{IpAddr, SocketAddr, SocketAddrV6, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
pub fn systemctl_args(action: &str, slot: &str) -> Vec<String> {
    let mut args = vec!["--user".into(), action.into()];
    if action == "show" {
        args.extend(["-p".into(), "ActiveState,NRestarts,MainPID".into()]);
    }
    args.push(format!("llmux2@{slot}.service"));
    args
}
pub fn unit_active(code: i32, state: &str) -> Result<bool> {
    if !matches!(code, 0 | 3) {
        return Err("invalid is-active exit code".into());
    }
    match state.trim() {
        "inactive" | "failed" => Ok(false),
        "active" | "reloading" | "activating" | "deactivating" | "refreshing" => Ok(true),
        state => Err(format!("unexpected unit state: {state}").into()),
    }
}
pub fn health(listen: &str) -> Result<bool> {
    let (host, scope, port) = scoped_address_parts(listen)?;
    let port = u16::try_from(port).map_err(|_| "Port out of range 0-65535")?;
    let operation = || -> io::Result<bool> {
        let timeout = Duration::from_secs(2);
        let address = socket_address(host, port, scope)?;
        let mut stream = TcpStream::connect_timeout(&address, timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        write!(stream, "GET /healthz HTTP/1.0\r\nHost: {}\r\n\r\n", address)?;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        Read::by_ref(&mut reader).take(65537).read_line(&mut line)?;
        let mut status = line.split_whitespace();
        if !status.next().is_some_and(|s| s.starts_with("HTTP/")) || status.next() != Some("200") {
            return Ok(false);
        }
        let mut content_length = None;
        let mut chunked = false;
        for _ in 0..100 {
            line.clear();
            Read::by_ref(&mut reader).take(65537).read_line(&mut line)?;
            if line.len() > 65536 || line.is_empty() {
                return Ok(false);
            }
            if line == "\r\n" || line == "\n" {
                return Ok(health_body(&mut reader, chunked, content_length)? == b"ok");
            }
            if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    content_length = Some(
                        value
                            .trim()
                            .parse::<u64>()
                            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
                    );
                }
                if name.eq_ignore_ascii_case("transfer-encoding") {
                    chunked = value.trim().eq_ignore_ascii_case("chunked");
                }
            }
        }
        Ok(false)
    };
    Ok(operation().unwrap_or(false))
}
fn health_body(
    reader: &mut impl BufRead,
    chunked: bool,
    length: Option<u64>,
) -> io::Result<Vec<u8>> {
    let mut prefix = Vec::with_capacity(3);
    if !chunked {
        Read::by_ref(reader)
            .take(length.unwrap_or(3).min(3))
            .read_to_end(&mut prefix)?;
        return Ok(prefix);
    }
    loop {
        let mut line = String::new();
        Read::by_ref(reader).take(65537).read_line(&mut line)?;
        let size = u64::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if size == 0 {
            return Ok(prefix);
        }
        let count = size.min((3 - prefix.len()) as u64) as usize;
        let start = prefix.len();
        prefix.resize(start + count, 0);
        reader.read_exact(&mut prefix[start..])?;
        if prefix.len() == 3 {
            return Ok(prefix);
        }
        let mut crlf = [0; 2];
        reader.read_exact(&mut crlf)?;
        if crlf != *b"\r\n" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid chunk terminator",
            ));
        }
    }
}
fn socket_address(host: IpAddr, port: u16, scope: Option<&str>) -> io::Result<SocketAddr> {
    let Some(scope) = scope else {
        return Ok(SocketAddr::new(host, port));
    };
    let IpAddr::V6(ip) = host else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "IPv4 address cannot have a scope id",
        ));
    };
    let scope_id = if scope.bytes().all(|b| b.is_ascii_digit()) {
        scope
            .parse::<u32>()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?
    } else {
        let name = std::ffi::CString::new(scope)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
        if index == 0 {
            return Err(io::Error::last_os_error());
        }
        index
    };
    Ok(SocketAddr::V6(SocketAddrV6::new(ip, port, 0, scope_id)))
}
pub struct Host {
    root: PathBuf,
}
impl Host {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}
impl Runtime for Host {
    fn systemctl(&mut self, action: &str, slot: &str) -> Result<Value> {
        let mut command = Command::new("systemctl");
        command.args(systemctl_args(action, slot));
        let result = run_command(&mut command, Duration::from_secs(130))?;
        let stdout = String::from_utf8_lossy(&result.stdout);
        if action == "is-active" && matches!(result.status.code(), Some(0 | 3)) {
            return unit_active(result.status.code().unwrap(), &stdout).map(Value::Bool);
        }
        if !result.status.success() {
            return Err(format!(
                "systemctl {action} {slot} failed: {}",
                String::from_utf8_lossy(&result.stderr).trim()
            )
            .into());
        }
        if action == "show" {
            let fields: serde_json::Map<String, Value> = stdout
                .lines()
                .filter_map(|line| line.split_once('='))
                .map(|(k, v)| (k.into(), json!(v)))
                .collect();
            return Ok(Value::Object(fields));
        }
        Ok(json!(true))
    }
    fn check(&mut self, binary: &Path, config: &Path) -> Result<String> {
        let result = run_command(
            Command::new(binary)
                .arg("--check")
                .arg(config)
                .current_dir(&self.root),
            Duration::from_secs(120),
        )?;
        if !result.status.success() {
            return Err(format!(
                "configuration validation failed: {}",
                String::from_utf8_lossy(&result.stderr).trim()
            )
            .into());
        }
        Ok(String::from_utf8(result.stdout)?.trim().into())
    }
    fn health(&mut self, listen: &str) -> Result<bool> {
        health(listen)
    }
    fn sleep(&mut self, seconds: f64) {
        thread::sleep(Duration::from_secs_f64(seconds));
    }
}
fn capture(mut pipe: impl Read + Send + 'static) -> mpsc::Receiver<io::Result<Vec<u8>>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = pipe.read_to_end(&mut bytes).map(|_| bytes);
        let _ = sender.send(result);
    });
    receiver
}
fn run_command(command: &mut Command, timeout: Duration) -> Result<Output> {
    let deadline = Instant::now() + timeout;
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = capture(child.stdout.take().unwrap());
    let stderr = capture(child.stderr.take().unwrap());
    let mut operation = || -> Result<Output> {
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                return Err("timeout".into());
            }
            thread::sleep(Duration::from_millis(1));
        };
        let stdout = stdout.recv_timeout(deadline.saturating_duration_since(Instant::now()))??;
        let stderr = stderr.recv_timeout(deadline.saturating_duration_since(Instant::now()))??;
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    };
    match operation() {
        Ok(output) => Ok(output),
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            if Instant::now() >= deadline {
                Err(format!(
                    "Command '[{}]' timed out after {} seconds",
                    std::iter::once(command.get_program())
                        .chain(command.get_args())
                        .map(|arg| format!(
                            "'{}'",
                            arg.to_string_lossy()
                                .replace('\\', "\\\\")
                                .replace('\'', "\\'")
                                .replace('\n', "\\n")
                                .replace('\r', "\\r")
                                .replace('\t', "\\t")
                        ))
                        .collect::<Vec<_>>()
                        .join(", "),
                    timeout.as_secs_f64()
                )
                .into())
            } else {
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scoped_health_socket_uses_numeric_and_named_interface_ids() {
        let ip = "fe80::1".parse().unwrap();
        let SocketAddr::V6(numeric) = socket_address(ip, 8080, Some("2")).unwrap() else {
            panic!()
        };
        assert_eq!(numeric.scope_id(), 2);
        assert_eq!(
            numeric.ip(),
            &"fe80::1".parse::<std::net::Ipv6Addr>().unwrap()
        );
        let SocketAddr::V6(named) = socket_address(ip, 8080, Some("lo")).unwrap() else {
            panic!()
        };
        assert_ne!(named.scope_id(), 0);
        assert!(socket_address(ip, 8080, Some("llmux-no-interface")).is_err());
        assert!(socket_address(ip, 8080, Some("4294967296")).is_err());
        assert!(socket_address(ip, 8080, Some("lo\0")).is_err());
    }
    #[test]
    fn subprocess_deadline_kills_and_reaps_a_stuck_command() {
        let error = run_command(
            Command::new("/bin/sh").args(["-c", "while :; do :; done"]),
            Duration::from_millis(10),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Command '['/bin/sh', '-c', 'while :; do :; done']' timed out after 0.01 seconds"
        );
    }
    #[test]
    fn subprocess_drains_both_pipes_without_deadlocking() {
        let output = run_command(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "host::tests::chatty_process_fixture",
                    "--nocapture",
                ])
                .env("LLMUX_OPS_TEST_CHATTY", "1"),
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(output.status.success());
        assert!(output
            .stdout
            .windows(8192)
            .any(|bytes| bytes == [b'o'; 8192]));
        assert_eq!(output.stderr, vec![b'e'; 262144]);
    }
    #[test]
    fn chatty_process_fixture() {
        if std::env::var_os("LLMUX_OPS_TEST_CHATTY").is_some() {
            io::stdout().write_all(&vec![b'o'; 262144]).unwrap();
            io::stderr().write_all(&vec![b'e'; 262144]).unwrap();
        }
    }
}
