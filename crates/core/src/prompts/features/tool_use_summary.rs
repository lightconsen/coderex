//! Tool-use summary feature.
//!
//! Produces a one-line, human-readable label for a tool call, e.g.
//! "Reading src/engine.rs" instead of the fixed per-tool activity label.
//! Used by the `tool_use_summaries` flag; callers must treat failure as
//! advisory and fall back to [`crate::types::ToolCall::activity_summary`].

use crate::api_client::ApiClient;
use crate::orchestrator::{OrchestratorError, run_lightweight_query};

/// System prompt for tool-use summaries.
pub const TOOL_USE_SUMMARY_SYSTEM: &str = "You label a coding agent's tool calls for a live activity feed. \
     Describe what the tool call is about to do in at most 8 words, in present \
     continuous form (for example: \"Reading src/engine.rs\"). Return only the \
     label: no quotes, no markdown, no trailing period, no explanation.";

/// Maximum number of input characters shown to the model.
///
/// A `Write` call can carry an entire file; the label needs to know what is
/// being written, not every byte of it.
const MAX_INPUT_CHARS: usize = 2_000;

/// Maximum characters in a returned summary.
pub const MAX_SUMMARY_CHARS: usize = 80;

/// Build the user prompt for a tool-use summary.
#[must_use]
pub fn tool_use_summary_user_prompt(name: &str, input: &serde_json::Value) -> String {
    let mut input = input.to_string();
    if input.chars().count() > MAX_INPUT_CHARS {
        input = input.chars().take(MAX_INPUT_CHARS).collect::<String>() + "…";
    }
    format!("Tool: {name}\nInput: {input}\n\nWrite the one-line label.")
}

/// Generate a one-line summary for a tool call.
///
/// # Errors
///
/// Returns [`OrchestratorError`] when the lightweight model query fails. The
/// caller is expected to fall back to a deterministic label rather than fail
/// the turn.
pub async fn generate_tool_use_summary(
    api_client: &dyn ApiClient,
    name: &str,
    input: &serde_json::Value,
) -> Result<String, OrchestratorError> {
    let user = tool_use_summary_user_prompt(name, input);
    let result = run_lightweight_query(api_client, TOOL_USE_SUMMARY_SYSTEM, &user, 64).await?;
    Ok(sanitize_tool_use_summary(&result.content)
        .unwrap_or_else(|| crate::types::ToolCall::summarize_activity(name, input)))
}

/// Reduce a model response to a single display-safe line.
///
/// Returns `None` when the response has no non-empty line; the caller then
/// uses the deterministic activity summary.
#[must_use]
pub fn sanitize_tool_use_summary(raw: &str) -> Option<String> {
    let line = raw.lines().map(str::trim).find(|line| !line.is_empty())?;
    let line = line
        .trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .trim();
    if line.is_empty() {
        return None;
    }

    let mut summary: String = line.chars().take(MAX_SUMMARY_CHARS).collect();
    if line.chars().count() > MAX_SUMMARY_CHARS {
        summary.push('…');
    }
    Some(summary)
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
                    input_tokens: 50,
                    output_tokens: 10,
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
    async fn generates_a_one_line_summary() {
        let api = MockApi::new("\"Reading src/engine.rs\"\n");
        let summary = generate_tool_use_summary(
            &api,
            "Read",
            &serde_json::json!({"file_path": "src/engine.rs"}),
        )
        .await
        .unwrap();
        assert_eq!(summary, "Reading src/engine.rs");
    }

    #[tokio::test]
    async fn empty_response_falls_back_to_activity_summary() {
        let api = MockApi::new("   \n");
        let input = serde_json::json!({"file_path": "src/engine.rs"});
        let summary = generate_tool_use_summary(&api, "Read", &input)
            .await
            .unwrap();
        assert_eq!(
            summary,
            crate::types::ToolCall::summarize_activity("Read", &input)
        );
    }

    #[test]
    fn sanitize_strips_quotes_and_keeps_first_line() {
        assert_eq!(
            sanitize_tool_use_summary("`Searching for TODO`\nignored"),
            Some("Searching for TODO".into())
        );
    }

    #[test]
    fn sanitize_truncates_long_lines() {
        let long = "a".repeat(200);
        let summary = sanitize_tool_use_summary(&long).unwrap();
        assert_eq!(summary.chars().count(), MAX_SUMMARY_CHARS + 1);
        assert!(summary.ends_with('…'));
    }

    #[test]
    fn sanitize_rejects_blank_output() {
        assert!(sanitize_tool_use_summary("\n  \n").is_none());
    }
}
