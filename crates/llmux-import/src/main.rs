use std::{env, fs, path::PathBuf};

fn run() -> Result<(), String> {
    let mut input = None;
    let mut output = None;
    let mut secrets = None;
    let mut account = None;
    let mut token = None;
    let mut kilo_free = None;
    let mut kilo_key = None;
    let mut replacements = false;
    let mut args = env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--openrouter-replacements" {
            if replacements {
                return Err("duplicate argument".into());
            }
            replacements = true;
            continue;
        }
        let target = match arg.to_str() {
            Some("--input") => &mut input,
            Some("--output") => &mut output,
            Some("--secrets-dir") => &mut secrets,
            Some("--cloudflare-account-id-file") => &mut account,
            Some("--cloudflare-token-file") => &mut token,
            Some("--kilo-free-models") => &mut kilo_free,
            Some("--kilo-key-file") => &mut kilo_key,
            _ => return Err(
                "usage: llmux-import --input export.json --output llmux.toml [--secrets-dir DIR] [--cloudflare-account-id-file PATH --cloudflare-token-file PATH] [--kilo-free-models PATH] [--kilo-key-file PATH] [--openrouter-replacements]"
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
    let content = fs::read_to_string(input).map_err(|_| "could not read input JSON")?;
    let options = llmux_import::ImportOptions {
        cloudflare_account_id_file: account.as_deref(),
        cloudflare_token_file: token.as_deref(),
        kilo_free_models: kilo_free.as_deref(),
        kilo_key_file: kilo_key.as_deref(),
        openrouter_replacements: replacements,
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
