//! Permission explanation feature.
//!
//! Generates a clear, concise explanation of the risks associated with a tool
//! call to help users make informed permission decisions.

use crate::api_client::ApiClient;
use crate::orchestrator::{OrchestratorError, run_side_query};
use crate::query_profiles::ModelProfile;

/// Explains the risks of a tool call in 1-2 sentences.
///
/// # Arguments
/// * `api_client` — The API client to use.
/// * `tool_name` — Name of the tool being called.
/// * `tool_input` — JSON input arguments for the tool.
///
/// # Returns
/// A brief risk explanation string.
pub async fn explain_permission(
    api_client: &dyn ApiClient,
    tool_name: &str,
    tool_input: &str,
) -> Result<String, OrchestratorError> {
    let system = crate::prompts::PERMISSION_EXPLAIN_SYSTEM;
    let user = crate::prompts::permission_explain_user_prompt(tool_name, tool_input);

    let result = run_side_query(
        api_client,
        system,
        &user,
        &ModelProfile::Main.resolve(),
        256,
    )
    .await?;

    Ok(result.content)
}

/// Explains the risks of a tool call with structured input.
///
/// Convenience wrapper that accepts a serde_json::Value for the input.
pub async fn explain_permission_from_value(
    api_client: &dyn ApiClient,
    tool_name: &str,
    tool_input: serde_json::Value,
) -> Result<String, OrchestratorError> {
    let input_str =
        serde_json::to_string_pretty(&tool_input).unwrap_or_else(|_| tool_input.to_string());
    explain_permission(api_client, tool_name, &input_str).await
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
                    input_tokens: 80,
                    output_tokens: 30,
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

    #[tokio::test]
    async fn explain_permission_returns_content() {
        let api = MockApi::new("This tool could delete important files.");
        let explanation = explain_permission(&api, "Write", "/etc/config").await;
        assert!(explanation.is_ok());
        assert!(!explanation.unwrap().is_empty());
    }

    #[tokio::test]
    async fn explain_perm_from_value() {
        let api = MockApi::new("Risk: modifies system configuration.");
        let input = serde_json::json!({"path": "/etc/config", "content": "new"});
        let explanation = explain_permission_from_value(&api, "Write", input).await;
        assert!(explanation.is_ok());
        assert!(!explanation.unwrap().is_empty());
    }

    #[tokio::test]
    async fn explain_permission_bash_command() {
        let api = MockApi::new("This command has destructive potential.");
        let explanation = explain_permission(&api, "Bash", "rm -rf /tmp/*").await;
        assert!(explanation.is_ok());
    }
}
