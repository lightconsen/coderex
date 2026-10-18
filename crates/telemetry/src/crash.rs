//! Crash reporting — panic hook integration.
//!
//! Crash reports are written to a pending directory during a panic (since
//! `rusqlite::Connection` is not `Sync` and cannot be used inside a panic
//! hook). On the next startup the telemetry system scans the pending
//! directory and ingests the files into the SQLite store.

use std::path::PathBuf;

/// Prefix that identifies a strict-mode structured crash summary.
///
/// A summary carrying this prefix contains only panic metadata (type, thread,
/// location) — never the free-form panic payload or backtrace.
pub(crate) const CRASH_SUMMARY_PREFIX: &str = "[strict-crash-summary]";

/// Payload value written under `redaction_status` for a structured crash.
pub(crate) const CRASH_STRUCTURED_MARKER: &str = "structured";

/// A captured crash report.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CrashReport {
    /// Panic info message.
    ///
    /// In non-strict mode this is the free-form `format!("{info}")` text, which
    /// may contain whatever the panicking code carried. In strict mode it is a
    /// [`CRASH_SUMMARY_PREFIX`] summary instead.
    pub panic_info: String,
    /// Backtrace at time of panic.
    ///
    /// Always `None` in strict mode.
    pub backtrace: Option<String>,
    /// When the crash occurred.
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Panic source location (`file:line:column`), when the runtime knows it.
    ///
    /// Defaulted on deserialization so pending files written by older builds
    /// still load.
    #[serde(default)]
    pub panic_location: Option<String>,
    /// Name of the panicking thread, when it has one.
    #[serde(default)]
    pub thread_name: Option<String>,
    /// Panic payload type (`&str`, `String`, or `unknown`).
    #[serde(default)]
    pub panic_type: Option<String>,
}

/// Fold a value to one line and cap its length.
///
/// The structured fields are metadata, not free-form text; this keeps a
/// malformed pending file from smuggling newlines or an unbounded blob into a
/// summary.
fn one_line(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(max_chars)
        .collect()
}

/// Extract `file:line:column` from the standard panic display prefix
/// (`panicked at <location>:\n<message>`), without keeping the message.
fn panic_location_from_text(text: &str) -> Option<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"^panicked at ([^\n]+?:\d+:\d+):")
            .expect("panic location regex is valid")
    });
    re.captures(text)
        .and_then(|caps| caps.get(1))
        .map(|location| one_line(location.as_str(), 200))
}

/// Build the strict-mode structured summary for a captured report.
///
/// Only panic type, thread name and location are retained. The panic payload
/// (which may contain conversation text) and the backtrace (which may contain
/// paths and source lines) are deliberately replaced by the literal
/// `omitted`, so a strict report can never be mistaken for a scrubbed one.
pub(crate) fn structured_summary(report: &CrashReport) -> String {
    let location = report
        .panic_location
        .as_deref()
        .map(|value| one_line(value, 200))
        .or_else(|| panic_location_from_text(&report.panic_info))
        .unwrap_or_else(|| "unknown".to_string());
    let thread = report
        .thread_name
        .as_deref()
        .map(|value| one_line(value, 100))
        .unwrap_or_else(|| "unknown".to_string());
    let panic_type = report
        .panic_type
        .as_deref()
        .map(|value| one_line(value, 50))
        .unwrap_or_else(|| "unknown".to_string());

    format!(
        "{CRASH_SUMMARY_PREFIX} type={panic_type}; thread={thread}; location={location}; message=omitted; backtrace=omitted"
    )
}

/// Produce a safe structured summary from whatever is stored in the crash row.
///
/// Ingest under strict mode already stores a summary; pass it through. A row
/// ingested before strict mode was enabled holds free-form text for which the
/// report metadata is gone, so only the location is recovered and the rest is
/// marked unavailable. The free-form text itself is never included.
pub(crate) fn structured_summary_from_stored(stored: &str) -> String {
    if stored.starts_with(CRASH_SUMMARY_PREFIX) {
        return stored.to_string();
    }
    let location = panic_location_from_text(stored).unwrap_or_else(|| "unknown".to_string());
    format!(
        "{CRASH_SUMMARY_PREFIX} type=unknown; thread=unknown; location={location}; message=omitted; backtrace=omitted"
    )
}

/// Default maximum size for the pending crash directory (50 MB).
const DEFAULT_MAX_PENDING_BYTES: u64 = 50 * 1024 * 1024;

/// Crash reporter that hooks into the panic handler.
#[derive(Debug, Clone)]
pub struct CrashReporter {
    pending_dir: PathBuf,
    max_pending_bytes: u64,
    /// Strict (confidentiality) mode, resolved once at construction from
    /// [`crate::hipaa_enabled`] (`CODEREX_HIPAA`).
    strict: bool,
}

impl CrashReporter {
    /// Create a new crash reporter.
    ///
    /// Strict mode follows the process-wide [`crate::hipaa_enabled`] trigger.
    pub fn new(pending_dir: PathBuf) -> Self {
        Self {
            pending_dir,
            max_pending_bytes: DEFAULT_MAX_PENDING_BYTES,
            strict: crate::hipaa_enabled(),
        }
    }

