//! Thread-safe wrappers for memory stores using per-key locking.
//!
//! Each store uses a [`DashMap`] to maintain fine-grained locks per session
//! or topic key. Operations on different keys proceed concurrently without
//! blocking, while operations on the same key are serialized.

use dashmap::DashMap;
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::Arc;

use crate::session_index::SessionIndex;
use crate::transcript::{TranscriptError, TranscriptStore};
use crate::long_term::LongTermStore;
use coderex_core::types::Message;

/// Type alias for a thread-safe [`SessionIndex`].
///
/// `rusqlite::Connection` is `!Sync`, so the index must be wrapped in a
/// [`Mutex`]. Callers share this via `Arc`.
pub type SharedSessionIndex = Arc<Mutex<SessionIndex>>;

/// Thread-safe [`TranscriptStore`] with per-session locking.
///
/// Concurrent appends to different sessions proceed without blocking.
/// Concurrent appends to the *same* session are serialized.
#[derive(Debug, Clone)]
pub struct ConcurrentTranscriptStore {
    base_path: PathBuf,
    locks: Arc<DashMap<String, Arc<Mutex<()>>>>,
}

impl ConcurrentTranscriptStore {
    /// Create a new concurrent transcript store.
    #[must_use]
    pub fn new(base_path: PathBuf) -> Self {
        Self {
            base_path,
            locks: Arc::new(DashMap::new()),
        }
    }

    /// Get or create the lock for a session (atomic get-or-insert).
    fn get_lock(&self, key: &str) -> Arc<Mutex<()>> {
        self.locks
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .value()
            .clone()
    }

    /// Append a message to a session's transcript (thread-safe).
    pub fn append(&self, session_id: &str, message: &Message) -> Result<(), TranscriptError> {
        let lock = self.get_lock(session_id);
        let _guard = lock.lock();
        let store = TranscriptStore::new(self.base_path.clone());
        store.append(session_id, message)
    }

    /// Read all messages from a session's transcript (thread-safe).
    pub fn read_all(&self, session_id: &str) -> Result<Vec<Message>, TranscriptError> {
        let lock = self.get_lock(session_id);
        let _guard = lock.lock();
        let store = TranscriptStore::new(self.base_path.clone());
        store.read_all(session_id)
    }

    /// Write a compact boundary marker (thread-safe).
    pub fn write_compact_boundary(&self, session_id: &str) -> Result<(), TranscriptError> {
        let lock = self.get_lock(session_id);
        let _guard = lock.lock();
        let store = TranscriptStore::new(self.base_path.clone());
        store.write_compact_boundary(session_id)
    }

    /// Rewrite the transcript with new messages (thread-safe).
    pub fn rewrite(
        &self,
        session_id: &str,
        messages: &[Message],
    ) -> Result<(), TranscriptError> {
        let lock = self.get_lock(session_id);
        let _guard = lock.lock();
        let store = TranscriptStore::new(self.base_path.clone());
        store.rewrite(session_id, messages)
    }

    /// Truncate messages before the given index (thread-safe).
    pub fn truncate_before(
        &self,
        session_id: &str,
        keep_from_index: usize,
    ) -> Result<(), TranscriptError> {
        let lock = self.get_lock(session_id);
        let _guard = lock.lock();
        let store = TranscriptStore::new(self.base_path.clone());
        store.truncate_before(session_id, keep_from_index)
    }

    /// List all session IDs.
    pub fn list_sessions(&self) -> std::io::Result<Vec<String>> {
        let store = TranscriptStore::new(self.base_path.clone());
        store.list_sessions()
    }
}

/// Thread-safe [`LongTermStore`] with per-topic locking.
///
/// Concurrent reads/writes to different topics proceed without blocking.
/// Operations on the *same* topic are serialized.
#[derive(Debug, Clone)]
pub struct ConcurrentLongTermStore {
    base_path: PathBuf,
    locks: Arc<DashMap<String, Arc<Mutex<()>>>>,
}

