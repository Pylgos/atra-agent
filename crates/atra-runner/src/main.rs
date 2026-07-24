use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    stdio: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .without_time()
        .compact()
        .init();

    let args = Args::parse();
    if !args.stdio {
        eprintln!("atra-runner: --stdio is required");
        std::process::exit(2);
    }

    if let Err(error) = atra_runner::run_stdio().await {
        tracing::error!(error = %format!("{error:#}"), "runner failed");
        eprintln!("atra-runner: {error:#}");
        std::process::exit(1);
    }
}
