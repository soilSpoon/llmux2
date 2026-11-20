mod coverage_ratchet;
mod merge_gate;
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
    let result = match args.as_slice() {
        [cmd] if cmd == "pr-policy" => pr_policy::run(),
        [cmd, path] if cmd == "coverage-ratchet" => coverage_ratchet::run(path),
        [cmd, number] if cmd == "merge-gate" => merge_gate::run(number),
        _ => Err(
            "usage: cargo xtask pr-policy | coverage-ratchet <coverage.json> | merge-gate <n>"
                .into(),
        ),
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
