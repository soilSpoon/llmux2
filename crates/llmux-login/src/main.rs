use llmux_login::{Endpoints, PendingLogin};
use std::path::PathBuf;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("llmux-login: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut args = std::env::args_os().skip(1);
    let mut dir = None;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--dir") if dir.is_none() => {
                dir = Some(PathBuf::from(args.next().ok_or("--dir requires a path")?))
            }
            Some("--help" | "-h") => {
                println!("usage: llmux-login [--dir DIR]\nContinue with ChatGPT. Default: ~/.config/llmux/codex/\nUse separate directories for different account/workspace registrations.");
                return Ok(());
            }
            _ => return Err("usage: llmux-login [--dir DIR]".into()),
        }
    }
    let dir = match dir {
        Some(dir) => dir,
        None => PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set; specify --dir")?)
            .join(".config/llmux/codex"),
    };
    let mut pending = PendingLogin::start(&dir, Endpoints::default()).await?;
    let result = loop {
        println!("Continue with ChatGPT:\n{}\nThis private sign-in URL may contain account hints; do not share it.", pending.authorization_url());
        if webbrowser::open(pending.authorization_url().as_str()).is_err() {
            eprintln!("Could not open the browser. Open the URL above manually.");
        }
        match pending.finish().await {
            Ok(result) => break result,
            Err(error) => {
                if let Some(client) = error.retry_client_id() {
                    eprintln!("Authorization code expired or was already used; restarting sign-in with the issued client ID.");
                    pending = PendingLogin::retry(&dir, Endpoints::default(), client).await?;
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
