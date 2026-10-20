//! Tracing initialization and panic hook installation.

use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

/// Initialize the tracing subscriber.
///
/// Controls:
/// - `CODEREX_LOG` — log level filter (default: `off`)
/// - `CODEREX_LOG_FORMAT` — output format: `pretty` (default) or `json`
/// - `CODEREX_LOG_FILE` — optional file path for additional file output
pub fn init_tracing() {
    let filter = EnvFilter::try_from_env("CODEREX_LOG").unwrap_or_else(|_| EnvFilter::new("off"));

    let format_json = std::env::var("CODEREX_LOG_FORMAT").is_ok_and(|v| v == "json");

    if format_json {
        tracing_subscriber::registry()
            .with(filter)
            .with(
                fmt::layer()
                    .json()
                    .with_target(false)
                    .with_writer(std::io::stderr),
            )
            .init();
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt::layer().with_target(false).with_writer(std::io::stderr))
            .init();
    }
}

/// Install a panic hook using the telemetry crash reporter.
///
/// Crash reports are written to `~/.coderex/crashes/pending/` and ingested
/// into the telemetry store on the next startup.
pub fn install_panic_hook() {
    let pending_dir = coderex_config::config_home()
        .join("crashes")
        .join("pending");
    let reporter = coderex_telemetry::CrashReporter::new(pending_dir);
    reporter.install_hook();

    // Release builds use `panic = "abort"`, so no destructor runs after a
    // panic. Wrap the telemetry hook to kill plugin monitors first; the
    // telemetry hook still runs afterwards.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        crate::plugin_monitors::terminate_all_monitors();
        previous(info);
    }));
}
