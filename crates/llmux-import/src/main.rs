use std::{env, fs, path::PathBuf};

fn run() -> Result<(), String> {
    let mut input = None;
    let mut output = None;
    let mut secrets = None;
    let mut fallback = None;
    let mut key = None;
    let mut args = env::args_os().skip(1);
    while let Some(arg) = args.next() {
        let target = match arg.to_str() {
            Some("--input") => &mut input,
            Some("--output") => &mut output,
            Some("--secrets-dir") => &mut secrets,
            Some("--omniroute-fallback") => &mut fallback,
            Some("--omniroute-key") => &mut key,
            _ => return Err(
                "usage: llmux-import --input export.json --output llmux.toml [--secrets-dir DIR] [--omniroute-fallback BASE_URL --omniroute-key SECRET_REF]"
                    .into(),
            ),
        };
        if target.is_some() {
            return Err("duplicate argument".into());
        }
        *target = Some(PathBuf::from(args.next().ok_or("missing argument value")?));
    }
    let input = input.ok_or("--input is required")?;
    let output = output.ok_or("--output is required")?;
    let fallback = match (fallback.as_deref(), key.as_deref()) {
        (None, None) => None,
        (Some(url), Some(key)) => Some(llmux_import::OmniRouteFallback {
            base_url: url.to_str().ok_or("--omniroute-fallback must be UTF-8")?,
            key: key.to_str().ok_or("--omniroute-key must be UTF-8")?,
        }),
        _ => {
            return Err("--omniroute-fallback and --omniroute-key must be provided together".into())
        }
    };
    let content = fs::read_to_string(input).map_err(|_| "could not read input JSON")?;
    let report =
        llmux_import::import_with_fallback(&content, &output, secrets.as_deref(), fallback)?;
    for warning in report.warnings {
        eprintln!("warning: {warning}");
    }
    eprintln!(
        "imported {} models, {} deployments, {} credentials, {} client keys",
        report.models, report.deployments, report.credentials, report.client_keys
    );
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("llmux-import: {error}");
        std::process::exit(1);
    }
}
