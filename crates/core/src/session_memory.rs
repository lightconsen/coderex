//! Session-memory bridge — structured state that survives compaction.
//!
//! The compaction summary is regenerated each time and capped in size, so
//! structured state (current work, key decisions, open questions, file
//! changes) has no durable carrier across compactions. This trait lets the
//! engine persist such state through an injectable backend without core
//! depending on the memory crate (`coderex-memory` depends on core, so the
//! implementation lives there and is wired by the binary).

use async_trait::async_trait;

/// Backend for persistent session memory that survives compaction.
///
/// The document format is backend-defined; the engine treats it as opaque
/// text and injects it (when non-empty) alongside the compaction summary
/// inside a `<session_memory>` section.
#[async_trait]
pub trait SessionMemoryBackend: Send + Sync {
    /// Load the current memory document for the session (`None` when absent).
    async fn load_memory(&self, session_id: &str) -> Option<String>;
    /// Persist an updated memory document for the session.
    async fn save_memory(&self, session_id: &str, content: &str);
}
