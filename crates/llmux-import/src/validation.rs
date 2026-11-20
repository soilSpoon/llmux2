use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

struct ValidationMarker(PathBuf);

impl ValidationMarker {
    fn create(dir: &Path) -> Result<Self, String> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let name = format!(
            "validation-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        );
        let path = dir.join(name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .map_err(|_| "could not create public validation file")?;
        let marker = Self(path);
        file.write_all(b"public-validation-only")
            .map_err(|_| "could not write public validation file")?;
        Ok(marker)
    }
}

impl Drop for ValidationMarker {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub(super) fn validate(config: &Value, fallback: bool, dir: &Path) -> Result<(), String> {
    let mut config = config.clone();
    let mut temporary = None;
    if fallback {
        let reference = if let Some(reference) = config["credentials"][0]["key"].as_str() {
            reference.to_owned()
        } else {
            let marker = ValidationMarker::create(dir)?;
            let reference = format!("file:{}", marker.0.display());
            temporary = Some(marker);
            reference
        };
        config["server"]["client_keys"] = json!([reference]);
    }
    let result = (|| {
        let value = toml::Value::try_from(&config).map_err(|_| "could not encode configuration")?;
        let content = toml::to_string(&value).map_err(|_| "could not encode configuration")?;
        llmux_config::RuntimeConfig::parse(&content)
            .map(|_| ())
            .map_err(|_| "generated configuration failed llmux validation (check upstream URLs)")
    })();
    if let Some(marker) = temporary {
        fs::remove_file(&marker.0).map_err(|_| "could not remove public validation file")?;
    }
    result.map_err(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_validation_removes_the_public_marker() {
        let dir = tempfile::tempdir().unwrap();
        let config = json!({"server":{"listen":"invalid"},"credentials":[],"models":[]});
        assert!(validate(&config, true, dir.path()).is_err());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
