mod coverage_ratchet;
mod pr_policy;

use std::{
    env,
    process::{Command, ExitCode},
};

fn git(args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    String::from_utf8(output.stdout).map_err(|e| e.to_string())
}

fn print_errors(errors: &[String]) {
    for error in errors {
        println!("::error::{error}");
    }
}

fn main() -> ExitCode {
    let args: Vec<_> = env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "bench") {
        return match Command::new("cargo")
            .args(["run", "--release", "--locked", "-p", "llmux-bench", "--"])
            .args(&args[1..])
            .status()
        {
            Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
            Err(error) => {
                eprintln!("{error}");
                ExitCode::FAILURE
            }
        };
    }
    let result = match args.as_slice() {
        [cmd] if cmd == "pr-policy" => pr_policy::run(),
        [cmd, path] if cmd == "coverage-ratchet" => coverage_ratchet::run(path),
        _ => Err("usage: cargo xtask pr-policy | coverage-ratchet <coverage.json>".into()),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
