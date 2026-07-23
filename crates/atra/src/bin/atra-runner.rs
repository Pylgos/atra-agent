use clap::Parser;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    stdio: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args = Args::parse();
    if !args.stdio {
        eprintln!("atra-runner: --stdio is required");
        std::process::exit(2);
    }

    if let Err(error) = atra_runner::run_stdio().await {
        eprintln!("atra-runner: {error:#}");
        std::process::exit(1);
    }
}
