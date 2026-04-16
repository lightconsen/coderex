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
        for line in reader.lines() {
            let line = line?;
            if line.is_empty() {
                continue;
            }
            let msg: Message = serde_json::from_str(&line)?;
            messages.push(msg);
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
        for line in reader.lines() {
            let line = line?;
            if line.is_empty() {
                continue;
            }
            if !skipped && SKIP_FIRST_PROMPT_PATTERN.is_match(&line) {
                skipped = true;
                continue;
            }
            let msg: Message = serde_json::from_str(&line)?;
            messages.push(msg);
        }
        Ok(messages)
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
}