impl ConcurrentLongTermStore {
    /// Create a new concurrent long-term store.
    #[must_use]
    pub fn new(base_path: PathBuf) -> Self {
        Self {
            base_path,
            locks: Arc::new(DashMap::new()),
        }
    }

    fn get_lock(&self, key: &str) -> Arc<Mutex<()>> {
        self.locks
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .value()
            .clone()
    }

    /// Read a memory topic (thread-safe).
    pub fn read(&self, topic: &str) -> std::io::Result<Option<crate::long_term::MemoryEntry>> {
        let lock = self.get_lock(topic);
        let _guard = lock.lock();
        let store = LongTermStore::new(self.base_path.clone());
        store.read(topic)
    }

    /// Write a memory topic (thread-safe).
    pub fn write(&self, topic: &str, content: &str) -> std::io::Result<()> {
        let lock = self.get_lock(topic);
        let _guard = lock.lock();
        let store = LongTermStore::new(self.base_path.clone());
        store.write(topic, content)
    }

    /// Delete a memory by topic (thread-safe).
    pub fn delete(&self, topic: &str) -> std::io::Result<()> {
        let lock = self.get_lock(topic);
        let _guard = lock.lock();
        let store = LongTermStore::new(self.base_path.clone());
        store.delete(topic)
    }

    /// Search memories by query string (thread-safe).
    pub fn search(&self, query: &str) -> std::io::Result<Vec<crate::long_term::MemoryEntry>> {
        let store = LongTermStore::new(self.base_path.clone());
        store.search(query)
    }

    /// List all memories (thread-safe).
    pub fn list_all(&self) -> std::io::Result<Vec<crate::long_term::MemoryEntry>> {
        let store = LongTermStore::new(self.base_path.clone());
        store.list_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coderex_core::types::Message;

    #[test]
    fn concurrent_append_same_session_no_corruption() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ConcurrentTranscriptStore::new(tmp.path().to_path_buf());
        // Pre-create directory and file to avoid any creation races
        let path = tmp.path().join("shared_session.jsonl");
        std::fs::write(&path, "").unwrap();

        let mut handles = Vec::new();
        for i in 0..20 {
            let s = store.clone();
            let h = std::thread::spawn(move || {
                s.append("shared_session", &Message::user(format!("msg-{i}")))
            });
            handles.push(h);
        }

        for h in handles {
            h.join().unwrap().unwrap();
        }

        let messages = store.read_all("shared_session").unwrap();
        assert_eq!(messages.len(), 20, "expected 20 messages but got {}", messages.len());
    }

    #[test]
    fn concurrent_append_different_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ConcurrentTranscriptStore::new(tmp.path().to_path_buf());

        let mut handles = Vec::new();
        for i in 0..10 {
            let s = store.clone();
            let h = std::thread::spawn(move || {
                s.append(&format!("session_{i}"), &Message::user("hello"))
            });
            handles.push(h);
        }

        for h in handles {
            h.join().unwrap().unwrap();
        }

        let sessions = store.list_sessions().unwrap();
        assert_eq!(sessions.len(), 10);
    }

    #[test]
    fn concurrent_long_term_read_write() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ConcurrentLongTermStore::new(tmp.path().to_path_buf());

        let mut handles = Vec::new();
        for i in 0..10 {
            let s = store.clone();
            let h = std::thread::spawn(move || {
                s.write(&format!("topic_{i}"), &format!("content {i}"))
            });
            handles.push(h);
        }

        for h in handles {
            h.join().unwrap().unwrap();
        }

        let all = store.list_all().unwrap();
        assert_eq!(all.len(), 10);
    }

    #[test]
    fn concurrent_write_same_topic_serialized() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ConcurrentLongTermStore::new(tmp.path().to_path_buf());
        store.write("shared", "initial").unwrap();

        let mut handles = Vec::new();
        for i in 0..10 {
            let s = store.clone();
            let h = std::thread::spawn(move || {
                s.write("shared", &format!("update-{i}"))
            });
            handles.push(h);
        }

        for h in handles {
            h.join().unwrap().unwrap();
        }

        let entry = store.read("shared").unwrap().unwrap();
        assert!(entry.content.starts_with("update-"));
    }
}
