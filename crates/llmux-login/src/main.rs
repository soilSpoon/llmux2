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
    "usage: llmux-login [codex|xai|nous] [--dir DIR]\n\
     codex: Continue with ChatGPT. Default dir: ~/.config/llmux/codex/\n\
     xai: Continue with xAI (api.x.ai). Default dir: ~/.config/llmux/xai/\n\
     nous: Nous Portal subscription (device code). Default dir: ~/.config/llmux/nous/ (state.json)\n\
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

fn parse_args(
    args: impl Iterator<Item = std::ffi::OsString>,
) -> Result<(Option<String>, Option<PathBuf>), Box<dyn std::error::Error + Send + Sync>> {
    let mut dir = None;
    let mut provider = None;
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--dir") if dir.is_none() => {
                dir = Some(PathBuf::from(args.next().ok_or("--dir requires a path")?))
            }
            Some("codex" | "xai" | "nous") if provider.is_none() => {
                provider = Some(arg.to_str().unwrap().to_owned())
            }
            Some("--help" | "-h") => {
                println!("{}", usage());
                std::process::exit(0);
            }
            _ => return Err(usage().into()),
        }
    }
    Ok((provider, dir))
}

fn default_dir(provider: &str) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    Ok(
        PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set; specify --dir")?)
            .join(format!(".config/llmux/{provider}")),
    )
}

async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (provider, dir) = parse_args(std::env::args_os().skip(1))?;
    let provider = provider.as_deref().unwrap_or("codex");
    let dir = match dir {
        Some(dir) => dir,
        None => default_dir(provider)?,
    };
    if provider == "nous" {
        let state_path = std::path::absolute(dir.join("state.json"))?;
        llmux_login::nous::NousLogin::default()
            .run(&state_path, |url, code| {
                println!("Open this URL to approve the sign-in:\n{url}\nIf prompted, enter code: {code}\nWaiting for approval...");
            })
            .await?;
        println!(
            "Sign-in complete. State written to {} (mode 0600). Paste this into llmux.toml:\n{}",
            state_path.display(),
            llmux_login::nous::snippet(&state_path)
        );
        return Ok(());
    }
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
                    eprintln!(
                        "Authorization code expired or was already used; restarting sign-in."
                    );
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn args(values: &[&str]) -> impl Iterator<Item = OsString> {
        values
            .iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn provider_selection_defaults_to_codex() {
        assert_eq!(parse_args(args(&[])).unwrap(), (None, None));
        assert_eq!(
            parse_args(args(&["xai"])).unwrap(),
            (Some("xai".into()), None)
        );
        assert_eq!(
            parse_args(args(&["codex", "--dir", "/tmp/d"])).unwrap(),
            (Some("codex".into()), Some(PathBuf::from("/tmp/d")))
        );
        assert_eq!(
            parse_args(args(&["nous", "--dir", "/tmp/n"])).unwrap(),
            (Some("nous".into()), Some(PathBuf::from("/tmp/n")))
        );
        assert!(default_dir("nous").unwrap().ends_with(".config/llmux/nous"));
        assert!(parse_args(args(&["gitlab"])).is_err());
        assert!(parse_args(args(&["xai", "codex"])).is_err());
        assert!(parse_args(args(&["--dir"])).is_err());
    }

    #[test]
    fn provider_endpoints_and_dirs() {
        let xai = endpoints("xai").unwrap();
        assert_eq!(xai.issuer.as_str(), "https://auth.x.ai/");
        assert_eq!(xai.authorize.as_str(), "https://auth.x.ai/oauth2/authorize");
        let codex = endpoints("codex").unwrap();
        assert!(codex
            .authorize
            .as_str()
            .starts_with("https://auth.openai.com/"));
        assert!(default_dir("xai").unwrap().ends_with(".config/llmux/xai"));
        assert!(default_dir("codex")
            .unwrap()
            .ends_with(".config/llmux/codex"));
        assert!(Flow::codex(codex).dynamic());
        assert!(!Flow::xai(xai).dynamic());
    }
}
