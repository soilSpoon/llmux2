use crate::{Deployment, Host, Result};
use std::{
    env,
    fs::{self, OpenOptions},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::PathBuf,
};
#[derive(Debug, PartialEq)]
enum Action {
    Guard,
    Status,
    Deploy {
        binary: Option<PathBuf>,
        config: Option<PathBuf>,
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
fn parse(args: &[String], guard: bool) -> std::result::Result<Action, String> {
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
            Err(format!("unrecognized arguments: {}", args.join(" ")))
        };
    }
    let Some(command) = args.first() else {
        return Err("the following arguments are required: command".into());
    };
    if !matches!(command.as_str(), "deploy" | "status") {
        return Err(format!(
            "argument command: invalid choice: '{command}' (choose from 'deploy', 'status')"
        ));
    }
    if args.get(1).is_some_and(|a| a == "-h" || a == "--help") {
        return Ok(Action::Help(format!(
            "usage: llmux2-deploy {command} {}\n",
            if command == "deploy" {
                "[-h] [--binary BINARY] [--config CONFIG]"
            } else {
                "[-h]"
            }
        )));
    }
    if command == "status" {
        return if args.len() == 1 {
            Ok(Action::Status)
        } else {
            Err(format!("unrecognized arguments: {}", args[1..].join(" ")))
        };
    }
    let mut binary = None;
    let mut config = None;
    let mut index = 1;
    while index < args.len() {
        if matches!(args[index].as_str(), "-h" | "--help") {
            return Ok(Action::Help(
                "usage: llmux2-deploy deploy [-h] [--binary BINARY] [--config CONFIG]\n".into(),
            ));
        }
        let (flag, inline) = args[index]
            .split_once('=')
            .map_or((args[index].as_str(), None), |(flag, value)| {
                (flag, Some(value))
            });
        let target = match flag {
            "--binary" => &mut binary,
            "--config" => &mut config,
            _ => {
                return Err(format!(
                    "unrecognized arguments: {}",
                    args[index..].join(" ")
                ))
            }
        };
        let value = if let Some(value) = inline {
            value
        } else {
            index += 1;
            args.get(index)
                .filter(|s| !s.starts_with('-'))
                .ok_or_else(|| format!("argument {flag}: expected one argument"))?
        };
        *target = Some(PathBuf::from(value));
        index += 1;
    }
    Ok(Action::Deploy { binary, config })
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
        Action::Deploy { binary, config } => {
            let slot = deployment.deploy(
                &binary.unwrap_or_else(|| root.join("bin/llmux-server")),
                &config.unwrap_or_else(|| root.join("llmux.toml")),
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
    let args: Vec<_> = env::args().skip(1).collect();
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
