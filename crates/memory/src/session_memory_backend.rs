//! `SessionMemoryBackend` implementation over the disk-persistent
//! `SessionMemoryStore`.
//!
//! The memory document is stored as one `Architecture`-category fact per
//! update (same convention as `session_memory_agent.rs`): each save replaces
//! the "current document" notion by appending a newest fact, and load returns
//! the most recent one.

use async_trait::async_trait;
use chrono::Utc;
use coderex_core::session_memory::SessionMemoryBackend;
use std::sync::Arc;
use uuid::Uuid;

use crate::session_memory::{FactCategory, SessionFact, SessionMemoryStore};

/// Session-memory backend backed by [`SessionMemoryStore`] persistence.
#[derive(Debug)]
pub struct StoreSessionMemoryBackend {
    store: Arc<SessionMemoryStore>,
}

impl StoreSessionMemoryBackend {
    /// Create a backend over the given store.
    #[must_use]
    pub fn new(store: Arc<SessionMemoryStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl SessionMemoryBackend for StoreSessionMemoryBackend {
    async fn load_memory(&self, session_id: &str) -> Option<String> {
        let sid = Uuid::parse_str(session_id).ok()?;
        self.store
            .get_facts_by_category(sid, FactCategory::Architecture)
            .pop()
            .map(|f| f.fact)
    }

    async fn save_memory(&self, session_id: &str, content: &str) {
        let Ok(sid) = Uuid::parse_str(session_id) else {
            return;
        };
        let fact = SessionFact {
            id: Uuid::new_v4(),
            session_id: sid,
            fact: content.to_string(),
            category: FactCategory::Architecture,
            created_at: Utc::now(),
            confidence: 0.9,
        };
        self.store.extract_fact(sid, fact);
        // Persistence failure is non-fatal: the in-memory copy is still
        // authoritative for this session's lifetime.
        if let Err(e) = self.store.persist(sid) {
            tracing::warn!("SessionMemory: persist failed ({e}) — keeping in-memory copy");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (Arc<SessionMemoryStore>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (
            Arc::new(SessionMemoryStore::new(dir.path().join("session"))),
            dir,
        )
    }

    #[tokio::test]
    async fn save_then_load_roundtrip_via_trait() {
        let (store, _dir) = temp_store();
        let backend = StoreSessionMemoryBackend::new(store);
        let sid = Uuid::new_v4().to_string();

        backend
            .save_memory(&sid, "Current State: Auth middleware fixed.")
            .await;
        backend
            .save_memory(&sid, "Current State: Compaction wired.")
            .await;

        let loaded = backend.load_memory(&sid).await;
        assert_eq!(loaded.as_deref(), Some("Current State: Compaction wired."));
    }

    #[tokio::test]
    async fn load_missing_session_returns_none() {
        let (store, _dir) = temp_store();
        let backend = StoreSessionMemoryBackend::new(store);
        assert_eq!(backend.load_memory(&Uuid::new_v4().to_string()).await, None);
    }

    #[tokio::test]
    async fn invalid_session_id_is_ignored() {
        let (store, _dir) = temp_store();
        let backend = StoreSessionMemoryBackend::new(store);
        backend.save_memory("not-a-uuid", "content").await;
        assert_eq!(backend.load_memory("not-a-uuid").await, None);
    }
}
