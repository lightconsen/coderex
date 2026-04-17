//! Compact summary feature.
//!
//! Generates a concise summary of the conversation for context compaction,
//! preserving key decisions, file paths, and current state of work.

use crate::api_client::ApiClient;
use crate::orchestrator::{OrchestratorError, run_side_query};
use crate::query_profiles::ModelProfile;

/// Generates a compact summary of recent conversation.
///
/// # Arguments
/// * `api_client` — The API client to use.
/// * `conversation` — The conversation text to summarize.
/// * `focus` — Optional focus instruction (e.g., "focus on the auth changes").
///
/// # Returns
/// A summary string suitable for compacting the conversation context.
pub async fn generate_compact_summary(
    api_client: &dyn ApiClient,
    conversation: &str,
    focus: Option<&str>,
) -> Result<String, OrchestratorError> {
    let system = crate::prompts::COMPACT_SUMMARY_SYSTEM;
    let focus_line = focus.unwrap_or("");
    let user = crate::prompts::compact_summary_user_prompt(focus_line);
    let full_user = format!("Conversation:\n{conversation}\n\n{user}");

    let result = run_side_query(
        api_client,
        system,
        &full_user,
        &ModelProfile::Main.resolve(),
        512,
    )
    .await?;

    Ok(result.content)
}

/// Generates a compact summary from a list of messages.
///
/// Convenience wrapper that joins messages before summarizing.
pub async fn generate_compact_summary_from_messages(
    api_client: &dyn ApiClient,
    messages: &[String],
    focus: Option<&str>,
) -> Result<String, OrchestratorError> {
    let conversation = messages.join("\n");
    generate_compact_summary(api_client, &conversation, focus).await
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
                usage: UsageSummary {
                    input_tokens: 200,
                    output_tokens: 80,
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
        ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
            Box::pin(futures::stream::empty())
        }
    }

    #[tokio::test]
    async fn compact_summary_returns_content() {
        let api = MockApi::new("Summary: Fixed auth middleware. Key files: engine.rs, api_client.rs.");
        let summary = generate_compact_summary(&api, "user: fix auth\nassistant: reading files...", Some("focus on auth")).await;
        assert!(summary.is_ok());
        assert!(!summary.unwrap().is_empty());
    }

    #[tokio::test]
    async fn compact_summary_from_messages() {
        let api = MockApi::new("Conversation summary here.");
        let messages = vec!["user: hello".into(), "assistant: hi".into()];
        let summary = generate_compact_summary_from_messages(&api, &messages, None).await;
        assert!(summary.is_ok());
    }
}
