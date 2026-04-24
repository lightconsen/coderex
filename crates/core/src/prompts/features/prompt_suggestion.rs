//! Prompt suggestion feature.
//!
//! Suggests follow-up prompts or next actions based on the current conversation
//! state, helping users stay productive without needing to think about what to ask next.

use crate::api_client::ApiClient;
use crate::orchestrator::{
    BackgroundFeatureKind, BackgroundFeatureRequest, OrchestratorError, run_background_feature,
};

/// Suggests follow-up prompts based on the conversation state.
///
/// # Arguments
/// * `api_client` — The API client to use.
/// * `conversation_summary` — Summary of recent conversation.
/// * `max_suggestions` — Maximum number of suggestions to return.
///
/// # Returns
/// A list of suggested prompts as strings.
pub async fn suggest_prompts(
    api_client: &dyn ApiClient,
    conversation_summary: &str,
    max_suggestions: usize,
) -> Result<Vec<String>, OrchestratorError> {
    let system = "You are a helpful AI assistant. Suggest 2-4 concise follow-up prompts \
                  the user might want to ask next. Return ONLY a JSON array of strings.";
    let user = format!(
        "Based on this conversation, suggest up to {max_suggestions} follow-up prompts:\n\n\
         {conversation_summary}\n\n\
         Return a JSON array like: [\"prompt 1\", \"prompt 2\"]"
    );

    let result = run_background_feature(
        api_client,
        BackgroundFeatureRequest {
            kind: BackgroundFeatureKind::PromptSuggestion,
            system_prompt: system.to_string(),
            user_prompt: user,
            parent_context: Some(conversation_summary.to_string()),
            allowed_tools: None,
            max_tokens: 256,
            writable: false,
        },
    )
    .await?;

    Ok(parse_string_array(&result.content))
}

/// Suggests the next best action based on the current task state.
///
/// # Arguments
/// * `api_client` — The API client to use.
/// * `current_task` — Description of the current task or goal.
/// * `completed_steps` — Steps already completed.
///
/// # Returns
/// A suggestion for the next action to take.
pub async fn suggest_next_action(
    api_client: &dyn ApiClient,
    current_task: &str,
    completed_steps: &[String],
) -> Result<String, OrchestratorError> {
    let system = "You are a task planning assistant. Suggest the most logical next step.";
    let steps_text = completed_steps
        .iter()
        .enumerate()
        .map(|(i, s)| format!("{i}. {s}"))
        .collect::<Vec<_>>()
        .join("\n");
    let user = format!(
        "Task: {current_task}\n\nCompleted steps:\n{steps_text}\n\n\
         What is the next step? Answer in 1-2 sentences."
    );

    let result = run_background_feature(
        api_client,
        BackgroundFeatureRequest {
            kind: BackgroundFeatureKind::NextActionSuggestion,
            system_prompt: system.to_string(),
            user_prompt: user,
            parent_context: Some(format!("Task: {current_task}")),
            allowed_tools: None,
            max_tokens: 128,
            writable: false,
        },
    )
    .await?;

    Ok(result.content)
}

/// Parse a JSON array of strings from content.
fn parse_string_array(content: &str) -> Vec<String> {
    let value: serde_json::Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(_) => {
            // Fallback: treat each non-empty line as a suggestion
            return content.lines()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect();
        }
    };

    let Some(arr) = value.as_array() else { return Vec::new() };

    arr.iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect()
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
                    input_tokens: 100,
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

    #[test]
    fn parse_string_array_from_json() {
        let arr = parse_string_array(r#"["Suggest A", "Suggest B"]"#);
        assert_eq!(arr, vec!["Suggest A", "Suggest B"]);
    }

    #[test]
    fn parse_string_array_fallback_to_lines() {
        let arr = parse_string_array("Suggest A\nSuggest B\nSuggest C");
        assert_eq!(arr.len(), 3);
    }

    #[test]
    fn parse_string_array_invalid() {
        let arr = parse_string_array(r#"{"not": "an array"}"#);
        assert!(arr.is_empty());
    }

    #[tokio::test]
    async fn suggest_prompts_returns_list() {
        let api = MockApi::new(r#"["Fix the test", "Add docs"]"#);
        let prompts = suggest_prompts(&api, "Working on auth fix", 3).await;
        assert!(prompts.is_ok());
        assert_eq!(prompts.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn suggest_next_action_returns_suggestion() {
        let api = MockApi::new("Now write unit tests for the new function.");
        let steps = vec!["Implemented the function".into(), "Added type signatures".into()];
        let next = suggest_next_action(&api, "Implement foo", &steps).await;
        assert!(next.is_ok());
    }
}
