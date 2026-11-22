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
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let file = format!("validation-{}-{}", std::process::id(), id);
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
        // llmux-config requires printable ASCII without whitespace and
        // resolves secrets by file contents, so markers hold opaque
        // distinct bytes per reference, never the reference itself.
        file.write_all(format!("public-validation-only-{id}").as_bytes())
            .map_err(|_| "could not write public validation file")?;
        Ok(marker)
    }
}

impl Drop for ValidationMarker {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn marker_for<'a>(
    dir: &Path,
    markers: &'a mut HashMap<String, ValidationMarker>,
    reference: &str,
) -> Result<&'a ValidationMarker, String> {
    if !markers.contains_key(reference) {
        let marker = ValidationMarker::create(dir)?;
        markers.insert(reference.to_owned(), marker);
    }
    Ok(&markers[reference])
}

fn substitute_refs(
    value: &mut Value,
    dir: &Path,
    markers: &mut HashMap<String, ValidationMarker>,
) -> Result<(), String> {
    match value {
        Value::String(reference) => {
            let resolvable = if let Some(name) = reference.strip_prefix("env:") {
                !name.is_empty() && std::env::var_os(name).is_some()
            } else if let Some(path) = reference.strip_prefix("file:") {
                !path.is_empty() && fs::read_to_string(path).is_ok()
            } else {
                true
            };
            // References that resolve here are left for llmux-config to
            // resolve to their real values, so equality and validity match
            // deploy-time semantics; unresolvable ones get a marker.
            if !resolvable {
                let marker = marker_for(dir, markers, reference)?;
                *reference = format!("file:{}", marker.0.display());
            }
        }
        Value::Array(list) => {
            for item in list {
                substitute_refs(item, dir, markers)?;
            }
        }
        Value::Object(object) => {
            for (_, item) in object.iter_mut() {
                substitute_refs(item, dir, markers)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn validate(config: &Value, dir: &Path) -> Result<(), String> {
    let mut config = config.clone();
    let mut markers: HashMap<String, ValidationMarker> = HashMap::new();
    substitute_refs(&mut config, dir, &mut markers)?;
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
        assert!(validate(&config, dir.path()).is_err());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn resolved_references_compare_by_real_value() {
        // Two differently named env references holding the same value
        // resolve equal at deploy time, so import must agree.
        unsafe {
            std::env::set_var("LLMUX_TEST_GATE_EQ", "shared-test-key");
            std::env::set_var("LLMUX_TEST_OWNER_EQ", "shared-test-key");
        }
        let dir = tempfile::tempdir().unwrap();
        let deployment = json!({"id":"d","auth":"none","provider":"openai_compat","base_url":"http://127.0.0.1/","upstream_model":"m"});
        let config = json!({"server":{"listen":"127.0.0.1:1","client_keys":["env:LLMUX_TEST_GATE_EQ"]},"credentials":[],"models":[{"name":"m","allowed_keys":["env:LLMUX_TEST_OWNER_EQ"],"deployments":[deployment]}]});
        assert!(validate(&config, dir.path()).is_ok());
        unsafe {
            std::env::remove_var("LLMUX_TEST_GATE_EQ");
            std::env::remove_var("LLMUX_TEST_OWNER_EQ");
        }
    }

    #[test]
    fn unresolvable_file_reference_with_spaces_validates() {
        let dir = tempfile::tempdir().unwrap();
        let deployment = json!({"id":"d","auth":"none","provider":"openai_compat","base_url":"http://127.0.0.1/","upstream_model":"m"});
        let config = json!({"server":{"listen":"127.0.0.1:1","client_keys":["file:/private/Workers AI/token"]},"credentials":[],"models":[{"name":"m","allowed_keys":["file:/private/Workers AI/token"],"deployments":[deployment]}]});
        assert!(validate(&config, dir.path()).is_ok());
    }

    #[test]
    fn distinct_env_references_stay_distinct() {
        let dir = tempfile::tempdir().unwrap();
        let deployment = json!({"id":"d","auth":"none","provider":"openai_compat","base_url":"http://127.0.0.1/","upstream_model":"m"});
        // dashboard_key must differ from every client_keys entry, and a
        // subset allowed_keys entry must still validate.
        let ok = json!({"server":{"listen":"127.0.0.1:1","client_keys":["env:GATE"],"dashboard_key":"env:ADMIN"},"credentials":[],"models":[{"name":"m","allowed_keys":["env:GATE"],"deployments":[deployment.clone()]}]});
        assert!(validate(&ok, dir.path()).is_ok());
        // An allowed_keys entry naming a different env ref is not a subset.
        let bad = json!({"server":{"listen":"127.0.0.1:1","client_keys":["env:GATE"]},"credentials":[],"models":[{"name":"m","allowed_keys":["env:ADMIN"],"deployments":[deployment]}]});
        assert!(validate(&bad, dir.path()).is_err());
    }
}
