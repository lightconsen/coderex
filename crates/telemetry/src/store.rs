//! Local SQLite storage for telemetry events.

use crate::TelemetryError;
use rusqlite::{params, Connection};
use std::path::Path;

/// SQLite-backed store for telemetry events.
#[derive(Debug)]
pub struct TelemetryStore {
    conn: Connection,
}

impl TelemetryStore {
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

            CREATE TABLE IF NOT EXISTS crash_reports (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                panic_info  TEXT NOT NULL,
                backtrace   TEXT,
                created_at  TEXT NOT NULL,
                uploaded_at TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_events_uploaded
                ON telemetry_events(uploaded_at);

            CREATE INDEX IF NOT EXISTS idx_events_created
                ON telemetry_events(created_at);

            -- Retention: delete events older than 7 days
            CREATE TRIGGER IF NOT EXISTS telemetry_retention
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
            "
        )?;

        Ok(Self { conn })
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
             LIMIT ?1"
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
    pub fn insert_crash(&self, panic_info: &str, backtrace: Option<&str>) -> Result<i64, TelemetryError> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO crash_reports (panic_info, backtrace, created_at)
             VALUES (?1, ?2, ?3)",
            params![panic_info, backtrace, now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Get pending crash reports.
    pub fn pending_crashes(&self, limit: usize) -> Result<Vec<PendingCrash>, TelemetryError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, panic_info, backtrace, created_at
             FROM crash_reports
             WHERE uploaded_at IS NULL
             ORDER BY created_at ASC
             LIMIT ?1"
        )?;

        let rows = stmt.query_map([limit], |row| {
            Ok(PendingCrash {
                id: row.get(0)?,
                panic_info: row.get(1)?,
                backtrace: row.get(2)?,
                created_at: row.get(3)?,
            })
        })?;

        let mut crashes = Vec::new();
        for row in rows {
            crashes.push(row?);
        }
        Ok(crashes)
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
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM telemetry_events",
            [],
            |row| row.get(0),
        )?;
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
}

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
#[derive(Debug, Clone)]
pub struct PendingCrash {
    /// Database row ID.
    pub id: i64,
    /// Panic info string.
    pub panic_info: String,
    /// Optional backtrace.
    pub backtrace: Option<String>,
    /// ISO-8601 timestamp.
    pub created_at: String,
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
}

impl TelemetryEvent {
    fn to_db(&self) -> (String, String, String) {
        let event_type = match self {
            TelemetryEvent::SessionStart { .. } => "session_start",
            TelemetryEvent::ToolUsed { .. } => "tool_used",
            TelemetryEvent::ApiCall { .. } => "api_call",
            TelemetryEvent::Error { .. } => "error",
        };
        let payload = serde_json::to_string(self).unwrap_or_default();
        let created_at = chrono::Utc::now().to_rfc3339();
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
        let id = store.insert_crash("panic at foo.rs:42", Some("backtrace...")).unwrap();
        let pending = store.pending_crashes(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, id);
        assert_eq!(pending[0].panic_info, "panic at foo.rs:42");
        assert_eq!(pending[0].backtrace.as_deref(), Some("backtrace..."));
    }
}
