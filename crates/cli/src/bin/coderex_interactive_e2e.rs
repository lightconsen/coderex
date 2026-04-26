use anyhow::Result;
use clap::Parser;

/// Run PTY-driven interactive E2E scenarios against `coderex`.
#[derive(Debug, Parser)]
struct Args {
    /// Path to a JSON scenario file.
    #[arg(long)]
    scenario: std::path::PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    coderex_cli::interactive_e2e::run_scenario_file(&args.scenario)
}
