//! Local SQLite storage for telemetry events.

use crate::TelemetryError;
use rusqlite::{Connection, params};
use std::path::Path;

/// SQLite-backed store for telemetry events.
#[derive(Debug)]
pub struct TelemetryStore {
    conn: Connection,
    db_path: std::path::PathBuf,
}

impl TelemetryStore {
    /// Return the filesystem path of this store.
    pub fn path(&self) -> &std::path::Path {
        &self.db_path
    }

    /// Open (or create) the telemetry database at the given path.
    ///
    /// Creates parent directories and initializes the schema if needed.
    pub fn open(path: &Path) -> Result<Self, TelemetryError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(path)?;
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS telemetry_events (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                event_type  TEXT NOT NULL,
                payload     TEXT NOT NULL,
                created_at  TEXT NOT NULL,
                uploaded_at TEXT
            );

            -- `panic_info` holds raw text only in verbose mode (a local copy);
            -- every other mode stores a structured summary. The classification
            -- columns are added to older databases by `ensure_crash_columns`.
            CREATE TABLE IF NOT EXISTS crash_reports (
                id                      INTEGER PRIMARY KEY AUTOINCREMENT,
                panic_info              TEXT NOT NULL,
                backtrace               TEXT,
                created_at              TEXT NOT NULL,
                uploaded_at             TEXT,
                panic_template          TEXT,
                panic_fields            TEXT,
                panic_message_len       INTEGER,
                panic_message_sha256    TEXT,
                backtrace_symbols       TEXT,
                backtrace_frame_count   INTEGER,
                backtrace_symbols_len   INTEGER,
                backtrace_symbols_sha256 TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_events_uploaded
                ON telemetry_events(uploaded_at);

            CREATE INDEX IF NOT EXISTS idx_events_created
                ON telemetry_events(created_at);

            CREATE INDEX IF NOT EXISTS idx_crash_uploaded
                ON crash_reports(uploaded_at);

            CREATE INDEX IF NOT EXISTS idx_crash_created
                ON crash_reports(created_at);

            -- Retention: delete oldest 100 events when table exceeds 10000 rows
            CREATE TRIGGER IF NOT EXISTS telemetry_retention_count
            AFTER INSERT ON telemetry_events
            WHEN (SELECT COUNT(*) FROM telemetry_events) > 10000
            BEGIN
                DELETE FROM telemetry_events
                WHERE id IN (
                    SELECT id FROM telemetry_events
                    ORDER BY created_at ASC
                    LIMIT 100
                );
            END;
",
        )?;

        // Databases created before the structured crash taxonomy lack the
        // classification columns. Add them in place so existing rows survive.
        ensure_crash_columns(&conn)?;

        // The verbose crash path keeps a raw local copy in this database. Make
        // the file (and SQLite's sidecars) user-only on Unix, matching the
        // pending-file mode.
        restrict_db_permissions(path);

