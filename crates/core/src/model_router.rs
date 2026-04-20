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

use crate::api_client::{ApiClient, ApiError, ApiResponse, StreamEvent};
use crate::provider::{Provider, ProviderError, provider_error_to_api};
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
}

impl std::fmt::Debug for ModelRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelRouter")
            .field("name", &self.name)
            .field("routes", &self.routes)
            .field("max_fallbacks", &self.max_fallbacks)
            .finish()
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
        }
    }

    /// Set the maximum number of fallback attempts.
    #[must_use]
    pub const fn with_max_fallbacks(mut self, max: u32) -> Self {
        self.max_fallbacks = max;
        self
    }

    /// Add a provider to the routing chain.
    pub fn add_provider(
        &mut self,
        provider: Box<dyn Provider>,
    ) {
        self.routes.push(RouteEntry {
            provider,
            priority: self.routes.len() as u32,
            models: None,
        });
        self.routes.sort_by_key(|r| r.priority);
    }

    /// Add a provider with a specific priority.
    pub fn add_provider_with_priority(
        &mut self,
        provider: Box<dyn Provider>,
        priority: u32,
    ) {
        self.routes.push(RouteEntry {
            provider,
            priority,
            models: None,
        });
        self.routes.sort_by_key(|r| r.priority);
    }

    /// Add a provider restricted to specific models.
    pub fn add_provider_for_models(
        &mut self,
        provider: Box<dyn Provider>,
        models: Vec<String>,
    ) {
        self.routes.push(RouteEntry {
            provider,
            priority: self.routes.len() as u32,
            models: Some(models),
        });
        self.routes.sort_by_key(|r| r.priority);
    }

    /// Get eligible providers for a model.
    fn eligible_providers(
        &self,
        model: &ModelId,
    ) -> Vec<Box<dyn Provider>> {
        self.routes
            .iter()
            .filter(|entry| {
                if let Some(ref allowed) = entry.models {
                    allowed.contains(&model.0) || allowed.iter().any(|m| model.0.starts_with(m))
                } else {
                    entry.provider.supports_model(model)
                }
            })
            .map(|entry| entry.provider.clone_box())
            .collect()
    }

    /// Try each eligible provider in order, falling back on eligible errors.
    async fn route_create_message(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        model: &ModelId,
        max_tokens: u32,
    ) -> Result<ApiResponse, ProviderError> {
        let providers = self.eligible_providers(model);
        if providers.is_empty() {
            return Err(ProviderError {
                message: format!("no provider available for model '{}'", model.0),
                kind: crate::provider::ProviderErrorKind::InvalidRequest,
                provider: self.name.clone(),
            });
        }

        let mut last_error = None;
        let mut attempts = 0;

        for provider in providers {
            if attempts > self.max_fallbacks {
                break;
            }
            attempts += 1;

            tracing::debug!(
                "router '{}' trying provider '{}' for model '{}'",
                self.name,
                provider.name(),
                model.0
            );

            match provider.create_message(system, messages, model, max_tokens).await {
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
        let mut router = ModelRouter::new(self.name.clone())
            .with_max_fallbacks(self.max_fallbacks);
        for entry in &self.routes {
            router.add_provider_with_priority(entry.provider.clone_box(), entry.priority);
        }
        Box::new(router)
    }

    async fn create_message(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        model: &ModelId,
        max_tokens: u32,
    ) -> Result<ApiResponse, ApiError> {
        self.route_create_message(system, messages, model, max_tokens)
            .await
            .map_err(provider_error_to_api)
    }

    async fn create_message_streaming(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        model: &ModelId,
        max_tokens: u32,
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        // For streaming, we try the first provider that succeeds in starting the stream
        let providers: Vec<_> = self.routes.iter()
            .filter(|entry| {
                if let Some(ref allowed) = entry.models {
                    allowed.contains(&model.0)
                } else {
                    entry.provider.supports_model(model)
                }
            })
            .map(|entry| entry.provider.clone_box())
            .collect();

        let system = system.to_string();
        let messages: Vec<serde_json::Value> = messages.to_vec();
        let model = model.clone();

        Box::pin(async_stream::stream! {
            for provider in providers {
                let stream = provider.create_message_streaming(
                    &system,
                    &messages,
                    &model,
                    max_tokens,
                ).await;

                use futures::StreamExt;
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
            .create_message("", &[], &ModelId::sonnet(), 1024)
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
            .create_message("", &[], &ModelId::sonnet(), 1024)
            .await;

        assert!(result.is_err());
    }
}
