mod alloc_counter;
mod compare_gateways;
mod copy_census;
mod http_client;
mod live_reliability;
mod mock_upstream;
mod process;
#[cfg(test)]
mod test_support;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn help(args: &[String]) -> Option<&'static str> {
    if !args.iter().any(|arg| arg == "--help" || arg == "-h") {
        return None;
    }
    Some(match args.first().map(String::as_str) {
        Some("mock-upstream") => "usage: llmux-bench mock-upstream [options]\n\n--requests N (4096)\n--concurrency N (64)\n--workers N (2)\n--fixed-rate-rounds N (9, at least 2)\n--upstream-delay-ms N (0)\n-h, --help",
        Some("copy-census") => "usage: llmux-bench copy-census\n\nRuns an in-process loopback allocation census; no options.\n-h, --help",
        Some("compare-gateways") => "usage: llmux-bench compare-gateways --gateway NAME URL KEY_REF --gateway NAME URL KEY_REF --output DIRECTORY [options]\n\nKEY_REF must be file:PATH or env:VARIABLE.\n--model NAME (repeatable) | --models-file PATH | --coverage PATH\n--top N (25)\n--responses N (0)\n--timeout SECONDS (45)\n--max-requests N (80)\n-h, --help",
        Some("live-reliability") => "usage: llmux-bench live-reliability [options]\n\n--binary PATH (target/release/llmux-server)\n--config PATH (required unless --offline)\n--primary HOST\n--fallback HOST\n--bad-key-hosts all|none|HOST,HOST (all)\n--offline\n--out DIRECTORY (live-reliability)\n-h, --help",
        _ => "usage: llmux-bench {mock-upstream|copy-census|compare-gateways|live-reliability} [options]",
    })
}

#[tokio::main]
async fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if let Some(text) = help(&args) {
        println!("{text}");
        return;
    }
    let result = match args.split_first() {
        Some((command, args)) if command == "mock-upstream" => mock_upstream::run(args).await,
        Some((command, args)) if command == "copy-census" => copy_census::run(args).await,
        Some((command, args)) if command == "compare-gateways" => compare_gateways::run(args).await,
        Some((command, args)) if command == "live-reliability" => live_reliability::run(args).await,
        _ => Err(
            "usage: llmux-bench {mock-upstream|copy-census|compare-gateways|live-reliability} [options]".into(),
        ),
    };
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn help_preserves_flags_and_does_not_start_a_benchmark() {
        assert!(help(&[]).is_none());
        assert!(help(&["--help".into()])
            .unwrap()
            .contains("live-reliability"));
        for (command, flag) in [
            ("mock-upstream", "--fixed-rate-rounds"),
            ("copy-census", "in-process loopback"),
            ("compare-gateways", "--max-requests"),
            ("live-reliability", "--bad-key-hosts"),
        ] {
            for option in ["--help", "-h"] {
                assert!(help(&[command.into(), option.into()])
                    .unwrap()
                    .contains(flag));
            }
        }
    }
}
