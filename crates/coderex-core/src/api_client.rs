//! Anthropic API client with retry and model routing.

use crate::types::{ModelId, UsageSummary};
use futures::Stream;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use thiserror::Error;
use tokio::time::{sleep, Duration};

/// Errors from API operations.
#[derive(Debug, Error)]
pub enum ApiError {
    /// HTTP request failed.
    #[error("HTTP error: {0}")]
    Http(String),
    /// Authentication failed or missing API key.
    #[error("authentication error: {0}")]
    Auth(String),
    /// API returned an error status.
    #[error("API error ({status}): {message}")]
    Response {
        /// HTTP status code.
        status: u16,
        /// Error message from API.
        message: String,
    },
    /// Retry attempts exhausted.
    #[error("retry exhausted after {attempts} attempts")]
    RetryExhausted {
        /// Number of retry attempts.
        attempts: u32,
        /// The last error message encountered.
        last_error_message: String,
    },
    /// Invalid model identifier.
    #[error("invalid model: {0}")]
    InvalidModel(String),
}

impl Clone for ApiError {
    fn clone(&self) -> Self {
        match self {
            Self::Http(msg) => Self::Http(msg.clone()),
            Self::Auth(msg) => Self::Auth(msg.clone()),
            Self::Response { status, message } => Self::Response {
                status: *status,
                message: message.clone(),
            },
            Self::RetryExhausted { attempts, last_error_message } => Self::RetryExhausted {
                attempts: *attempts,
                last_error_message: last_error_message.clone(),
            },
            Self::InvalidModel(msg) => Self::InvalidModel(msg.clone()),
        }
    }
}

impl From<reqwest::Error> for ApiError {
    fn from(e: reqwest::Error) -> Self {
        Self::Http(e.to_string())
    }
}

/// Configuration for the API client.
#[derive(Debug, Clone)]
pub struct ApiConfig {
    /// Anthropic API key.
    pub api_key: String,
    /// Base URL for the API (default: https://api.anthropic.com).
    pub base_url: String,
    /// Maximum number of retry attempts for transient failures.
    pub max_retries: u32,
    /// Initial backoff duration for retries.
    pub retry_backoff: Duration,
}

impl ApiConfig {
    /// Create a config with the given API key and defaults.
    #[must_use]
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            base_url: "https://api.anthropic.com".into(),
            max_retries: 3,
            retry_backoff: Duration::from_secs(1),
        }
    }

    /// Create a config with custom settings.
    #[must_use]
    pub const fn with_retries(mut self, max_retries: u32, backoff: Duration) -> Self {
        self.max_retries = max_retries;
        self.retry_backoff = backoff;
        self
    }
}

/// Result from a non-streaming API call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiResponse {
    /// The text content from the assistant.
    pub content: String,
    /// Token usage summary.
    pub usage: UsageSummary,
    /// Whether the response was stopped due to token limit.
    pub stop_reason: String,
}

/// Result from a streaming API call.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A text delta chunk.
    TextDelta(String),
    /// A tool call block start.
    ToolCallBlockStart {
        /// Tool call ID.
        id: String,
        /// Tool name.
        name: String,
    },
    /// A tool call input delta.
    ToolCallInputDelta {
        /// Tool call ID.
        id: String,
        /// JSON input delta fragment.
        delta: String,
    },
    /// A tool call block completed.
    ToolCallBlockEnd {
        /// Tool call ID.
        id: String,
        /// Complete JSON input.
        input: serde_json::Value,
    },
    /// Token usage summary (emitted at end of stream).
    UsageSummary(UsageSummary),
    /// Stream completed.
    StreamEnd,
    /// Error during streaming.
    Error(ApiError),
}

/// Trait for API clients.
///
/// This abstraction allows swapping between different API providers
/// or mocking for tests.
#[async_trait::async_trait]
pub trait ApiClient: Send + Sync {
    /// Clone the trait object into a boxed clone.
    fn clone_box(&self) -> Box<dyn ApiClient>;

    /// Send a non-streaming message to the API.
    async fn create_message(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        model: &ModelId,
        max_tokens: u32,
    ) -> Result<ApiResponse, ApiError>;

    /// Send a streaming message to the API.
    ///
    /// Returns a stream of `StreamEvent` values.
    async fn create_message_streaming(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        model: &ModelId,
        max_tokens: u32,
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>>;
}

/// Anthropic API client implementation.
///
/// Supports:
/// - API key authentication via `x-api-key` header
/// - Anthropic version header
/// - Exponential backoff retry for transient failures
/// - Model routing based on `ModelId`
#[derive(Debug)]
pub struct AnthropicApiClient {
    config: ApiConfig,
    client: Client,
}

impl AnthropicApiClient {
    /// Create a new Anthropic API client.
    #[must_use]
    pub fn new(config: ApiConfig) -> Self {
        Self {
            config,
            client: Client::new(),
        }
    }

    /// Resolve the model string from a ModelId.
    fn resolve_model(model: &ModelId) -> &str {
        &model.0
    }

