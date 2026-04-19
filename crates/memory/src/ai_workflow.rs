//! AI-powered memory workflows.
//!
//! Bridges `coderex-core` prompt features with `coderex-memory` storage,
//! so session memory updates and long-term memory extraction are not just
//! AI-generated text but actually persisted.

use crate::long_term::LongTermStore;
use crate::session_memory::{FactCategory, SessionFact, SessionMemoryStore};
use chrono::Utc;
use coderex_core::api_client::ApiClient;
use coderex_core::prompts;
use uuid::Uuid;

/// Errors from AI memory workflows.
#[derive(Debug, thiserror::Error)]
pub enum MemoryWorkflowError {
    /// Orchestrator-level error.
    #[error("AI error: {0}")]
    Ai(#[from] coderex_core::orchestrator::OrchestratorError),
    /// Storage I/O error.
    #[error("storage error: {0}")]
    Storage(#[from] std::io::Error),
    /// Serialization error.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

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

/// Extract long-term memories from conversation and persist them.
///
/// # Arguments
/// * `api_client` — The API client for AI calls.
/// * `store` — The long-term memory store to write into.
/// * `conversation` — Conversation text to analyze.
///
/// # Returns
/// The topics that were written.
pub async fn extract_long_term_memories_ai(
    api_client: &dyn ApiClient,
    store: &LongTermStore,
    conversation: &str,
) -> Result<Vec<String>, MemoryWorkflowError> {
    let entries = prompts::extract_memories(api_client, conversation, "").await?;

    let mut written = Vec::new();
    for (topic, content) in entries {
        let safe_topic = topic.trim();
        if safe_topic.is_empty() {
            continue;
        }
        store.write(safe_topic, &content)?;
        written.push(safe_topic.to_string());
    }

    Ok(written)
}

/// Extract a single focused memory and persist it.
///
/// # Arguments
/// * `api_client` — The API client for AI calls.
/// * `store` — The long-term memory store to write into.
/// * `conversation` — Conversation text to analyze.
/// * `topic` — The specific topic to extract.
///
/// # Returns
/// Whether a memory was found and written.
pub async fn extract_single_memory_ai(
    api_client: &dyn ApiClient,
    store: &LongTermStore,
    conversation: &str,
    topic: &str,
) -> Result<bool, MemoryWorkflowError> {
    let Some(content) = prompts::extract_single_memory(api_client, conversation, topic).await? else {
        return Ok(false);
    };

    store.write(topic, &content)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use coderex_core::api_client::{ApiClient, ApiResponse, StreamEvent};
    use coderex_core::types::{ModelId, UsageSummary};
    use std::pin::Pin;
    use futures::Stream;

    #[derive(Clone)]
    struct MockApi { response_content: String }

    impl MockApi {
        fn new(content: &str) -> Self { Self { response_content: content.into() } }
    }

    #[async_trait::async_trait]
    impl ApiClient for MockApi {
        fn clone_box(&self) -> Box<dyn ApiClient> {
            Box::new(self.clone())
        }

        async fn create_message(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
        ) -> Result<ApiResponse, coderex_core::api_client::ApiError> {
            Ok(ApiResponse {
                content: self.response_content.clone(),
                usage: UsageSummary {
                    input_tokens: 100,
                    output_tokens: 50,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                },
                stop_reason: "end_turn".into(),
            })
        }

        async fn create_message_streaming(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
        ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
            Box::pin(futures::stream::empty())
        }
    }

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

    #[tokio::test]
    async fn extract_long_term_memories_ai_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LongTermStore::new(tmp.path().to_path_buf());
        let api = MockApi::new(
            r#"[{"file": "test_topic", "content": "test memory content"}]"#
        );

        let result = extract_long_term_memories_ai(&api, &store, "conversation text").await;
        assert!(result.is_ok());
        let written = result.unwrap();
        assert!(!written.is_empty());

        let entry = store.read("test_topic").unwrap();
        assert!(entry.is_some());
    }
}
