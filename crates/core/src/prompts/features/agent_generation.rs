//! Agent generation feature.
//!
//! Generates structured agent definitions (prompts, tool configs, behavior specs)
//! that can be registered as new slash commands or team members.

use crate::api_client::ApiClient;
use crate::orchestrator::{OrchestratorError, run_side_query};
use crate::query_profiles::ModelProfile;

/// Represents a generated agent specification.
#[derive(Debug, Clone)]
pub struct AgentSpec {
    /// Agent name (used in /agent_name command).
    pub name: String,
    /// System prompt for the agent.
    pub system_prompt: String,
    /// Short description for help listing.
    pub description: String,
    /// Tool whitelist (None means all tools available).
    pub allowed_tools: Option<Vec<String>>,
    /// Model profile preference.
    pub model_profile: String,
}

/// Generates an agent specification from a description.
///
/// # Arguments
/// * `api_client` — The API client to use.
/// * `description` — Description of what the agent should do.
///
/// # Returns
/// An AgentSpec with the generated configuration.
pub async fn generate_agent(
    api_client: &dyn ApiClient,
    description: &str,
) -> Result<AgentSpec, OrchestratorError> {
    let system = "You are an agent configuration generator. Given a description, \
                  create a complete agent specification with name, system prompt, \
                  description, tool whitelist, and model preference. \
                  Return ONLY a JSON object with fields: name, system_prompt, \
                  description, allowed_tools (array or null), model_profile.";
    let user = format!(
        "Create an agent specification for: {description}\n\n\
         Return JSON: {{\n\
           \"name\": \"short_snake_case_name\",\n\
           \"system_prompt\": \"detailed system prompt\",\n\
           \"description\": \"short description for help menu\",\n\
           \"allowed_tools\": null,\n\
           \"model_profile\": \"Main\"\n\
         }}"
    );

    let result = run_side_query(
        api_client,
        system,
        &user,
        &ModelProfile::Main.resolve_runtime(),
        512,
    )
    .await?;

    parse_agent_spec(&result.content)
}

/// Parses an agent specification from JSON content.
fn parse_agent_spec(content: &str) -> Result<AgentSpec, OrchestratorError> {
    let value: serde_json::Value = serde_json::from_str(content)
        .map_err(|e| OrchestratorError::Config(format!("invalid agent spec JSON: {e}")))?;

    let name = value
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| OrchestratorError::Config("missing 'name' field".into()))?
        .to_string();

    let system_prompt = value
        .get("system_prompt")
        .and_then(|v| v.as_str())
        .ok_or_else(|| OrchestratorError::Config("missing 'system_prompt' field".into()))?
        .to_string();

    let description = value
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or(&name)
        .to_string();

    let allowed_tools = value
        .get("allowed_tools")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        });

    let model_profile = value
        .get("model_profile")
        .and_then(|v| v.as_str())
        .unwrap_or("Main")
        .to_string();

    Ok(AgentSpec {
        name,
        system_prompt,
        description,
        allowed_tools,
        model_profile,
    })
}

/// Generates a system prompt for a custom agent role.
///
/// Simpler version that returns just the prompt text.
pub async fn generate_agent_prompt(
    api_client: &dyn ApiClient,
    role_description: &str,
) -> Result<String, OrchestratorError> {
    let system = "You are a prompt engineer. Write a clear, effective system prompt \
                  for an AI agent given the role description.";
    let user = format!("Write a system prompt for an agent that: {role_description}");

    let result = run_side_query(
        api_client,
        system,
        &user,
        &ModelProfile::Main.resolve_runtime(),
        512,
    )
    .await?;

    Ok(result.content)
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
                    output_tokens: 100,
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
    fn parse_agent_spec_valid_json() {
        let json = r#"{
            "name": "code_reviewer",
            "system_prompt": "You are a code reviewer.",
            "description": "Reviews code for quality",
            "allowed_tools": ["Read", "Grep"],
            "model_profile": "Main"
        }"#;
        let spec = parse_agent_spec(json).unwrap();
        assert_eq!(spec.name, "code_reviewer");
        assert_eq!(spec.description, "Reviews code for quality");
        assert!(spec.allowed_tools.is_some());
        assert_eq!(spec.allowed_tools.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn parse_agent_spec_missing_fields() {
        let json = r#"{"name": "test"}"#;
        let result = parse_agent_spec(json);
        assert!(result.is_err());
    }

    #[test]
    fn parse_agent_spec_invalid_json() {
        let result = parse_agent_spec("not json");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn generate_agent_returns_spec() {
        let api = MockApi::new(
            r#"{
            "name": "test_agent",
            "system_prompt": "You are a test agent.",
            "description": "Tests things",
            "allowed_tools": null,
            "model_profile": "Main"
        }"#,
        );
        let spec = generate_agent(&api, "a testing agent").await;
        assert!(spec.is_ok());
        assert_eq!(spec.unwrap().name, "test_agent");
    }

    #[tokio::test]
    async fn generate_agent_prompt_returns_prompt() {
        let api = MockApi::new("You are a security specialist that audits code...");
        let prompt = generate_agent_prompt(&api, "security auditor").await;
        assert!(prompt.is_ok());
    }
}
