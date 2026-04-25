//! Provider router — explicit routing layer between the engine and providers.

use crate::api_client::{ApiClient, ApiError, ApiResponse, RequestContext, StreamEvent};
use crate::model_router::ModelRouter;
use crate::tool_executor::ToolInfo;
use crate::types::ModelId;
use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

/// Explicit provider-routing layer used by the engine.
#[derive(Debug)]
pub struct ProviderRouter {
    inner: ModelRouter,
}

impl ProviderRouter {
    /// Wrap an existing model router.
    #[must_use]
    pub const fn from_model_router(inner: ModelRouter) -> Self {
        Self { inner }
    }

    /// Borrow the underlying model router.
    #[must_use]
    pub const fn inner(&self) -> &ModelRouter {
        &self.inner
    }
}

#[async_trait]
impl ApiClient for ProviderRouter {
    fn clone_box(&self) -> Box<dyn ApiClient> {
        Box::new(Self::from_model_router(self.inner.clone_box_router()))
    }

    fn with_request_context(&self, context: RequestContext) -> Box<dyn ApiClient> {
        Box::new(Self::from_model_router(
            self.inner.clone_with_request_context(context),
        ))
    }

    async fn create_message(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        model: &ModelId,
        max_tokens: u32,
        tools: &[ToolInfo],
    ) -> Result<ApiResponse, ApiError> {
        self.inner
            .create_message(system, messages, model, max_tokens, tools)
            .await
    }

    async fn create_message_streaming(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        model: &ModelId,
        max_tokens: u32,
        tools: &[ToolInfo],
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        self.inner
            .create_message_streaming(system, messages, model, max_tokens, tools)
            .await
    }
}
