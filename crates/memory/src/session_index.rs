//! SQLite session metadata index.

use chrono::{DateTime, Utc};
use rusqlite::Connection;
use rusqlite::params;
use std::collections::HashMap;
use std::path::Path;
use thiserror::Error;

/// Errors from session index operations.
#[derive(Debug, Error)]
pub enum SessionIndexError {
    /// A database error occurred.
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
}

/// Session metadata for the index.
#[derive(Debug, Clone)]
pub struct SessionMeta {
    /// Unique session identifier.
    pub session_id: String,
    /// Path to the project directory.
    pub project_path: String,
    /// AI-generated title.
    pub ai_title: Option<String>,
    /// User-assigned tag.
    pub tag: Option<String>,
    /// Associated PR URL.
    pub pr_url: Option<String>,
    /// When the session was created.
    pub created_at: DateTime<Utc>,
    /// When the session was last active.
    pub last_active_at: DateTime<Utc>,
    /// Total cost in USD.
    pub total_cost_usd: f64,
    /// Total token count.
    pub total_tokens: i64,
}

/// Session metadata index for fast listing and search.
#[derive(Debug)]
pub struct SessionIndex {
    conn: Connection,
}

impl SessionIndex {
    /// Open or create the session index at the given path.
    pub fn open(path: &Path) -> Result<Self, SessionIndexError> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                session_id TEXT PRIMARY KEY,
                project_path TEXT NOT NULL,
                ai_title TEXT,
                tag TEXT,
                pr_url TEXT,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                last_active_at DATETIME,
                total_cost_usd REAL DEFAULT 0.0,
                total_tokens INTEGER DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_project_path ON sessions(project_path);
            CREATE INDEX IF NOT EXISTS idx_last_active ON sessions(last_active_at DESC);
            CREATE VIRTUAL TABLE IF NOT EXISTS session_fts USING fts5(
                session_id,
                content
            );
            CREATE TABLE IF NOT EXISTS session_tfidf (
                session_id TEXT NOT NULL,
                term TEXT NOT NULL,
                tf REAL NOT NULL,
                PRIMARY KEY (session_id, term)
            );
            CREATE TABLE IF NOT EXISTS session_idf (
                term TEXT PRIMARY KEY,
                df INTEGER NOT NULL DEFAULT 0
            );",
        )?;
        Ok(Self { conn })
    }

    /// Create or update a session entry.
    ///
    /// Re-indexing an existing `session_id` updates the row (including the
    /// full-text entry) instead of duplicating it. The update is atomic.
    pub fn create(&self, meta: &SessionMeta) -> Result<(), SessionIndexError> {
        self.upsert_with_content(meta, None)
    }

    /// Create or update a session entry atomically, with searchable content.
    ///
    /// The row keeps its original `created_at` when the session already
    /// exists, while `last_active_at` and the metadata are replaced. The
    /// optional `content` (typically the transcript text) joins the title and
    /// tag in the FTS entry, so [`SessionIndex::search`] matches message
    /// content as well as metadata. The metadata row and its FTS entry are
    /// written in one transaction, so a re-index cannot leave the two out of
    /// step.
    pub fn upsert_with_content(
        &self,
        meta: &SessionMeta,
        content: Option<&str>,
    ) -> Result<(), SessionIndexError> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO sessions (
                session_id, project_path, ai_title, tag, pr_url,
                created_at, last_active_at, total_cost_usd, total_tokens
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            ON CONFLICT(session_id) DO UPDATE SET
                project_path = excluded.project_path,
                ai_title = excluded.ai_title,
                tag = excluded.tag,
                pr_url = excluded.pr_url,
                last_active_at = excluded.last_active_at,
                total_cost_usd = excluded.total_cost_usd,
                total_tokens = excluded.total_tokens",
            params![
                meta.session_id,
                meta.project_path,
                meta.ai_title,
                meta.tag,
                meta.pr_url,
                meta.created_at.to_rfc3339(),
                meta.last_active_at.to_rfc3339(),
                meta.total_cost_usd,
                meta.total_tokens,
            ],
        )?;

        tx.execute(
            "DELETE FROM session_fts WHERE session_id = ?1",
            [meta.session_id.as_str()],
        )?;

        let combined = searchable_text(meta, content);
        if !combined.is_empty() {
            tx.execute(
                "INSERT INTO session_fts (session_id, content) VALUES (?1, ?2)",
                [meta.session_id.as_str(), combined.as_str()],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    /// Update the last-active timestamp for a session.
    pub fn update_last_active(
        &self,
        session_id: &str,
        last_active: &DateTime<Utc>,
    ) -> Result<(), SessionIndexError> {
        self.conn.execute(
            "UPDATE sessions SET last_active_at = ?1 WHERE session_id = ?2",
            params![last_active.to_rfc3339(), session_id],
        )?;
        Ok(())
    }

    /// Update cost and token counts for a session.
    pub fn update_cost_and_tokens(
        &self,
        session_id: &str,
        cost: f64,
        tokens: i64,
    ) -> Result<(), SessionIndexError> {
        self.conn.execute(
            "UPDATE sessions SET total_cost_usd = ?1, total_tokens = ?2 WHERE session_id = ?3",
            params![cost, tokens, session_id],
        )?;
        Ok(())
    }

    /// List the most recent sessions.
    pub fn list_recent(&self, limit: usize) -> Result<Vec<SessionMeta>, SessionIndexError> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM sessions ORDER BY last_active_at DESC LIMIT ?1")?;
        let rows = stmt.query_map([limit], row_to_meta)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// List all sessions for a project.
    pub fn list_by_project(
        &self,
        project_path: &str,
    ) -> Result<Vec<SessionMeta>, SessionIndexError> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM sessions WHERE project_path = ?1 ORDER BY last_active_at DESC",
        )?;
        let rows = stmt.query_map([project_path], row_to_meta)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Get a single session by ID.
    pub fn get(&self, session_id: &str) -> Result<Option<SessionMeta>, SessionIndexError> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM sessions WHERE session_id = ?1")?;
        let row = stmt.query_row([session_id], row_to_meta);
        match row {
            Ok(meta) => Ok(Some(meta)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Search sessions by full-text query on title and tags.
    ///
    /// Returns matching sessions ordered by last_active_at.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SessionMeta>, SessionIndexError> {
        // Prefix-match every token, not just the last: FTS treats `a b*` as a
        // prefix query only for `b`, so a multi-word search was asymmetric.
        let search_term = query
            .split_whitespace()
            .map(|token| format!("{}*", token.trim_end_matches('*')))
            .collect::<Vec<_>>()
            .join(" ");
        let mut stmt = self.conn.prepare(
            "SELECT s.* FROM sessions s
             WHERE s.session_id IN (
                 SELECT session_id FROM session_fts WHERE session_fts MATCH ?1
             )
             ORDER BY s.last_active_at DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![search_term, limit], row_to_meta)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    // ─── TF-IDF semantic search ─────────────────────────────────────────────

    /// Index the content of a session for semantic (TF-IDF cosine similarity) search.
    ///
    /// Tokenizes the content, stores term frequencies in the `tfidf` table.
    /// On subsequent `search_semantic` calls, query tokens are compared
    /// against stored term frequencies using cosine similarity.
    // TF-IDF weights; token counts are far below 2^53.
    #[allow(clippy::cast_precision_loss)]
    pub fn index_content(&self, session_id: &str, content: &str) -> Result<(), SessionIndexError> {
        self.conn.execute(
            "DELETE FROM session_tfidf WHERE session_id = ?1",
            [session_id],
        )?;

        let terms = tokenize(content);
        if terms.is_empty() {
            return Ok(());
        }

        let total: f64 = terms.values().sum::<usize>() as f64;
        for (term, count) in &terms {
            let tf = *count as f64 / total;
            self.conn.execute(
                "INSERT OR REPLACE INTO session_tfidf (session_id, term, tf) VALUES (?1, ?2, ?3)",
                params![session_id, term, tf],
            )?;
        }

        // Update document frequency count
        for term in terms.keys() {
            self.conn
                .execute("UPDATE session_idf SET df = df + 1 WHERE term = ?1", [term])?;
            if self.conn.execute(
                "INSERT OR IGNORE INTO session_idf (term, df) VALUES (?1, 1)",
                [term],
            )? == 0
            {
                // Already existed, df was updated above
            }
        }

        Ok(())
    }

    /// Search sessions by semantic similarity to the query.
    ///
    /// Computes TF-IDF cosine similarity between query tokens and stored
    /// session term frequencies. Returns the top `limit` sessions ordered
    /// by similarity score.
    // TF-IDF weights; token and document counts are far below 2^53.
    #[allow(clippy::cast_precision_loss)]
    pub fn search_semantic(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(String, f64)>, SessionIndexError> {
        let query_terms = tokenize(query);
        if query_terms.is_empty() {
            return Ok(Vec::new());
        }

        let total_query: f64 = query_terms.values().sum::<usize>() as f64;

        // Get total document count for IDF
        let doc_count: i64 = self.conn.query_row(
            "SELECT COUNT(DISTINCT session_id) FROM session_tfidf",
            [],
            |row| row.get(0),
        )?;
        let doc_count = doc_count.max(1);

        // Load all stored TF values into memory
        let mut stmt = self
            .conn
            .prepare("SELECT session_id, term, tf FROM session_tfidf")?;
        let rows = stmt.query_map([], |row| {
            let sid: String = row.get(0)?;
            let term: String = row.get(1)?;
            let tf: f64 = row.get(2)?;
            Ok((sid, term, tf))
        })?;

        // Group by session: session_id -> { term -> tf }
        let mut session_tfs: HashMap<String, HashMap<String, f64>> = HashMap::new();
        for row in rows {
            let (sid, term, tf) = row?;
            session_tfs.entry(sid).or_default().insert(term, tf);
        }

        // Build IDF lookup
        let mut idf_stmt = self.conn.prepare("SELECT term, df FROM session_idf")?;
        let idf_rows = idf_stmt.query_map([], |row| {
            let term: String = row.get(0)?;
            let df: i64 = row.get(1)?;
            Ok((term, df))
        })?;
        let idf_map: HashMap<String, i64> = idf_rows.filter_map(std::result::Result::ok).collect();

        // Compute IDF for query terms
        let query_idf: HashMap<String, f64> = query_terms
            .keys()
            .map(|term| {
                let df = idf_map.get(term).copied().unwrap_or(0);
                (term.clone(), (doc_count as f64) / (1.0 + df as f64))
            })
            .collect();

        // Compute query vector norm
        let query_norm: f64 = query_terms
            .iter()
            .map(|(term, count)| {
                let qtf = *count as f64 / total_query;
                let idf = query_idf.get(term).copied().unwrap_or(1.0);
                (qtf * idf).powi(2)
            })
            .sum::<f64>()
            .sqrt();

        if query_norm == 0.0 {
            return Ok(Vec::new());
        }

        // Compute cosine similarity for each session
        let mut results: Vec<(String, f64)> = session_tfs
            .into_iter()
            .filter_map(|(sid, terms)| {
                let mut dot = 0.0f64;
                let mut s_norm_sq = 0.0f64;

                for (term, tf) in &terms {
                    let df = idf_map.get(term).copied().unwrap_or(0);
                    let idf = (doc_count as f64) / (1.0 + df as f64);
                    s_norm_sq += (tf * idf).powi(2);

                    if let Some(&qtf_count) = query_terms.get(term) {
                        let qtf = qtf_count as f64 / total_query;
                        dot = (*tf * idf).mul_add(qtf * idf, dot);
                    }
                }

                let s_norm = s_norm_sq.sqrt();
                if s_norm == 0.0 || dot == 0.0 {
                    return None;
                }
                Some((sid, dot / (s_norm * query_norm)))
            })
            .collect();

        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(limit);
        Ok(results)
    }
}

/// Combine the metadata and optional content into one FTS document.
fn searchable_text(meta: &SessionMeta, content: Option<&str>) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if let Some(title) = meta
        .ai_title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
    {
        parts.push(title);
    }
    if let Some(tag) = meta.tag.as_deref().filter(|tag| !tag.trim().is_empty()) {
        parts.push(tag);
    }
    if let Some(content) = content.filter(|content| !content.trim().is_empty()) {
        parts.push(content);
    }
    parts.join(" ").trim().to_string()
}

/// Tokenize text into lowercase alphanumeric tokens.
fn tokenize(text: &str) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for token in text
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|s| s.len() > 1)
    {
        *counts.entry(token.to_string()).or_default() += 1;
    }
    counts
}

