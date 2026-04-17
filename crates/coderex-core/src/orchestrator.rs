//! AI orchestrator — high-level entry points for different call patterns.
//!
//! This module provides the product-layer orchestration that sits above the
//! raw `QueryEngine` and `ApiClient`. It selects models, prompts, and execution
//! strategies based on intent rather than raw parameters.
//!
//! Call patterns:
//! - `run_main_agent`: full interactive agent loop with tools and streaming
//! - `run_side_query`: single-shot non-streaming decision/explanation query
//! - `run_lightweight_query`: fast small-model query for titles, parsing, etc.
//! - `run_forked_agent`: isolated sub-agent with its own context and tool set

use crate::api_client::{ApiClient, ApiError};
use crate::query_profiles::{ModelProfile, PromptProfile, QueryKind, QueryRequest};
use crate::types::{Message, ModelId};

/// Result from a query execution.
#[derive(Debug, Clone)]
pub struct QueryResult {
    /// The text content from the response.
    pub content: String,
    /// Token usage summary.
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub cache_read_tokens: usize,
    pub cache_creation_tokens: usize,
}

/// Errors from orchestrator operations.
#[derive(Debug, thiserror::Error)]
pub enum OrchestratorError {
    /// API-level error.
    #[error("API error: {0}")]
    Api(#[from] ApiError),
    /// Invalid configuration.
    #[error("orchestrator config error: {0}")]
    Config(String),
}

// ─── Orchestrator ───────────────────────────────────────────────────────────

/// Orchestrator — routes AI calls through product-level entry points.
///
/// The orchestrator doesn't replace the `QueryEngine` (the core execution
/// kernel) — it wraps it with intent-aware routing that selects models,
/// prompts, and call patterns based on what the caller wants to achieve.
pub struct Orchestrator {
    /// API client for direct queries (side/lightweight).
    api_client: Box<dyn ApiClient>,
    /// Default model profile for the session.
    default_model: ModelProfile,
    /// Default prompt profile for the session.
    default_prompt: PromptProfile,
}

impl Orchestrator {
    /// Create a new orchestrator.
    #[must_use]
    pub fn new(
        api_client: Box<dyn ApiClient>,
    ) -> Self {
        Self {
            api_client,
            default_model: ModelProfile::Main,
            default_prompt: PromptProfile::DefaultAgent,
        }
    }

    /// Set the default model profile.
    pub fn set_default_model(&mut self, profile: ModelProfile) {
        self.default_model = profile;
    }

    /// Set the default prompt profile.
    pub fn set_default_prompt(&mut self, profile: PromptProfile) {
        self.default_prompt = profile;
    }

    /// Execute a query based on its kind.
    ///
    /// This is the main dispatch point — it routes to the appropriate
    /// execution strategy based on the query request.
    pub async fn run(&self, request: QueryRequest) -> Result<QueryResult, OrchestratorError> {
        match request.kind {
            QueryKind::MainAgent => self.run_main_agent(&request).await,
            QueryKind::SideQuery => self.run_side_query(&request).await,
            QueryKind::Lightweight => self.run_lightweight_query(&request).await,
            QueryKind::ForkedAgent => self.run_forked_agent(&request).await,
        }
    }

    /// Run the main agent loop.
    ///
    /// This wraps the full `QueryEngine::submit_streaming` pipeline with
    /// product-level concerns: model selection, prompt profile assembly,
    /// and tool whitelist management.
    ///
    /// In a full implementation, this would invoke `QueryEngine` directly.
    /// Here we provide the routing and parameter translation layer.
    async fn run_main_agent(&self, request: &QueryRequest) -> Result<QueryResult, OrchestratorError> {
        let model = self.resolve_model(&request.model_profile);
        let system = self.build_system_prompt(request);

        let messages = Self::build_messages(&request.user_prompt);
        let max_tokens = if request.max_tokens > 0 {
            request.max_tokens
        } else {
            4096
        };

        let response = self.api_client
            .create_message(&system, &messages, &model, max_tokens)
            .await?;

        Ok(QueryResult {
            content: response.content,
            input_tokens: response.usage.input_tokens,
            output_tokens: response.usage.output_tokens,
            cache_read_tokens: response.usage.cache_read_tokens,
            cache_creation_tokens: response.usage.cache_creation_tokens,
        })
    }

