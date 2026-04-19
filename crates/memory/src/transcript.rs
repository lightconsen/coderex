//! JSONL transcript store — append-only session transcripts.

use coderex_core::types::Message;
use std::sync::LazyLock;
use regex::Regex;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
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
static SKIP_FIRST_PROMPT_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""role"\s*:\s*"System""#).expect("invalid skip regex")
});

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
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let line = serde_json::to_string(message)?;
        writeln!(file, "{line}")?;
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
    pub fn rewrite(
        &self,
        session_id: &str,
        messages: &[Message],
    ) -> Result<(), TranscriptError> {
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
        store.append("session_2", &Message::assistant("second")).unwrap();
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
        store.append("session_3", &Message::system("you are a helper")).unwrap();
        store.append("session_3", &Message::user("hi")).unwrap();
        store.append("session_3", &Message::assistant("hello")).unwrap();

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
        store.append("session_t", &Message::assistant("second")).unwrap();
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
        writeln!(file, "{}", serde_json::to_string(&Message::user("first")).unwrap()).unwrap();
        writeln!(file, "this is not json").unwrap();
        writeln!(file, "{}", serde_json::to_string(&Message::user("third")).unwrap()).unwrap();
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
        writeln!(file, "{}", serde_json::to_string(&Message::user("a")).unwrap()).unwrap();
        writeln!(file, "bad json").unwrap();
        writeln!(file, "{}", serde_json::to_string(&Message::user("c")).unwrap()).unwrap();
        drop(file);

        store.truncate_before("session_truncate_corrupt", 1).unwrap();
        let messages = store.read_all("session_truncate_corrupt").unwrap();
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn rewrite_produces_correct_transcript() {
        let (store, _tmp) = temp_store();
        let new_messages = vec![
            Message::user("rewritten"),
            Message::assistant("ok"),
        ];
        store.rewrite("session_r", &new_messages).unwrap();

        let messages = store.read_all("session_r").unwrap();
        assert_eq!(messages.len(), 2);
        if let coderex_core::types::MessageContent::Text(s) = &messages[0].content {
            assert_eq!(s, "rewritten");
        }
    }
}
