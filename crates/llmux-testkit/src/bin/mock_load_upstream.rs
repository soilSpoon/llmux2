use std::{io, time::Duration};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> io::Result<()> {
    let usage = || io::Error::other("usage: mock_load_upstream [--hold] [--delay-ms N]");
    let mut hold = false;
    let mut delay = Duration::ZERO;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--hold" => hold = true,
            "--delay-ms" => {
                let ms = args.next().and_then(|s| s.parse().ok()).ok_or_else(usage)?;
                delay = Duration::from_millis(ms);
            }
            _ => return Err(usage()),
        }
    }
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    eprintln!("mock_load_upstream listening on {}", listener.local_addr()?);
    axum::serve(
        listener,
        llmux_testkit::mock_load_upstream::router(hold, delay),
    )
    .await
}
