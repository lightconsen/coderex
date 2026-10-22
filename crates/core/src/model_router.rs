//! Model router — priority-based routing with fallback chains.
//!
//! The [`ModelRouter`] implements [`ApiClient`](crate::api_client::ApiClient) and
//! routes requests through a chain of [`Provider`](crate::provider::Provider)s.
//! When a provider fails with a fallback-eligible error, the router tries the
//! next provider in the chain.
//!
//! # Example
//!
//! ```rust,ignore
//! use coderex_core::model_router::ModelRouter;
//! use coderex_core::providers::AnthropicProvider;
//!
//! let mut router = ModelRouter::new("main");
//! router.add_provider(Box::new(AnthropicProvider::new(config)));
//! ```

use crate::api_client::{ApiClient, ApiError, ApiResponse, RequestContext, StreamEvent};
use crate::provider::{Provider, ProviderError, provider_error_to_api};
use crate::tool_executor::ToolInfo;
use crate::types::ModelId;
use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

/// A routing chain entry with priority and optional model filter.
pub struct RouteEntry {
    /// The provider to route to.
    pub provider: Box<dyn Provider>,
    /// Priority (lower = tried first).
    pub priority: u32,
    /// Optional model filter — if set, only routes requests for these models.
    pub models: Option<Vec<String>>,
}

impl std::fmt::Debug for RouteEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouteEntry")
            .field("provider", &self.provider.name())
            .field("priority", &self.priority)
            .field("models", &self.models)
            .finish()
    }
}

/// Routes API requests through a priority-ordered chain of providers.
///
/// Implements [`ApiClient`] for drop-in replacement of single-provider
/// clients in the [`QueryEngine`](crate::engine::QueryEngine).
pub struct ModelRouter {
    /// Name of this router (for debugging).
    name: String,
    /// Provider chain, sorted by priority.
    routes: Vec<RouteEntry>,
    /// Maximum number of fallback attempts.
    max_fallbacks: u32,
    /// Optional explicit fallback provider allowlist.
    fallback_providers: Vec<String>,
    /// Optional per-request context propagated into providers.
    request_context: Option<RequestContext>,
}

impl std::fmt::Debug for ModelRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelRouter")
            .field("name", &self.name)
            .field("routes", &self.routes)
            .field("max_fallbacks", &self.max_fallbacks)
            .field("fallback_providers", &self.fallback_providers)
            .finish_non_exhaustive()
    }
}

