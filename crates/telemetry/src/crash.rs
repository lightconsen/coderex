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

/// Default maximum size for the pending crash directory (50 MB).
const DEFAULT_MAX_PENDING_BYTES: u64 = 50 * 1024 * 1024;

/// Crash reporter that hooks into the panic handler.
#[derive(Debug, Clone)]
pub struct CrashReporter {
    pending_dir: PathBuf,
    max_pending_bytes: u64,
}

impl CrashReporter {
    /// Create a new crash reporter.
    pub fn new(pending_dir: PathBuf) -> Self {
        Self {
            pending_dir,
            max_pending_bytes: DEFAULT_MAX_PENDING_BYTES,
        }
    }

    /// Set the maximum allowed size of the pending crash directory.
    pub fn with_max_pending_bytes(mut self, bytes: u64) -> Self {
        self.max_pending_bytes = bytes;
        self
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
                backtrace: Some(format!("{:?}", std::backtrace::Backtrace::force_capture())),
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
    /// Before ingestion, if the total size of the pending directory exceeds
    /// `max_pending_bytes`, oldest files are deleted until under the limit.
    ///
    /// Returns the number of crashes ingested.
    pub fn ingest_pending(
        &self,
        store: &crate::TelemetryStore,
    ) -> Result<usize, crate::TelemetryError> {
        self.enforce_size_limit();

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

    /// Delete oldest crash files until the pending directory is under the size limit.
    fn enforce_size_limit(&self) {
        let total_size = self.pending_dir_size();
        if total_size <= self.max_pending_bytes {
            return;
        }

        let mut files: Vec<(std::path::PathBuf, u64, std::time::SystemTime)> =
            match std::fs::read_dir(&self.pending_dir) {
                Ok(entries) => entries
                    .flatten()
                    .filter_map(|e| {
                        let path = e.path();
                        if path.extension().and_then(|s| s.to_str()) != Some("json") {
                            return None;
                        }
                        let meta = e.metadata().ok()?;
                        let size = meta.len();
                        let mtime = meta.modified().ok()?;
                        Some((path, size, mtime))
                    })
                    .collect(),
                Err(_) => return,
            };

        // Sort by mtime ascending (oldest first)
        files.sort_by(|a, b| a.2.cmp(&b.2));

        let mut to_free = total_size - self.max_pending_bytes;
        for (path, size, _) in files {
            if to_free == 0 {
                break;
            }
            let _ = std::fs::remove_file(&path);
            to_free = to_free.saturating_sub(size);
        }
    }

    /// Compute total size of all `.json` files in the pending directory.
    fn pending_dir_size(&self) -> u64 {
        match std::fs::read_dir(&self.pending_dir) {
            Ok(entries) => entries
                .flatten()
                .filter_map(|e| {
                    let path = e.path();
                    if path.extension().and_then(|s| s.to_str()) != Some("json") {
                        return None;
                    }
                    e.metadata().ok().map(|m| m.len())
                })
                .sum(),
            Err(_) => 0,
        }
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
