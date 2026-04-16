//! Tracing initialization and panic hook installation.

use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Initialize the tracing subscriber.
///
/// Controls:
/// - `CODEREX_LOG` — log level filter (default: `info`)
/// - `CODEREX_LOG_FORMAT` — output format: `pretty` (default) or `json`
/// - `CODEREX_LOG_FILE` — optional file path for additional file output
pub fn init_tracing() {
    let filter = EnvFilter::try_from_env("CODEREX_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let format_json = std::env::var("CODEREX_LOG_FORMAT")
        .map(|v| v == "json")
        .unwrap_or(false);

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
            .with(
                fmt::layer()
                    .with_target(false)
                    .with_writer(std::io::stderr),
            )
            .init();
    }
}

/// Install a panic hook that writes crash information to `~/.claude/crash.log`.
pub fn install_panic_hook() {
    use std::backtrace::Backtrace;

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(home) = home::home_dir() {
            let crash_path = home.join(".claude").join("crash.log");
            let backtrace = Backtrace::force_capture();
            let log = format!(
                "panic: {info}\nbacktrace:\n{backtrace}\n",
            );
            if let Some(parent) = crash_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&crash_path, log);
        }

        default_hook(info);
    }));
}