impl ModelRouter {
    /// Create a new model router with the given name.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            routes: Vec::new(),
            max_fallbacks: 2,
            fallback_providers: Vec::new(),
            request_context: None,
        }
    }

    /// Set the maximum number of fallback attempts.
    #[must_use]
    pub const fn with_max_fallbacks(mut self, max: u32) -> Self {
        self.max_fallbacks = max;
        self
    }

    /// Restrict fallbacks to an explicit provider allowlist.
    #[must_use]
    pub fn with_fallback_providers(mut self, providers: Vec<String>) -> Self {
        self.fallback_providers = providers
            .into_iter()
            .map(|provider| provider.to_ascii_lowercase())
            .collect();
        self
    }

    /// Add a provider to the routing chain.
    pub fn add_provider(&mut self, provider: Box<dyn Provider>) {
        self.routes.push(RouteEntry {
            provider,
            priority: u32::try_from(self.routes.len()).unwrap_or(u32::MAX),
            models: None,
        });
        self.routes.sort_by_key(|r| r.priority);
    }

    /// Add a provider with a specific priority.
    pub fn add_provider_with_priority(&mut self, provider: Box<dyn Provider>, priority: u32) {
        self.routes.push(RouteEntry {
            provider,
            priority,
            models: None,
        });
        self.routes.sort_by_key(|r| r.priority);
    }

    /// Add a provider restricted to specific models.
    pub fn add_provider_for_models(&mut self, provider: Box<dyn Provider>, models: Vec<String>) {
        self.routes.push(RouteEntry {
            provider,
            priority: u32::try_from(self.routes.len()).unwrap_or(u32::MAX),
            models: Some(models),
        });
        self.routes.sort_by_key(|r| r.priority);
    }

    /// Clone this router as a concrete value.
    #[must_use]
    pub fn clone_box_router(&self) -> Self {
        let mut router = Self::new(self.name.clone()).with_max_fallbacks(self.max_fallbacks);
        router = router.with_fallback_providers(self.fallback_providers.clone());
        router.request_context.clone_from(&self.request_context);
        for entry in &self.routes {
            router.routes.push(RouteEntry {
                provider: entry.provider.clone_box(),
                priority: entry.priority,
                models: entry.models.clone(),
            });
        }
        router.routes.sort_by_key(|route| route.priority);
        router
    }

    /// Clone this router with per-request context.
    #[must_use]
    pub fn clone_with_request_context(&self, context: RequestContext) -> Self {
        let mut router = self.clone_box_router();
        router.request_context = Some(context);
        router
    }

    /// Get eligible providers for a model.
    fn eligible_providers(&self, model: &ModelId) -> Vec<Box<dyn Provider>> {
        self.routes
            .iter()
            .filter(|entry| {
                entry.models.as_ref().map_or_else(
                    || entry.provider.supports_model(model),
                    |allowed| {
                        allowed.contains(&model.0) || allowed.iter().any(|m| model.0.starts_with(m))
                    },
                )
            })
            .map(|entry| entry.provider.clone_box())
            .collect()
    }

    fn providers_for_request(&self, model: &ModelId) -> Vec<Box<dyn Provider>> {
        let eligible = self.eligible_providers(model);
        let eligible: Vec<_> = if let Some(context) = self.request_context.clone() {
            eligible
                .into_iter()
                .map(|provider| provider.with_request_context(context.clone()))
                .collect()
        } else {
            eligible
        };
        if self.fallback_providers.is_empty() || eligible.len() <= 1 {
            return eligible;
        }

        let mut filtered = Vec::new();
        for (idx, provider) in eligible.into_iter().enumerate() {
            if idx == 0
                || self
                    .fallback_providers
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(provider.name()))
            {
                filtered.push(provider);
            }
        }
        filtered
    }

    fn response_to_stream_events(resp: ApiResponse) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        if let Some(block) = resp.thinking {
            if !block.thinking.is_empty() {
                events.push(StreamEvent::ThinkingDelta(block.thinking));
            }
            if !block.signature.is_empty() {
                events.push(StreamEvent::SignatureDelta(block.signature));
            }
        }
        if !resp.content.is_empty() {
            events.push(StreamEvent::TextDelta(resp.content));
        }
        for tc in resp.tool_calls {
            events.push(StreamEvent::ToolCallBlockStart {
                id: tc.id.clone(),
                name: tc.name.clone(),
            });
            events.push(StreamEvent::ToolCallBlockEnd {
                id: tc.id,
                input: tc.input,
            });
        }
        events.push(StreamEvent::UsageSummary(resp.usage));
        events.push(StreamEvent::StreamEnd);
        events
    }

    /// Try each eligible provider in order, falling back on eligible errors.
    async fn route_create_message(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        model: &ModelId,
        max_tokens: u32,
        tools: &[ToolInfo],
    ) -> Result<ApiResponse, ProviderError> {
        let providers = self.providers_for_request(model);
        if providers.is_empty() {
            return Err(ProviderError {
                message: format!("no provider available for model '{}'", model.0),
                kind: crate::provider::ProviderErrorKind::InvalidRequest,
                provider: self.name.clone(),
            });
        }

        let mut last_error = None;

        for (attempt, provider) in providers.into_iter().enumerate() {
            if attempt > usize::try_from(self.max_fallbacks).unwrap_or(usize::MAX) {
                break;
            }

            tracing::debug!(
                "router '{}' trying provider '{}' for model '{}'",
                self.name,
                provider.name(),
                model.0
            );

            match provider
                .create_message(
                    system,
                    messages,
                    model,
                    max_tokens,
                    if provider.capabilities().tool_use {
                        tools
                    } else {
                        &[]
                    },
                )
                .await
            {
                Ok(result) => return Ok(result),
                Err(err) => {
                    tracing::warn!(
                        "provider '{}' failed: {} (retryable={}, fallback_eligible={})",
                        provider.name(),
                        err.message,
                        err.is_retryable(),
                        err.is_fallback_eligible()
                    );
                    if err.is_fallback_eligible() {
                        last_error = Some(err);
                        continue;
                    }
                    return Err(err);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| ProviderError {
            message: "all providers exhausted".into(),
            kind: crate::provider::ProviderErrorKind::Unavailable,
            provider: self.name.clone(),
        }))
    }
}

#[async_trait]
impl ApiClient for ModelRouter {
    fn clone_box(&self) -> Box<dyn ApiClient> {
        Box::new(self.clone_box_router())
    }

    fn with_request_context(&self, context: RequestContext) -> Box<dyn ApiClient> {
        Box::new(self.clone_with_request_context(context))
    }

    async fn create_message(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        model: &ModelId,
        max_tokens: u32,
        tools: &[ToolInfo],
    ) -> Result<ApiResponse, ApiError> {
        self.route_create_message(system, messages, model, max_tokens, tools)
            .await
            .map_err(provider_error_to_api)
    }