/// Convert a database row to `SessionMeta`.
fn row_to_meta(row: &rusqlite::Row) -> rusqlite::Result<SessionMeta> {
    let parse_dt = |s: String| -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(&s).map_or_else(|_| Utc::now(), |dt| dt.with_timezone(&Utc))
    };
    Ok(SessionMeta {
        session_id: row.get(0)?,
        project_path: row.get(1)?,
        ai_title: row.get(2)?,
        tag: row.get(3)?,
        pr_url: row.get(4)?,
        created_at: parse_dt(row.get(5)?),
        last_active_at: parse_dt(row.get(6)?),
        total_cost_usd: row.get(7)?,
        total_tokens: row.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_index() -> (SessionIndex, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sessions.db");
        let index = SessionIndex::open(&path).unwrap();
        (index, tmp)
    }

    fn test_meta(id: &str) -> SessionMeta {
        SessionMeta {
            session_id: id.into(),
            project_path: "/test/project".into(),
            ai_title: Some("Test Session".into()),
            tag: None,
            pr_url: None,
            created_at: Utc::now(),
            last_active_at: Utc::now(),
            total_cost_usd: 0.0,
            total_tokens: 0,
        }
    }

    #[test]
    fn create_and_get_returns_same_data() {
        let (index, _tmp) = temp_index();
        let meta = test_meta("test_1");
        index.create(&meta).unwrap();

        let retrieved = index.get("test_1").unwrap().unwrap();
        assert_eq!(retrieved.session_id, "test_1");
        assert_eq!(retrieved.project_path, "/test/project");
        assert_eq!(retrieved.ai_title, Some("Test Session".into()));
    }

    #[test]
    fn update_last_active_changes_value() {
        let (index, _tmp) = temp_index();
        let meta = test_meta("test_2");
        index.create(&meta).unwrap();

        let new_time = Utc::now() + chrono::Duration::hours(1);
        index.update_last_active("test_2", &new_time).unwrap();

        let retrieved = index.get("test_2").unwrap().unwrap();
        assert!(retrieved.last_active_at >= new_time);
    }

    #[test]
    fn list_recent_returns_ordered_by_last_active() {
        let (index, _tmp) = temp_index();

        let mut meta1 = test_meta("recent_1");
        meta1.last_active_at = Utc::now() - chrono::Duration::hours(2);
        index.create(&meta1).unwrap();

        let mut meta2 = test_meta("recent_2");
        meta2.last_active_at = Utc::now();
        index.create(&meta2).unwrap();

        let recent = index.list_recent(10).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].session_id, "recent_2");
        assert_eq!(recent[1].session_id, "recent_1");
    }

    #[test]
    fn list_by_project_filters_correctly() {
        let (index, _tmp) = temp_index();

        let mut meta1 = test_meta("proj_a_1");
        meta1.project_path = "/project/a".into();
        index.create(&meta1).unwrap();

        let mut meta2 = test_meta("proj_b_1");
        meta2.project_path = "/project/b".into();
        index.create(&meta2).unwrap();

        let sessions_a = index.list_by_project("/project/a").unwrap();
        assert_eq!(sessions_a.len(), 1);
        assert_eq!(sessions_a[0].session_id, "proj_a_1");

        let sessions_b = index.list_by_project("/project/b").unwrap();
        assert_eq!(sessions_b.len(), 1);
        assert_eq!(sessions_b[0].session_id, "proj_b_1");
    }

    #[test]
    fn create_on_existing_session_id_replaces() {
        let (index, _tmp) = temp_index();

        let mut meta = test_meta("replace_1");
        meta.ai_title = Some("First".into());
        index.create(&meta).unwrap();

        let mut meta2 = test_meta("replace_1");
        meta2.ai_title = Some("Second".into());
        index.create(&meta2).unwrap();

        let retrieved = index.get("replace_1").unwrap().unwrap();
        assert_eq!(retrieved.ai_title, Some("Second".into()));
    }

    #[test]
    fn search_finds_matching_title() {
        let (index, _tmp) = temp_index();

        let mut meta1 = test_meta("s1");
        meta1.ai_title = Some("Authentication fix".into());
        index.create(&meta1).unwrap();

        let mut meta2 = test_meta("s2");
        meta2.ai_title = Some("UI redesign".into());
        index.create(&meta2).unwrap();

        let results = index.search("auth", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].session_id, "s1");
    }

    #[test]
    fn search_respects_limit() {
        let (index, _tmp) = temp_index();

        for i in 0..5 {
            let mut meta = test_meta(&format!("search_{i}"));
            meta.ai_title = Some(format!("Test session {i}"));
            index.create(&meta).unwrap();
        }

        let results = index.search("test", 2).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn semantic_search_finds_indexed_content() {
        let (index, _tmp) = temp_index();

        index
            .index_content("sess_auth", "authentication login oauth2 token session")
            .unwrap();
        index
            .index_content("sess_ui", "react component button styling css")
            .unwrap();
        index
            .index_content("sess_db", "database migration postgresql schema")
            .unwrap();

        let results = index.search_semantic("authentication token", 3).unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].0, "sess_auth");
        assert!(results[0].1 > 0.0);
    }

    #[test]
    fn semantic_search_returns_empty_for_no_matches() {
        let (index, _tmp) = temp_index();

        index.index_content("sess_a", "hello world").unwrap();

        // Query has completely different terms than indexed content
        let results = index.search_semantic("quantum physics", 5).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn semantic_search_respects_limit() {
        let (index, _tmp) = temp_index();

        for i in 0..5 {
            index
                .index_content(&format!("sem_{i}"), &format!("test session number {i}"))
                .unwrap();
        }

        let results = index.search_semantic("test session", 2).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn semantic_search_empty_query_returns_empty() {
        let (index, _tmp) = temp_index();
        index.index_content("sess_x", "some content").unwrap();

        let results = index.search_semantic("", 5).unwrap();
        assert!(results.is_empty());
    }
}
