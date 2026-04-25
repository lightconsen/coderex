//! JSONL transcript store — append-only session transcripts.

use chrono::{DateTime, Utc};
use coderex_core::types::Message;
use flate2::Compression;
use flate2::write::GzEncoder;
use regex::Regex;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use thiserror::Error;

/// Errors from transcript operations.
#[derive(Debug, Error)]
pub enum TranscriptError {
    /// An I/O error occurred.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A JSON parsing error occurred.
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Regex pattern to skip the first system prompt during replay.
static SKIP_FIRST_PROMPT_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""role"\s*:\s*"System""#).expect("invalid skip regex"));

/// Append-only JSONL transcript store.
///
/// Stored at `<base_path>/<session_id>.jsonl`.
/// Each line is a JSON-serialized `Message`.
#[derive(Debug)]
pub struct TranscriptStore {
    base_path: PathBuf,
}

impl TranscriptStore {
    /// Create a new transcript store rooted at the given path.
    #[must_use]
    pub const fn new(base_path: PathBuf) -> Self {
        Self { base_path }
    }

    /// Get the file path for a session.
    fn session_path(&self, session_id: &str) -> PathBuf {
        self.base_path.join(format!("{session_id}.jsonl"))
    }

    /// Append a message to a session's transcript.
    pub fn append(&self, session_id: &str, message: &Message) -> Result<(), TranscriptError> {
        let path = self.session_path(session_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        let line = serde_json::to_string(message)?;
        writeln!(file, "{line}")?;
        file.sync_all()?;
        Ok(())
    }

    /// Read all messages from a session's transcript.
    ///
    /// Returns an empty vec if the file does not exist.
    pub fn read_all(&self, session_id: &str) -> Result<Vec<Message>, TranscriptError> {
        let path = self.session_path(session_id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = std::fs::File::open(&path)?;
        let reader = BufReader::new(file);
        let mut messages = Vec::new();
        let mut line_no: usize = 0;
        for line in reader.lines() {
            line_no += 1;
            let line = line?;
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<Message>(&line) {
                Ok(msg) => messages.push(msg),
                Err(e) => {
                    tracing::warn!(
                        "skipping corrupted transcript line {} in {:?}: {}",
                        line_no,
                        path,
                        e
                    );
                }
            }
        }
        Ok(messages)
    }

    /// Read all messages, skipping the first system prompt.
    ///
    /// Useful for session replay where the initial system prompt
    /// should not be re-displayed.
    pub fn read_all_skip_first_prompt(
        &self,
        session_id: &str,
    ) -> Result<Vec<Message>, TranscriptError> {
        let path = self.session_path(session_id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = std::fs::File::open(&path)?;
        let reader = BufReader::new(file);
        let mut messages = Vec::new();
        let mut skipped = false;
        let mut line_no: usize = 0;
        for line in reader.lines() {
            line_no += 1;
            let line = line?;
            if line.is_empty() {
                continue;
            }
            if !skipped && SKIP_FIRST_PROMPT_PATTERN.is_match(&line) {
                skipped = true;
                continue;
            }
            match serde_json::from_str::<Message>(&line) {
                Ok(msg) => messages.push(msg),
                Err(e) => {
                    tracing::warn!(
                        "skipping corrupted transcript line {} in {:?}: {}",
                        line_no,
                        path,
                        e
                    );
                }
            }
        }
        Ok(messages)
    }

    /// Write a compact boundary marker to the transcript.
    ///
    /// Appends a special marker message indicating where compact occurred.
    pub fn write_compact_boundary(&self, session_id: &str) -> Result<(), TranscriptError> {
        let boundary_msg = Message::system(
            "<coderex_compact_boundary>\n\
            Context was compacted at this point.\n\
            Earlier conversation history has been removed.\n\
            </coderex_compact_boundary>",
        );
        self.append(session_id, &boundary_msg)
    }

    /// Rewrite the transcript, keeping only messages from a given index onward.
    ///
    /// Uses a temp file + atomic rename for crash safety.
    pub fn truncate_before(
        &self,
        session_id: &str,
        keep_from_index: usize,
    ) -> Result<(), TranscriptError> {
        let path = self.session_path(session_id);
        if !path.exists() {
            return Ok(());
        }

        let messages = self.read_all(session_id)?;
        if keep_from_index >= messages.len() {
            // Nothing to keep — truncate to empty
            std::fs::write(&path, "")?;
            return Ok(());
        }

        // Write kept messages to a temp file
        let temp_path = path.with_extension("jsonl.tmp");
        let mut file = std::fs::File::create(&temp_path)?;
        for msg in &messages[keep_from_index..] {
            let line = serde_json::to_string(msg)?;
            writeln!(file, "{line}")?;
        }

        // Atomically rename
        std::fs::rename(&temp_path, &path)?;
        Ok(())
    }

    /// Rewrite the transcript with a new set of messages (full replace).
    ///
    /// Uses a temp file + atomic rename for crash safety.
    pub fn rewrite(&self, session_id: &str, messages: &[Message]) -> Result<(), TranscriptError> {
        let path = self.session_path(session_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Write to a temp file
        let temp_path = path.with_extension("jsonl.tmp");
        let mut file = std::fs::File::create(&temp_path)?;
        for msg in messages {
            let line = serde_json::to_string(msg)?;
            writeln!(file, "{line}")?;
        }

        // Atomically rename
        std::fs::rename(&temp_path, &path)?;
        Ok(())
    }

    /// List all session IDs found in the base directory.
    pub fn list_sessions(&self) -> std::io::Result<Vec<String>> {
        let mut sessions = Vec::new();
        if !self.base_path.exists() {
            return Ok(sessions);
        }
        for entry in std::fs::read_dir(&self.base_path)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    sessions.push(stem.to_string());
                }
            }
        }
        sessions.sort();
        Ok(sessions)
    }

    /// Get the file modification time for a session.
    pub fn session_modified_at(&self, session_id: &str) -> std::io::Result<Option<DateTime<Utc>>> {
        let path = self.session_path(session_id);
        if !path.exists() {
            return Ok(None);
        }
        let metadata = path.metadata()?;
        metadata
            .modified()
            .map(|t| Some(DateTime::<Utc>::from(t)))
            .or(Ok(None))
    }

    /// Create a compressed archive of a session transcript.
    ///
    /// Writes `<archive_dir>/<session_id>.jsonl.gz`.
    pub fn archive_session(
        &self,
        session_id: &str,
        archive_dir: &Path,
    ) -> std::io::Result<PathBuf> {
        let source = self.session_path(session_id);
        if !source.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "session not found",
            ));
        }
        std::fs::create_dir_all(archive_dir)?;
        let dest = archive_dir.join(format!("{session_id}.jsonl.gz"));

