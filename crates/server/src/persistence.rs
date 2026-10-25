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
    /// Fingerprint of the key that opened the session.
    ///
    /// Not the key. This row used to carry the token verbatim, which put a
    /// working credential for this daemon in a file that gets backed up,
    /// synced, and grepped — and nothing ever read it back, so the file was
    /// the only place it could leak from. What is kept is a salted digest,
    /// truncated, for one purpose: saying which key a session was opened with.
    pub token_fingerprint: String,
    /// ISO 8601 timestamp of creation.
    pub created_at: String,
    /// ISO 8601 timestamp of last activity.
    pub last_activity: String,
}

/// SQLite-backed session store.
#[derive(Debug)]
pub struct SessionStore {
    conn: Connection,
    /// Salt for the fingerprints, kept beside them in the same file.
    ///
    /// It does not make a weak key strong — an API key is guessable — but it
    /// keeps the stored digests from being matched against a table computed
    /// for every key anyone has ever used.
    salt: String,
}

impl SessionStore {
    /// Open or create a session store at the given path.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;

        // A store written before this kept the tokens themselves. It cannot be
        // carried forward without carrying them, so it is discarded — loudly.
        let legacy = conn
            .prepare("SELECT name FROM pragma_table_info('sessions') WHERE name = 'auth_token'")
            .and_then(|mut stmt| stmt.exists([]))
            .unwrap_or(false);
        if legacy {
            tracing::warn!(
                path = %path.display(),
                "this session store holds plaintext auth tokens from an older build; \
                 dropping the table rather than carrying the tokens forward"
            );
            conn.execute("DROP TABLE sessions", [])?;
        }

