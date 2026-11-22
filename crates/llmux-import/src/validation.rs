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
    fn create(dir: &Path, name: &str) -> Result<Self, String> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let file = format!(
            "validation-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        );
        let path = dir.join(file);
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
        // Contents must differ per reference: llmux-config resolves secrets
        // by file contents, so identical bytes would compare equal.
        file.write_all(format!("public-validation-only:{name}").as_bytes())
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
                    let marker = ValidationMarker::create(dir, reference)?;
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
    let mut markers: HashMap<String, ValidationMarker> = HashMap::new();
    if !external.is_empty() {
        for credential in config["credentials"].as_array_mut().into_iter().flatten() {
            if let Some(key) = credential["key"]
                .as_str()
                .filter(|key| external.contains(key))
                .map(str::to_owned)
            {
                let marker = match markers.get(&key) {
                    Some(marker) => marker,
                    None => {
                        let marker = ValidationMarker::create(dir, &key)?;
                        markers.entry(key).or_insert(marker)
                    }
                };
                credential["key"] = format!("file:{}", marker.0.display()).into();
            }
        }
    }
    substitute_env(&mut config, dir, &mut markers)?;
    (|| {
        let value = toml::Value::try_from(&config).map_err(|_| "could not encode configuration")?;
        let content = toml::to_string(&value).map_err(|_| "could not encode configuration")?;
        llmux_config::RuntimeConfig::parse(&content)
            .map(|_| ())
            .map_err(|_| "generated configuration failed llmux validation (check upstream URLs)")
    })()
    .map_err(str::to_owned)
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

    #[test]
    fn distinct_env_references_stay_distinct() {
        let dir = tempfile::tempdir().unwrap();
        let deployment = json!({"id":"d","auth":"none","provider":"openai_compat","base_url":"http://127.0.0.1/","upstream_model":"m"});
        // dashboard_key must differ from every client_keys entry, and a
        // subset allowed_keys entry must still validate.
        let ok = json!({"server":{"listen":"127.0.0.1:1","client_keys":["env:GATE"],"dashboard_key":"env:ADMIN"},"credentials":[],"models":[{"name":"m","allowed_keys":["env:GATE"],"deployments":[deployment.clone()]}]});
        assert!(validate(&ok, dir.path(), &[]).is_ok());
        // An allowed_keys entry naming a different env ref is not a subset.
        let bad = json!({"server":{"listen":"127.0.0.1:1","client_keys":["env:GATE"]},"credentials":[],"models":[{"name":"m","allowed_keys":["env:ADMIN"],"deployments":[deployment]}]});
        assert!(validate(&bad, dir.path(), &[]).is_err());
    }
}
