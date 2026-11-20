mod cli;
mod host;

pub use cli::main_entry;
pub use host::{health, systemctl_args, unit_active, Host};
use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    net::IpAddr,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub trait Runtime {
    fn systemctl(&mut self, action: &str, slot: &str) -> Result<Value>;
    fn check(&mut self, binary: &Path, config: &Path) -> Result<String>;
    fn health(&mut self, listen: &str) -> Result<bool>;
    fn sleep(&mut self, seconds: f64);
}
pub fn atomic_write(
    path: &Path,
    writer: impl FnOnce(&mut File) -> io::Result<()>,
    mode: u32,
) -> io::Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let (temporary, mut output) = loop {
        let name = parent.join(format!(
            ".{}.{}-{}",
            path.file_name().unwrap_or_default().to_string_lossy(),
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&name)
        {
            Ok(file) => break (Temporary(name), file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    writer(&mut output)?;
    output.flush()?;
    output.sync_all()?;
    output.set_permissions(fs::Permissions::from_mode(mode))?;
    drop(output);
    fs::rename(&temporary.0, path)
}
struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
pub fn address_parts(listen: &str) -> Result<(IpAddr, i64)> {
    let (address, _, port) = scoped_address_parts(listen)?;
    Ok((address, port))
}
fn scoped_address_parts(listen: &str) -> Result<(IpAddr, Option<&str>, i64)> {
    let (host, port) = listen
        .rsplit_once(':')
        .ok_or("not enough values to unpack (expected 2, got 1)")?;
    let host = host.trim_matches(['[', ']']);
    let (ip, scope) = host
        .split_once('%')
        .map_or((host, None), |(ip, scope)| (ip, Some(scope)));
    let address: IpAddr = ip
        .parse()
        .map_err(|_| format!("'{host}' does not appear to be an IPv4 or IPv6 address"))?;
    if scope.is_some_and(|scope| scope.is_empty() || scope.contains('%'))
        || (scope.is_some() && address.is_ipv4())
    {
        return Err(format!("'{host}' does not appear to be an IPv4 or IPv6 address").into());
    }
    let unspecified = address.is_unspecified();
    let address = if unspecified {
        if address.is_ipv4() {
            IpAddr::from([127, 0, 0, 1])
        } else {
            IpAddr::from(std::net::Ipv6Addr::LOCALHOST)
        }
    } else {
        address
    };
    let port = port
        .trim()
        .parse()
        .map_err(|_| format!("invalid literal for int() with base 10: '{port}'"))?;
    Ok((address, if unspecified { None } else { scope }, port))
}
pub struct Deployment<R> {
    pub root: PathBuf,
    pub runtime: R,
}
impl<R: Runtime> Deployment<R> {
    pub fn new(root: PathBuf, runtime: R) -> Self {
        Self { root, runtime }
    }
    pub fn active_slots(&mut self) -> Result<Vec<String>> {
        let mut active = Vec::new();
        for slot in ["a", "b"] {
            if self.runtime.systemctl("is-active", slot)? == true {
                active.push(slot.into());
            }
        }
        Ok(active)
    }
    pub fn record(&self) -> Result<Option<Value>> {
        let bytes = match fs::read(self.root.join("active.json")) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let record: Value = serde_json::from_slice(&bytes)?;
        if !matches!(record["slot"].as_str(), Some("a" | "b")) {
            return Err("invalid recorded active slot".into());
        }
        address_parts(
            record["listen"]
                .as_str()
                .ok_or("invalid recorded listen address")?,
        )?;
        Ok(Some(record))
    }
    pub fn status(&mut self) -> Result<Value> {
        let record = self.record()?;
        let active = self.active_slots()?;
        let healthy = if let Some(record) = &record {
            self.runtime.health(record["listen"].as_str().unwrap())?
        } else {
            false
        };
        Ok(json!({"active":active,"recorded":record,"healthy":healthy}))
    }
    pub fn guard(&mut self) -> Result<()> {
        if !self.active_slots()?.is_empty() {
            return Ok(());
        }
        if let Some(record) = self.record()? {
            if !self.runtime.health(record["listen"].as_str().unwrap())? {
                self.runtime
                    .systemctl("start", record["slot"].as_str().unwrap())?;
            }
        }
        Ok(())
    }
    pub fn deploy(&mut self, binary: &Path, config: &Path, canary: f64) -> Result<String> {
        let resolve = |path: &Path| -> io::Result<PathBuf> {
            Ok(fs::canonicalize(path).unwrap_or(if path.is_absolute() {
                path.to_owned()
            } else {
                std::env::current_dir()?.join(path)
            }))
        };
        let binary = resolve(binary)?;
        let config = resolve(config)?;
        let listen = self.runtime.check(&binary, &config)?;
        address_parts(&listen)?;
        let active = self.active_slots()?;
        if active.len() > 1 {
            return Err(
                "both slots are active; inspect status and stop the unwanted slot first".into(),
            );
        }
        let old = active.first().map(String::as_str);
        let idle = match old {
            Some("a") => "b",
            Some(_) => "a",
            // With nothing running, stage over the slot the record does not
            // name so a failed deploy leaves a working recovery build.
            None => match self.record() {
                Ok(Some(record)) if record["slot"] == "a" => "b",
                // An unreadable record cannot identify a recovery build; a
                // successful deploy rewrites it, so do not block on it.
                _ => "a",
            },
        };
        let target_binary = self.root.join(format!("bin/llmux-server-{idle}"));
        let target_config = self.root.join(format!("llmux-{idle}.toml"));
        for (source, target, mode) in [
            (&binary, &target_binary, 0o700),
            (&config, &target_config, 0o600),
        ] {
            let mut incoming = File::open(source)?;
            atomic_write(
                target,
                |output| io::copy(&mut incoming, output).map(|_| ()),
                mode,
            )?;
        }
        if self.runtime.check(&target_binary, &target_config)? != listen {
            return Err("installed configuration changed its listen address".into());
        }
        let mut stopping_old = false;
        let result = (|| -> Result<()> {
            self.runtime.systemctl("start", idle)?;
            let mut last_pid = None;
            for sample in 0..7 {
                if sample > 0 {
                    self.runtime.sleep(0.5);
                }
                let state = self.runtime.systemctl("show", idle)?;
                let current_pid = state["MainPID"].as_str().unwrap_or("");
                if state["ActiveState"] != "active"
                    || state["NRestarts"] != "0"
                    || current_pid.is_empty()
                    || !current_pid.bytes().all(|c| c.is_ascii_digit())
                    || !current_pid.bytes().any(|c| c != b'0')
                    || last_pid.as_deref().is_some_and(|pid| pid != current_pid)
                {
                    return Err(
                        "new slot did not remain active without restarts during settle window"
                            .into(),
                    );
                }
                last_pid = Some(current_pid.to_owned());
            }
            let mut healthy = false;
            for _ in 0..60 {
                if self.runtime.health(&listen)? {
                    healthy = true;
                    break;
                }
                self.runtime.sleep(0.5);
            }
            if !healthy {
                return Err("new slot health check timed out".into());
            }
            let pid = last_pid.unwrap_or_default();
            let mut canary_left = canary;
            while canary_left > 0.0 {
                let step = canary_left.min(0.5);
                self.runtime.sleep(step);
                canary_left -= step;
                let state = self.runtime.systemctl("show", idle)?;
                let current_pid = state["MainPID"].as_str().unwrap_or("");
                if state["ActiveState"] != "active"
                    || state["NRestarts"] != "0"
                    || current_pid.is_empty()
                    || !current_pid.bytes().all(|c| c.is_ascii_digit())
                    || !current_pid.bytes().any(|c| c != b'0')
                    || current_pid != pid
                {
                    return Err("new slot did not remain stable during the canary window".into());
                }
            }
            if let Some(old) = old {
                stopping_old = true;
                self.runtime.systemctl("stop", old)?;
            }
            let record = format!(
                "{{\"slot\": {}, \"listen\": {}}}",
                serde_json::to_string(idle)?,
                serde_json::to_string(&listen)?
            );
            atomic_write(
                &self.root.join("active.json"),
                |output| output.write_all(record.as_bytes()),
                0o600,
            )?;
            self.runtime.systemctl("enable", idle)?;
            self.runtime
                .systemctl("disable", if idle == "a" { "b" } else { "a" })?;
            Ok(())
        })();
        if let Err(error) = result {
            if !stopping_old {
                if let Err(cleanup) = self.runtime.systemctl("stop", idle) {
                    return Err(format!(
                        "deployment failed: {error}; new slot cleanup also failed: {cleanup}"
                    )
                    .into());
                }
                return Err(format!("deployment failed; old slot unchanged: {error}").into());
            }
            return Err(format!(
                "handoff reached old-slot stop; new slot remains running; inspect status: {error}"
            )
            .into());
        }
        Ok(idle.into())
    }
}
