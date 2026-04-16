//! CodeRex CLI — startup orchestrator and entry point.

#![warn(missing_docs)]

mod cli;
mod orchestrator;
mod tracing_setup;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_setup::init_tracing();
    tracing_setup::install_panic_hook();

    let args = cli::parse_args();
    let orchestrator = orchestrator::StartupOrchestrator::new();
    orchestrator.run(args).await
}
