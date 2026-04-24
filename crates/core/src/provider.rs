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
use crate::tool_executor::ToolInfo;
use crate::types::ModelId;
use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

/// Provider API style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderApiStyle {
    /// Anthropic messages API style.
    AnthropicMessages,
    /// OpenAI chat completions API style.
    OpenAiChat,
    /// OpenAI responses API style.
    OpenAiResponses,
}

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
    /// Request is blocked by provider access policy.
    AccessPolicy,
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
    /// Supports streaming tool deltas / partial tool assembly.
    pub structured_tool_streaming: bool,
    /// Supports issuing multiple tool calls safely in one response.
    pub parallel_tool_calls: bool,
    /// Whether textual tool-call fallback is expected to be useful.
    pub text_tool_fallback: bool,
    /// Supports prompt caching.
    pub prompt_caching: bool,
    /// Supports vision / image input.
    pub vision: bool,
}

/// Resolved provider interaction profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedProviderProfile {
    /// Canonical profile name.
    pub name: &'static str,
    /// Provider API style.
    pub api_style: ProviderApiStyle,
    /// Provider capability matrix.
    pub capabilities: ProviderCapabilities,
}

impl ResolvedProviderProfile {
    /// Resolve a provider profile from explicit hint and base URL.
    #[must_use]
    pub fn resolve(
        profile_hint: Option<&str>,
        base_url: &str,
        default_style: ProviderApiStyle,
    ) -> Self {
        let explicit = profile_hint.unwrap_or_default().to_ascii_lowercase();
        let host = base_url.to_ascii_lowercase();
        let name = if explicit.contains("dashscope") || host.contains("dashscope") {
            "dashscope"
        } else if explicit.contains("kimi")
            || host.contains("kimi.com")
            || host.contains("moonshot")
        {
            "kimi"
        } else if explicit.contains("groq") || host.contains("groq") {
            "groq"
        } else if explicit.contains("openai") || host.contains("openai.com") {
            "openai"
        } else if explicit.contains("anthropic") || host.contains("anthropic.com") {
            "anthropic"
        } else {
            "custom"
        };

        match (name, default_style) {
            ("anthropic", ProviderApiStyle::AnthropicMessages) => Self {
                name,
                api_style: ProviderApiStyle::AnthropicMessages,
                capabilities: ProviderCapabilities {
                    streaming: true,
                    tool_use: true,
                    structured_tool_streaming: true,
                    parallel_tool_calls: true,
                    text_tool_fallback: true,
                    prompt_caching: true,
                    vision: true,
                },
            },
            ("dashscope", ProviderApiStyle::AnthropicMessages) => Self {
                name,
                api_style: ProviderApiStyle::AnthropicMessages,
                capabilities: ProviderCapabilities {
                    streaming: true,
                    tool_use: true,
                    structured_tool_streaming: true,
                    parallel_tool_calls: true,
                    text_tool_fallback: true,
                    prompt_caching: true,
                    vision: true,
                },
            },
            ("dashscope", ProviderApiStyle::OpenAiChat) => Self {
                name,
                api_style: ProviderApiStyle::OpenAiChat,
                capabilities: ProviderCapabilities {
                    streaming: true,
                    tool_use: true,
                    structured_tool_streaming: true,
                    parallel_tool_calls: true,
                    text_tool_fallback: true,
                    prompt_caching: false,
                    vision: false,
                },
            },
            ("kimi", _) => Self {
                name,
                api_style: default_style,
                capabilities: ProviderCapabilities {
                    streaming: true,
                    tool_use: false,
                    structured_tool_streaming: false,
                    parallel_tool_calls: false,
                    text_tool_fallback: true,
                    prompt_caching: false,
                    vision: false,
                },
            },
            ("groq", _) | ("openai", _) | ("custom", _) => Self {
                name,
                api_style: default_style,
                capabilities: ProviderCapabilities {
                    streaming: true,
                    tool_use: true,
                    structured_tool_streaming: true,
                    parallel_tool_calls: true,
                    text_tool_fallback: true,
                    prompt_caching: false,
                    vision: name == "openai",
                },
            },
            _ => Self {
                name,
                api_style: default_style,
                capabilities: ProviderCapabilities {
                    streaming: true,
                    tool_use: true,
                    structured_tool_streaming: true,
                    parallel_tool_calls: true,
                    text_tool_fallback: true,
                    prompt_caching: matches!(default_style, ProviderApiStyle::AnthropicMessages),
                    vision: matches!(default_style, ProviderApiStyle::AnthropicMessages),
                },
            },
        }
    }
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
        tools: &[ToolInfo],
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
        tools: &[ToolInfo],
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
        ApiError::AccessPolicy(_) => ProviderErrorKind::AccessPolicy,
        ApiError::Response { status, .. } => match status {
            401 => ProviderErrorKind::Auth,
            403 if matches_access_policy_message(&err.to_string()) => {
                ProviderErrorKind::AccessPolicy
            }
            429 => ProviderErrorKind::RateLimit,
            500 | 502 | 503 | 504 => ProviderErrorKind::Unavailable,
            400 | 403 | 404 => ProviderErrorKind::InvalidRequest,
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
        ProviderErrorKind::AccessPolicy => ApiError::AccessPolicy(err.message),
        ProviderErrorKind::RateLimit | ProviderErrorKind::Unavailable => {
            ApiError::Http(err.message)
        }
        ProviderErrorKind::InvalidRequest => ApiError::InvalidModel(err.message),
        _ => ApiError::Http(err.message),
    }
}

fn matches_access_policy_message(message: &str) -> bool {
    let lowered = message.to_ascii_lowercase();
    lowered.contains("coding agent")
        || lowered.contains("coding agents")
        || lowered.contains("access policy")
        || lowered.contains("policy denied")
        || lowered.contains("not allowed for this api key")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_kimi_profile_disables_structured_tools() {
        let profile = ResolvedProviderProfile::resolve(
            Some("kimi"),
            "https://api.kimi.com/coding/",
            ProviderApiStyle::AnthropicMessages,
        );

        assert_eq!(profile.name, "kimi");
        assert!(!profile.capabilities.tool_use);
        assert!(!profile.capabilities.structured_tool_streaming);
        assert!(profile.capabilities.text_tool_fallback);
    }

    #[test]
    fn classify_access_policy_errors() {
        let err = classify_api_error(
            "anthropic",
            ApiError::Response {
                status: 403,
                message: "This API is only available for Coding Agents".into(),
            },
        );

        assert_eq!(err.kind, ProviderErrorKind::AccessPolicy);
        assert!(!err.is_fallback_eligible());
    }
}
