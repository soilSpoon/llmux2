use crate::Result;
use llmux_config::RuntimeConfig;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

pub fn binaries() -> Result<HashMap<String, PathBuf>> {
    let executable = std::env::current_exe()?;
    Ok([
        ("llmux-server".into(), executable.clone()),
        ("mock_load_upstream".into(), executable),
    ]
    .into())
}

pub fn command(binary: &Path, args: &[String]) -> Result<Command> {
    let mut command = Command::new(binary);
    command
        .args(["--exact", "test_support::process_fixture", "--nocapture"])
        .env("LLMUX_BENCH_TEST_SERVER_ARGS", serde_json::to_string(args)?);
    Ok(command)
}

#[test]
fn process_fixture() {
    let Ok(args) = std::env::var("LLMUX_BENCH_TEST_SERVER_ARGS") else {
        return;
    };
    let args: Vec<String> = serde_json::from_str(&args).unwrap();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let app = if args.first().is_some_and(|s| s == "--delay-ms") {
                llmux_testkit::mock_load_upstream::router(
                    args.iter().any(|s| s == "--hold"),
                    Duration::from_millis(args[1].parse().unwrap()),
                )
            } else {
                let config = std::fs::read_to_string(&args[0]).unwrap();
                llmux_server::app(RuntimeConfig::parse(&config).unwrap())
                    .await
                    .unwrap()
            };
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            eprintln!(
                "{} listening on {}",
                std::env::current_exe()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_string_lossy(),
                listener.local_addr().unwrap()
            );
            axum::serve(listener, app).await.unwrap();
        });
}
