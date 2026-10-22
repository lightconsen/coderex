//! Compact summary feature.
//!
//! Generates a concise summary of the conversation for context compaction,
//! preserving key decisions, file paths, and current state of work.

use crate::api_client::ApiClient;
use crate::orchestrator::{OrchestratorError, run_side_query};
use crate::query_profiles::ModelProfile;

/// Generates a compact summary of recent conversation.
///
/// Tries the main model first; on failure retries with the lightweight
/// profile as a fallback (mirrors codex's compaction model fallback chain).
///
/// # Arguments
/// * `api_client` — The API client to use.
/// * `conversation` — The conversation text to summarize.
/// * `focus` — Optional focus instruction (e.g., "focus on the auth changes").
/// * `max_tokens` — Output budget for the summary.
///
/// # Returns
/// A summary string suitable for compacting the conversation context.
pub async fn generate_compact_summary(
    api_client: &dyn ApiClient,
    conversation: &str,
    focus: Option<&str>,
    max_tokens: u32,
) -> Result<String, OrchestratorError> {
    let system = crate::prompts::COMPACT_SUMMARY_SYSTEM;
    let focus_line = focus.unwrap_or("");
    let user = crate::prompts::compact_summary_user_prompt(focus_line);
    let full_user = format!("Conversation:\n{conversation}\n\n{user}");

    // Fallback chain: Main → Lightweight. A degraded-but-present summary
    // beats a failed compaction (which would fall through to rule-based
    // truncation and lose far more context).
    let profiles = [ModelProfile::Main, ModelProfile::Lightweight];
    let mut last_err = None;
    for profile in profiles {
        match run_side_query(
            api_client,
            system,
            &full_user,
            &profile.resolve_runtime(),
            max_tokens,
        )
        .await
        {
            Ok(result) => return Ok(result.content),
            Err(err) => {
                tracing::warn!(
                    "compact summary: profile {profile:?} failed ({err}); trying fallback"
                );
                last_err = Some(err);
            }
        }
    }

    Err(last_err
        .unwrap_or_else(|| OrchestratorError::Config("no summarization profile available".into())))
}

/// Generates a compact summary from a list of messages.
///
/// Convenience wrapper that joins messages before summarizing.
pub async fn generate_compact_summary_from_messages(
    api_client: &dyn ApiClient,
    messages: &[String],
    focus: Option<&str>,
    max_tokens: u32,
) -> Result<String, OrchestratorError> {
    let conversation = messages.join("\n");
    generate_compact_summary(api_client, &conversation, focus, max_tokens).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_client::{ApiResponse, StreamEvent};
    use crate::tool_executor::ToolInfo;
    use crate::types::{ModelId, UsageSummary};
    use futures::Stream;
    use std::pin::Pin;

    #[derive(Clone)]
    struct MockApi {
        response_content: String,
    }

    impl MockApi {
        fn new(content: &str) -> Self {
            Self {
                response_content: content.into(),
            }
        }
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
                    input_tokens: 200,
                    output_tokens: 80,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                },
                stop_reason: "end_turn".into(),
                thinking: None,
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

    #[tokio::test]
    async fn compact_summary_returns_content() {
        let api =
            MockApi::new("Summary: Fixed auth middleware. Key files: engine.rs, api_client.rs.");
        let summary = generate_compact_summary(
            &api,
            "user: fix auth\nassistant: reading files...",
            Some("focus on auth"),
            4096,
        )
        .await;
        assert!(summary.is_ok());
        assert!(!summary.unwrap().is_empty());
    }

    #[tokio::test]
    async fn compact_summary_from_messages() {
        let api = MockApi::new("Conversation summary here.");
        let messages = vec!["user: hello".into(), "assistant: hi".into()];
        let summary = generate_compact_summary_from_messages(&api, &messages, None, 4096).await;
        assert!(summary.is_ok());
    }

    /// Mock that fails on the main model but succeeds on the fallback.
    struct FailingMainMock;

    #[async_trait::async_trait]
    impl ApiClient for FailingMainMock {
        fn clone_box(&self) -> Box<dyn ApiClient> {
            Box::new(Self)
        }

        async fn create_message(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            model: &ModelId,
            _max_tokens: u32,
            _tools: &[ToolInfo],
        ) -> Result<ApiResponse, crate::api_client::ApiError> {
            // Fail when asked for the main model (first profile in the
            // fallback chain); succeed for anything else.
            if model.0.contains("sonnet") {
                return Err(crate::api_client::ApiError::RetryExhausted {
                    attempts: 1,
                    last_error_message: "main model down".into(),
                });
            }
            Ok(ApiResponse {
                content: "fallback summary".into(),
                tool_calls: Vec::new(),
                usage: UsageSummary {
                    input_tokens: 100,
                    output_tokens: 40,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                },
                stop_reason: "end_turn".into(),
                thinking: None,
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

    #[tokio::test]
    async fn compact_summary_falls_back_to_lightweight_profile() {
        let api = FailingMainMock;
        let summary = generate_compact_summary(&api, "conversation", None, 4096).await;
        assert!(summary.is_ok(), "fallback should recover: {summary:?}");
        assert_eq!(summary.unwrap(), "fallback summary");
    }
}
