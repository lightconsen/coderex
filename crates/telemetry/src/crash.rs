//! Crash reporting — panic hook integration.
//!
//! Crash reports are written to a pending directory during a panic (since
//! `rusqlite::Connection` is not `Sync` and cannot be used inside a panic
//! hook). On the next startup the telemetry system scans the pending
//! directory and ingests the files into the SQLite store.

use std::path::PathBuf;

/// A captured crash report.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CrashReport {
    /// Panic info message.
    pub panic_info: String,
    /// Backtrace at time of panic.
    pub backtrace: Option<String>,
    /// When the crash occurred.
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// Crash reporter that hooks into the panic handler.
#[derive(Debug, Clone)]
pub struct CrashReporter {
    pending_dir: PathBuf,
}

impl CrashReporter {
    /// Create a new crash reporter.
    pub fn new(pending_dir: PathBuf) -> Self {
        Self { pending_dir }
    }

    /// Install a panic hook that records crashes to the pending directory.
    ///
    /// The original panic hook is preserved and called after recording.
    pub fn install_hook(&self) {
        let pending_dir = self.pending_dir.clone();
        let default_hook = std::panic::take_hook();

        std::panic::set_hook(Box::new(move |info| {
            let report = CrashReport {
                panic_info: format!("{info}"),
                backtrace: Some(format!(
                    "{:?}",
                    std::backtrace::Backtrace::force_capture()
                )),
                timestamp: chrono::Utc::now(),
            };

            let _ = std::fs::create_dir_all(&pending_dir);
            let filename = format!("crash_{}.json", chrono::Utc::now().timestamp_millis());
            let path = pending_dir.join(filename);
            if let Ok(json) = serde_json::to_string(&report) {
                let _ = std::fs::write(&path, json);
            }

            default_hook(info);
        }));
    }

    /// Scan the pending directory and ingest crash files.
    ///
    /// Returns the number of crashes ingested.
    pub fn ingest_pending(&self,
        store: &crate::TelemetryStore,
    ) -> Result<usize, crate::TelemetryError> {
        let mut count = 0;
        let entries = match std::fs::read_dir(&self.pending_dir) {
            Ok(entries) => entries,
            Err(_) => return Ok(0), // Directory may not exist yet
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }

            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    if let Ok(report) = serde_json::from_str::<CrashReport>(&content) {
                        let backtrace = report.backtrace.as_deref();
                        if store.insert_crash(&report.panic_info, backtrace).is_ok() {
                            let _ = std::fs::remove_file(&path);
                            count += 1;
                        }
                    }
                }
                Err(_) => continue,
            }
        }

        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_report_serialization() {
        let report = CrashReport {
            panic_info: "test panic".into(),
            backtrace: Some("backtrace...".into()),
            timestamp: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("test panic"));
    }
}
