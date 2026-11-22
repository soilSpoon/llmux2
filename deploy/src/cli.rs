use crate::{
    alert::{self, Thresholds},
    Deployment, Host, Result,
};
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
/// Which ops binary invoked main_entry.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tool {
    Deploy,
    Guard,
    Alert,
}
#[derive(Debug, PartialEq)]
enum Action {
    Guard,
    Alert {
        config: Option<PathBuf>,
    },
    Status,
    Deploy {
        binary: Option<PathBuf>,
        config: Option<PathBuf>,
        canary: f64,
    },
    Help(String),
}
fn usage(tool: Tool) -> &'static str {
    match tool {
        Tool::Guard => "usage: llmux2-guard [-h]",
        Tool::Deploy => "usage: llmux2-deploy [-h] {deploy,status} ...",
        Tool::Alert => "usage: llmux2-alert [-h] [--config CONFIG]",
    }
}
fn tool_name(tool: Tool) -> &'static str {
    match tool {
        Tool::Guard => "llmux2-guard",
        Tool::Deploy => "llmux2-deploy",
        Tool::Alert => "llmux2-alert",
    }
}
fn display_args(args: &[OsString]) -> String {
    args.iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}
fn parse(args: &[OsString], tool: Tool) -> std::result::Result<Action, String> {
    if args.first().is_some_and(|a| a == "-h" || a == "--help") {
        let about = match tool {
            Tool::Alert => "Threshold alerting for the running llmux2 gateway.\n",
            _ => "Slot deployment and recovery for llmux2.\n",
        };
        return Ok(Action::Help(format!("{}\n\n{about}", usage(tool))));
    }
    if tool == Tool::Guard {
        return if args.is_empty() {
            Ok(Action::Guard)
        } else {
            Err(format!("unrecognized arguments: {}", display_args(args)))
        };
    }
    if tool == Tool::Alert {
        let mut config = None;
        let mut index = 0;
        while index < args.len() {
            if args[index] == "-h" || args[index] == "--help" {
                return Ok(Action::Help(
                    "usage: llmux2-alert [-h] [--config CONFIG]\n".into(),
                ));
            }
            let bytes = args[index].as_bytes();
            let (flag, inline) = flag_value(bytes, &args[index]);
            if flag != OsStr::new("--config") {
                return Err(format!(
                    "unrecognized arguments: {}",
                    display_args(&args[index..])
                ));
            }
            let value = if let Some(value) = inline {
                value
            } else {
                index += 1;
                args.get(index)
                    .filter(|s| !s.as_bytes().starts_with(b"-"))
                    .map(OsString::as_os_str)
                    .ok_or_else(|| "argument --config: expected one argument".to_string())?
            };
            config = Some(PathBuf::from(value));
            index += 1;
        }
        return Ok(Action::Alert { config });
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
        let (flag, inline) = flag_value(bytes, &args[index]);
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
fn flag_value<'a>(bytes: &'a [u8], arg: &'a OsString) -> (&'a OsStr, Option<&'a OsStr>) {
    bytes
        .iter()
        .position(|b| *b == b'=')
        .map_or((arg.as_os_str(), None), |index| {
            (
                OsStr::from_bytes(&bytes[..index]),
                Some(OsStr::from_bytes(&bytes[index + 1..])),
            )
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
    // Alerting is read-only stats work and must not wait on a deploy lock.
    if let Action::Alert { config } = action {
        return alert(&root, config);
    }
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
        Action::Alert { .. } | Action::Help(_) => unreachable!(),
    }
    drop(lock);
    Ok(())
}
/// One alert pass; exits 2 on a bad config file, 1 on any breach.
fn alert(root: &std::path::Path, config: Option<PathBuf>) -> Result<()> {
    let path = config.unwrap_or_else(|| root.join("alerts.toml"));
    let thresholds = match Thresholds::load(&path) {
        Ok(thresholds) => thresholds,
        Err(error) => {
            eprintln!("llmux2-alert: invalid config {}: {error}", path.display());
            std::process::exit(2);
        }
    };
    let record = Deployment::new(root.to_owned(), Host::new(root.to_owned())).record()?;
    let mut monitor = Host::new(root.to_owned());
    let alerts = alert::check(root, &thresholds, record.as_ref(), &mut monitor)?;
    if alerts.is_empty() {
        println!("ok");
    } else {
        for line in alerts {
            eprintln!("ALERT {line}");
        }
        std::process::exit(1);
    }
    Ok(())
}
pub fn main_entry(tool: Tool) -> i32 {
    unsafe {
        libc::umask(0o077);
    }
    let args: Vec<_> = env::args_os().skip(1).collect();
    let action = match parse(&args, tool) {
        Ok(Action::Help(text)) => {
            print!("{text}");
            return 0;
        }
        Ok(action) => action,
        Err(error) => {
            eprintln!("{}\n{}: error: {error}", usage(tool), tool_name(tool));
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