        let store = Self {
            conn,
            db_path: path.to_path_buf(),
        };
        // Bound the store on open: the row-count trigger keeps the row count in
        // hand, but a burst of large payloads could still grow the file past
        // what the user expects to pay for on disk.
        store.prune_to_size(DEFAULT_MAX_EVENT_STORE_BYTES)?;
        Ok(store)
    }

    /// Insert a single event.
    pub fn insert(&self, event: &TelemetryEvent) -> Result<i64, TelemetryError> {
        let (event_type, payload, created_at) = event.to_db();
        self.conn.execute(
            "INSERT INTO telemetry_events (event_type, payload, created_at)
             VALUES (?1, ?2, ?3)",
            params![event_type, payload, created_at],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Insert a batch of events.
    pub fn insert_batch(&self, events: &[TelemetryEvent]) -> Result<(), TelemetryError> {
        let tx = self.conn.unchecked_transaction()?;
        for event in events {
            let (event_type, payload, created_at) = event.to_db();
            tx.execute(
                "INSERT INTO telemetry_events (event_type, payload, created_at)
                 VALUES (?1, ?2, ?3)",
                params![event_type, payload, created_at],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Get events that haven't been uploaded yet, up to a limit.
    pub fn pending_events(&self, limit: usize) -> Result<Vec<PendingEvent>, TelemetryError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, event_type, payload, created_at
             FROM telemetry_events
             WHERE uploaded_at IS NULL
             ORDER BY created_at ASC
             LIMIT ?1",
        )?;

        let rows = stmt.query_map([limit], |row| {
            Ok(PendingEvent {
                id: row.get(0)?,
                event_type: row.get(1)?,
                payload: row.get(2)?,
                created_at: row.get(3)?,
            })
        })?;

        let mut events = Vec::new();
        for row in rows {
            events.push(row?);
        }
        Ok(events)
    }

    /// Mark events as uploaded.
    pub fn mark_uploaded(&self, ids: &[i64]) -> Result<(), TelemetryError> {
        if ids.is_empty() {
            return Ok(());
        }
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "UPDATE telemetry_events SET uploaded_at = datetime('now')
             WHERE id IN ({placeholders})"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        for (idx, id) in ids.iter().enumerate() {
            stmt.raw_bind_parameter(idx + 1, *id)?;
        }
        stmt.raw_execute()?;
        Ok(())
    }

    /// Insert a crash report.
    ///
    /// This legacy entry point stores free-form text as given and leaves the
    /// structured columns empty. The ingestion path uses
    /// `Self::insert_crash_record`.
    pub fn insert_crash(
        &self,
        panic_info: &str,
        backtrace: Option<&str>,
    ) -> Result<i64, TelemetryError> {
        self.insert_crash_record(&CrashRecord {
            panic_info: panic_info.to_string(),
            backtrace: backtrace.map(str::to_string),
            ..CrashRecord::default()
        })
    }

    /// Insert a crash report with its structured classification.
    pub(crate) fn insert_crash_record(&self, record: &CrashRecord) -> Result<i64, TelemetryError> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO crash_reports (
                panic_info, backtrace, created_at, panic_template, panic_fields,
                panic_message_len, panic_message_sha256, backtrace_symbols,
                backtrace_frame_count, backtrace_symbols_len,
                backtrace_symbols_sha256
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                record.panic_info,
                record.backtrace,
                now,
                record.panic_template,
                record.panic_fields,
                record.panic_message_len,
                record.panic_message_sha256,
                record.backtrace_symbols,
                record.backtrace_frame_count,
                record.backtrace_symbols_len,
                record.backtrace_symbols_sha256,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Get pending crash reports.
    pub fn pending_crashes(&self, limit: usize) -> Result<Vec<PendingCrash>, TelemetryError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, panic_info, backtrace, created_at, panic_template,
                    panic_fields, panic_message_len, panic_message_sha256,
                    backtrace_symbols, backtrace_frame_count,
                    backtrace_symbols_len, backtrace_symbols_sha256
             FROM crash_reports
             WHERE uploaded_at IS NULL
             ORDER BY created_at ASC
             LIMIT ?1",
        )?;

        let rows = stmt.query_map([limit], |row| {
            Ok(PendingCrash {
                id: row.get(0)?,
                panic_info: row.get(1)?,
                backtrace: row.get(2)?,
                created_at: row.get(3)?,
                panic_template: row.get(4)?,
                panic_fields: row.get(5)?,
                panic_message_len: row.get(6)?,
                panic_message_sha256: row.get(7)?,
                backtrace_symbols: row.get(8)?,
                backtrace_frame_count: row.get(9)?,
                backtrace_symbols_len: row.get(10)?,
                backtrace_symbols_sha256: row.get(11)?,
            })
        })?;

        let mut crashes = Vec::new();
        for row in rows {
            crashes.push(row?);
        }
        Ok(crashes)
    }

    /// Delete crash rows older than `max_age`.
    ///
    /// Applies to uploaded and pending rows alike: this is local retention for
    /// the verbose raw copy, which must not live forever. Returns the number of
    /// rows removed.
    pub fn prune_crashes(&self, max_age: chrono::Duration) -> Result<usize, TelemetryError> {
        let cutoff = (chrono::Utc::now() - max_age).to_rfc3339();
        let deleted = self.conn.execute(
            "DELETE FROM crash_reports WHERE created_at < ?1",
            params![cutoff],
        )?;
        Ok(deleted)
    }

    /// Mark crash reports as uploaded.
    pub fn mark_crashes_uploaded(&self, ids: &[i64]) -> Result<(), TelemetryError> {
        if ids.is_empty() {
            return Ok(());
        }
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "UPDATE crash_reports SET uploaded_at = datetime('now')
             WHERE id IN ({placeholders})"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        for (idx, id) in ids.iter().enumerate() {
            stmt.raw_bind_parameter(idx + 1, *id)?;
        }
        stmt.raw_execute()?;
        Ok(())
    }

    /// Count total events.
    pub fn event_count(&self) -> Result<i64, TelemetryError> {
        let count: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM telemetry_events", [], |row| {
                    row.get(0)
                })?;
        Ok(count)
    }

    /// Count pending (not uploaded) events.
    pub fn pending_count(&self) -> Result<i64, TelemetryError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM telemetry_events WHERE uploaded_at IS NULL",
            [],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// Check whether the database file exceeds the given size limit (bytes).
    ///
    /// Uses `PRAGMA page_count * page_size`.
    pub fn db_size_bytes(&self) -> Result<i64, TelemetryError> {
        let page_count: i64 = self
            .conn
            .query_row("PRAGMA page_count", [], |row| row.get(0))?;
        let page_size: i64 = self
            .conn
            .query_row("PRAGMA page_size", [], |row| row.get(0))?;
        Ok(page_count * page_size)
    }

    /// Prune oldest events until the database is under `max_bytes`.
    ///
    /// Deletes in chunks of 100 until under limit or no more events.
    pub fn prune_to_size(&self, max_bytes: i64) -> Result<usize, TelemetryError> {
        let mut deleted = 0;
        loop {
            let size = self.db_size_bytes()?;
            if size <= max_bytes {
                break;
            }
            let n = self.conn.execute(
                "DELETE FROM telemetry_events
                 WHERE id IN (
                     SELECT id FROM telemetry_events
                     ORDER BY created_at ASC
                     LIMIT 100
                 )",
                [],
            )?;
            if n == 0 {
                break;
            }
            deleted += n;
        }
        Ok(deleted)
    }
}

