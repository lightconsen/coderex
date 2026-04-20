//! Agent summary feature.
//!
//! Summarizes what a sub-agent is currently doing, for display in the
//! team view, status updates, and away summaries.

use crate::api_client::ApiClient;
use crate::orchestrator::{
    BackgroundFeatureKind, BackgroundFeatureRequest, OrchestratorError, run_background_feature,
};

/// Generates a short summary of what an agent is doing.
///
/// # Arguments
/// * `api_client` — The API client to use.
/// * `agent_activity` — Description of the agent's recent activity (tool calls, messages).
///
/// # Returns
/// A 3-5 word present-continuous summary (e.g., "Reading runAgent.ts").
pub async fn summarize_agent_activity(
    api_client: &dyn ApiClient,
    agent_activity: &str,
) -> Result<String, OrchestratorError> {
    let system = crate::prompts::AGENT_SUMMARY_SYSTEM;
    let user = format!(
        "Here is the agent's recent activity:\n\n{agent_activity}\n\n\
         Summarize what the agent is currently doing in 3-5 words."
    );

    let result = run_background_feature(
        api_client,
        BackgroundFeatureRequest {
            kind: BackgroundFeatureKind::AgentSummary,
            system_prompt: system.to_string(),
            user_prompt: user,
            parent_context: Some(agent_activity.to_string()),
            allowed_tools: None,
            max_tokens: 64,
            writable: false,
        },
    )
    .await?;

    Ok(result.content.lines().next()
        .unwrap_or(&result.content)
        .chars().take(60)
        .collect())
}

/// Generates an away summary — what happened while the user was away.
///
/// # Arguments
/// * `api_client` — The API client to use.
/// * `events` — List of events that occurred during absence.
///
/// # Returns
/// A concise summary of recent activity.
pub async fn generate_away_summary(
    api_client: &dyn ApiClient,
    events: &[String],
) -> Result<String, OrchestratorError> {
    let system = "You are a helpful AI assistant. Summarize recent events concisely.";
    let events_text = events.join("\n");
    let user = format!(
        "While the user was away, these events occurred:\n\n{events_text}\n\n\
         Provide a brief summary of what happened."
    );

    let result = run_background_feature(
        api_client,
        BackgroundFeatureRequest {
            kind: BackgroundFeatureKind::AwaySummary,
            system_prompt: system.to_string(),
            user_prompt: user,
            parent_context: Some(events_text),
            allowed_tools: None,
            max_tokens: 256,
            writable: false,
        },
    )
    .await?;

    Ok(result.content)
}

/// Generates a team status report from multiple agent summaries.
///
/// # Arguments
/// * `api_client` — The API client to use.
/// * `agent_summaries` — List of (agent_name, summary) pairs.
///
/// # Returns
/// A formatted team status report.
pub async fn generate_team_status(
    api_client: &dyn ApiClient,
    agent_summaries: &[(String, String)],
) -> Result<String, OrchestratorError> {
    let system = "You are a project coordinator. Format a team status report from individual agent updates.";
    let input = agent_summaries
        .iter()
        .map(|(name, summary)| format!("- **{name}**: {summary}"))
        .collect::<Vec<_>>()
        .join("\n");
    let user = format!("Format this team status update:\n\n{input}");

    let result = run_background_feature(
        api_client,
        BackgroundFeatureRequest {
            kind: BackgroundFeatureKind::TeamStatus,
            system_prompt: system.to_string(),
            user_prompt: user,
            parent_context: Some(input),
            allowed_tools: None,
            max_tokens: 256,
            writable: false,
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
                    input_tokens: 100,
                    output_tokens: 20,
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
    async fn summarize_agent_activity_returns_short_summary() {
        let api = MockApi::new("Reading engine.rs for tool execution logic");
        let summary = summarize_agent_activity(&api, "agent read 5 files, searched for 'tool'").await;
        assert!(summary.is_ok());
        assert!(summary.unwrap().len() <= 60);
    }

    #[tokio::test]
    async fn generate_away_summary_returns_content() {
        let api = MockApi::new("Agent completed 3 tasks while you were away.");
        let events = vec!["Agent started task".into(), "Agent completed task".into()];
        let summary = generate_away_summary(&api, &events).await;
        assert!(summary.is_ok());
    }

    #[tokio::test]
    async fn generate_team_status_returns_content() {
        let api = MockApi::new("Team is making progress on 2 active tasks.");
        let summaries = vec![
            ("Agent A".into(), "Working on auth".into()),
            ("Agent B".into(), "Writing tests".into()),
        ];
        let status = generate_team_status(&api, &summaries).await;
        assert!(status.is_ok());
    }
}
