//! Memory extraction feature.
//!
//! Extracts long-term memories from conversation that should be preserved
//! beyond the current session, writing them to the project memory system.

use crate::api_client::ApiClient;
use crate::orchestrator::{
    BackgroundFeatureKind, BackgroundFeatureRequest, OrchestratorError, run_background_feature,
    run_side_query,
};
use crate::tool_executor::ToolInfo;
use crate::query_profiles::ModelProfile;

/// Extracts long-term memories from recent conversation.
///
/// # Arguments
/// * `api_client` — The API client to use.
/// * `conversation` — Conversation text to analyze.
/// * `memory_directory` — Path to the project memory directory.
///
/// # Returns
/// A list of memory entries that should be preserved, each as (filename, content).
pub async fn extract_memories(
    api_client: &dyn ApiClient,
    conversation: &str,
    memory_directory: &str,
) -> Result<Vec<(String, String)>, OrchestratorError> {
    let system = crate::prompts::EXTRACT_MEMORIES_SYSTEM;
    let user = format!(
        "Analyze the following conversation and extract information worth preserving:\n\n\
         {conversation}\n\n\
         Write memory files to: {memory_directory}\n\n\
         Return a JSON array of objects with 'file' and 'content' fields:\n\
         [{{\"file\": \"user_preferences.md\", \"content\": \"...\"}}]"
    );

    let result = run_background_feature(
        api_client,
        BackgroundFeatureRequest {
            kind: BackgroundFeatureKind::MemoryExtraction,
            system_prompt: system.to_string(),
            user_prompt: user,
            parent_context: Some(conversation.to_string()),
            allowed_tools: Some(vec!["Write".into()]),
            max_tokens: 1024,
            writable: true,
        },
    )
    .await?;

    Ok(parse_memory_entries(&result.content))
}

/// Parse memory entries from the JSON response.
fn parse_memory_entries(content: &str) -> Vec<(String, String)> {
    let value: serde_json::Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    let Some(arr) = value.as_array() else { return Vec::new() };

    arr.iter()
        .filter_map(|entry| {
            let file = entry.get("file")?.as_str()?.to_string();
            let content = entry.get("content")?.as_str()?.to_string();
            Some((file, content))
        })
        .collect()
}

/// Extracts a single memory entry for a specific topic.
///
/// Convenience wrapper for focused extraction.
pub async fn extract_single_memory(
    api_client: &dyn ApiClient,
    conversation: &str,
    topic: &str,
) -> Result<Option<String>, OrchestratorError> {
    let system = crate::prompts::EXTRACT_MEMORIES_SYSTEM;
    let user = format!(
        "From this conversation, extract only the memory related to: {topic}\n\n\
         Conversation:\n{conversation}\n\n\
         Return just the memory content, or 'none' if nothing relevant."
    );

    let result = run_side_query(
        api_client,
        system,
        &user,
        &ModelProfile::Lightweight.resolve(),
        256,
    )
    .await?;

    let content = result.content.trim();
    if content.eq_ignore_ascii_case("none") || content.is_empty() {
        Ok(None)
    } else {
        Ok(Some(content.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_client::{ApiResponse, StreamEvent};
    use crate::types::{ModelId, UsageSummary};
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
            _tools: &[ToolInfo],
        ) -> Result<ApiResponse, crate::api_client::ApiError> {
            Ok(ApiResponse {
                content: self.response_content.clone(),
                tool_calls: Vec::new(),
                usage: UsageSummary {
                    input_tokens: 500,
                    output_tokens: 200,
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
            _tools: &[ToolInfo],
        ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
            Box::pin(futures::stream::empty())
        }
    }

    #[test]
    fn parse_memory_entries_from_json() {
        let json = r#"[
            {"file": "user_preferences.md", "content": "prefers dark mode"},
            {"file": "project_arch.md", "content": "uses orchestrator pattern"}
        ]"#;
        let entries = parse_memory_entries(json);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "user_preferences.md");
        assert_eq!(entries[1].0, "project_arch.md");
    }

    #[test]
    fn parse_memory_entries_invalid_json() {
        let entries = parse_memory_entries("not json");
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_memory_entries_not_array() {
        let entries = parse_memory_entries(r#"{"key": "value"}"#);
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn extract_memories_returns_entries() {
        let api = MockApi::new(
            r#"[{"file": "feedback_testing.md", "content": "user wants terse responses"}]"#
        );
        let memories = extract_memories(&api, "conversation here", "/memory").await;
        assert!(memories.is_ok());
        assert_eq!(memories.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn extract_single_memory_returns_some() {
        let api = MockApi::new("User prefers using the Explore agent for codebase searches.");
        let memory = extract_single_memory(&api, "conversation", "search preference").await;
        assert!(memory.is_ok());
        assert!(memory.unwrap().is_some());
    }

    #[tokio::test]
    async fn extract_single_memory_returns_none() {
        let api = MockApi::new("none");
        let memory = extract_single_memory(&api, "conversation", "irrelevant topic").await;
        assert!(memory.is_ok());
        assert!(memory.unwrap().is_none());
    }
}