    /// Run a side query — single-shot, non-streaming, for decisions/explanations.
    ///
    /// Typical uses: permission explanation, mode classification,
    /// memory selection, model validation.
    pub async fn run_side_query(&self, request: &QueryRequest) -> Result<QueryResult, OrchestratorError> {
        let model = self.resolve_model(&request.model_profile);
        let system = request.custom_system_prompt
            .as_deref()
            .unwrap_or_else(|| crate::prompts::system_prompt_for_profile(request.prompt_profile));

        let mut user_text = request.user_prompt.clone();
        if let Some(ref append) = request.append_system_prompt {
            user_text.clone_from(append);
        }

        let messages = Self::build_messages(&user_text);
        let max_tokens = if request.max_tokens > 0 {
            request.max_tokens
        } else {
            1024
        };

        let response = self.api_client
            .create_message(&system, &messages, &model, max_tokens)
            .await?;

        Ok(QueryResult {
            content: response.content,
            input_tokens: response.usage.input_tokens,
            output_tokens: response.usage.output_tokens,
            cache_read_tokens: response.usage.cache_read_tokens,
            cache_creation_tokens: response.usage.cache_creation_tokens,
        })
    }

    /// Run a lightweight query — fast, small-model, for titles/parsing/classification.
    ///
    /// Typical uses: session title, rename, date parsing, shell prefix,
    /// tool use summary, hook evaluation.
    pub async fn run_lightweight_query(&self, request: &QueryRequest) -> Result<QueryResult, OrchestratorError> {
        // Force lightweight model regardless of request
        let model = ModelProfile::Lightweight.resolve();
        let system = request.custom_system_prompt
            .as_deref()
            .unwrap_or_else(|| crate::prompts::system_prompt_for_profile(request.prompt_profile));

        let messages = Self::build_messages(&request.user_prompt);
        let max_tokens = if request.max_tokens > 0 {
            request.max_tokens.min(1024) // cap lightweight queries
        } else {
            512
        };

        let response = self.api_client
            .create_message(&system, &messages, &model, max_tokens)
            .await?;

        Ok(QueryResult {
            content: response.content,
            input_tokens: response.usage.input_tokens,
            output_tokens: response.usage.output_tokens,
            cache_read_tokens: response.usage.cache_read_tokens,
            cache_creation_tokens: response.usage.cache_creation_tokens,
        })
    }

    /// Run a forked agent — isolated sub-task with its own context.
    ///
    /// Forked agents get an independent context window with optional
    /// inheritance from the parent session, tool whitelisting, and
    /// isolated model routing.
    ///
    /// Unlike side queries, forked agents can:
    /// - inherit a subset of parent conversation context
    /// - operate under a restricted tool whitelist
    /// - use a different model profile (e.g., Background)
    /// - produce results that are recycled back into the main session
    async fn run_forked_agent(&self, request: &QueryRequest) -> Result<QueryResult, OrchestratorError> {
        // Build a system prompt that includes the parent session context
        let mut system = self.build_system_prompt(request);

        // If inheriting session context, append the context note
        if request.inherit_session {
            system.push_str("\n\nYou are a sub-agent working on a sub-task of the main conversation. \
                            You have access to the parent session's context but should focus only \
                            on your assigned task.");
        }

        // Route through the API client with forked-agent parameters
        let model = self.resolve_model(&request.model_profile);
        let messages = Self::build_messages(&request.user_prompt);
        let max_tokens = if request.max_tokens > 0 {
            request.max_tokens
        } else {
            2048 // forked agents get more tokens than side queries
        };

        let response = self.api_client
            .create_message(&system, &messages, &model, max_tokens)
            .await?;

        Ok(QueryResult {
            content: response.content,
            input_tokens: response.usage.input_tokens,
            output_tokens: response.usage.output_tokens,
            cache_read_tokens: response.usage.cache_read_tokens,
            cache_creation_tokens: response.usage.cache_creation_tokens,
        })
    }

