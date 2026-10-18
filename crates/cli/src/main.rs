//! CodeRex CLI — startup orchestrator and entry point.

#![warn(missing_docs)]

use anyhow::Result;
use coderex_cli::{orchestrator, parse_args};

#[tokio::main]
async fn main() -> Result<()> {
    // The host injects itself as the WASM compiler. Handle the hidden
    // subcommand before tracing setup and clap parsing so it can never be
    // mistaken for a user-facing flag.
    if let Some(code) = coderex_cli::cli::maybe_run_internal_wasm_compile() {
        std::process::exit(code);
    }

    coderex_cli::tracing_setup::init_tracing();
    coderex_cli::tracing_setup::install_panic_hook();

    // From here on, WASM plugins compile out-of-process by re-executing this
    // binary with `--internal-wasm-compile`. No sibling helper is required.
    coderex_cli::cli::install_wasm_compiler()?;

    let args = parse_args();
    let mut orchestrator = orchestrator::StartupOrchestrator::new();

    orchestrator.run(args).await
}