        conn.execute(
            "CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                token_fingerprint TEXT NOT NULL,
                created_at TEXT NOT NULL,
                last_activity TEXT NOT NULL
            )",
            [],
        )?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS store_meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            )",
            [],
        )?;

        let salt = if let Ok(salt) = conn.query_row(
            "SELECT value FROM store_meta WHERE key = 'fingerprint_salt'",
            [],
            |row| row.get::<_, String>(0),
        ) {
            salt
        } else {
            let salt = uuid::Uuid::new_v4().simple().to_string();
            conn.execute(
                "INSERT OR REPLACE INTO store_meta (key, value) VALUES ('fingerprint_salt', ?1)",
                [&salt],
            )?;
            salt
        };

        debug!("Session store opened at {:?}", path);
        Ok(Self { conn, salt })
    }

    /// The fingerprint this store keeps for a token.
    ///
    /// Salted and truncated: enough to tell two keys apart in an audit, and
    /// not enough to be worth attacking.
    #[must_use]
    pub fn fingerprint(&self, token: &str) -> String {
        use sha2::Digest as _;
        let mut hasher = sha2::Sha256::new();
        hasher.update(self.salt.as_bytes());
        hasher.update(b"\0");
        hasher.update(token.as_bytes());
        let digest = hasher.finalize();
        digest[..8].iter().fold(String::new(), |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        })
    }

    /// Save a session to the database.
    pub fn save(&mut self, session: &PersistedSession) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT OR REPLACE INTO sessions (id, token_fingerprint, created_at, last_activity)
             VALUES (?1, ?2, ?3, ?4)",
            [
                &session.id,
                &session.token_fingerprint,
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
            "SELECT id, token_fingerprint, created_at, last_activity FROM sessions \
             ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(PersistedSession {
                id: row.get(0)?,
                token_fingerprint: row.get(1)?,
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
        self.conn
            .execute("DELETE FROM sessions WHERE id = ?1", [session_id])?;
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
///
/// The fingerprint is passed in rather than computed here because it needs the
/// store's salt, and the store is what owns it.
#[must_use]
pub fn persist_session(sess: &super::Session, token_fingerprint: String) -> PersistedSession {
    let now = Utc::now().to_rfc3339();
    PersistedSession {
        id: sess.id.clone(),
        token_fingerprint,
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
        // A restored session carries the fingerprint, not the key: the key is
        // not on disk to carry, and nothing reads this back.
        auth_token: ps.token_fingerprint.clone(),
        // The role is not persisted, and a restored session gets the narrower
        // surface: coming back from the store must not widen what it may run.
        role: super::SessionRole::default(),
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

    /// A path no other test is using, with nothing left at it.
    ///
    /// The names are reused between runs, so a file left by an earlier run
    /// would otherwise be inherited — which is how a test that creates its own
    /// legacy table came to find one already there.
    fn temp_db() -> std::path::PathBuf {
        let n = DB_COUNTER.fetch_add(1, Ordering::Relaxed);
        // The counter restarts in every test process, so include the pid:
        // nextest runs each test in its own process, and two processes that
        // picked the same number would open the same SQLite file.
        let path = std::env::temp_dir().join(format!(
            "coderex_session_test_{}_{n}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn save_and_load_session() {
        let path = temp_db();
        let mut store = SessionStore::open(&path).unwrap();
        let ps = PersistedSession {
            id: "sess_test".to_string(),
            token_fingerprint: store.fingerprint("tok123"),
            created_at: Utc::now().to_rfc3339(),
            last_activity: Utc::now().to_rfc3339(),
        };
        store.save(&ps).unwrap();
        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "sess_test");
        assert_eq!(loaded[0].token_fingerprint, store.fingerprint("tok123"));
        // Cleanup
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn remove_session() {
        let path = temp_db();
        let mut store = SessionStore::open(&path).unwrap();
        let ps = PersistedSession {
            id: "sess_remove".to_string(),
            token_fingerprint: store.fingerprint("tok"),
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
        let path = temp_db();
        let store = SessionStore::open(&path).unwrap();
        let sess = super::super::Session {
            id: "sess_rt".to_string(),
            auth_token: "secret-key".to_string(),
            role: super::super::SessionRole::default(),
            created_at: Instant::now(),
            last_activity: Instant::now(),
            authenticated: true,
        };

        let persisted = persist_session(&sess, store.fingerprint(&sess.auth_token));
        assert_eq!(persisted.id, "sess_rt");
        assert_ne!(
            persisted.token_fingerprint, "secret-key",
            "the key itself reached the store"
        );

        let restored = restore_session(&persisted);
        assert_eq!(restored.id, "sess_rt");
        assert_eq!(restored.auth_token, persisted.token_fingerprint);
        assert!(restored.authenticated);
        let _ = std::fs::remove_file(&path);
    }

    /// The file must not contain the key, in any form a `grep` would find.
    #[test]
    fn the_stored_file_does_not_contain_the_key() {
        let path = temp_db();
        let mut store = SessionStore::open(&path).unwrap();
        let ps = PersistedSession {
            id: "sess_leak".to_string(),
            token_fingerprint: store.fingerprint("super-secret-key"),
            created_at: Utc::now().to_rfc3339(),
            last_activity: Utc::now().to_rfc3339(),
        };
        store.save(&ps).unwrap();
        drop(store);

        let bytes = std::fs::read(&path).unwrap();
        assert!(
            !String::from_utf8_lossy(&bytes).contains("super-secret-key"),
            "the credential is in the session store"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Two stores with different salts fingerprint the same key differently,
    /// so the digests are not a lookup table.
    #[test]
    fn fingerprints_are_salted_per_store() {
        let first = SessionStore::open(&temp_db()).unwrap();
        let second = SessionStore::open(&temp_db()).unwrap();

        assert_ne!(
            first.fingerprint("the-same-key"),
            second.fingerprint("the-same-key")
        );
        // And stable within one store.
        assert_eq!(
            first.fingerprint("the-same-key"),
            first.fingerprint("the-same-key")
        );
    }

    /// A store written by an older build holds tokens; it is dropped rather
    /// than carried forward.
    #[test]
    fn a_store_holding_plaintext_tokens_is_discarded() {
        let path = temp_db();
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute(
                "CREATE TABLE sessions (
                    id TEXT PRIMARY KEY,
                    auth_token TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    last_activity TEXT NOT NULL
                )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO sessions VALUES ('sess_old', 'plaintext-key', '', '')",
                [],
            )
            .unwrap();
        }

        let store = SessionStore::open(&path).unwrap();
        assert!(store.load_all().unwrap().is_empty());

        let bytes = std::fs::read(&path).unwrap();
        assert!(
            !String::from_utf8_lossy(&bytes).contains("plaintext-key"),
            "the old credential survived the migration"
        );
        let _ = std::fs::remove_file(&path);
    }
}
