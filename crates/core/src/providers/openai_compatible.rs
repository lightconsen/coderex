//! OpenAI-compatible API provider.
//!
//! Supports any provider that uses the OpenAI chat completions API format,
//! including OpenAI, Groq, Together, Fireworks, and self-hosted endpoints.

use crate::api_client::{ApiError, ApiResponse, StreamEvent};
use crate::provider::{Provider, ProviderCapabilities, ProviderError, classify_api_error};
use crate::types::ModelId;
use async_trait::async_trait;
use futures::Stream;
use reqwest::Client;
use std::pin::Pin;
use tokio::time::{sleep, Duration};

/// OpenAI-compatible API provider.
#[derive(Debug, Clone)]
pub struct OpenAiCompatibleProvider {
    name: String,
    base_url: String,
    api_key: String,
    client: Client,
    model_map: std::collections::HashMap<String, String>,
    max_retries: u32,
    retry_backoff: Duration,
}

impl OpenAiCompatibleProvider {
    /// Create a new OpenAI-compatible provider.
    ///
    /// # Arguments
    /// * `name` — Provider display name (e.g., "openai", "groq").
    /// * `base_url` — API base URL (e.g., "https://api.openai.com").
    /// * `api_key` — API key for authentication.
    #[must_use]
    pub fn new(name: impl Into<String>, base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            base_url: base_url.into(),
            api_key: api_key.into(),
            client: Client::new(),
            model_map: std::collections::HashMap::new(),
            max_retries: 3,
            retry_backoff: Duration::from_secs(1),
        }
    }

    /// Map a generic model ID to this provider's model string.
    #[must_use]
    pub fn with_model_map(mut self, map: std::collections::HashMap<String, String>) -> Self {
        self.model_map = map;
        self
    }

    /// Set retry policy.
    #[must_use]
    pub const fn with_retries(mut self, max_retries: u32, backoff: Duration) -> Self {
        self.max_retries = max_retries;
        self.retry_backoff = backoff;
        self
    }

    fn resolve_model_inner(&self, model: &ModelId) -> String {
        self.model_map
            .get(&model.0)
            .cloned()
            .unwrap_or_else(|| model.0.clone())
    }

    async fn with_retry<F, Fut, T>(&self, operation: F) -> Result<T, ApiError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T, ApiError>>,
    {
        let mut last_error = None;
        for attempt in 0..=self.max_retries {
            if attempt > 0 {
                let backoff = self.retry_backoff * 2_u32.pow(attempt - 1);
                sleep(backoff).await;
            }
            match operation().await {
                Ok(v) => return Ok(v),
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
            attempts: self.max_retries + 1,
            last_error_message: last_error.unwrap().to_string(),
        })
    }

    fn is_openai_api(&self) -> bool {
        self.base_url.contains("openai.com")
    }
}

fn is_retryable(err: &ApiError) -> bool {
    matches!(
        err,
        ApiError::Http(_) | ApiError::Response { status: 429 | 500 | 502 | 503 | 504, .. }
    )
}

#[async_trait]
impl Provider for OpenAiCompatibleProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_use: true,
            prompt_caching: false,
            vision: self.is_openai_api(),
        }
    }

    async fn create_message(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        model: &ModelId,
        max_tokens: u32,
    ) -> Result<ApiResponse, ProviderError> {
        let url = format!(
            "{}/v1/chat/completions",
            self.base_url.strip_suffix('/').unwrap_or(&self.base_url)
        );
        let model_name = self.resolve_model_inner(model);

        let mut body_messages = vec![serde_json::json!({
            "role": "system",
            "content": system,
        })];
        body_messages.extend(messages.iter().cloned());

        let body = serde_json::json!({
            "model": model_name,
            "messages": body_messages,
            "max_tokens": max_tokens,
        });

        self.with_retry(|| async {
            let req = self
                .client
                .post(&url)
                .header("content-type", "application/json")
                .header("Authorization", format!("Bearer {}", &self.api_key));

            let resp = req.json(&body).send().await.map_err(ApiError::from)?;
            let status = resp.status().as_u16();

            if status >= 400 {
                let err_text = resp.text().await.unwrap_or_default();
                return Err(ApiError::Response {
                    status,
                    message: err_text,
                });
            }

            let json: serde_json::Value = resp.json().await.map_err(ApiError::from)?;

            let content = json["choices"]
                .as_array()
                .and_then(|arr| arr.first())
                .and_then(|c| c["message"]["content"].as_str())
                .unwrap_or("")
                .to_string();

            let stop_reason = json["choices"]
                .as_array()
                .and_then(|arr| arr.first())
                .and_then(|c| c["finish_reason"].as_str())
                .unwrap_or("unknown")
                .to_string();

            #[allow(clippy::cast_possible_truncation)]
            let usage = crate::types::UsageSummary {
                input_tokens: json["usage"]["prompt_tokens"].as_u64().unwrap_or(0) as usize,
                output_tokens: json["usage"]["completion_tokens"].as_u64().unwrap_or(0) as usize,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            };

            Ok(ApiResponse {
                content,
                tool_calls: Vec::new(),
                usage,
                stop_reason,
            })
        })
        .await
        .map_err(|e| classify_api_error(self.name(), e))
    }

    async fn create_message_streaming(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        model: &ModelId,
        max_tokens: u32,
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        // For now, delegate to non-streaming and emit as a single chunk
        // A full implementation would use SSE streaming
        let result = self.create_message(system, messages, model, max_tokens).await;
        match result {
            Ok(resp) => {
                let events = vec![
                    StreamEvent::TextDelta(resp.content),
                    StreamEvent::UsageSummary(resp.usage),
                    StreamEvent::StreamEnd,
                ];
                Box::pin(futures::stream::iter(events))
            }
            Err(e) => {
                let api_err = crate::provider::provider_error_to_api(e);
                Box::pin(futures::stream::iter(vec![StreamEvent::Error(api_err)]))
            }
        }
    }

    fn resolve_model(&self, model: &ModelId) -> String {
        self.resolve_model_inner(model)
    }

    fn supports_model(&self, _model: &ModelId) -> bool {
        true
    }

    fn clone_box(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}