        let content = std::fs::read(&source)?;
        let mut encoder = GzEncoder::new(File::create(&dest)?, Compression::default());
        encoder.write_all(&content)?;
        encoder.finish()?;

        Ok(dest)
    }

    /// Prune old sessions: archive and delete sessions beyond the limits.
    ///
    /// Sessions are sorted by last-modified time (oldest first).
    /// Archives sessions older than `max_sessions` to `<base_path>/archives/`
    /// and deletes the originals.
    pub fn prune(&self, max_sessions: usize) -> std::io::Result<PruneResult> {
        let sessions = self.list_sessions()?;
        if sessions.len() <= max_sessions {
            return Ok(PruneResult {
                archived: 0,
                deleted: 0,
                remaining: sessions.len(),
            });
        }

        // Sort by modification time (oldest first)
        let mut session_times: Vec<_> = sessions
            .into_iter()
            .filter_map(|id| {
                self.session_modified_at(&id)
                    .ok()
                    .flatten()
                    .map(|t| (id, t))
            })
            .collect();
        session_times.sort_by_key(|(_, t)| *t);

        let to_prune_count = session_times.len() - max_sessions;
        let to_prune = &session_times[..to_prune_count];

        let archive_dir = self.base_path.join("archives");
        std::fs::create_dir_all(&archive_dir)?;

        let mut archived = 0;
        let mut deleted = 0;

        for (id, _) in to_prune {
            // Archive
            if self.archive_session(id, &archive_dir).is_ok() {
                archived += 1;
                // Delete original
                let path = self.session_path(id);
                if std::fs::remove_file(&path).is_ok() {
                    deleted += 1;
                }
            }
        }

        let remaining = self.list_sessions()?.len();
        Ok(PruneResult {
            archived,
            deleted,
            remaining,
        })
    }

    /// Create a backup copy of a session transcript.
    ///
    /// Writes `<session_path>.bak`.
    pub fn backup(&self, session_id: &str) -> std::io::Result<PathBuf> {
        let source = self.session_path(session_id);
        if !source.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "session not found",
            ));
        }
        let backup_path = source.with_extension("jsonl.bak");
        std::fs::copy(&source, &backup_path)?;
        Ok(backup_path)
    }

    /// Restore a session from its backup.
    ///
    /// Reads `<session_path>.bak` and replaces the current transcript.
    /// Returns `None` if no backup exists.
    pub fn restore(&self, session_id: &str) -> std::io::Result<Option<PathBuf>> {
        let source = self.session_path(session_id);
        let backup_path = source.with_extension("jsonl.bak");
        if !backup_path.exists() {
            return Ok(None);
        }
        std::fs::copy(&backup_path, &source)?;
        Ok(Some(source))
    }

    // ─── Encryption (feature-gated) ────────────────────────────────────────

    /// Append an encrypted message to a session's transcript.
    ///
    /// The file is stored at `<base_path>/<session_id>.jsonl.enc`.
    /// Each line is `nonce_hex:ciphertext_hex`.
    #[cfg(feature = "encryption")]
    pub fn append_encrypted(
        &self,
        session_id: &str,
        message: &Message,
        cipher: &aes_gcm::Aes256Gcm,
    ) -> Result<(), TranscriptError> {
        use aes_gcm::aead::{Aead, AeadCore};
        use rand_core::OsRng;
        let path = self.encrypted_session_path(session_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        let line = serde_json::to_string(message)?;
        let nonce_bytes = <aes_gcm::Aes256Gcm as AeadCore>::generate_nonce(&mut OsRng);
        let ciphertext = cipher
            .encrypt(&nonce_bytes, line.as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("{e}")))?;
        writeln!(
            file,
            "{}:{}",
            hex::encode(nonce_bytes.as_slice()),
            hex::encode(&ciphertext)
        )?;
        Ok(())
    }

    /// Read all messages from an encrypted session transcript.
    ///
    /// Reads from `<base_path>/<session_id>.jsonl.enc`.
    /// Returns an empty vec if the file does not exist.
    #[cfg(feature = "encryption")]
    pub fn read_all_decrypted(
        &self,
        session_id: &str,
        cipher: &aes_gcm::Aes256Gcm,
    ) -> Result<Vec<Message>, TranscriptError> {
        use aes_gcm::aead::Aead;
        let path = self.encrypted_session_path(session_id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = std::fs::File::open(&path)?;
        let reader = BufReader::new(file);
        let mut messages = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.is_empty() {
                continue;
            }
            let Some((nonce_hex, ct_hex)) = line.split_once(':') else {
                tracing::warn!("skipping malformed encrypted line in {:?}", path);
                continue;
            };
            let nonce = hex::decode(nonce_hex)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            let ct = hex::decode(ct_hex)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

            let plaintext = cipher
                .decrypt(nonce.as_slice().into(), ct.as_ref())
                .map_err(|e| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{e}"))
                })?;

            match serde_json::from_slice::<Message>(&plaintext) {
                Ok(msg) => messages.push(msg),
                Err(e) => {
                    tracing::warn!("skipping corrupted decrypted line in {:?}: {}", path, e);
                }
            }
        }
        Ok(messages)
    }

    /// Get the file path for an encrypted session.
    #[cfg(feature = "encryption")]
    fn encrypted_session_path(&self, session_id: &str) -> PathBuf {
        self.base_path.join(format!("{session_id}.jsonl.enc"))
    }
}

