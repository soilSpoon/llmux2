mod compare_gateways;
mod mock_upstream;
mod process;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[tokio::main]
async fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let result = match args.split_first() {
        Some((command, args)) if command == "mock-upstream" => mock_upstream::run(args).await,
        Some((command, args)) if command == "compare-gateways" => compare_gateways::run(args).await,
        _ => Err("usage: llmux-bench mock-upstream [options]".into()),
    };
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
