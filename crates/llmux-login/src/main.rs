use llmux_login::{Endpoints, Flow, PendingLogin};
use std::path::PathBuf;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("llmux-login: {error}");
        std::process::exit(1);
    }
}

fn usage() -> &'static str {
    "usage: llmux-login [codex|xai] [--dir DIR]\n\
     codex: Continue with ChatGPT. Default dir: ~/.config/llmux/codex/\n\
     xai: Continue with xAI (api.x.ai). Default dir: ~/.config/llmux/xai/\n\
     Use separate directories for different account/workspace registrations."
}

fn endpoints(provider: &str) -> Result<Endpoints, &'static str> {
    match provider {
        "codex" => Ok(Endpoints::default()),
        "xai" => Ok(Endpoints {
            authorize: url::Url::parse("https://auth.x.ai/oauth2/authorize").unwrap(),
            token: url::Url::parse("https://auth.x.ai/oauth2/token").unwrap(),
            issuer: url::Url::parse("https://auth.x.ai").unwrap(),
        }),
        _ => Err(usage()),
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut args = std::env::args_os().skip(1);
    let mut dir = None;
    let mut provider = None;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--dir") if dir.is_none() => {
                dir = Some(PathBuf::from(args.next().ok_or("--dir requires a path")?))
            }
            Some("codex" | "xai") if provider.is_none() => {
                provider = Some(arg.to_str().unwrap().to_owned())
            }
            Some("--help" | "-h") => {
                println!("{}", usage());
                return Ok(());
            }
            _ => return Err(usage().into()),
        }
    }
    let provider = provider.as_deref().unwrap_or("codex");
    let dir = match dir {
        Some(dir) => dir,
        None => PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set; specify --dir")?)
            .join(format!(".config/llmux/{provider}")),
    };
    let flow = match provider {
        "codex" => Flow::codex(endpoints(provider)?),
        "xai" => Flow::xai(endpoints(provider)?),
        _ => return Err(usage().into()),
    };
    let mut pending = PendingLogin::start(&dir, flow.clone()).await?;
    let label = if provider == "xai" { "xAI" } else { "ChatGPT" };
    let result = loop {
        println!("Continue with {label}:\n{}\nThis private sign-in URL may contain account hints; do not share it.", pending.authorization_url());
        if webbrowser::open(pending.authorization_url().as_str()).is_err() {
            eprintln!("Could not open the browser. Open the URL above manually.");
        }
        match pending.finish().await {
            Ok(result) => break result,
            Err(error) => {
                if let Some(client) = error.retry_client_id() {
                    eprintln!("Authorization code expired or was already used; restarting sign-in with the issued client ID.");
                    pending = PendingLogin::retry(&dir, flow.clone(), client).await?;
                } else {
                    return Err(error.into());
                }
            }
        }
    };
    println!(
        "Sign-in complete. Paste this into llmux.toml:\n{}",
        result.snippet
    );
    Ok(())
}