    /// Set the maximum allowed size of the pending crash directory.
    pub fn with_max_pending_bytes(mut self, bytes: u64) -> Self {
        self.max_pending_bytes = bytes;
        self
    }

    /// Override strict mode explicitly.
    ///
    /// Production callers use [`Self::new`], which resolves `CODEREX_HIPAA`.
    /// Tests use this so they do not have to mutate process-wide environment
    /// variables.
    pub fn with_strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    /// Install a panic hook that records crashes to the pending directory.
    ///
    /// The original panic hook is preserved and called after recording.
    ///
    /// In strict mode the free-form panic payload and backtrace are never
    /// written to the pending file: only panic type, thread and location are
    /// recorded. This is the one place where the raw payload exists, so it is
    /// also the earliest place to drop it.
    pub fn install_hook(&self) {
        let pending_dir = self.pending_dir.clone();
        let strict = self.strict;
        let default_hook = std::panic::take_hook();

        std::panic::set_hook(Box::new(move |info| {
            let panic_type = if info.payload().is::<&str>() {
                "&str"
            } else if info.payload().is::<String>() {
                "String"
            } else {
                "unknown"
            };
            let panic_location = info.location().map(|location| {
                format!(
                    "{}:{}:{}",
                    location.file(),
                    location.line(),
                    location.column()
                )
            });
            let thread_name = std::thread::current().name().map(str::to_string);

            let mut report = CrashReport {
                panic_info: format!("{info}"),
                backtrace: if strict {
                    None
                } else {
                    Some(format!("{:?}", std::backtrace::Backtrace::force_capture()))
                },
                timestamp: chrono::Utc::now(),
                panic_location,
                thread_name,
                panic_type: Some(panic_type.to_string()),
            };
            if strict {
                report.panic_info = structured_summary(&report);
            }

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

        // The same scrubber the event path uses, applied here too: a panic
        // message is `format!("{info}")` of whatever the panic carried, and a
        // backtrace is a list of paths. Both leave the machine — the store is
        // uploaded from — so both are scrubbed before they are stored.
        //
        // Scrubbing removes patterns, not meaning: ordinary conversation text
        // inside a panic message survives it. Strict mode therefore does not
        // rely on the scrubber for crash text at all — it stores a structured
        // summary and no backtrace.
        let scrubber = crate::Scrubber::new();

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
                        let (panic_info, backtrace) = if self.strict {
                            // Never store free-form panic text in strict mode.
                            // The payload is structured and explicitly marked;
                            // the message and backtrace are omitted. The
                            // metadata-only summary still goes through the
                            // scrubber as defence in depth.
                            (scrubber.scrub(&structured_summary(&report)), None)
                        } else {
                            (
                                scrubber.scrub(&report.panic_info),
                                report
                                    .backtrace
                                    .as_deref()
                                    .map(|backtrace| scrubber.scrub(backtrace)),
                            )
                        };
                        if store
                            .insert_crash(&panic_info, backtrace.as_deref())
                            .is_ok()
                        {
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

    fn crash_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("coderex_crash_{}_{}", std::process::id(), name))
    }

    fn ingest_one(name: &str, report: &CrashReport, strict: bool) -> (String, Option<String>) {
        let dir = crash_dir(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("crash_1.json"),
            serde_json::to_string(report).unwrap(),
        )
        .unwrap();

        let reporter = CrashReporter::new(dir.clone()).with_strict(strict);
        let store = crate::TelemetryStore::open(&dir.join("telemetry.db")).unwrap();
        assert_eq!(reporter.ingest_pending(&store).unwrap(), 1);

        let crashes = store.pending_crashes(10).unwrap();
        assert_eq!(crashes.len(), 1);
        let result = (crashes[0].panic_info.clone(), crashes[0].backtrace.clone());
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    fn report_with(panic_info: &str, backtrace: Option<&str>) -> CrashReport {
        CrashReport {
            panic_info: panic_info.into(),
            backtrace: backtrace.map(str::to_string),
            timestamp: chrono::Utc::now(),
            panic_location: None,
            thread_name: None,
            panic_type: None,
        }
    }

    /// A crash message and its backtrace leave the machine, so they go through
    /// the same scrubber the event path uses. They used to be stored — and
    /// uploaded — exactly as the panic produced them.
    #[test]
    fn an_ingested_crash_is_scrubbed() {
        let report = report_with(
            "failed for user@example.com from 10.0.0.7 at /Users/somebody/project",
            Some("at /home/somebody/project/src/main.rs:12"),
        );
        let (panic_info, backtrace) = ingest_one("scrubbed", &report, false);
        let stored = format!("{}\n{}", panic_info, backtrace.unwrap_or_default());
        assert!(!stored.contains("user@example.com"), "{stored}");
        assert!(!stored.contains("10.0.0.7"), "{stored}");
        assert!(!stored.contains("somebody"), "{stored}");
    }

    /// Non-strict mode keeps diagnostics but runs the high-entropy detector:
    /// the secret goes, the prose stays. That residual prose is the honest
    /// limitation — patterns, not meaning.
    #[test]
    fn non_strict_mode_redacts_secrets_but_keeps_prose() {
        let report = report_with(
            "panicked at src/main.rs:10:5:\nuser said: here is the key A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6 and mail bob@example.com",
            None,
        );
        let (panic_info, _) = ingest_one("non_strict", &report, false);
        assert!(
            !panic_info.contains("A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6"),
            "{panic_info}"
        );
        assert!(!panic_info.contains("bob@example.com"), "{panic_info}");
        assert!(panic_info.contains("<SECRET>"), "{panic_info}");
        assert!(panic_info.contains("<EMAIL>"), "{panic_info}");
        // The detector has no semantic understanding: ordinary conversation
        // text is not removed outside strict mode.
        assert!(panic_info.contains("user said"), "{panic_info}");
    }

    /// An ordinary backtrace is diagnostics, not a secret: paths, frames and
    /// Rust symbols survive non-strict scrubbing (only the home user is
    /// redacted, as before).
    #[test]
    fn non_strict_backtrace_keeps_paths_and_symbols() {
        let backtrace = "std::panicking::begin_panic_handler\n    at /rustc/abcdef0123456789abcdef0123456789/library/std/src/panicking.rs:123:5\ncoderex_core::engine::QueryEngine::submit\n    at /home/somebody/project/crates/core/src/engine.rs:42:9\nat file.rs:12:9";
        let report = report_with("panicked at src/main.rs:1:1:\nboom", Some(backtrace));
        let (_, stored) = ingest_one("backtrace", &report, false);
        let stored = stored.expect("non-strict mode keeps the backtrace");
        for expected in [
            "std::panicking::begin_panic_handler",
            "/rustc/abcdef0123456789abcdef0123456789/library/std/src/panicking.rs:123:5",
            "coderex_core::engine::QueryEngine::submit",
            "crates/core/src/engine.rs:42:9",
            "file.rs:12:9",
        ] {
            assert!(
                stored.contains(expected),
                "{expected} missing from {stored}"
            );
        }
        assert!(!stored.contains("/home/somebody"), "{stored}");
    }

    /// Strict mode never stores free-form panic text or a backtrace. The panic
    /// message here contains a fake conversation, an email and a token; none of
    /// them reach the store, and the stored text is an explicitly marked
    /// structured summary.
    #[test]
    fn strict_mode_stores_only_a_marked_structured_summary() {
        let report = CrashReport {
            panic_info: "panicked at src/main.rs:10:5:\nuser said: my key is A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6 and mail bob@example.com".into(),
            backtrace: Some("at /home/somebody/project/src/main.rs:10:5".into()),
            timestamp: chrono::Utc::now(),
            panic_location: Some("src/main.rs:10:5".into()),
            thread_name: Some("main".into()),
            panic_type: Some("&str".into()),
        };
        let (panic_info, backtrace) = ingest_one("strict", &report, true);

        assert!(backtrace.is_none(), "strict mode must omit the backtrace");
        assert!(panic_info.starts_with(CRASH_SUMMARY_PREFIX), "{panic_info}");
        assert!(panic_info.contains("type=&str"), "{panic_info}");
        assert!(panic_info.contains("thread=main"), "{panic_info}");
        assert!(
            panic_info.contains("location=src/main.rs:10:5"),
            "{panic_info}"
        );
        assert!(panic_info.contains("message=omitted"), "{panic_info}");
        assert!(panic_info.contains("backtrace=omitted"), "{panic_info}");
        for forbidden in [
            "user said",
            "bob@example.com",
            "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6",
            "/home/somebody",
        ] {
            assert!(!panic_info.contains(forbidden), "{forbidden}: {panic_info}");
        }
    }

    /// A legacy pending file written before strict mode was enabled has no
    /// metadata fields and unscrubbed text. Strict ingestion recovers only the
    /// location and drops everything else.
    #[test]
    fn strict_mode_handles_legacy_free_form_pending_file() {
        let legacy = serde_json::json!({
            "panic_info": "panicked at src/old.rs:7:2:\nuser said something private",
            "backtrace": "at /home/somebody/secret.rs:7:2",
            "timestamp": "2026-01-01T00:00:00Z"
        });
        let dir = crash_dir("legacy");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("crash_1.json"), legacy.to_string()).unwrap();

        let reporter = CrashReporter::new(dir.clone()).with_strict(true);
        let store = crate::TelemetryStore::open(&dir.join("telemetry.db")).unwrap();
        assert_eq!(reporter.ingest_pending(&store).unwrap(), 1);
        let crashes = store.pending_crashes(10).unwrap();
        let stored = &crashes[0].panic_info;
        assert!(stored.starts_with(CRASH_SUMMARY_PREFIX), "{stored}");
        assert!(stored.contains("location=src/old.rs:7:2"), "{stored}");
        assert!(!stored.contains("user said"), "{stored}");
        assert!(crashes[0].backtrace.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn crash_report_serialization() {
        let report = report_with("test panic", Some("backtrace..."));
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("test panic"));
    }
}
