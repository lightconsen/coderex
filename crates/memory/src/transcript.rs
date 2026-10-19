//! JSONL transcript store — append-only session transcripts.
//!
//! A transcript is `<base_path>/<session_id>.jsonl`, one [`TranscriptEntry`]
//! per line. An ordinary line is a bare [`Message`] — the format has not
//! changed — while compaction boundaries and session headers carry a `"type"`
//! tag. See [`crate::entry`] for the entry model and why it is not larger.
//!
//! # What this store writes, and what it reads
//!
//! This store writes **bare messages only**: [`TranscriptStore::append`] and
//! [`TranscriptStore::rewrite`] serialize a [`Message`] directly. That is the
//! format production persists — the engine represents a compaction boundary as
//! a system message carrying [`COMPACT_BOUNDARY_MARKER`](crate::entry::COMPACT_BOUNDARY_MARKER),
//! and it is written unchanged. There is deliberately no API here for writing
//! tagged entries: production does not call one, and inventing a writer with
//! no caller would describe a format nothing writes.
//!
//! The read side is deliberately wider than the write side.
//! [`TranscriptStore::read_entries`] accepts both bare messages and tagged
//! lines, and classifies every line through
//! [`TranscriptEntry::from_message`](crate::entry::TranscriptEntry::from_message)
//! (so a boundary is a boundary whether it was written as a bare system
//! message or as a tagged entry). [`TranscriptStore::read_all`] then turns
//! those entries back into messages. Classification on read is the single
//! documented path for both forms.
//!
//! # The session-id sanitization rule
//!
//! The session id becomes the transcript's file name, so it is the one part of
//! the path that comes from outside. [`sanitize_session_id`] is the rule, and
//! every method here applies it before it touches the filesystem:
//!
//! > A session id must be 1 to [`MAX_SESSION_ID_LEN`] characters long and every
//! > character must be an ASCII letter, digit, `-`, or `_`. Anything else —
//! > empty, a path separator, a `.` or `..`, a control character, a character
//! > outside ASCII — is refused.
//!
//! A UUID, which is what [`coderex_core::state::SessionState`] generates,
//! satisfies this as-is. The rule is a refusal, not a rewrite: an id that does
//! not satisfy it is reported rather than mangled into one that does, because
//! two ids that sanitize to the same name would be the same transcript.

use chrono::{DateTime, Utc};
use coderex_core::types::{Message, Role};
use flate2::Compression;
use flate2::write::GzEncoder;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

use crate::entry::TranscriptEntry;

/// Longest a session id may be.
pub const MAX_SESSION_ID_LEN: usize = 128;

/// Errors from transcript operations.
#[derive(Debug, Error)]
pub enum TranscriptError {
    /// An I/O error occurred.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A JSON parsing error occurred.
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
    /// A session id cannot be a transcript file name.
    #[error("'{0}' is not a usable session id")]
    InvalidSessionId(String),
}

impl TranscriptError {
    /// The same failure as an [`std::io::Error`].
    ///
    /// The archive, backup, restore and prune methods report in `io::Result`
    /// because they are file operations; a bad session id reaches them through
    /// this rather than being unwrapped.
    fn into_io(self) -> std::io::Error {
        match self {
            Self::Io(e) => e,
            other => std::io::Error::new(std::io::ErrorKind::InvalidInput, other.to_string()),
        }
    }
}

/// The session-id sanitization rule: check, and return the id unchanged.
///
/// See the module docs for the rule in full. This refuses an id that cannot be
/// a single file name; it does not rewrite one.
///
/// # Errors
///
/// [`TranscriptError::InvalidSessionId`] when the id cannot be a file name.
pub fn sanitize_session_id(session_id: &str) -> Result<(), TranscriptError> {
    let refuse = || TranscriptError::InvalidSessionId(session_id.to_string());

    if session_id.is_empty() || session_id.len() > MAX_SESSION_ID_LEN {
        return Err(refuse());
    }
    if !session_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(refuse());
    }
    Ok(())
}