    async fn create_message_streaming(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        model: &ModelId,
        max_tokens: u32,
        tools: &[ToolInfo],
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        use futures::StreamExt as _;

        // For streaming, we try the first provider that succeeds in starting the stream
        let providers = self.providers_for_request(model);

        let system = system.to_string();
        let messages: Vec<serde_json::Value> = messages.to_vec();
        let model = model.clone();
        let tools: Vec<ToolInfo> = tools.to_vec();

        Box::pin(async_stream::stream! {
            for provider in providers {
                if !provider.capabilities().streaming {
                    match provider
                        .create_message(
                            &system,
                            &messages,
                            &model,
                            max_tokens,
                            if provider.capabilities().tool_use {
                                &tools
                            } else {
                                &[]
                            },
                        )
                        .await
                    {
                        Ok(response) => {
                            for event in Self::response_to_stream_events(response) {
                                yield event;
                            }
                            break;
                        }
                        Err(err) => {
                            yield StreamEvent::Error(provider_error_to_api(err.clone()));
                            if err.is_fallback_eligible() {
                                continue;
                            }
                            yield StreamEvent::StreamEnd;
                            break;
                        }
                    }
                }

                let stream = provider.create_message_streaming(
                    &system,
                    &messages,
                    &model,
                    max_tokens,
                    if provider.capabilities().tool_use {
                        &tools
                    } else {
                        &[]
                    },
                ).await;

                let mut pinned = stream;
                let mut had_error = false;

                while let Some(event) = pinned.next().await {
                    if matches!(event, StreamEvent::Error(_)) {
                        had_error = true;
                    }
                    yield event;
                }

                if !had_error {
                    break;
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ProviderCapabilities, ProviderErrorKind};
    use crate::types::UsageSummary;

    struct FailingProvider {
        name: String,
        fallback_eligible: bool,
    }

    #[async_trait::async_trait]
    impl Provider for FailingProvider {
        fn name(&self) -> &str {
            &self.name
        }
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }
        async fn create_message(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[ToolInfo],
        ) -> Result<ApiResponse, ProviderError> {
            Err(ProviderError {
                message: "test failure".into(),
                kind: if self.fallback_eligible {
                    ProviderErrorKind::Unavailable
                } else {
                    ProviderErrorKind::Auth
                },
                provider: self.name.clone(),
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
        fn resolve_model(&self, model: &ModelId) -> String {
            model.0.clone()
        }
        fn supports_model(&self, _model: &ModelId) -> bool {
            true
        }
        fn clone_box(&self) -> Box<dyn Provider> {
            Box::new(Self {
                name: self.name.clone(),
                fallback_eligible: self.fallback_eligible,
            })
        }
    }

    struct SuccessProvider;

    #[async_trait::async_trait]
    impl Provider for SuccessProvider {
        fn name(&self) -> &str {
            "success"
        }
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }
        async fn create_message(
            &self,
            _system: &str,
            _messages: &[serde_json::Value],
            _model: &ModelId,
            _max_tokens: u32,
            _tools: &[ToolInfo],
        ) -> Result<ApiResponse, ProviderError> {
            Ok(ApiResponse {
                content: "success".into(),
                tool_calls: Vec::new(),
                usage: UsageSummary {
                    input_tokens: 0,
                    output_tokens: 0,
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
        fn resolve_model(&self, model: &ModelId) -> String {
            model.0.clone()
        }
        fn supports_model(&self, _model: &ModelId) -> bool {
            true
        }
        fn clone_box(&self) -> Box<dyn Provider> {
            Box::new(Self)
        }
    }

    #[tokio::test]
    async fn fallback_to_next_provider() {
        let mut router = ModelRouter::new("test");
        router.add_provider(Box::new(FailingProvider {
            name: "fail1".into(),
            fallback_eligible: true,
        }));
        router.add_provider(Box::new(SuccessProvider));

        let result = router
            .create_message("", &[], &ModelId::sonnet(), 1024, &[])
            .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap().content, "success");
    }

    #[tokio::test]
    async fn non_fallback_error_stops_immediately() {
        let mut router = ModelRouter::new("test");
        router.add_provider(Box::new(FailingProvider {
            name: "fail1".into(),
            fallback_eligible: false,
        }));
        router.add_provider(Box::new(SuccessProvider));

        let result = router
            .create_message("", &[], &ModelId::sonnet(), 1024, &[])
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn fallback_allowlist_restricts_router_chain() {
        let mut router =
            ModelRouter::new("test").with_fallback_providers(vec!["success".to_string()]);
        router.add_provider(Box::new(FailingProvider {
            name: "fail1".into(),
            fallback_eligible: true,
        }));
        router.add_provider(Box::new(FailingProvider {
            name: "skip-me".into(),
            fallback_eligible: true,
        }));
        router.add_provider(Box::new(SuccessProvider));

        let result = router
            .create_message("", &[], &ModelId::sonnet(), 1024, &[])
            .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap().content, "success");
    }
}
