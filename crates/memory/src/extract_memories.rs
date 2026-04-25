//! Long-term memory extraction agent — AI-powered memory extraction.

use crate::ai_workflow::MemoryWorkflowError;
use crate::long_term::LongTermStore;
use coderex_core::api_client::ApiClient;
use coderex_core::prompts;

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
    let Some(content) = prompts::extract_single_memory(api_client, conversation, topic).await?
    else {
        return Ok(false);
    };

    store.write(topic, &content)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai_workflow::tests::MockApi;

    #[tokio::test]
    async fn extract_long_term_memories_ai_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LongTermStore::new(tmp.path().to_path_buf());
        let api = MockApi::new(r#"[{"file": "test_topic", "content": "test memory content"}]"#);

        let result = extract_long_term_memories_ai(&api, &store, "conversation text").await;
        assert!(result.is_ok());
        let written = result.unwrap();
        assert!(!written.is_empty());

        let entry = store.read("test_topic").unwrap();
        assert!(entry.is_some());
    }
}
