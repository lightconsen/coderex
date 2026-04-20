//! Session memory update agent — AI-powered session fact extraction.

use crate::ai_workflow::MemoryWorkflowError;
use crate::session_memory::{FactCategory, SessionFact, SessionMemoryStore};
use chrono::Utc;
use coderex_core::api_client::ApiClient;
use coderex_core::prompts;
use uuid::Uuid;

/// Update session memory by calling AI and persisting the result.
///
/// # Arguments
/// * `api_client` — The API client for AI calls.
/// * `store` — The session memory store to write into.
/// * `session_id` — The session to update.
/// * `current_memory` — Existing memory text (empty string if none).
/// * `conversation_excerpt` — Recent conversation to analyze.
///
/// # Returns
/// The updated memory content.
pub async fn update_session_memory_ai(
    api_client: &dyn ApiClient,
    store: &SessionMemoryStore,
    session_id: Uuid,
    current_memory: &str,
    conversation_excerpt: &str,
) -> Result<String, MemoryWorkflowError> {
    let updated = if current_memory.is_empty() {
        prompts::create_session_memory(api_client, conversation_excerpt).await?
    } else {
        prompts::update_session_memory(api_client, current_memory, conversation_excerpt).await?
    };

    // Persist as a session fact
    let fact = SessionFact {
        id: Uuid::new_v4(),
        session_id,
        fact: updated.clone(),
        category: FactCategory::Architecture,
        created_at: Utc::now(),
        confidence: 0.9,
    };
    store.extract_fact(session_id, fact);
    store.persist(session_id)?;

    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai_workflow::tests::MockApi;
    use uuid::Uuid;

    #[tokio::test]
    async fn update_session_memory_ai_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionMemoryStore::new(tmp.path().to_path_buf());
        let api = MockApi::new("Current State: testing AI workflow.");
        let sid = Uuid::new_v4();

        let result = update_session_memory_ai(&api, &store, sid, "", "user: test conversation").await;
        assert!(result.is_ok());

        let facts = store.get_facts(sid);
        assert_eq!(facts.len(), 1);
    }
}