    /// Execute a request with retry logic.
    async fn with_retry<F, Fut, T>(&self, operation: F) -> Result<T, ApiError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T, ApiError>>,
    {
        let mut last_error = None;
        for attempt in 0..=self.config.max_retries {
            if attempt > 0 {
                let backoff = self.config.retry_backoff * 2_u32.pow(attempt - 1);
                tracing::debug!("API retry {attempt}/{}, backing off {backoff:?}", self.config.max_retries);
                sleep(backoff).await;
            }

            match operation().await {
                Ok(value) => return Ok(value),
                Err(e) => {
                    if is_retryable(&e) {
                        last_error = Some(e);
                    } else {
                        return Err(e);
                    }
                }
            }
        }

        Err(ApiError::RetryExhausted {
            attempts: self.config.max_retries + 1,
            last_error_message: last_error.unwrap().to_string(),
        })
    }
}

#[async_trait::async_trait]
impl ApiClient for AnthropicApiClient {
    fn clone_box(&self) -> Box<dyn ApiClient> {
        Box::new(AnthropicApiClient {
            config: self.config.clone(),
            client: self.client.clone(),
        })
    }

    async fn create_message(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        model: &ModelId,
        max_tokens: u32,
    ) -> Result<ApiResponse, ApiError> {
        if self.config.api_key.is_empty() {
            return Err(ApiError::Auth(
                "ANTHROPIC_API_KEY environment variable is not set".into(),
            ));
        }

        let url = format!(
            "{}/v1/messages",
            self.config.base_url.strip_suffix('/').unwrap_or(&self.config.base_url)
        );

        let model_name = Self::resolve_model(model);

        let body = serde_json::json!({
            "model": model_name,
            "system": system,
            "messages": messages,
            "max_tokens": max_tokens,
        });

        let response = self
            .with_retry(|| async {
                let resp = self
                    .client
                    .post(&url)
                    .header("x-api-key", &self.config.api_key)
                    .header("anthropic-version", "2023-06-01")
                    .header("content-type", "application/json")
                    .json(&body)
                    .send()
                    .await
                    .map_err(ApiError::from)?;

                let status = resp.status().as_u16();
                if status >= 400 {
                    let err_text = resp.text().await.unwrap_or_default();
                    return Err(ApiError::Response {
                        status,
                        message: err_text,
                    });
                }

                let json: serde_json::Value = resp.json().await.map_err(ApiError::from)?;

                let content = json["content"]
                    .as_array()
                    .and_then(|arr| arr.first())
                    .and_then(|block| block["text"].as_str())
                    .unwrap_or("")
                    .to_string();

                let stop_reason = json["stop_reason"]
                    .as_str()
                    .unwrap_or("unknown")
                    .to_string();

                #[allow(clippy::cast_possible_truncation)]
                let usage = UsageSummary {
                    input_tokens: json["usage"]["input_tokens"].as_u64().unwrap_or(0) as usize,
                    output_tokens: json["usage"]["output_tokens"].as_u64().unwrap_or(0) as usize,
                    cache_read_tokens: json["usage"]["cache_read_input_tokens"].as_u64().unwrap_or(0) as usize,
                    cache_creation_tokens: json["usage"]["cache_creation_input_tokens"].as_u64().unwrap_or(0) as usize,
                };

                Ok(ApiResponse {
                    content,
                    usage,
                    stop_reason,
                })
            })
            .await?;

        Ok(response)
    }

    async fn create_message_streaming(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        model: &ModelId,
        max_tokens: u32,
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
        if self.config.api_key.is_empty() {
            return Box::pin(futures::stream::once(async {
                StreamEvent::Error(ApiError::Auth(
                    "ANTHROPIC_API_KEY environment variable is not set".into(),
                ))
            }));
        }

        let url = format!(
            "{}/v1/messages",
            self.config.base_url.strip_suffix('/').unwrap_or(&self.config.base_url)
        );

        let model_name = Self::resolve_model(model).to_string();
        let body = serde_json::json!({
            "model": model_name,
            "system": system,
            "messages": messages,
            "max_tokens": max_tokens,
            "stream": true,
        });

        let api_key = self.config.api_key.clone();
        let client = self.client.clone();

        Box::pin(futures::stream::once(async move {
            let response = client
                .post(&url)
                .header("x-api-key", &api_key)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .json(&body)
                .send()
                .await;

            match response {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    if status >= 400 {
                        let err_text = resp.text().await.unwrap_or_default();
                        return futures::stream::once(async move {
                            StreamEvent::Error(ApiError::Response {
                                status,
                                message: err_text,
                            })
                        })
                        .chain(futures::stream::once(async move { StreamEvent::StreamEnd }))
                        .boxed();
                    }

                    parse_sse_stream(resp.bytes_stream()).boxed()
                }
                Err(e) => futures::stream::once(async move { StreamEvent::Error(ApiError::from(e)) })
                    .chain(futures::stream::once(async move { StreamEvent::StreamEnd }))
                    .boxed(),
            }
        })
        .flatten())
    }
}

