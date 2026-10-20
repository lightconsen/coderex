//! Anthropic API provider.
//!
//! Wraps the existing [`AnthropicApiClient`](crate::api_client::AnthropicApiClient)
//! logic in the [`Provider`](crate::provider::Provider) trait.

use crate::api_client::{
    AnthropicApiClient, ApiClient, ApiConfig, ApiResponse, RequestContext, StreamEvent,
};
use crate::provider::{Provider, ProviderCapabilities, ProviderError, classify_api_error};
use crate::types::ModelId;
use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

/// Anthropic API provider.
#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    client: AnthropicApiClient,
}

impl AnthropicProvider {
    /// Create a new Anthropic provider with the given API config.
    #[must_use]
    pub fn new(config: ApiConfig) -> Self {
        Self {
            client: AnthropicApiClient::new(config),
        }
    }

    /// Create from an existing API client.
    #[must_use]
    pub const fn from_client(client: AnthropicApiClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.client.resolved_profile().capabilities
    }

    fn with_request_context(&self, context: RequestContext) -> Box<dyn Provider> {
        Box::new(Self::from_client(
            self.client.cloned_with_request_context(context),
        ))
    }

    async fn create_message(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        model: &ModelId,
        max_tokens: u32,
        tools: &[crate::tool_executor::ToolInfo],
    ) -> Result<ApiResponse, ProviderError> {
        self.client
            .create_message(system, messages, model, max_tokens, tools)
            .await
            .map_err(|e| classify_api_error(self.name(), &e))
    }

    async fn create_message_streaming(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        model: &ModelId,
        max_tokens: u32,
        tools: &[crate::tool_executor::ToolInfo],
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        self.client
            .create_message_streaming(system, messages, model, max_tokens, tools)
            .await
    }

    fn resolve_model(&self, model: &ModelId) -> String {
        model.0.clone()
    }

    fn supports_model(&self, _model: &ModelId) -> bool {
        // Anthropic supports all models (it's the default)
        true
    }

    fn clone_box(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}