/// Add the structured-crash columns to a database created by an older build.
fn ensure_crash_columns(conn: &Connection) -> Result<(), TelemetryError> {
    const COLUMNS: [(&str, &str); 8] = [
        ("panic_template", "TEXT"),
        ("panic_fields", "TEXT"),
        ("panic_message_len", "INTEGER"),
        ("panic_message_sha256", "TEXT"),
        ("backtrace_symbols", "TEXT"),
        ("backtrace_frame_count", "INTEGER"),
        ("backtrace_symbols_len", "INTEGER"),
        ("backtrace_symbols_sha256", "TEXT"),
    ];

    let mut existing = std::collections::HashSet::new();
    {
        let mut stmt = conn.prepare("PRAGMA table_info(crash_reports)")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
        for row in rows {
            existing.insert(row?);
        }
    }

    for (name, kind) in COLUMNS {
        if !existing.contains(name) {
            // Names and types are compile-time constants, never user input.
            conn.execute_batch(&format!(
                "ALTER TABLE crash_reports ADD COLUMN {name} {kind}"
            ))?;
        }
    }
    Ok(())
}

/// Restrict the database and its SQLite sidecars to the current user.
#[allow(clippy::missing_const_for_fn)] // the unix body performs I/O, so it cannot be const there
fn restrict_db_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut candidates = vec![path.to_path_buf()];
        candidates.push(std::path::PathBuf::from(format!("{}-wal", path.display())));
        candidates.push(std::path::PathBuf::from(format!("{}-shm", path.display())));
        for candidate in candidates {
            if let Ok(metadata) = std::fs::metadata(&candidate) {
                let mut permissions = metadata.permissions();
                permissions.set_mode(0o600);
                let _ = std::fs::set_permissions(&candidate, permissions);
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Cap on the on-disk event store.
///
/// The row-count trigger bounds how many events are kept, but payloads vary in
/// size, so a count alone does not bound the file. Matches the crash store's
/// pending-byte budget.
pub const DEFAULT_MAX_EVENT_STORE_BYTES: i64 = 50 * 1024 * 1024;

/// A telemetry event pending upload.
#[derive(Debug, Clone)]
pub struct PendingEvent {
    /// Database row ID.
    pub id: i64,
    /// Event type string.
    pub event_type: String,
    /// JSON payload.
    pub payload: String,
    /// ISO-8601 timestamp.
    pub created_at: String,
}

/// A crash report pending upload.
///
/// `panic_info` is a structured summary in every mode except verbose, where it
/// is the raw local copy. `backtrace` is likewise a verbose-only local copy;
/// the upload uses the symbol-only `backtrace_symbols` instead.
#[derive(Debug, Clone)]
pub struct PendingCrash {
    /// Database row ID.
    pub id: i64,
    /// Panic info string (structured summary, or raw text in verbose mode).
    pub panic_info: String,
    /// Optional raw backtrace (verbose mode only; never uploaded).
    pub backtrace: Option<String>,
    /// ISO-8601 timestamp.
    pub created_at: String,
    /// Wire identifier of the classified panic template.
    pub panic_template: Option<String>,
    /// JSON object of safe parsed fields.
    pub panic_fields: Option<String>,
    /// Byte length of the panic message.
    pub panic_message_len: Option<i64>,
    /// SHA-256 of the panic message.
    pub panic_message_sha256: Option<String>,
    /// JSON array of symbol-only backtrace frames.
    pub backtrace_symbols: Option<String>,
    /// Total symbol-bearing frames parsed.
    pub backtrace_frame_count: Option<i64>,
    /// Byte length of the joined symbol representation.
    pub backtrace_symbols_len: Option<i64>,
    /// SHA-256 of the joined symbol representation.
    pub backtrace_symbols_sha256: Option<String>,
}

/// A crash report about to be stored, including its structured classification.
///
/// This is an internal ingestion shape; it exists so the crash module can hand
/// every column to the store in one value.
#[derive(Debug, Clone, Default)]
pub struct CrashRecord {
    /// Structured summary, or raw text in verbose mode.
    pub panic_info: String,
    /// Raw backtrace in verbose mode only.
    pub backtrace: Option<String>,
    /// Wire identifier of the panic template.
    pub panic_template: Option<String>,
    /// JSON object of safe parsed fields.
    pub panic_fields: Option<String>,
    /// Byte length of the panic message.
    pub panic_message_len: Option<i64>,
    /// SHA-256 of the panic message.
    pub panic_message_sha256: Option<String>,
    /// JSON array of symbol-only frames.
    pub backtrace_symbols: Option<String>,
    /// Total symbol-bearing frames parsed.
    pub backtrace_frame_count: Option<i64>,
    /// Byte length of the joined symbol representation.
    pub backtrace_symbols_len: Option<i64>,
    /// SHA-256 of the joined symbol representation.
    pub backtrace_symbols_sha256: Option<String>,
}

/// Types of telemetry events.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TelemetryEvent {
    /// Session started.
    SessionStart {
        /// Unique session identifier.
        session_id: String,
        /// When the event occurred.
        #[serde(with = "chrono::serde::ts_seconds")]
        timestamp: chrono::DateTime<chrono::Utc>,
    },
    /// Tool was executed.
    ToolUsed {
        /// Tool name.
        tool_name: String,
        /// Whether execution succeeded.
        success: bool,
        /// Duration in milliseconds.
        duration_ms: u64,
        /// When the event occurred.
        #[serde(with = "chrono::serde::ts_seconds")]
        timestamp: chrono::DateTime<chrono::Utc>,
    },
    /// API call was made.
    ApiCall {
        /// Provider name.
        provider: String,
        /// Model name.
        model: String,
        /// Input tokens.
        tokens_in: u32,
        /// Output tokens.
        tokens_out: u32,
        /// When the event occurred.
        #[serde(with = "chrono::serde::ts_seconds")]
        timestamp: chrono::DateTime<chrono::Utc>,
    },
    /// Error occurred.
    Error {
        /// Error type/category.
        error_type: String,
        /// Error message (sanitized, no PII).
        message: String,
        /// When the event occurred.
        #[serde(with = "chrono::serde::ts_seconds")]
        timestamp: chrono::DateTime<chrono::Utc>,
    },
    /// Session ended cleanly.
    SessionEnd {
        /// Unique session identifier, matching the session's start event.
        session_id: String,
        /// Wall-clock duration of the session in seconds.
        duration_secs: u64,
        /// When the event occurred.
        #[serde(with = "chrono::serde::ts_seconds")]
        timestamp: chrono::DateTime<chrono::Utc>,
    },
}

impl TelemetryEvent {
    fn to_db(&self) -> (String, String, String) {
        let (event_type, timestamp) = match self {
            Self::SessionStart { timestamp, .. } => ("session_start", *timestamp),
            Self::ToolUsed { timestamp, .. } => ("tool_used", *timestamp),
            Self::ApiCall { timestamp, .. } => ("api_call", *timestamp),
            Self::Error { timestamp, .. } => ("error", *timestamp),
            Self::SessionEnd { timestamp, .. } => ("session_end", *timestamp),
        };
        let payload = serde_json::to_string(self).unwrap_or_default();
        let created_at = timestamp.to_rfc3339();
        (event_type.to_string(), payload, created_at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> TelemetryStore {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        TelemetryStore::open(tmp.path()).unwrap()
    }

    #[test]
    fn insert_and_retrieve() {
        let store = temp_store();
        let event = TelemetryEvent::SessionStart {
            session_id: "test-123".into(),
            timestamp: chrono::Utc::now(),
        };
        store.insert(&event).unwrap();
        assert_eq!(store.event_count().unwrap(), 1);
        assert_eq!(store.pending_count().unwrap(), 1);
    }

    #[test]
    fn batch_insert() {
        let store = temp_store();
        let events = vec![
            TelemetryEvent::ToolUsed {
                tool_name: "Bash".into(),
                success: true,
                duration_ms: 100,
                timestamp: chrono::Utc::now(),
            },
            TelemetryEvent::ApiCall {
                provider: "anthropic".into(),
                model: "claude-sonnet".into(),
                tokens_in: 100,
                tokens_out: 50,
                timestamp: chrono::Utc::now(),
            },
        ];
        store.insert_batch(&events).unwrap();
        assert_eq!(store.event_count().unwrap(), 2);
    }

    /// `prune_to_size` bounds the file by bytes, which the row-count trigger
    /// cannot do: a few large payloads occupy more disk than many small ones.
    #[test]
    fn prune_to_size_deletes_oldest_events_when_over_budget() {
        let store = temp_store();

        for i in 0..10 {
            store
                .insert(&TelemetryEvent::SessionStart {
                    session_id: format!("s-{i}"),
                    timestamp: chrono::Utc::now(),
                })
                .unwrap();
        }
        assert_eq!(store.pending_events(100).unwrap().len(), 10);

        // A budget of zero cannot be satisfied, so everything is dropped — and
        // the loop must terminate rather than spin.
        let deleted = store.prune_to_size(0).unwrap();
        assert_eq!(deleted, 10);
        assert!(store.pending_events(100).unwrap().is_empty());

        // A generous budget leaves the store alone.
        store
            .insert(&TelemetryEvent::SessionStart {
                session_id: "kept".into(),
                timestamp: chrono::Utc::now(),
            })
            .unwrap();
        assert_eq!(store.prune_to_size(i64::MAX).unwrap(), 0);
        assert_eq!(store.pending_events(100).unwrap().len(), 1);
    }

    #[test]
    fn mark_uploaded() {
        let store = temp_store();
        let event = TelemetryEvent::SessionStart {
            session_id: "test-456".into(),
            timestamp: chrono::Utc::now(),
        };
        let id = store.insert(&event).unwrap();
        store.mark_uploaded(&[id]).unwrap();
        assert_eq!(store.pending_count().unwrap(), 0);
    }

    #[test]
    fn crash_report_roundtrip() {
        let store = temp_store();
        let id = store
            .insert_crash("panic at foo.rs:42", Some("backtrace..."))
            .unwrap();
        let pending = store.pending_crashes(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, id);
        assert_eq!(pending[0].panic_info, "panic at foo.rs:42");
        assert_eq!(pending[0].backtrace.as_deref(), Some("backtrace..."));
    }

    #[test]
    fn crash_record_roundtrip_carries_classification() {
        let store = temp_store();
        let record = CrashRecord {
            panic_info: "[strict-crash-summary] type=&str; thread=main; location=src/a.rs:1:1; template=index_out_of_bounds; message_len=40; message=omitted; backtrace=omitted".into(),
            backtrace: None,
            panic_template: Some("index_out_of_bounds".into()),
            panic_fields: Some(r#"{"panic_container_len":3,"panic_index":7}"#.into()),
            panic_message_len: Some(40),
            panic_message_sha256: Some("a".repeat(64)),
            backtrace_symbols: Some(r#"["crate::run"]"#.into()),
            backtrace_frame_count: Some(1),
            backtrace_symbols_len: Some(10),
            backtrace_symbols_sha256: Some("b".repeat(64)),
        };
        store.insert_crash_record(&record).unwrap();

        let pending = store.pending_crashes(10).unwrap();
        assert_eq!(pending.len(), 1);
        let row = &pending[0];
        assert_eq!(row.panic_template.as_deref(), Some("index_out_of_bounds"));
        assert_eq!(row.panic_fields.as_deref(), record.panic_fields.as_deref());
        assert_eq!(row.panic_message_len, Some(40));
        assert_eq!(row.panic_message_sha256, record.panic_message_sha256);
        assert_eq!(row.backtrace_symbols, record.backtrace_symbols);
        assert_eq!(row.backtrace_frame_count, Some(1));
        assert_eq!(row.backtrace_symbols_len, Some(10));
        assert_eq!(
            row.backtrace_symbols_sha256,
            record.backtrace_symbols_sha256
        );
    }

    /// A database created before the structured taxonomy is migrated in place;
    /// the old row keeps loading with empty classification columns.
    #[test]
    fn older_crash_schema_is_migrated_in_place() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            let conn = Connection::open(tmp.path()).unwrap();
            conn.execute_batch(
                "CREATE TABLE crash_reports (
                    id          INTEGER PRIMARY KEY AUTOINCREMENT,
                    panic_info  TEXT NOT NULL,
                    backtrace   TEXT,
                    created_at  TEXT NOT NULL,
                    uploaded_at TEXT
                 );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO crash_reports (panic_info, backtrace, created_at)
                 VALUES (?1, ?2, ?3)",
                params![
                    "legacy text",
                    "legacy backtrace",
                    "2020-01-01T00:00:00+00:00"
                ],
            )
            .unwrap();
        }

        let store = TelemetryStore::open(tmp.path()).unwrap();
        let pending = store.pending_crashes(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].panic_info, "legacy text");
        assert!(pending[0].panic_template.is_none());
        assert!(pending[0].backtrace_symbols.is_none());

        // New rows can use the added columns.
        store
            .insert_crash_record(&CrashRecord {
                panic_info: "[strict-crash-summary]".into(),
                panic_template: Some("explicit_panic".into()),
                ..CrashRecord::default()
            })
            .unwrap();
        let pending = store.pending_crashes(10).unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[1].panic_template.as_deref(), Some("explicit_panic"));
    }

    /// Local crash retention: rows older than the maximum age are deleted.
    #[test]
    fn prune_crashes_removes_rows_past_retention() {
        let store = temp_store();
        store
            .conn
            .execute(
                "INSERT INTO crash_reports (panic_info, created_at)
                 VALUES (?1, ?2)",
                params!["old", "2020-01-01T00:00:00+00:00"],
            )
            .unwrap();
        store
            .insert_crash("[strict-crash-summary] fresh", None)
            .unwrap();

        let deleted = store.prune_crashes(chrono::Duration::days(30)).unwrap();
        assert_eq!(deleted, 1);
        let pending = store.pending_crashes(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].panic_info.contains("fresh"));
    }
}
