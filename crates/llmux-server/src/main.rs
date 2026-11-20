use llmux_config::RuntimeConfig;
use std::{error::Error, path::PathBuf};

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("llmux-server: {err}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut args = std::env::args_os().skip(1);
    let first = args.next();
    let check = first.as_deref() == Some(std::ffi::OsStr::new("--check"));
    let path = (if check { args.next() } else { first })
        .map_or_else(|| PathBuf::from("llmux.toml"), PathBuf::from);
    if args.next().is_some() {
        return Err("usage: llmux-server [--check] [path/to/llmux.toml]".into());
    }

    let config = RuntimeConfig::load(path)?;
    if check {
        println!("{}", config.server.listen);
        return Ok(());
    }
    let address = config.server.listen;
    let shutdown_timeout = config.server.shutdown_timeout;
    let drain_grace = config.server.drain_grace;
    let app = llmux_server::app(config).await?;
    let listener = llmux_server::bind(address).await?;
    eprintln!("llmux-server listening on {}", listener.local_addr()?);
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let shutdown = async move {
        #[cfg(unix)]
        tokio::select! {
            _ = terminate.recv() => {},
            _ = tokio::signal::ctrl_c() => {},
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
    };
    llmux_server::serve(listener, app, shutdown, shutdown_timeout, drain_grace).await?;
    Ok(())
}
