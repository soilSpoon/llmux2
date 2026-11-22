use std::{env, fs, path::PathBuf};

fn run() -> Result<(), String> {
    let mut input = None;
    let mut output = None;
    let mut secrets = None;
    let mut account = None;
    let mut token = None;
    let mut workers_models = None;
    let mut kilo_free = None;
    let mut kilo_key = None;
    let mut devin_models = None;
    let mut nous_state = None;
    let mut nous_models = None;
    let mut replacements = false;
    let mut fallback = None;
    let mut key = None;
    let mut workers_rpm: Option<u32> = None;
    let mut args = env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--openrouter-replacements" {
            if replacements {
                return Err("duplicate argument".into());
            }
            replacements = true;
            continue;
        }
        if arg == "--workers-ai-rpm" {
            if workers_rpm.is_some() {
                return Err("duplicate argument".into());
            }
            let value = args.next().ok_or("missing argument value")?;
            workers_rpm = Some(
                value
                    .to_str()
                    .and_then(|v| v.parse::<u32>().ok())
                    .filter(|n| *n > 0)
                    .ok_or("--workers-ai-rpm must be a positive integer")?,
            );
            continue;
        }
        let target = match arg.to_str() {
            Some("--input") => &mut input,
            Some("--output") => &mut output,
            Some("--secrets-dir") => &mut secrets,
            Some("--cloudflare-account-id-file") => &mut account,
            Some("--cloudflare-token-file") => &mut token,
            Some("--workers-ai-models") => &mut workers_models,
            Some("--kilo-free-models") => &mut kilo_free,
            Some("--kilo-key-file") => &mut kilo_key,
            Some("--devin-models") => &mut devin_models,
            Some("--nous-state") => &mut nous_state,
            Some("--nous-models") => &mut nous_models,
            Some("--omniroute-fallback") => &mut fallback,
            Some("--omniroute-key") => &mut key,
            _ => return Err(
                "usage: llmux-import --input export.json --output llmux.toml [--secrets-dir DIR] [--cloudflare-account-id-file PATH --cloudflare-token-file PATH] [--workers-ai-models PATH] [--kilo-free-models PATH] [--kilo-key-file PATH] [--devin-models PATH] [--nous-state PATH --nous-models PATH] [--openrouter-replacements] [--workers-ai-rpm RPM] [--omniroute-fallback BASE_URL --omniroute-key SECRET_REF]"
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
    let options = llmux_import::ImportOptions {
        cloudflare_account_id_file: account.as_deref(),
        cloudflare_token_file: token.as_deref(),
        workers_ai_models: workers_models.as_deref(),
        kilo_free_models: kilo_free.as_deref(),
        kilo_key_file: kilo_key.as_deref(),
        devin_models: devin_models.as_deref(),
        nous_state: nous_state.as_deref(),
        nous_models: nous_models.as_deref(),
        openrouter_replacements: replacements,
        workers_ai_rpm: workers_rpm,
        omniroute_fallback: fallback,
    };
    let report =
        llmux_import::import_with_options(&content, &output, secrets.as_deref(), &options)?;
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