    /// Resolve a model profile to a concrete model.
    fn resolve_model(&self, profile: &ModelProfile) -> ModelId {
        profile.resolve()
    }

    /// Build the system prompt for a request.
    fn build_system_prompt(&self, request: &QueryRequest) -> String {
        if let Some(ref override_prompt) = request.custom_system_prompt {
            return override_prompt.clone();
        }

        let base = crate::prompts::system_prompt_for_profile(request.prompt_profile);
        let mut system = base.to_string();

        if let Some(ref append) = request.append_system_prompt {
            system.push_str("\n\n");
            system.push_str(append);
        }

        system
    }

    /// Build a message list from a user prompt string.
    fn build_messages(user_prompt: &str) -> Vec<serde_json::Value> {
        let msg = Message::user(user_prompt);
        serde_json::to_value(&msg).map(|v| vec![v]).unwrap_or_default()
    }
}

/// Convenience function: run a side query without creating an orchestrator.
///
/// This is useful for quick one-off queries from anywhere in the codebase.
pub async fn run_side_query(
    api_client: &dyn ApiClient,
    system_prompt: &str,
    user_prompt: &str,
    model: &ModelId,
    max_tokens: u32,
) -> Result<QueryResult, OrchestratorError> {
    let messages = Orchestrator::build_messages(user_prompt);
    let response = api_client
        .create_message(system_prompt, &messages, model, max_tokens)
        .await?;

    Ok(QueryResult {
        content: response.content,
        input_tokens: response.usage.input_tokens,
        output_tokens: response.usage.output_tokens,
        cache_read_tokens: response.usage.cache_read_tokens,
        cache_creation_tokens: response.usage.cache_creation_tokens,
    })
}

/// Convenience function: run a lightweight query without creating an orchestrator.
pub async fn run_lightweight_query(
    api_client: &dyn ApiClient,
    system_prompt: &str,
    user_prompt: &str,
    max_tokens: u32,
) -> Result<QueryResult, OrchestratorError> {
    run_side_query(
        api_client,
        system_prompt,
        user_prompt,
        &ModelProfile::Lightweight.resolve(),
        max_tokens.min(1024),
    )
    .await
}

/// Run a forked agent query without creating an orchestrator.
///
/// Forked agents operate with an independent context window, optional
/// inheritance from the parent session, and a tool whitelist.
///
/// # Arguments
/// * `api_client` — The API client to use.
/// * `system_prompt` — System prompt for the sub-agent.
/// * `user_prompt` — User prompt (the task for the sub-agent).
/// * `parent_context` — Optional inherited context from parent session.
/// * `allowed_tools` — Optional tool whitelist (None = all tools).
/// * `model` — Model profile to use.
/// * `max_tokens` — Token budget for this sub-task.
///
/// # Returns
/// The sub-agent's response as a QueryResult.
pub async fn run_forked_agent(
    api_client: &dyn ApiClient,
    system_prompt: &str,
    user_prompt: &str,
    parent_context: Option<&str>,
    allowed_tools: Option<&[String]>,
    model: &ModelId,
    max_tokens: u32,
) -> Result<QueryResult, OrchestratorError> {
    // Build enhanced system prompt with parent context
    let full_system = match parent_context {
        Some(ctx) => format!(
            "{system_prompt}\n\n\
             You are a sub-agent working on a sub-task. Parent session context:\n\
             {ctx}\n\n\
             Focus only on your assigned task. Use the tools available to you."
        ),
        None => format!(
            "{system_prompt}\n\n\
             You are a sub-agent working on an isolated task. Focus only on \
             your assigned assignment."
        ),
    };

    // Append tool whitelist note if provided
    let full_system = match allowed_tools {
        Some(tools) => {
            let tools_list = tools.join(", ");
            format!("{full_system}\n\nAvailable tools: {tools_list}")
        }
        None => full_system,
    };

    let messages = Orchestrator::build_messages(user_prompt);
    let max_tokens = if max_tokens > 0 { max_tokens } else { 2048 };

    let response = api_client
        .create_message(&full_system, &messages, model, max_tokens)
        .await?;

    Ok(QueryResult {
        content: response.content,
        input_tokens: response.usage.input_tokens,
        output_tokens: response.usage.output_tokens,
        cache_read_tokens: response.usage.cache_read_tokens,
        cache_creation_tokens: response.usage.cache_creation_tokens,
    })
}

/// Parse a JSON string field from response content.
fn parse_json_string_field(content: &str, field: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(content).ok()?;
    value.get(field).and_then(|v| v.as_str()).map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_client::{ApiClient, ApiResponse, StreamEvent};
    use crate::types::UsageSummary;
    use std::pin::Pin;
    use futures::Stream;

    #[derive(Clone)]
    struct MockApi;

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
        ) -> Result<ApiResponse, ApiError> {
            Ok(ApiResponse {
                content: r#"{"title": "Fix auth bug"}"#.into(),
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
        ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
            Box::pin(futures::stream::empty())
        }
    }

    #[tokio::test]
    async fn orchestrator_dispatch_side_query() {
        let orchestrator = Orchestrator::new(Box::new(MockApi));
        let request = QueryRequest::side_query("test");
        let result = orchestrator.run(request).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn orchestrator_dispatch_lightweight() {
        let orchestrator = Orchestrator::new(Box::new(MockApi));
        let request = QueryRequest::lightweight("classify");
        let result = orchestrator.run(request).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn orchestrator_dispatch_main_agent() {
        let orchestrator = Orchestrator::new(Box::new(MockApi));
        let request = QueryRequest {
            kind: QueryKind::MainAgent,
            user_prompt: "hello".into(),
            custom_system_prompt: None,
            append_system_prompt: None,
            prompt_profile: PromptProfile::DefaultAgent,
            allowed_tools: None,
            model_profile: ModelProfile::Main,
            inherit_session: false,
            stream: true,
            max_tokens: 4096,
        };
        let result = orchestrator.run(request).await;
        assert!(result.is_ok());
    }

    #[test]
    fn build_messages_creates_valid_json() {
        let msgs = Orchestrator::build_messages("hello");
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].get("role").unwrap().as_str().unwrap() == "User");
    }

    #[tokio::test]
    async fn orchestrator_dispatch_forked_agent() {
        let orchestrator = Orchestrator::new(Box::new(MockApi));
        let request = QueryRequest {
            kind: QueryKind::ForkedAgent,
            user_prompt: "analyze this code".into(),
            custom_system_prompt: Some("You are a code analysis agent.".into()),
            append_system_prompt: Some("Focus on security.".into()),
            prompt_profile: PromptProfile::DefaultAgent,
            allowed_tools: None,
            model_profile: ModelProfile::Background,
            inherit_session: true,
            stream: false,
            max_tokens: 2048,
        };
        let result = orchestrator.run(request).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn forked_agent_free_function() {
        let result = run_forked_agent(
            &MockApi,
            "You are a code reviewer.",
            "Review the auth module for vulnerabilities.",
            Some("Parent session: working on auth fix"),
            Some(&["Read".into(), "Grep".into()]),
            &ModelProfile::Main.resolve(),
            1024,
        )
        .await;
        assert!(result.is_ok());
        assert!(!result.unwrap().content.is_empty());
    }

    #[tokio::test]
    async fn forked_agent_without_context() {
        let result = run_forked_agent(
            &MockApi,
            "You are an isolated agent.",
            "Summarize this text.",
            None, // no parent context
            None, // no tool whitelist
            &ModelProfile::Lightweight.resolve(),
            512,
        )
        .await;
        assert!(result.is_ok());
    }
}
