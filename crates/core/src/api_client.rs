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
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>>;
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

    /// Returns true if this is the official Anthropic API.
    fn is_anthropic_api(&self) -> bool {
        let url = self.config.base_url.strip_suffix('/').unwrap_or(&self.config.base_url);
        url == "https://api.anthropic.com"
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
                let mut req = self
                    .client
                    .post(&url)
                    .header("content-type", "application/json");

                if self.is_anthropic_api() {
                    req = req
                        .header("x-api-key", &self.config.api_key)
                        .header("anthropic-version", "2023-06-01");
                } else {
                    req = req.header("Authorization", format!("Bearer {}", &self.config.api_key));
                }

                let resp = req
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
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
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
        let api_key = self.config.api_key.clone();
        let is_anthropic = self.is_anthropic_api();
        let body = serde_json::json!({
            "model": model_name.clone(),
            "system": system,
            "messages": messages,
            "max_tokens": max_tokens,
            "stream": true,
        });
        let client = self.client.clone();

        Box::pin(futures::stream::once(async move {
            let mut req = client
                .post(&url)
                .header("content-type", "application/json");

            if is_anthropic {
                req = req
                    .header("x-api-key", &api_key)
                    .header("anthropic-version", "2023-06-01");
            } else {
                req = req.header("Authorization", format!("Bearer {}", &api_key));
            }

            let response = req.json(&body).send().await;

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
/// State carried across SSE message parses to track content block indices.
#[derive(Debug, Default)]
struct SseParserState {
    /// Maps content block index -> tool_call_id for tool_use blocks.
    index_to_id: std::collections::HashMap<usize, String>,
}

fn parse_sse_stream(
    stream: impl futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
) -> impl futures::Stream<Item = StreamEvent> + Send + 'static {
    use futures::StreamExt;

    let pinned = Box::pin(stream);

    futures::stream::unfold(
        (pinned, String::new(), SseParserState::default()),
        |(mut inner, mut buffer, mut state)| async move {
            loop {
                if let Some(pos) = find_sse_message_boundary(&buffer) {
                    let message = buffer[..pos].to_string();
                    buffer.drain(..=pos);

                    if let Some(event) = parse_sse_message(&message, &mut state) {
                        return Some((event, (inner, buffer, state)));
                    }
                    continue;
                }

                match inner.next().await {
                    Some(Ok(chunk)) => {
                        // Lossy decode: invalid UTF-8 sequences become U+FFFD
                        // instead of silently dropping bytes.
                        buffer.push_str(&String::from_utf8_lossy(&chunk));
                    }
                    Some(Err(e)) => {
                        tracing::warn!("SSE stream network error: {e}");
                        return Some((
                            StreamEvent::Error(ApiError::from(e)),
                            (inner, buffer, state),
                        ));
                    }
                    None => {
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
fn parse_sse_message(message: &str, state: &mut SseParserState) -> Option<StreamEvent> {
    // Extract the `data:` line (accept both "data: " and "data:")
    let data_line = message
        .lines()
        .find(|line| line.starts_with("data:"))?;
    let json_str = data_line.strip_prefix("data:").map(|s| s.trim_start())?;

    let value: serde_json::Value = serde_json::from_str(json_str).ok()?;
    let event_type = value.get("type").and_then(|v| v.as_str())?;

    match event_type {
        "content_block_start" => {
            let index = value.get("index").and_then(serde_json::Value::as_u64)? as usize;
            let block = value.get("content_block")?;
            let block_type = block.get("type")?.as_str()?;
            match block_type {
                "tool_use" => {
                    let id = block.get("id")?.as_str()?.to_string();
                    let name = block.get("name")?.as_str()?.to_string();
                    state.index_to_id.insert(index, id.clone());
                    Some(StreamEvent::ToolCallBlockStart { id, name })
                }
                _ => None,
            }
        }
        "content_block_delta" => {
            let index = value.get("index").and_then(serde_json::Value::as_u64)? as usize;
            let delta = value.get("delta")?;
            let delta_type = delta.get("type")?.as_str()?;
            match delta_type {
                "text_delta" => {
                    let text = delta.get("text")?.as_str()?;
                    if text.is_empty() {
                        None
                    } else {
                        Some(StreamEvent::TextDelta(text.to_string()))
                    }
                }
                "input_json_delta" => {
                    let id = state.index_to_id.get(&index)?.clone();
                    let partial_json = delta.get("partial_json")?.as_str()?;
                    Some(StreamEvent::ToolCallInputDelta {
                        id,
                        delta: partial_json.to_string(),
                    })
                }
                _ => None,
            }
        }
        "content_block_stop" => {
            let index = value.get("index").and_then(serde_json::Value::as_u64)? as usize;
            let id = state.index_to_id.remove(&index)?.clone();
            Some(StreamEvent::ToolCallBlockEnd {
                id,
                input: serde_json::Value::Null,
            })
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

    // ─── SSE parser tests ───────────────────────────────────────────────

    #[test]
    fn parse_sse_message_text_delta() {
        let msg = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n";
        let mut state = SseParserState::default();
        let event = parse_sse_message(msg, &mut state);
        assert!(matches!(event, Some(StreamEvent::TextDelta(ref t)) if t == "Hello"));
    }

    #[test]
    fn parse_sse_message_tool_use_start() {
        let msg = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01ABC\",\"name\":\"read\",\"input\":{}}}\n\n";
        let mut state = SseParserState::default();
        let event = parse_sse_message(msg, &mut state);
        assert!(
            matches!(event, Some(StreamEvent::ToolCallBlockStart { ref id, ref name }) if id == "toolu_01ABC" && name == "read")
        );
        assert_eq!(state.index_to_id.get(&1), Some(&"toolu_01ABC".to_string()));
    }

    #[test]
    fn parse_sse_message_input_json_delta() {
        let mut state = SseParserState::default();
        state.index_to_id.insert(1, "toolu_01ABC".to_string());

        let msg = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"file\\\":\\\"test.txt\\\"}\"}}\n\n";
        let event = parse_sse_message(msg, &mut state);
        assert!(
            matches!(event, Some(StreamEvent::ToolCallInputDelta { ref id, ref delta }) if id == "toolu_01ABC" && delta == "{\"file\":\"test.txt\"}")
        );
    }

    #[test]
    fn parse_sse_message_content_block_stop() {
        let mut state = SseParserState::default();
        state.index_to_id.insert(1, "toolu_01ABC".to_string());

        let msg = "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n";
        let event = parse_sse_message(msg, &mut state);
        assert!(
            matches!(event, Some(StreamEvent::ToolCallBlockEnd { ref id, ref input }) if id == "toolu_01ABC" && input.is_null())
        );
        assert!(!state.index_to_id.contains_key(&1));
    }

    #[test]
    fn parse_sse_message_usage_summary() {
        let msg = "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":100,\"output_tokens\":50}}\n\n";
        let mut state = SseParserState::default();
        let event = parse_sse_message(msg, &mut state);
        assert!(
            matches!(event, Some(StreamEvent::UsageSummary(ref u)) if u.input_tokens == 100 && u.output_tokens == 50)
        );
    }

    #[test]
    fn parse_sse_message_ignores_unknown_types() {
        let msg = "event: ping\ndata: {\"type\":\"ping\"}\n\n";
        let mut state = SseParserState::default();
        let event = parse_sse_message(msg, &mut state);
        assert!(event.is_none());
    }

    #[test]
    fn parse_sse_message_ignores_text_block_start() {
        let msg = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n";
        let mut state = SseParserState::default();
        let event = parse_sse_message(msg, &mut state);
        assert!(event.is_none());
    }

    #[tokio::test]
    async fn parse_sse_stream_full_tool_call_sequence() {
        let sse = r#"event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"I'll read the file."}}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_01XYZ","name":"read","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"file\":\"hello.txt\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":10,"output_tokens":20}}

event: message_stop
data: {"type":"message_stop"}

"#;

        let stream = futures::stream::iter(vec![Ok(bytes::Bytes::from(sse))]);
        let events: Vec<StreamEvent> = parse_sse_stream(stream).collect().await;

        assert_eq!(events.len(), 6); // text + tool_start + tool_delta + tool_end + usage + stream_end
        assert!(matches!(&events[0], StreamEvent::TextDelta(t) if t == "I'll read the file."));
        assert!(matches!(&events[1], StreamEvent::ToolCallBlockStart { id, name } if id == "toolu_01XYZ" && name == "read"));
        assert!(matches!(&events[2], StreamEvent::ToolCallInputDelta { id, delta } if id == "toolu_01XYZ" && delta == "{\"file\":\"hello.txt\"}"));
        assert!(matches!(&events[3], StreamEvent::ToolCallBlockEnd { id, .. } if id == "toolu_01XYZ"));
        assert!(matches!(&events[4], StreamEvent::UsageSummary(u) if u.input_tokens == 10 && u.output_tokens == 20));
    }

    #[tokio::test]
    async fn parse_sse_stream_stress_large_payload() {
        let large_text = "x".repeat(100_000);
        let sse = format!(
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{}\"}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
            large_text
        );

        let stream = futures::stream::iter(vec![Ok(bytes::Bytes::from(sse))]);
        let events: Vec<StreamEvent> = parse_sse_stream(stream).collect().await;

        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], StreamEvent::TextDelta(t) if t.len() == 100_000));
    }

    #[tokio::test]
    async fn parse_sse_stream_stress_fragmented_chunks() {
        // Send the SSE payload one byte at a time to stress the buffer accumulation
        let sse = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" fragmented \"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> =
            sse.bytes().map(|b| Ok(bytes::Bytes::from(vec![b]))).collect();

        let stream = futures::stream::iter(chunks);
        let events: Vec<StreamEvent> = parse_sse_stream(stream).collect().await;

        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], StreamEvent::TextDelta(t) if t == " fragmented "));
    }

    #[tokio::test]
    async fn parse_sse_stream_stress_multiple_tool_calls() {
        let mut sse = String::new();
        for i in 0..50 {
            sse.push_str(&format!(
                "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":{i},\"content_block\":{{\"type\":\"tool_use\",\"id\":\"tool_{i}\",\"name\":\"read\",\"input\":{{}}}}}}\n\n"
            ));
            sse.push_str(&format!(
                "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":{i},\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"{{\\\"file\\\":\\\"f{i}.txt\\\"}}\"}}}}\n\n"
            ));
            sse.push_str(&format!(
                "event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":{i}}}\n\n"
            ));
        }
        sse.push_str("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");

        let stream = futures::stream::iter(vec![Ok(bytes::Bytes::from(sse))]);
        let events: Vec<StreamEvent> = parse_sse_stream(stream).collect().await;

        let tool_starts = events.iter().filter(|e| matches!(e, StreamEvent::ToolCallBlockStart { .. })).count();
        let tool_deltas = events.iter().filter(|e| matches!(e, StreamEvent::ToolCallInputDelta { .. })).count();
        let tool_ends = events.iter().filter(|e| matches!(e, StreamEvent::ToolCallBlockEnd { .. })).count();

        assert_eq!(tool_starts, 50);
        assert_eq!(tool_deltas, 50);
        assert_eq!(tool_ends, 50);
    }

    #[tokio::test]
    async fn parse_sse_stream_handles_malformed_data_gracefully() {
        let sse = "event: content_block_delta\ndata: not valid json\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n";
        let stream = futures::stream::iter(vec![Ok(bytes::Bytes::from(sse))]);
        let events: Vec<StreamEvent> = parse_sse_stream(stream).collect().await;

        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], StreamEvent::TextDelta(t) if t == "ok"));
    }

    #[tokio::test]
    async fn parse_sse_stream_empty_body() {
        let stream = futures::stream::iter(vec![Ok(bytes::Bytes::from(""))]);
        let events: Vec<StreamEvent> = parse_sse_stream(stream).collect().await;
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], StreamEvent::StreamEnd));
    }
}
