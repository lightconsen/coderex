//! Provider trait — abstract over different LLM API providers.
//!
//! The [`Provider`] trait defines the interface for a specific LLM provider
//! (Anthropic, OpenAI, Groq, etc.). Each provider handles authentication,
//! model mapping, request formatting, and error classification.
//!
//! The [`ModelRouter`](crate::model_router::ModelRouter) sits above providers
//! and implements [`ApiClient`](crate::api_client::ApiClient) to provide
//! unified access with fallback chains.

use crate::api_client::{ApiError, ApiResponse};
use crate::types::{ModelId, UsageSummary};
use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

/// Error classification for provider-level errors.
///
/// Determines whether an error is retryable and whether fallback to
/// another provider should be attempted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderErrorKind {
    /// Transient error — safe to retry.
    Retryable,
    /// Authentication error — retry won't help without config change.
    Auth,
    /// Rate limit — retry after backoff.
    RateLimit,
    /// Invalid model or request — retry won't help.
    InvalidRequest,
    /// Provider is unavailable — fallback eligible.
    Unavailable,
    /// Unknown error — conservative, don't retry.
    Unknown,
}

/// A provider-specific error with classification.
#[derive(Debug, Clone)]
pub struct ProviderError {
    /// The underlying error message.
    pub message: String,
    /// Classification of the error.
    pub kind: ProviderErrorKind,
    /// Which provider emitted this error.
    pub provider: String,
}

impl ProviderError {
    /// Whether this error is safe to retry.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self.kind,
            ProviderErrorKind::Retryable
                | ProviderErrorKind::RateLimit
                | ProviderErrorKind::Unavailable
        )
    }

    /// Whether this error makes the provider eligible for fallback.
    #[must_use]
    pub const fn is_fallback_eligible(&self) -> bool {
        matches!(
            self.kind,
            ProviderErrorKind::Unavailable
                | ProviderErrorKind::Retryable
                | ProviderErrorKind::RateLimit
        )
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} provider error: {}", self.provider, self.message)
    }
}

impl std::error::Error for ProviderError {}

/// Provider capability flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProviderCapabilities {
    /// Supports streaming responses.
    pub streaming: bool,
    /// Supports tool use / function calling.
    pub tool_use: bool,
    /// Supports prompt caching.
    pub prompt_caching: bool,
    /// Supports vision / image input.
    pub vision: bool,
}

/// Abstract provider for a specific LLM API.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Provider name (e.g., "anthropic", "openai", "groq").
    fn name(&self) -> &str;

    /// Provider capabilities.
    fn capabilities(&self) -> ProviderCapabilities;

    /// Send a non-streaming message.
    ///
    /// # Errors
    /// Returns a [`ProviderError`] on failure.
    async fn create_message(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        model: &ModelId,
        max_tokens: u32,
    ) -> Result<ApiResponse, ProviderError>;

    /// Send a streaming message.
    ///
    /// # Errors
    /// Returns a [`ProviderError`] on failure. Note that stream errors
    /// are typically emitted as `StreamEvent::Error` within the stream.
    async fn create_message_streaming(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        model: &ModelId,
        max_tokens: u32,
    ) -> Pin<Box<dyn Stream<Item = crate::api_client::StreamEvent> + Send + 'static>>;

    /// Map a generic ModelId to this provider's model string.
    fn resolve_model(&self, model: &ModelId) -> String;

    /// Whether this provider can handle the given model.
    fn supports_model(&self, model: &ModelId) -> bool;

    /// Clone the provider into a boxed trait object.
    fn clone_box(&self) -> Box<dyn Provider>;
}

/// Convert an [`ApiError`] to a [`ProviderError`] with classification.
#[must_use]
pub fn classify_api_error(provider: &str, err: ApiError) -> ProviderError {
    let kind = match &err {
        ApiError::Auth(_) => ProviderErrorKind::Auth,
        ApiError::Response { status, .. } => match status {
            429 => ProviderErrorKind::RateLimit,
            500 | 502 | 503 | 504 => ProviderErrorKind::Unavailable,
            400 | 401 | 403 | 404 => ProviderErrorKind::InvalidRequest,
            _ => ProviderErrorKind::Unknown,
        },
        ApiError::RetryExhausted { .. } => ProviderErrorKind::Unavailable,
        ApiError::Http(_) => ProviderErrorKind::Retryable,
        ApiError::InvalidModel(_) => ProviderErrorKind::InvalidRequest,
    };

    ProviderError {
        message: err.to_string(),
        kind,
        provider: provider.into(),
    }
}

/// Convert a [`ProviderError`] back to an [`ApiError`].
#[must_use]
pub fn provider_error_to_api(err: ProviderError) -> ApiError {
    match err.kind {
        ProviderErrorKind::Auth => ApiError::Auth(err.message),
        ProviderErrorKind::RateLimit | ProviderErrorKind::Unavailable => {
            ApiError::Http(err.message)
        }
        ProviderErrorKind::InvalidRequest => ApiError::InvalidModel(err.message),
        _ => ApiError::Http(err.message),
    }
}
