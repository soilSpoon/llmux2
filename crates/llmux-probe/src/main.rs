//! `llmux-probe` — one-shot `big-pickle` probe harness.
//!
//! Default is a dry run that builds and hashes the adapter request without
//! opening a connection. `--execute` sends exactly one request and never
//! retries. The evidence record is the only output.

use llmux_probe::{load_config, run_probe, Evidence, Mode};
use std::{path::PathBuf, process::ExitCode};

const USAGE: &str = "usage: llmux-probe --run-id ID --target TARGET_ROW.json [--execute]

Without --execute the harness is a dry run: it builds and hashes the request
through the merged opencode adapter but opens no connection.";

#[derive(Default)]
struct Args {
    run_id: Option<String>,
    target: Option<PathBuf>,
    execute: bool,
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut parsed = Args::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--run-id" => {
                parsed.run_id = Some(args.next().ok_or("--run-id needs a value")?);
            }
            "--target" => {
                parsed.target = Some(PathBuf::from(args.next().ok_or("--target needs a path")?));
            }
            "--execute" => parsed.execute = true,
            flag if flag.starts_with("--") => {
                return Err(format!(
                    "unknown flag {}\n{USAGE}",
                    flag.split('=').next().unwrap_or(flag)
                ));
            }
            other => return Err(format!("unexpected argument `{other}`\n{USAGE}")),
        }
    }
    Ok(parsed)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args = match parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(err) => return finish(Evidence::refused("", &err)),
    };
    let run_id = args.run_id.unwrap_or_default();
    let Some(target) = args.target else {
        return finish(Evidence::refused(
            &run_id,
            "--target is unset; a target row is required",
        ));
    };
    let config = match load_config(&run_id, &target) {
        Ok(config) => config,
        Err(err) => return finish(Evidence::refused(&run_id, &err)),
    };
    let mode = if args.execute {
        Mode::Execute
    } else {
        Mode::DryRun
    };
    finish(run_probe(&config, mode).await)
}

fn finish(evidence: Evidence) -> ExitCode {
    match serde_json::to_string_pretty(&evidence) {
        Ok(record) => println!("{record}"),
        Err(err) => {
            eprintln!("llmux-probe: could not serialize evidence: {err}");
            return ExitCode::FAILURE;
        }
    }
    if let Some(refusal) = &evidence.refusal {
        eprintln!("llmux-probe: {refusal}");
    }
    if evidence.exit_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
