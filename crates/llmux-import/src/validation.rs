use serde_json::{json, Value};
use std::{fs, path::Path};

pub(super) fn validate(config: &Value, fallback: bool, dir: &Path) -> Result<(), String> {
    let mut config = config.clone();
    let mut temporary = None;
    if fallback {
        let reference = if let Some(reference) = config["credentials"][0]["key"].as_str() {
            reference.to_owned()
        } else {
            let name = format!("validation-{}", std::process::id());
            let reference = super::write_secret(dir, &name, "public-validation-only")?;
            temporary = Some(dir.join(name));
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
    if let Some(path) = temporary {
        fs::remove_file(path).map_err(|_| "could not remove public validation file")?;
    }
    result.map_err(str::to_owned)
}
