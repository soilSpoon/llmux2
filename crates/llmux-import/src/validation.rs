use serde_json::Value;
use std::{
    collections::HashMap,
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

fn substitute_env(
    value: &mut Value,
    dir: &Path,
    markers: &mut HashMap<String, ValidationMarker>,
) -> Result<(), String> {
    match value {
        Value::String(reference) if reference.starts_with("env:") => {
            // Secret references cannot be resolved on the importing host; a
            // per-name marker keeps allowed_keys ⊆ client_keys checks exact.
            let marker = match markers.get(reference) {
                Some(marker) => marker,
                None => {
                    let marker = ValidationMarker::create(dir)?;
                    markers.entry(reference.clone()).or_insert(marker)
                }
            };
            *reference = format!("file:{}", marker.0.display());
        }
        Value::Array(list) => {
            for item in list {
                substitute_env(item, dir, markers)?;
            }
        }
        Value::Object(object) => {
            for (_, item) in object.iter_mut() {
                substitute_env(item, dir, markers)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn validate(config: &Value, dir: &Path, external: &[&str]) -> Result<(), String> {
    let mut config = config.clone();
    let mut temporary = None;
    if !external.is_empty() {
        let marker = ValidationMarker::create(dir)?;
        let reference = format!("file:{}", marker.0.display());
        for credential in config["credentials"].as_array_mut().into_iter().flatten() {
            if credential["key"]
                .as_str()
                .is_some_and(|key| external.contains(&key))
            {
                credential["key"] = reference.clone().into();
            }
        }
        temporary = Some(marker);
    }
    let mut markers = HashMap::new();
    substitute_env(&mut config, dir, &mut markers)?;
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
    use serde_json::json;

    #[test]
    fn failed_validation_removes_the_public_marker() {
        let dir = tempfile::tempdir().unwrap();
        let config = json!({"server":{"listen":"invalid"},"credentials":[],"models":[]});
        assert!(validate(&config, dir.path(), &[]).is_err());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
