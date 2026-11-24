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

fn substitute_ref(
    reference: &mut String,
    dir: &Path,
    markers: &mut HashMap<String, ValidationMarker>,
) -> Result<(), String> {
    // References resolving here are left for llmux-config to resolve to
    // their real values, so equality and validity match deploy-time
    // semantics. Malformed or deterministically unreadable references
    // fail now, exactly as deploy preflight would; only valid references
    // absent on this host (an unset env var or a file living on the
    // deploy host) get a marker.
    let marker_needed = if let Some(name) = reference.strip_prefix("env:") {
        if name.is_empty() {
            return Err("empty env: reference in generated configuration".to_owned());
        }
        std::env::var_os(name).is_none()
    } else if let Some(path) = reference.strip_prefix("file:") {
        if path.is_empty() {
            return Err("empty file: reference in generated configuration".to_owned());
        }
        match fs::read_to_string(path) {
            Ok(_) => false,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                return Err(format!("key file {path} is not valid UTF-8"));
            }
            Err(_) => true,
        }
    } else {
        false
    };
    if marker_needed {
        let marker = marker_for(dir, markers, reference)?;
        *reference = format!("file:{}", marker.0.display());
    }
    Ok(())
}

fn each_string(
    value: &mut Value,
    dir: &Path,
    markers: &mut HashMap<String, ValidationMarker>,
) -> Result<(), String> {
    match value {
        Value::String(reference) => substitute_ref(reference, dir, markers),
        Value::Array(list) => {
            for item in list {
                each_string(item, dir, markers)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn substitute_refs(
    config: &mut Value,
    dir: &Path,
    markers: &mut HashMap<String, ValidationMarker>,
) -> Result<(), String> {
    // Only the fields llmux-config resolves as secrets or references:
    // other strings (model names, aliases, URLs) are plain values.
    if let Some(server) = config["server"].as_object_mut() {
        for key in ["client_keys", "dashboard_key", "end_user_salt"] {
            if let Some(field) = server.get_mut(key) {
                each_string(field, dir, markers)?;
            }
        }
    }
    for credential in config["credentials"].as_array_mut().into_iter().flatten() {
        if let Some(object) = credential.as_object_mut() {
            for key in ["key", "refresh_token", "client_secret"] {
                if let Some(field) = object.get_mut(key) {
                    each_string(field, dir, markers)?;
                }
            }
        }
    }
    for model in config["models"].as_array_mut().into_iter().flatten() {
        if let Some(object) = model.as_object_mut() {
            if let Some(field) = object.get_mut("allowed_keys") {
                each_string(field, dir, markers)?;
            }
            if let Some(deployments) = object.get_mut("deployments") {
                for deployment in deployments.as_array_mut().into_iter().flatten() {
                    if let Some(headers) = deployment
                        .as_object_mut()
                        .and_then(|object| object.get_mut("headers"))
                        .and_then(|headers| headers.as_object_mut())
                    {
                        for (_, header_value) in headers {
                            each_string(header_value, dir, markers)?;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

pub(super) fn validate(config: &Value, dir: &Path) -> Result<(), String> {
    let mut config = config.clone();
    let mut markers: HashMap<String, ValidationMarker> = HashMap::new();
    substitute_refs(&mut config, dir, &mut markers)?;
    (|| {
        let value = super::config_to_toml(&config)?;
        let content = toml::to_string(&value).map_err(|_| "could not encode configuration")?;
        llmux_config::RuntimeConfig::parse(&content)
            .map(|_| ())
            .map_err(|err| format!("generated configuration failed llmux validation: {err}"))
    })()
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
    fn non_secret_fields_are_not_treated_as_references() {
        // aliases are plain names at runtime, so env:/file:-shaped
        // strings there must not be resolved or rejected.
        let dir = tempfile::tempdir().unwrap();
        let deployment = json!({"id":"d","auth":"none","provider":"openai_compat","base_url":"http://127.0.0.1/","upstream_model":"m"});
        let config = json!({"server":{"listen":"127.0.0.1:1","client_keys":["env:GATE"]},"credentials":[],"models":[{"name":"m","aliases":["env:","file:"],"deployments":[deployment]}]});
        assert!(validate(&config, dir.path()).is_ok());
    }

    #[test]
    fn malformed_references_fail_import_like_deploy_preflight() {
        let dir = tempfile::tempdir().unwrap();
        let deployment = json!({"id":"d","auth":"none","provider":"openai_compat","base_url":"http://127.0.0.1/","upstream_model":"m"});
        let model = json!({"name":"m","allowed_keys":["file:"],"deployments":[deployment]});
        let config = json!({"server":{"listen":"127.0.0.1:1","client_keys":["file:"]},"credentials":[],"models":[model]});
        assert!(validate(&config, dir.path()).is_err());
        let env_config = json!({"server":{"listen":"127.0.0.1:1","client_keys":["env:"]},"credentials":[],"models":[]});
        assert!(validate(&env_config, dir.path()).is_err());
    }

    #[test]
    fn non_utf8_key_file_fails_import() {
        let dir = tempfile::tempdir().unwrap();
        let key_file = dir.path().join("key.bin");
        fs::write(&key_file, [0x80u8, 0x81, 0x82]).unwrap();
        let deployment = json!({"id":"d","auth":"none","provider":"openai_compat","base_url":"http://127.0.0.1/","upstream_model":"m"});
        let reference = format!("file:{}", key_file.display());
        let config = json!({"server":{"listen":"127.0.0.1:1","client_keys":[reference.clone()]},"credentials":[],"models":[{"name":"m","allowed_keys":[reference],"deployments":[deployment]}]});
        assert!(validate(&config, dir.path()).is_err());
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
