//! Session persistence — save/load sessions to SQLite.

use chrono::Utc;
use rusqlite::{Connection, Result};
use std::path::Path;
use std::time::Instant;
use tracing::{debug, info};

/// Stored session row for persistence.
#[derive(Debug, Clone)]
pub struct PersistedSession {
    /// Unique session ID.
    pub id: String,
    /// Auth token used.
    pub auth_token: String,
    /// ISO 8601 timestamp of creation.
    pub created_at: String,
    /// ISO 8601 timestamp of last activity.
    pub last_activity: String,
}

/// SQLite-backed session store.
#[derive(Debug)]
pub struct SessionStore {
    conn: Connection,
}

impl SessionStore {
    /// Open or create a session store at the given path.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                auth_token TEXT NOT NULL,
                created_at TEXT NOT NULL,
                last_activity TEXT NOT NULL
            )",
            [],
        )?;
        debug!("Session store opened at {:?}", path);
        Ok(Self { conn })
    }

    /// Save a session to the database.
    pub fn save(&mut self, session: &PersistedSession) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT OR REPLACE INTO sessions (id, auth_token, created_at, last_activity)
             VALUES (?1, ?2, ?3, ?4)",
            [
                &session.id,
                &session.auth_token,
                &session.created_at,
                &now,
            ],
        )?;
        debug!("Saved session {}", session.id);
        Ok(())
    }

    /// Load all active sessions from the database.
    pub fn load_all(&self) -> Result<Vec<PersistedSession>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, auth_token, created_at, last_activity FROM sessions ORDER BY created_at"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(PersistedSession {
                id: row.get(0)?,
                auth_token: row.get(1)?,
                created_at: row.get(2)?,
                last_activity: row.get(3)?,
            })
        })?;
        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row?);
        }
        info!("Loaded {} sessions from store", sessions.len());
        Ok(sessions)
    }

    /// Remove a session from the database.
    pub fn remove(&mut self, session_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM sessions WHERE id = ?1",
            [session_id],
        )?;
        debug!("Removed session {} from store", session_id);
        Ok(())
    }

    /// Clear all sessions from the store.
    pub fn clear(&mut self) -> Result<()> {
        self.conn.execute("DELETE FROM sessions", [])?;
        info!("Cleared all sessions from store");
        Ok(())
    }
}

/// Convert a `super::Session` to a `PersistedSession`.
pub fn persist_session(sess: &super::Session) -> PersistedSession {
    let now = Utc::now().to_rfc3339();
    PersistedSession {
        id: sess.id.clone(),
        auth_token: sess.auth_token.clone(),
        created_at: now.clone(),
        last_activity: now,
    }
}

/// Convert a `PersistedSession` to an in-memory `super::Session`.
/// Note: `last_activity` is set to now since timestamps are approximated.
pub fn restore_session(ps: &PersistedSession) -> super::Session {
    let now = Instant::now();
    super::Session {
        id: ps.id.clone(),
        auth_token: ps.auth_token.clone(),
        created_at: now,
        last_activity: now,
        authenticated: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static DB_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_db() -> std::path::PathBuf {
        let n = DB_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("coderex_session_test_{n}.db"))
    }

    #[test]
    fn save_and_load_session() {
        let path = temp_db();
        let mut store = SessionStore::open(&path).unwrap();
        let ps = PersistedSession {
            id: "sess_test".to_string(),
            auth_token: "tok123".to_string(),
            created_at: Utc::now().to_rfc3339(),
            last_activity: Utc::now().to_rfc3339(),
        };
        store.save(&ps).unwrap();
        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "sess_test");
        assert_eq!(loaded[0].auth_token, "tok123");
        // Cleanup
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn remove_session() {
        let path = temp_db();
        let mut store = SessionStore::open(&path).unwrap();
        let ps = PersistedSession {
            id: "sess_remove".to_string(),
            auth_token: "tok".to_string(),
            created_at: Utc::now().to_rfc3339(),
            last_activity: Utc::now().to_rfc3339(),
        };
        store.save(&ps).unwrap();
        store.remove("sess_remove").unwrap();
        let loaded = store.load_all().unwrap();
        assert!(loaded.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn persist_restore_roundtrip() {
        let sess = super::super::Session {
            id: "sess_rt".to_string(),
            auth_token: "secret".to_string(),
            created_at: Instant::now(),
            last_activity: Instant::now(),
            authenticated: true,
        };
        let persisted = persist_session(&sess);
        assert_eq!(persisted.id, "sess_rt");
        assert_eq!(persisted.auth_token, "secret");
        let restored = restore_session(&persisted);
        assert_eq!(restored.id, "sess_rt");
        assert_eq!(restored.auth_token, "secret");
        assert!(restored.authenticated);
    }
}
