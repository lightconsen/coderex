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

    // A Windows update leaves the previous binary behind as `coderex.exe.old`;
    // it is removed on the next launch, when nothing has it mapped any more.
    if let Ok(exe) = std::env::current_exe() {
        coderex_cli::self_update::cleanup_after_update(&exe);
    }

    let args = parse_args();
    let mut orchestrator = orchestrator::StartupOrchestrator::new();

    orchestrator.run(args).await
}
