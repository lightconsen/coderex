//! Session memory update feature.
//!
//! Updates the session memory file with new information from the conversation
//! using an AI agent to maintain high-density, actionable technical content.

use crate::api_client::ApiClient;
use crate::orchestrator::{
    BackgroundFeatureKind, BackgroundFeatureRequest, OrchestratorError, run_background_feature,
};

/// Updates session memory based on recent conversation.
///
/// # Arguments
/// * `api_client` — The API client to use.
/// * `current_memory` — Current contents of the session memory file.
/// * `conversation_excerpt` — Recent conversation to extract insights from.
///
/// # Returns
/// The updated memory file content.
pub async fn update_session_memory(
    api_client: &dyn ApiClient,
    current_memory: &str,
    conversation_excerpt: &str,
) -> Result<String, OrchestratorError> {
    let system = crate::prompts::SESSION_MEMORY_UPDATE_INSTRUCTION;
    let user = format!(
        "Current memory file contents:\n```\n{current_memory}\n```\n\n\
         Recent conversation:\n{conversation_excerpt}\n\n\
         Update the memory file above. Return the COMPLETE updated content."
    );

    let result = run_background_feature(
        api_client,
        BackgroundFeatureRequest {
            kind: BackgroundFeatureKind::SessionMemoryUpdate,
            system_prompt: system.to_string(),
            user_prompt: user,
            parent_context: Some(conversation_excerpt.to_string()),
            allowed_tools: Some(vec!["Edit".into()]),
            max_tokens: 1024,
            writable: true,
        },
    )
    .await?;

    Ok(result.content)
}

/// Creates a new session memory entry from scratch.
///
/// Used when no memory file exists yet.
pub async fn create_session_memory(
    api_client: &dyn ApiClient,
    conversation_excerpt: &str,
) -> Result<String, OrchestratorError> {
    let system = crate::prompts::SESSION_MEMORY_UPDATE_INSTRUCTION;
    let user = format!(
        "Create a new session memory file based on this conversation:\n\n\
         {conversation_excerpt}\n\n\
         Include sections: Current State, Key Decisions, Open Questions, and File Changes."
    );

    let result = run_background_feature(
        api_client,
        BackgroundFeatureRequest {
            kind: BackgroundFeatureKind::SessionMemoryUpdate,
            system_prompt: system.to_string(),
            user_prompt: user,
            parent_context: Some(conversation_excerpt.to_string()),
            allowed_tools: Some(vec!["Edit".into()]),
            max_tokens: 512,
            writable: true,
        },
    )
    .await?;

    Ok(result.content)
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
        ) -> Result<ApiResponse, crate::api_client::ApiError> {
            Ok(ApiResponse {
                content: self.response_content.clone(),
                tool_calls: Vec::new(),
                usage: UsageSummary {
                    input_tokens: 300,
                    output_tokens: 150,
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
    async fn update_session_memory_returns_updated_content() {
        let api = MockApi::new("---\nname: session\nCurrent State: Auth middleware fixed.\n");
        let current = "---\nname: session\nCurrent State: Starting.\n";
        let updated = update_session_memory(&api, current, "user: fixed auth bug in middleware.rs").await;
        assert!(updated.is_ok());
        assert!(!updated.unwrap().is_empty());
    }

    #[tokio::test]
    async fn create_session_memory_returns_initial_content() {
        let api = MockApi::new("---\nname: session\nCurrent State: New session.\n");
        let result = create_session_memory(&api, "user: start new feature").await;
        assert!(result.is_ok());
    }
}