/// Result of a prune operation.
#[derive(Debug, Clone)]
pub struct PruneResult {
    /// Number of sessions archived.
    pub archived: usize,
    /// Number of originals deleted.
    pub deleted: usize,
    /// Number of sessions remaining.
    pub remaining: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use coderex_core::types::Message;

    fn temp_store() -> (TranscriptStore, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let store = TranscriptStore::new(tmp.path().to_path_buf());
        (store, tmp)
    }

    #[test]
    fn append_and_read_single_message() {
        let (store, _tmp) = temp_store();
        let msg = Message::user("hello world");
        store.append("session_1", &msg).unwrap();

        let messages = store.read_all("session_1").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, coderex_core::types::Role::User);
        if let coderex_core::types::MessageContent::Text(s) = &messages[0].content {
            assert_eq!(s, "hello world");
        } else {
            panic!("expected text content");
        }
    }

    #[test]
    fn append_and_read_multiple_messages_in_order() {
        let (store, _tmp) = temp_store();
        store.append("session_2", &Message::user("first")).unwrap();
        store
            .append("session_2", &Message::assistant("second"))
            .unwrap();
        store.append("session_2", &Message::user("third")).unwrap();

        let messages = store.read_all("session_2").unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, coderex_core::types::Role::User);
        assert_eq!(messages[1].role, coderex_core::types::Role::Assistant);
        assert_eq!(messages[2].role, coderex_core::types::Role::User);
    }

    #[test]
    fn read_all_missing_file_returns_empty() {
        let (store, _tmp) = temp_store();
        let messages = store.read_all("nonexistent").unwrap();
        assert!(messages.is_empty());
    }

    #[test]
    fn skip_first_prompt_skips_system_message() {
        let (store, _tmp) = temp_store();
        store
            .append("session_3", &Message::system("you are a helper"))
            .unwrap();
        store.append("session_3", &Message::user("hi")).unwrap();
        store
            .append("session_3", &Message::assistant("hello"))
            .unwrap();

        let messages = store.read_all_skip_first_prompt("session_3").unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, coderex_core::types::Role::User);
        assert_eq!(messages[1].role, coderex_core::types::Role::Assistant);
    }

    #[test]
    fn large_message_count_no_oom() {
        let (store, _tmp) = temp_store();
        for i in 0..100 {
            store
                .append("session_large", &Message::user(format!("msg-{i}")))
                .unwrap();
        }
        let messages = store.read_all("session_large").unwrap();
        assert_eq!(messages.len(), 100);
    }

    #[test]
    fn read_all_preserves_message_roundtrip() {
        let (store, _tmp) = temp_store();
        let original = Message::tool(coderex_core::types::ToolResult {
            tool_call_id: "tc_1".into(),
            output: "file content".into(),
            is_error: false,
        });
        store.append("session_rt", &original).unwrap();

        let messages = store.read_all("session_rt").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, coderex_core::types::Role::Tool);
    }

    #[test]
    fn write_compact_boundary_appends_marker() {
        let (store, _tmp) = temp_store();
        store.append("session_b", &Message::user("hello")).unwrap();
        store.write_compact_boundary("session_b").unwrap();

        let messages = store.read_all("session_b").unwrap();
        assert_eq!(messages.len(), 2);
        let boundary = &messages[1];
        assert!(matches!(boundary.role, coderex_core::types::Role::System));
        if let coderex_core::types::MessageContent::Text(s) = &boundary.content {
            assert!(s.contains("<coderex_compact_boundary>"));
        } else {
            panic!("expected text content");
        }
    }

    #[test]
    fn truncate_before_removes_messages_before_index() {
        let (store, _tmp) = temp_store();
        store.append("session_t", &Message::user("first")).unwrap();
        store
            .append("session_t", &Message::assistant("second"))
            .unwrap();
        store.append("session_t", &Message::user("third")).unwrap();

        store.truncate_before("session_t", 1).unwrap();

        let messages = store.read_all("session_t").unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, coderex_core::types::Role::Assistant);
        assert_eq!(messages[1].role, coderex_core::types::Role::User);
    }

    #[test]
    fn truncate_before_preserves_messages_at_and_after_index() {
        let (store, _tmp) = temp_store();
        store.append("session_p", &Message::user("a")).unwrap();
        store.append("session_p", &Message::assistant("b")).unwrap();
        store.append("session_p", &Message::user("c")).unwrap();
        store.append("session_p", &Message::assistant("d")).unwrap();

        store.truncate_before("session_p", 2).unwrap();

        let messages = store.read_all("session_p").unwrap();
        assert_eq!(messages.len(), 2);
        // Should be "c" and "d" — messages at index 2 and 3
        if let coderex_core::types::MessageContent::Text(s) = &messages[0].content {
            assert_eq!(s, "c");
        }
        if let coderex_core::types::MessageContent::Text(s) = &messages[1].content {
            assert_eq!(s, "d");
        }
    }

    #[test]
    fn read_all_skips_corrupted_lines() {
        let (store, _tmp) = temp_store();
        let path = store.session_path("session_corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(
            file,
            "{}",
            serde_json::to_string(&Message::user("first")).unwrap()
        )
        .unwrap();
        writeln!(file, "this is not json").unwrap();
        writeln!(
            file,
            "{}",
            serde_json::to_string(&Message::user("third")).unwrap()
        )
        .unwrap();
        drop(file);

        let messages = store.read_all("session_corrupt").unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, coderex_core::types::Role::User);
        assert_eq!(messages[1].role, coderex_core::types::Role::User);
    }

    #[test]
    fn truncate_before_skips_corrupted_lines() {
        let (store, _tmp) = temp_store();
        let path = store.session_path("session_truncate_corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(
            file,
            "{}",
            serde_json::to_string(&Message::user("a")).unwrap()
        )
        .unwrap();
        writeln!(file, "bad json").unwrap();
        writeln!(
            file,
            "{}",
            serde_json::to_string(&Message::user("c")).unwrap()
        )
        .unwrap();
        drop(file);

        store
            .truncate_before("session_truncate_corrupt", 1)
            .unwrap();
        let messages = store.read_all("session_truncate_corrupt").unwrap();
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn rewrite_produces_correct_transcript() {
        let (store, _tmp) = temp_store();
        let new_messages = vec![Message::user("rewritten"), Message::assistant("ok")];
        store.rewrite("session_r", &new_messages).unwrap();

        let messages = store.read_all("session_r").unwrap();
        assert_eq!(messages.len(), 2);
        if let coderex_core::types::MessageContent::Text(s) = &messages[0].content {
            assert_eq!(s, "rewritten");
        }
    }

    #[test]
    fn list_sessions_returns_all_ids() {
        let (store, _tmp) = temp_store();
        store.append("alpha", &Message::user("a")).unwrap();
        store.append("beta", &Message::user("b")).unwrap();
        store.append("gamma", &Message::user("c")).unwrap();

        let sessions = store.list_sessions().unwrap();
        assert_eq!(sessions, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn backup_and_restore_roundtrip() {
        let (store, _tmp) = temp_store();
        store.append("sess_bak", &Message::user("hello")).unwrap();
        store
            .append("sess_bak", &Message::assistant("world"))
            .unwrap();

        let backup_path = store.backup("sess_bak").unwrap();
        assert!(backup_path.exists());
        assert!(backup_path.to_string_lossy().ends_with(".jsonl.bak"));

        // Corrupt the original
        let orig_path = store.session_path("sess_bak");
        std::fs::write(&orig_path, "corrupted\n").unwrap();

        let restored = store.restore("sess_bak").unwrap().unwrap();
        assert_eq!(restored, orig_path);

        let messages = store.read_all("sess_bak").unwrap();
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn restore_missing_backup_returns_none() {
        let (store, _tmp) = temp_store();
        let result = store.restore("no_backup").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn prune_archives_oldest_sessions() {
        let (store, tmp) = temp_store();
        // Create 5 sessions
        for i in 0..5 {
            store
                .append(&format!("sess_{i}"), &Message::user(&format!("msg-{i}")))
                .unwrap();
            // Space out modification times so ordering is deterministic
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // Prune to keep only 3
        let result = store.prune(3).unwrap();
        assert_eq!(result.remaining, 3);
        assert_eq!(result.archived, 2);

        // Check archive was created
        let archive_dir = tmp.path().join("archives");
        assert!(archive_dir.exists());

        // Only 3 sessions should remain (not in archives dir)
        let sessions = store.list_sessions().unwrap();
        assert_eq!(sessions.len(), 3);
    }

    #[test]
    fn prune_noop_when_under_limit() {
        let (store, _tmp) = temp_store();
        store.append("sess_1", &Message::user("a")).unwrap();
        store.append("sess_2", &Message::user("b")).unwrap();

        let result = store.prune(10).unwrap();
        assert_eq!(result.remaining, 2);
        assert_eq!(result.archived, 0);
        assert_eq!(result.deleted, 0);
    }

    #[cfg(feature = "encryption")]
    #[test]
    fn encrypted_append_and_decrypt_roundtrip() {
        use crate::transcript_crypto::derive_cipher;

        let (store, _tmp) = temp_store();
        let key = crate::transcript_crypto::generate_master_key();
        let cipher = derive_cipher(&key, "enc_sess_1").unwrap();

        store
            .append_encrypted("enc_sess_1", &Message::user("secret hello"), &cipher)
            .unwrap();
        store
            .append_encrypted(
                "enc_sess_1",
                &Message::assistant("secret response"),
                &cipher,
            )
            .unwrap();

        let messages = store.read_all_decrypted("enc_sess_1", &cipher).unwrap();
        assert_eq!(messages.len(), 2);
        if let coderex_core::types::MessageContent::Text(s) = &messages[0].content {
            assert_eq!(s, "secret hello");
        } else {
            panic!("expected text");
        }
    }

    #[cfg(feature = "encryption")]
    #[test]
    fn wrong_key_cannot_decrypt() {
        use crate::transcript_crypto::derive_cipher;

        let (store, _tmp) = temp_store();
        let key1 = crate::transcript_crypto::generate_master_key();
        let key2 = crate::transcript_crypto::generate_master_key();
        let cipher1 = derive_cipher(&key1, "enc_sess_2").unwrap();
        let cipher2 = derive_cipher(&key2, "enc_sess_2").unwrap();

        store
            .append_encrypted("enc_sess_2", &Message::user("secret"), &cipher1)
            .unwrap();

        // Wrong key → decryption error
        let result = store.read_all_decrypted("enc_sess_2", &cipher2);
        assert!(result.is_err());
    }

    #[cfg(feature = "encryption")]
    #[test]
    fn encrypted_file_uses_enc_extension() {
        use crate::transcript_crypto::derive_cipher;

        let (store, tmp) = temp_store();
        let key = crate::transcript_crypto::generate_master_key();
        let cipher = derive_cipher(&key, "ext_check").unwrap();

        store
            .append_encrypted("ext_check", &Message::user("test"), &cipher)
            .unwrap();

        let enc_path = tmp.path().join("ext_check.jsonl.enc");
        assert!(enc_path.exists());
    }
}
