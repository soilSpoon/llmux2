use crate::{Deployment, Host, Result};
use std::{
    env,
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, OpenOptionsExt},
    },
    path::PathBuf,
};
#[derive(Debug, PartialEq)]
enum Action {
    Guard,
    Status,
    Deploy {
        binary: Option<PathBuf>,
        config: Option<PathBuf>,
        canary: f64,
    },
    Help(String),
}
fn usage(guard: bool) -> &'static str {
    if guard {
        "usage: llmux2-guard [-h]"
    } else {
        "usage: llmux2-deploy [-h] {deploy,status} ..."
    }
}
fn display_args(args: &[OsString]) -> String {
    args.iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}
fn parse(args: &[OsString], guard: bool) -> std::result::Result<Action, String> {
    if args.first().is_some_and(|a| a == "-h" || a == "--help") {
        return Ok(Action::Help(format!(
            "{}\n\nSlot deployment and recovery for llmux2.\n",
            usage(guard)
        )));
    }
    if guard {
        return if args.is_empty() {
            Ok(Action::Guard)
        } else {
            Err(format!("unrecognized arguments: {}", display_args(args)))
        };
    }
    let Some(command) = args.first() else {
        return Err("the following arguments are required: command".into());
    };
    if command != "deploy" && command != "status" {
        return Err(format!(
            "argument command: invalid choice: '{}' (choose from 'deploy', 'status')",
            command.to_string_lossy()
        ));
    }
    if args.get(1).is_some_and(|a| a == "-h" || a == "--help") {
        return Ok(Action::Help(format!(
            "usage: llmux2-deploy {} {}\n",
            command.to_string_lossy(),
            if command == "deploy" {
                "[-h] [--binary BINARY] [--config CONFIG] [--canary SECONDS]"
            } else {
                "[-h]"
            }
        )));
    }
    if command == "status" {
        return if args.len() == 1 {
            Ok(Action::Status)
        } else {
            Err(format!(
                "unrecognized arguments: {}",
                display_args(&args[1..])
            ))
        };
    }
    let mut binary = None;
    let mut config = None;
    let mut canary = 0.0;
    let mut index = 1;
    while index < args.len() {
        if args[index] == "-h" || args[index] == "--help" {
            return Ok(Action::Help(
                "usage: llmux2-deploy deploy [-h] [--binary BINARY] [--config CONFIG] [--canary SECONDS]\n".into(),
            ));
        }
        let bytes = args[index].as_bytes();
        let (flag, inline) = bytes.iter().position(|b| *b == b'=').map_or(
            (args[index].as_os_str(), None),
            |index| {
                (
                    OsStr::from_bytes(&bytes[..index]),
                    Some(OsStr::from_bytes(&bytes[index + 1..])),
                )
            },
        );
        let target = match flag.to_str() {
            Some("--binary") => Some(&mut binary),
            Some("--config") => Some(&mut config),
            Some("--canary") => None,
            _ => {
                return Err(format!(
                    "unrecognized arguments: {}",
                    display_args(&args[index..])
                ))
            }
        };
        let value = if let Some(value) = inline {
            value
        } else {
            index += 1;
            args.get(index)
                .filter(|s| !s.as_bytes().starts_with(b"-") || flag == "--canary")
                .map(OsString::as_os_str)
                .ok_or_else(|| {
                    format!("argument {}: expected one argument", flag.to_string_lossy())
                })?
        };
        match target {
            Some(target) => *target = Some(PathBuf::from(value)),
            None => {
                canary = value
                    .to_str()
                    .and_then(|v| v.parse::<f64>().ok())
                    .filter(|v| v.is_finite() && *v >= 0.0)
                    .ok_or_else(|| {
                        format!(
                            "argument --canary: invalid value: '{}'",
                            value.to_string_lossy()
                        )
                    })?;
            }
        }
        index += 1;
    }
    Ok(Action::Deploy {
        binary,
        config,
        canary,
    })
}
#[allow(deprecated)]
fn home() -> Result<PathBuf> {
    env::home_dir().ok_or_else(|| "Could not determine home directory".into())
}
fn execute(action: Action) -> Result<()> {
    let root = home()?.join("llmux2");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&root)?;
    let lock = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(root.join("deploy.lock"))?;
    lock.lock()?;
    let mut deployment = Deployment::new(root.clone(), Host::new(root.clone()));
    match action {
        Action::Guard => deployment.guard()?,
        Action::Status => println!("{}", serde_json::to_string_pretty(&deployment.status()?)?),
        Action::Deploy {
            binary,
            config,
            canary,
        } => {
            let slot = deployment.deploy(
                &binary.unwrap_or_else(|| root.join("bin/llmux-server")),
                &config.unwrap_or_else(|| root.join("llmux.toml")),
                canary,
            )?;
            println!("active slot: {slot}");
        }
        Action::Help(_) => unreachable!(),
    }
    drop(lock);
    Ok(())
}
pub fn main_entry(guard: bool) -> i32 {
    unsafe {
        libc::umask(0o077);
    }
    let args: Vec<_> = env::args_os().skip(1).collect();
    let action = match parse(&args, guard) {
        Ok(Action::Help(text)) => {
            print!("{text}");
            return 0;
        }
        Ok(action) => action,
        Err(error) => {
            eprintln!(
                "{}\n{}: error: {error}",
                usage(guard),
                if guard {
                    "llmux2-guard"
                } else {
                    "llmux2-deploy"
                }
            );
            return 2;
        }
    };
    match execute(action) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("llmux2: {error}");
            1
        }
    }
}
