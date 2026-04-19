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

    // Initialize telemetry subsystem before running the orchestrator
    let _telemetry = init_telemetry();

    orchestrator.run(args).await
}

/// Initialize the telemetry subsystem.
///
/// Creates the SQLite store, ingests any pending crash reports from
/// previous panics, and starts the background worker thread.
fn init_telemetry() -> Option<coderex_telemetry::TelemetryHandle> {
    let config_home = coderex_config::config_home();
    let db_path = config_home.join("telemetry.db");
    let pending_dir = config_home.join("crashes").join("pending");

    let store = match coderex_telemetry::TelemetryStore::open(&db_path) {
        Ok(store) => store,
        Err(e) => {
            tracing::warn!("failed to open telemetry store: {e}");
            return None;
        }
    };

    // Ingest crash reports from previous panics
    let crash_reporter = coderex_telemetry::CrashReporter::new(pending_dir);
    match crash_reporter.ingest_pending(&store) {
        Ok(count) => {
            if count > 0 {
                tracing::info!("ingested {count} pending crash reports");
            }
        }
        Err(e) => tracing::warn!("failed to ingest pending crashes: {e}"),
    }

    let (handle, _uploader) = coderex_telemetry::TelemetryHandle::new(store);
    Some(handle)
}