/// Parse an SSE byte stream into `StreamEvent`s.
///
/// Anthropic SSE format:
/// ```text
/// event: message_start
/// data: {"type":"message_start","message":{...}}
///
/// event: content_block_delta
/// data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"..."}}
///
/// event: message_delta
/// data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{...}}
///
/// event: message_stop
/// data: {"type":"message_stop"}
/// ```
fn parse_sse_stream(
    stream: impl futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
) -> impl futures::Stream<Item = StreamEvent> + Send + 'static {
    use futures::StreamExt;

    // Wrap in Pin so we can use it with unfold state
    let pinned = Box::pin(stream);

    futures::stream::unfold(
        (pinned, String::new()),
        |(mut inner, mut buffer)| async move {
            loop {
                // Check if we have a complete message in the buffer
                if let Some(pos) = find_sse_message_boundary(&buffer) {
                    let message = buffer[..pos].to_string();
                    buffer.drain(..=pos);

                    if let Some(event) = parse_sse_message(&message) {
                        return Some((event, (inner, buffer)));
                    }
                    // Skip unparseable messages, continue loop
                    continue;
                }

                // Need more data — pull from the inner stream
                match inner.next().await {
                    Some(Ok(chunk)) => {
                        let chunk_str =
                            std::str::from_utf8(&chunk).unwrap_or("");
                        buffer.push_str(chunk_str);
                    }
                    Some(Err(e)) => {
                        return Some((
                            StreamEvent::Error(ApiError::from(e)),
                            (inner, buffer),
                        ));
                    }
                    None => {
                        // Stream exhausted
                        return None;
                    }
                }
            }
        },
    )
    .chain(futures::stream::once(async move {
        StreamEvent::StreamEnd
    }))
}

/// Find the position of the next SSE message boundary (`\n\n` or `\r\n\r\n`).
fn find_sse_message_boundary(buffer: &str) -> Option<usize> {
    buffer
        .as_bytes()
        .windows(2)
        .position(|w| w == b"\n\n")
        .map(|pos| pos + 1) // include one newline
}

/// Parse a single SSE message block into a `StreamEvent`.
fn parse_sse_message(message: &str) -> Option<StreamEvent> {
    // Extract the `data:` line
    let data_line = message
        .lines()
        .find(|line| line.starts_with("data: "))?;
    let json_str = data_line.strip_prefix("data: ")?;

    let value: serde_json::Value = serde_json::from_str(json_str).ok()?;
    let event_type = value.get("type").and_then(|v| v.as_str())?;

    match event_type {
        "content_block_delta" => {
            let text = value
                .get("delta")
                .and_then(|d| d.get("text"))
                .and_then(|t| t.as_str())
                .unwrap_or("");
            if text.is_empty() {
                None
            } else {
                Some(StreamEvent::TextDelta(text.to_string()))
            }
        }
        "message_delta" => {
            #[allow(clippy::cast_possible_truncation)]
            let usage = value.get("usage").map(|u| UsageSummary {
                input_tokens: u.get("input_tokens").and_then(serde_json::Value::as_u64).unwrap_or(0) as usize,
                output_tokens: u.get("output_tokens").and_then(serde_json::Value::as_u64).unwrap_or(0) as usize,
                cache_read_tokens: u
                    .get("cache_read_input_tokens")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0) as usize,
                cache_creation_tokens: u
                    .get("cache_creation_input_tokens")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0) as usize,
            });
            usage.map(StreamEvent::UsageSummary)
        }
        _ => None,
    }
}
fn is_retryable(error: &ApiError) -> bool {
    match error {
        ApiError::Http(msg) => {
            msg.contains("timeout") || msg.contains("connect")
        }
        ApiError::Response { status, .. } => {
            *status >= 500 || *status == 429
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_api_key_returns_auth_error() {
        let config = ApiConfig::new(String::new());
        let client = AnthropicApiClient::new(config);

        let result = client
            .create_message("system", &[], &ModelId::sonnet(), 1024)
            .await;

        assert!(matches!(result, Err(ApiError::Auth(_))));
    }

    #[tokio::test]
    async fn config_defaults_are_correct() {
        let config = ApiConfig::new("test_key".into());
        assert_eq!(config.base_url, "https://api.anthropic.com");
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.retry_backoff, Duration::from_secs(1));
    }

    #[test]
    fn is_retryable_detects_server_errors() {
        let err = ApiError::Response {
            status: 503,
            message: "overloaded".into(),
        };
        assert!(is_retryable(&err));
    }

    #[test]
    fn is_retryable_detects_rate_limits() {
        let err = ApiError::Response {
            status: 429,
            message: "rate limited".into(),
        };
        assert!(is_retryable(&err));
    }

    #[test]
    fn is_retryable_rejects_auth_errors() {
        let err = ApiError::Auth("invalid key".into());
        assert!(!is_retryable(&err));
    }

    #[test]
    fn model_routing_resolves_correctly() {
        assert_eq!(AnthropicApiClient::resolve_model(&ModelId::sonnet()), "claude-sonnet-4-6");
        assert_eq!(AnthropicApiClient::resolve_model(&ModelId::opus()), "claude-opus-4-6");
        assert_eq!(AnthropicApiClient::resolve_model(&ModelId::haiku()), "claude-haiku-4-5-20251001");
    }
}
