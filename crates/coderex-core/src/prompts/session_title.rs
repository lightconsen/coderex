//! Session title generation feature.
//!
//! Generates a concise 3-7 word sentence-case title for the current session
//! using a lightweight model query.

use crate::api_client::ApiClient;
use crate::orchestrator::{QueryResult, OrchestratorError, run_lightweight_query};

/// Generates a session title from the first user message.
///
/// # Arguments
/// * `api_client` — The API client to use.
/// * `user_message` — The first user message (or a summary of the session start).
///
/// # Returns
/// A short title string.
pub async fn generate_session_title(
    api_client: &dyn ApiClient,
    user_message: &str,
) -> Result<String, OrchestratorError> {
    let system = crate::prompts::SESSION_TITLE_SYSTEM;
    let user = crate::prompts::session_title_user_prompt(user_message);

    let result = run_lightweight_query(
        api_client,
        system,
        &user,
        128,
    )
    .await?;

    Ok(parse_title(&result))
}

/// Generates a session title from multiple conversation messages.
///
/// # Arguments
/// * `api_client` — The API client to use.
/// * `messages` — Conversation messages to summarize from.
///
/// # Returns
/// A short title string.
pub async fn generate_session_title_from_messages(
    api_client: &dyn ApiClient,
    messages: &[String],
) -> Result<String, OrchestratorError> {
    let conversation = messages.join("\n");
    generate_session_title(api_client, &conversation).await
}

/// Parse a title from the query result.
///
/// Tries JSON first: `{"title": "..."}`, then falls back to
/// taking the first 60 characters.
#[must_use]
pub fn parse_title(result: &QueryResult) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&result.content) {
        if let Some(title) = value.get("title").and_then(|v| v.as_str()) {
            return title.to_string();
        }
    }
    // Fallback: take first line or first 60 chars
    result.content.lines().next()
        .unwrap_or(&result.content)
        .chars().take(60)
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
        ) -> Result<ApiResponse, crate::api_client::ApiError> {
            Ok(ApiResponse {
                content: self.response_content.clone(),
                usage: UsageSummary {
                    input_tokens: 50,
                    output_tokens: 10,
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
    async fn parse_json_title() {
        let api = MockApi::new(r#"{"title": "Fix authentication bug"}"#);
        let title = generate_session_title(&api, "user asked about auth").await;
        assert_eq!(title.unwrap(), "Fix authentication bug");
    }

    #[tokio::test]
    async fn parse_text_fallback() {
        let api = MockApi::new("This is a plain text response that is too long");
        let title = generate_session_title(&api, "user asked").await;
        let title = title.unwrap();
        assert!(title.len() <= 60);
    }

    #[test]
    fn parse_title_json() {
        let result = QueryResult {
            content: r#"{"title": "Add logging"}"#.into(),
            input_tokens: 50, output_tokens: 10,
            cache_read_tokens: 0, cache_creation_tokens: 0,
        };
        assert_eq!(parse_title(&result), "Add logging");
    }

    #[test]
    fn parse_title_text_fallback() {
        let result = QueryResult {
            content: "Some plain text response".into(),
            input_tokens: 50, output_tokens: 10,
            cache_read_tokens: 0, cache_creation_tokens: 0,
        };
        let title = parse_title(&result);
        assert_eq!(title, "Some plain text response");
    }

    #[test]
    fn parse_title_truncates_long_text() {
        let long = "a".repeat(100);
        let result = QueryResult {
            content: long.clone(),
            input_tokens: 50, output_tokens: 10,
            cache_read_tokens: 0, cache_creation_tokens: 0,
        };
        assert_eq!(parse_title(&result).len(), 60);
    }
}
