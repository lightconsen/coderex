//! CodeRex CLI — startup orchestrator and entry point.

#![warn(missing_docs)]

use anyhow::Result;
use coderex_cli::{parse_args, orchestrator};

#[tokio::main]
async fn main() -> Result<()> {
    coderex_cli::tracing_setup::init_tracing();
    coderex_cli::tracing_setup::install_panic_hook();

    let args = parse_args();
    let mut orchestrator = orchestrator::StartupOrchestrator::new();

    orchestrator.run(args).await
}
