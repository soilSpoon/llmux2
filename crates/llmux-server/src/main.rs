use llmux_config::RuntimeConfig;
use std::{error::Error, path::PathBuf};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("llmux-server: {err}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut args = std::env::args_os().skip(1);
    let path = args
        .next()
        .map_or_else(|| PathBuf::from("llmux.toml"), PathBuf::from);
    if args.next().is_some() {
        return Err("usage: llmux-server [path/to/llmux.toml]".into());
    }

    let config = RuntimeConfig::load(path)?;
    let address = config.server.listen;
    let app = llmux_server::app(config)?;
    let listener = TcpListener::bind(address).await?;
    eprintln!("llmux-server listening on {}", listener.local_addr()?);
    axum::serve(listener, app).await?;
    Ok(())
}
