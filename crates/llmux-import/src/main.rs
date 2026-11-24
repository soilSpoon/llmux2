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
    let mut excluded = None;
    let mut listen = None;
    let mut overlay = None;
    let mut free_providers = None;
    let mut canonical_map = None;
    let mut workers_rpm: Option<u32> = None;
    let mut scan_free = None;
    let mut scan_page_url = None;
    let mut scan_probe_base = None;
    let mut scan_key_file = None;
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
            Some("--exclude-models") => &mut excluded,
            Some("--listen") => &mut listen,
            Some("--overlay") => &mut overlay,
            Some("--free-providers") => &mut free_providers,
            Some("--canonical-map") => &mut canonical_map,
            Some("--scan-free") => &mut scan_free,
            Some("--scan-page-url") => &mut scan_page_url,
            Some("--scan-probe-base") => &mut scan_probe_base,
            Some("--scan-key-file") => &mut scan_key_file,
            _ => return Err(
                "usage: llmux-import --input export.json --output llmux.toml [--secrets-dir DIR] [--cloudflare-account-id-file PATH --cloudflare-token-file PATH] [--workers-ai-models PATH] [--kilo-free-models PATH] [--kilo-key-file PATH] [--devin-models PATH] [--nous-state PATH --nous-models PATH] [--openrouter-replacements] [--workers-ai-rpm RPM] [--omniroute-fallback BASE_URL --omniroute-key SECRET_REF] [--exclude-models PATH] [--listen HOST:PORT] [--overlay PATH] [--free-providers PATH] [--canonical-map PATH] | llmux-import --scan-free PROVIDER [--scan-page-url URL] [--scan-probe-base URL] [--scan-key-file PATH]"
                    .into(),
            ),
        };
        if target.is_some() {
            return Err("duplicate argument".into());
        }
        *target = Some(PathBuf::from(args.next().ok_or("missing argument value")?));
    }
    if let Some(provider) = scan_free {
        return scan_free_candidates(&provider, scan_page_url, scan_probe_base, scan_key_file);
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
        listen: listen.as_deref().and_then(|p| p.to_str()),
        exclude_models: excluded.as_deref(),
        overlay: overlay.as_deref(),
        free_providers: free_providers.as_deref(),
        canonical_map: canonical_map.as_deref(),
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

fn scan_free_candidates(
    provider: &std::path::Path,
    page_url: Option<PathBuf>,
    probe_base: Option<PathBuf>,
    key_file: Option<PathBuf>,
) -> Result<(), String> {
    use llmux_import::discovery;
    let provider = provider
        .to_str()
        .ok_or("--scan-free provider must be UTF-8")?;
    let url_of = |value: Option<PathBuf>, default: &str| -> Result<String, String> {
        match value {
            Some(path) => path
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| "scan urls must be UTF-8".to_string()),
            None => Ok(default.to_string()),
        }
    };
    let key = key_file
        .map(|path| {
            fs::read_to_string(&path)
                .map_err(|_| "could not read --scan-key-file".to_string())
                .map(|key| key.trim().to_string())
        })
        .transpose()?;
    let (scan, known) = match provider {
        "experlabs" | "xplabs" => {
            let page = url_of(page_url, discovery::EXPERLABS_PAGE)?;
            let base = url_of(probe_base, discovery::EXPERLABS_API)?;
            let known: Vec<String> = llmux_import::xplabs_free_ids()
                .map(|id| id.to_string())
                .collect();
            (discovery::ScanPlan::Experlabs { page, base }, known)
        }
        other => return Err(format!("--scan-free has no adapter for provider {other:?}")),
    };
    let client = discovery::client()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let rows = runtime.block_on(scan.run(&client, key.as_deref(), &known))?;
    for row in rows {
        let verdict = row
            .verdict
            .map(|v| v.to_string())
            .unwrap_or_else(|| "unverified".to_string());
        println!(
            "candidate {}\tlisted={}\tverdict={}\tevidence={}",
            row.id, row.listed, verdict, row.evidence
        );
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("llmux-import: {error}");
        std::process::exit(1);
    }
}