/// Append-only JSONL transcript store.
///
/// Stored at `<base_path>/<session_id>.jsonl`.
/// Each line is a JSON-serialized [`TranscriptEntry`].
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

    /// Get the file path for a session, applying the session-id rule.
    fn session_path(&self, session_id: &str) -> Result<PathBuf, TranscriptError> {
        sanitize_session_id(session_id)?;
        Ok(self.base_path.join(format!("{session_id}.jsonl")))
    }

    /// Append a message to a session's transcript.
    ///
    /// The line is the bare message, exactly what every existing transcript
    /// contains. Compaction boundaries are written through this method as the
    /// system messages the engine produces.
    pub fn append(&self, session_id: &str, message: &Message) -> Result<(), TranscriptError> {
        let path = self.session_path(session_id)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        let line = serde_json::to_string(message)?;
        writeln!(file, "{line}")?;
        file.sync_all()?;
        Ok(())
    }

    /// Read every entry from a session's transcript, classifying each line.
    ///
    /// Both forms are accepted: a bare [`Message`] line is classified through
    /// [`TranscriptEntry::from_message`] (so a compaction boundary written as
    /// a system message reads as [`crate::entry::EntryKind::CompactBoundary`]),
    /// and a tagged line is decoded as its typed entry. This is the single
    /// classification path; there is no separate reader for tagged files.
    ///
    /// Returns an empty vec if the file does not exist. Corrupted lines are
    /// skipped with a warning rather than failing the read.
    pub fn read_entries(&self, session_id: &str) -> Result<Vec<TranscriptEntry>, TranscriptError> {
        let path = self.session_path(session_id)?;
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = std::fs::File::open(&path)?;
        let reader = BufReader::new(file);
        let mut entries = Vec::new();
        let mut line_no: usize = 0;
        for line in reader.lines() {
            line_no += 1;
            let line = line?;
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<TranscriptEntry>(&line) {
                // A message is classified, so a boundary written as a plain
                // system message reads as a boundary here.
                Ok(TranscriptEntry::Message(message)) => {
                    entries.push(TranscriptEntry::from_message(message));
                }
                Ok(entry) => entries.push(entry),
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
        Ok(entries)
    }

    /// Read all messages from a session's transcript.
    ///
    /// Returns an empty vec if the file does not exist. A session header is not
    /// a message and is not returned; a compaction boundary comes back as the
    /// system message the engine uses for it.
    pub fn read_all(&self, session_id: &str) -> Result<Vec<Message>, TranscriptError> {
        Ok(self
            .read_entries(session_id)?
            .iter()
            .filter_map(TranscriptEntry::to_message)
            .collect())
    }

    /// Read all messages, skipping the first system prompt.
    ///
    /// Useful for session replay where the initial system prompt
    /// should not be re-displayed.
    pub fn read_all_skip_first_prompt(
        &self,
        session_id: &str,
    ) -> Result<Vec<Message>, TranscriptError> {
        let mut skipped = false;
        let mut messages = Vec::new();
        for entry in self.read_entries(session_id)? {
            let Some(message) = entry.to_message() else {
                continue;
            };
            if !skipped && matches!(message.role, Role::System) {
                skipped = true;
                continue;
            }
            messages.push(message);
        }
        Ok(messages)
    }

    /// Rewrite the transcript, keeping only messages from a given index onward.
    ///
    /// Uses a temp file + atomic rename for crash safety.
    pub fn truncate_before(
        &self,
        session_id: &str,
        keep_from_index: usize,
    ) -> Result<(), TranscriptError> {
        let path = self.session_path(session_id)?;
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
    ///
    /// Every line written is a bare message, exactly as before — a compaction
    /// boundary that is a system message in `messages` goes back out as the
    /// same system message. [`Self::read_entries`] classifies on the way in,
    /// so a boundary is a boundary when it is read.
    pub fn rewrite(&self, session_id: &str, messages: &[Message]) -> Result<(), TranscriptError> {
        let path = self.session_path(session_id)?;
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
        let path = self
            .session_path(session_id)
            .map_err(TranscriptError::into_io)?;
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
        let source = self
            .session_path(session_id)
            .map_err(TranscriptError::into_io)?;
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
                let Ok(path) = self.session_path(id) else {
                    continue;
                };
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
        let source = self
            .session_path(session_id)
            .map_err(TranscriptError::into_io)?;
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
        let source = self
            .session_path(session_id)
            .map_err(TranscriptError::into_io)?;
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
        let path = self.encrypted_session_path(session_id)?;
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
        let path = self.encrypted_session_path(session_id)?;
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
    fn encrypted_session_path(&self, session_id: &str) -> Result<PathBuf, TranscriptError> {
        sanitize_session_id(session_id)?;
        Ok(self.base_path.join(format!("{session_id}.jsonl.enc")))
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
            images: Vec::new(),
        });
        store.append("session_rt", &original).unwrap();

        let messages = store.read_all("session_rt").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, coderex_core::types::Role::Tool);
    }

    #[test]
    fn a_boundary_message_round_trips_through_append_and_read() {
        let (store, _tmp) = temp_store();
        store.append("session_b", &Message::user("hello")).unwrap();
        // The engine's boundary is a plain system message; `append` writes it
        // bare and `read_entries` classifies it on the way back in.
        store
            .append(
                "session_b",
                &Message::system(format!(
                    "{}\nContext was compacted.\n{}",
                    crate::entry::COMPACT_BOUNDARY_MARKER,
                    crate::entry::COMPACT_BOUNDARY_CLOSE
                )),
            )
            .unwrap();

        let entries = store.read_entries("session_b").unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].kind(), crate::entry::EntryKind::CompactBoundary);

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
        let path = store.session_path("session_corrupt").unwrap();
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
        let path = store.session_path("session_truncate_corrupt").unwrap();
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
        let orig_path = store.session_path("sess_bak").unwrap();
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

    // ─── The session-id rule ───────────────────────────────────────────────

    #[test]
    fn a_uuid_session_id_is_accepted() {
        let id = uuid::Uuid::new_v4().to_string();
        assert!(sanitize_session_id(&id).is_ok());
        assert!(sanitize_session_id("sess_1").is_ok());
        assert!(sanitize_session_id("ABC-def_123").is_ok());
    }

    #[test]
    fn a_session_id_that_is_not_a_file_name_is_refused() {
        let hostile = [
            "",           // empty
            "..",         // parent directory
            ".",          // current directory
            "a/b",        // path separator
            "a\\b",       // other path separator
            "../escape",  // traversal
            "with space", // whitespace
            "nul\0byte",  // control character
            "new\nline",  // newline
            "café",       // non-ASCII
            "dots.jsonl", // would change the extension
            "semi;colon", // shell metacharacter
        ];
        for id in hostile {
            assert!(
                sanitize_session_id(id).is_err(),
                "{id:?} should be refused as a session id"
            );
        }
    }

    #[test]
    fn an_over_long_session_id_is_refused() {
        let long = "a".repeat(MAX_SESSION_ID_LEN);
        assert!(sanitize_session_id(&long).is_ok());
        let too_long = "a".repeat(MAX_SESSION_ID_LEN + 1);
        assert!(sanitize_session_id(&too_long).is_err());
    }

    #[test]
    fn a_hostile_session_id_never_touches_the_filesystem() {
        let (store, tmp) = temp_store();
        let escape = tmp.path().join("escape.jsonl");

        // Every method that takes a session id must refuse, not write.
        assert!(store.append("../escape", &Message::user("x")).is_err());
        assert!(store.read_all("../escape").is_err());
        assert!(store.read_all_skip_first_prompt("../escape").is_err());
        assert!(store.read_entries("../escape").is_err());
        assert!(store.rewrite("../escape", &[Message::user("x")]).is_err());
        assert!(store.truncate_before("../escape", 0).is_err());
        assert!(store.session_modified_at("../escape").is_err());
        assert!(store.backup("../escape").is_err());
        assert!(store.restore("../escape").is_err());
        assert!(store.archive_session("../escape", tmp.path()).is_err());

        assert!(!escape.exists(), "a refused id must not create a file");
    }

    // ─── Entry model ───────────────────────────────────────────────────────

    #[test]
    fn a_boundary_written_as_a_message_reads_back_as_a_boundary_entry() {
        let (store, _tmp) = temp_store();
        store
            .append("sess_entries", &Message::user("hello"))
            .unwrap();
        store
            .append(
                "sess_entries",
                &Message::system(format!(
                    "{}\nremoved\n{}",
                    crate::entry::COMPACT_BOUNDARY_MARKER,
                    crate::entry::COMPACT_BOUNDARY_CLOSE
                )),
            )
            .unwrap();

        let entries = store.read_entries("sess_entries").unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind(), crate::entry::EntryKind::UserText);
        assert_eq!(entries[1].kind(), crate::entry::EntryKind::CompactBoundary);

        // And read_all still hands back the message a message-only reader
        // expects.
        let messages = store.read_all("sess_entries").unwrap();
        assert_eq!(messages.len(), 2);
        assert!(matches!(
            messages[1].role,
            coderex_core::types::Role::System
        ));
    }

    #[test]
    fn a_bare_boundary_line_is_classified_on_read() {
        let (store, _tmp) = temp_store();
        let path = store.session_path("sess_bare_boundary").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // What the engine writes today: a plain system message.
        std::fs::write(
            &path,
            format!(
                "{}\n",
                serde_json::to_string(&Message::system(
                    "<coderex_compact_boundary>\nremoved\n</coderex_compact_boundary>"
                ))
                .unwrap()
            ),
        )
        .unwrap();

        let entries = store.read_entries("sess_bare_boundary").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind(), crate::entry::EntryKind::CompactBoundary);
    }

    #[test]
    fn raw_and_tagged_boundary_lines_are_classified_the_same_way() {
        let (store, _tmp) = temp_store();
        let path = store.session_path("sess_both_forms").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let bare = serde_json::to_string(&Message::system(format!(
            "{}\nwritten bare\n{}",
            crate::entry::COMPACT_BOUNDARY_MARKER,
            crate::entry::COMPACT_BOUNDARY_CLOSE
        )))
        .unwrap();
        let tagged = serde_json::to_string(&crate::entry::TranscriptEntry::compact_boundary(
            "written tagged",
        ))
        .unwrap();
        std::fs::write(&path, format!("{bare}\n{tagged}\n")).unwrap();

        let entries = store.read_entries("sess_both_forms").unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind(), crate::entry::EntryKind::CompactBoundary);
        assert_eq!(entries[1].kind(), crate::entry::EntryKind::CompactBoundary);

        // Both forms also survive the message-only view.
        let messages = store.read_all("sess_both_forms").unwrap();
        assert_eq!(messages.len(), 2);
        for message in messages {
            assert!(matches!(message.role, coderex_core::types::Role::System));
        }
    }

    #[test]
    fn a_session_header_is_not_returned_as_a_message() {
        let (store, _tmp) = temp_store();
        // Tagged lines are read, even though this store does not write them.
        // Write one by hand to prove the read side accepts the form.
        let path = store.session_path("sess_header").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                "{}\n",
                serde_json::to_string(&crate::entry::TranscriptEntry::session_meta(
                    "sess_header",
                    "/tmp/p",
                    None
                ))
                .unwrap()
            ),
        )
        .unwrap();
        store
            .append("sess_header", &Message::user("hello"))
            .unwrap();

        let entries = store.read_entries("sess_header").unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind(), crate::entry::EntryKind::SessionMeta);

        // The header is metadata; a message reader sees only the message.
        let messages = store.read_all("sess_header").unwrap();
        assert_eq!(messages.len(), 1);
    }
}
