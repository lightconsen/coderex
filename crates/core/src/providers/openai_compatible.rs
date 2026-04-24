//! OpenAI-compatible API provider.
//!
//! Supports any provider that uses the OpenAI chat completions API format,
//! including OpenAI, Groq, Together, Fireworks, and self-hosted endpoints.

use crate::api_client::{ApiError, ApiResponse, StreamEvent};
use crate::provider::{
    Provider, ProviderApiStyle, ProviderCapabilities, ProviderError, ResolvedProviderProfile,
    classify_api_error,
};
use crate::types::{ModelId, ToolCall};
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use reqwest::Client;
use std::collections::{BTreeMap, HashMap};
use std::pin::Pin;
use tokio::time::{Duration, sleep};
use uuid::Uuid;

/// OpenAI-compatible API provider.
#[derive(Debug, Clone)]
pub struct OpenAiCompatibleProvider {
    name: String,
    base_url: String,
    api_key: String,
    client: Client,
    model_map: std::collections::HashMap<String, String>,
    provider_profile: Option<String>,
    default_headers: BTreeMap<String, String>,
    session_id: Option<String>,
    device_id: Option<String>,
    client_name: String,
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
    pub fn new(
        name: impl Into<String>,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            base_url: base_url.into(),
            api_key: api_key.into(),
            client: Client::new(),
            model_map: std::collections::HashMap::new(),
            provider_profile: None,
            default_headers: BTreeMap::new(),
            session_id: std::env::var("CODEREX_SESSION_ID").ok(),
            device_id: std::env::var("CODEREX_DEVICE_ID").ok(),
            client_name: "cli".into(),
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

    /// Set a provider profile hint used to shape transport headers/metadata.
    #[must_use]
    pub fn with_provider_profile(mut self, profile: impl Into<String>) -> Self {
        self.provider_profile = Some(profile.into());
        self
    }

    /// Add default headers to each request.
    #[must_use]
    pub fn with_default_headers(mut self, headers: BTreeMap<String, String>) -> Self {
        self.default_headers = headers;
        self
    }

    /// Set explicit session/device context used in headers and metadata.
    #[must_use]
    pub fn with_session_context(
        mut self,
        session_id: impl Into<String>,
        device_id: impl Into<String>,
    ) -> Self {
        self.session_id = Some(session_id.into());
        self.device_id = Some(device_id.into());
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

    fn resolved_profile(&self) -> ResolvedProviderProfile {
        ResolvedProviderProfile::resolve(
            self.provider_profile.as_deref(),
            &self.base_url,
            ProviderApiStyle::OpenAiChat,
        )
    }

    fn log_request_start(
        &self,
        request_id: &str,
        model: &str,
        streaming: bool,
        tool_count: usize,
        tool_fallback: bool,
    ) {
        tracing::info!(
            provider = self.name(),
            profile = self.resolved_profile().name,
            request_id = %request_id,
            model,
            base_url = %self.base_url,
            streaming,
            tool_count,
            tool_fallback,
            "provider request started"
        );
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

    fn user_agent() -> String {
        format!(
            "CodeRex/{} ({}; {})",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    }

    fn apply_standard_headers(
        &self,
        mut req: reqwest::RequestBuilder,
        request_id: &str,
    ) -> reqwest::RequestBuilder {
        req = req
            .header("User-Agent", Self::user_agent())
            .header("X-CodeRex-Client", &self.client_name)
            .header("X-CodeRex-Request-Id", request_id);
        if let Some(session_id) = self
            .session_id
            .clone()
            .or_else(|| std::env::var("CODEREX_SESSION_ID").ok())
        {
            if !session_id.trim().is_empty() {
                req = req.header("X-CodeRex-Session-Id", session_id);
            }
        }
        if self.resolved_profile().name == "dashscope" {
            req = req
                .header("X-DashScope-UserAgent", Self::user_agent())
                .header("X-DashScope-AuthType", "api_key")
                .header("X-DashScope-CacheControl", "enable");
        }
        for (k, v) in &self.default_headers {
            req = req.header(k, v);
        }
        req
    }

    fn attach_metadata(&self, body: &mut serde_json::Value, request_id: &str) {
        if let Some(obj) = body.as_object_mut() {
            if obj.contains_key("metadata") {
                return;
            }
            let session_id = self
                .session_id
                .clone()
                .or_else(|| std::env::var("CODEREX_SESSION_ID").ok())
                .unwrap_or_else(|| "unknown".to_string());
            let device_id = self
                .device_id
                .clone()
                .or_else(|| std::env::var("CODEREX_DEVICE_ID").ok())
                .unwrap_or_else(|| "unknown".to_string());
            obj.insert(
                "metadata".to_string(),
                serde_json::json!({
                    "sessionId": session_id,
                    "deviceId": device_id,
                    "requestId": request_id,
                }),
            );
        }
    }

    fn build_request_body(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        model: &ModelId,
        max_tokens: u32,
        tools: &[crate::tool_executor::ToolInfo],
        request_id: &str,
        stream: bool,
    ) -> serde_json::Value {
        let mut body_messages = vec![serde_json::json!({
            "role": "system",
            "content": system,
        })];
        body_messages.extend(messages.iter().cloned());

        let mut body = serde_json::json!({
            "model": self.resolve_model_inner(model),
            "messages": body_messages,
            "max_tokens": max_tokens,
        });
        if stream {
            body.as_object_mut()
                .unwrap()
                .insert("stream".into(), true.into());
            body.as_object_mut().unwrap().insert(
                "stream_options".into(),
                serde_json::json!({
                    "include_usage": true
                }),
            );
        }
        self.attach_metadata(&mut body, request_id);

        if !tools.is_empty() && self.resolved_profile().capabilities.tool_use {
            let tools_json: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.input_schema,
                        }
                    })
                })
                .collect();
            body.as_object_mut()
                .unwrap()
                .insert("tools".to_string(), serde_json::Value::Array(tools_json));
            body.as_object_mut()
                .unwrap()
                .insert("tool_choice".to_string(), "auto".into());
        }

        body
    }

    fn parse_usage(value: &serde_json::Value) -> crate::types::UsageSummary {
        #[allow(clippy::cast_possible_truncation)]
        {
            crate::types::UsageSummary {
                input_tokens: value["prompt_tokens"].as_u64().unwrap_or(0) as usize,
                output_tokens: value["completion_tokens"].as_u64().unwrap_or(0) as usize,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            }
        }
    }

    fn parse_response_json(&self, json: serde_json::Value) -> ApiResponse {
        let choice = json["choices"].as_array().and_then(|arr| arr.first());
        let message = choice.and_then(|c| c["message"].as_object());

        let content = message
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();

        let stop_reason = choice
            .and_then(|c| c["finish_reason"].as_str())
            .unwrap_or("unknown")
            .to_string();

        let mut tool_calls = Vec::new();
        if let Some(tool_calls_arr) = message
            .and_then(|m| m.get("tool_calls"))
            .and_then(|t| t.as_array())
        {
            for tc in tool_calls_arr {
                let id = tc["id"].as_str().unwrap_or("").to_string();
                let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
                let args_str = tc["function"]["arguments"].as_str().unwrap_or("{}");
                let input = serde_json::from_str(args_str)
                    .unwrap_or_else(|_| serde_json::json!({"raw": args_str}));
                if !id.is_empty() && !name.is_empty() {
                    tool_calls.push(ToolCall { id, name, input });
                }
            }
        }

        let usage = Self::parse_usage(&json["usage"]);

        ApiResponse {
            content,
            tool_calls,
            usage,
            stop_reason,
        }
    }

    fn response_to_stream_events(resp: ApiResponse) -> Vec<StreamEvent> {
        let mut events = Vec::new();
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
}

fn is_retryable(err: &ApiError) -> bool {
    matches!(
        err,
        ApiError::Http(_)
            | ApiError::Response {
                status: 429 | 500 | 502 | 503 | 504,
                ..
            }
    )
}

#[async_trait]
impl Provider for OpenAiCompatibleProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> ProviderCapabilities {
        let mut capabilities = self.resolved_profile().capabilities;
        capabilities.vision = self.is_openai_api();
        capabilities
    }

    async fn create_message(
        &self,
        system: &str,
        messages: &[serde_json::Value],
        model: &ModelId,
        max_tokens: u32,
        tools: &[crate::tool_executor::ToolInfo],
    ) -> Result<ApiResponse, ProviderError> {
        let url = format!(
            "{}/v1/chat/completions",
            self.base_url.strip_suffix('/').unwrap_or(&self.base_url)
        );
        let request_id = Uuid::new_v4().to_string();
        let capabilities = self.capabilities();
        let tool_fallback = !tools.is_empty() && !capabilities.tool_use;
        self.log_request_start(
            &request_id,
            &self.resolve_model_inner(model),
            false,
            if capabilities.tool_use {
                tools.len()
            } else {
                0
            },
            tool_fallback,
        );
        let body = self.build_request_body(
            system,
            messages,
            model,
            max_tokens,
            if capabilities.tool_use { tools } else { &[] },
            &request_id,
            false,
        );

        self.with_retry(|| async {
            let req = self
                .client
                .post(&url)
                .header("content-type", "application/json")
                .header("Authorization", format!("Bearer {}", &self.api_key));
            let req = self.apply_standard_headers(req, &request_id);

            let resp = req.json(&body).send().await.map_err(ApiError::from)?;
            let status = resp.status().as_u16();
            let server_request_id = resp
                .headers()
                .get("x-request-id")
                .or_else(|| resp.headers().get("request-id"))
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);

            if status >= 400 {
                let err_text = resp.text().await.unwrap_or_default();
                if err_text.to_ascii_lowercase().contains("coding agent")
                    || err_text.to_ascii_lowercase().contains("access policy")
                {
                    return Err(ApiError::AccessPolicy(err_text));
                }
                return Err(ApiError::Response {
                    status,
                    message: err_text,
                });
            }

            let json: serde_json::Value = resp.json().await.map_err(ApiError::from)?;
            tracing::info!(
                provider = self.name(),
                profile = self.resolved_profile().name,
                request_id = %request_id,
                status,
                server_request_id = server_request_id.as_deref().unwrap_or(""),
                tool_fallback,
                "provider response received"
            );
            Ok(self.parse_response_json(json))
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
        tools: &[crate::tool_executor::ToolInfo],
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + 'static>> {
        let provider = self.clone();
        let system = system.to_string();
        let messages = messages.to_vec();
        let model = model.clone();
        let tools = tools.to_vec();
        Box::pin(async_stream::stream! {
            let url = format!(
                "{}/v1/chat/completions",
                provider.base_url.strip_suffix('/').unwrap_or(&provider.base_url)
            );
            let request_id = Uuid::new_v4().to_string();
            let capabilities = provider.capabilities();
            let tool_fallback = !tools.is_empty() && !capabilities.tool_use;
            provider.log_request_start(
                &request_id,
                &provider.resolve_model_inner(&model),
                true,
                if capabilities.tool_use { tools.len() } else { 0 },
                tool_fallback,
            );
            let body = provider.build_request_body(
                &system,
                &messages,
                &model,
                max_tokens,
                if capabilities.tool_use { &tools } else { &[] },
                &request_id,
                true,
            );

            let req = provider
                .client
                .post(&url)
                .header("content-type", "application/json")
                .header("Authorization", format!("Bearer {}", &provider.api_key));
            let req = provider.apply_standard_headers(req, &request_id);

            let response = req.json(&body).send().await;
            let mut used_fallback = false;

            match response {
                Ok(resp) if resp.status().is_success() => {
                    let server_request_id = resp
                        .headers()
                        .get("x-request-id")
                        .or_else(|| resp.headers().get("request-id"))
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    tracing::info!(
                        provider = provider.name(),
                        profile = provider.resolved_profile().name,
                        request_id = %request_id,
                        status = resp.status().as_u16(),
                        server_request_id = server_request_id.as_deref().unwrap_or(""),
                        tool_fallback,
                        "streaming provider response started"
                    );
                    let stream = parse_openai_sse_stream(resp.bytes_stream());
                    futures::pin_mut!(stream);
                    while let Some(event) = stream.next().await {
                        yield event;
                    }
                }
                Ok(resp) => {
                    used_fallback = true;
                    let server_request_id = resp
                        .headers()
                        .get("x-request-id")
                        .or_else(|| resp.headers().get("request-id"))
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    tracing::debug!(
                        provider = provider.name(),
                        profile = provider.resolved_profile().name,
                        request_id = %request_id,
                        status = resp.status().as_u16(),
                        server_request_id = server_request_id.as_deref().unwrap_or(""),
                        fallback_reason = "streaming_start_failed",
                        "falling back to non-streaming OpenAI-compatible request"
                    );
                }
                Err(err) => {
                    used_fallback = true;
                    tracing::debug!(
                        provider = provider.name(),
                        profile = provider.resolved_profile().name,
                        request_id = %request_id,
                        fallback_reason = "streaming_transport_failed",
                        error = %err,
                        "falling back to non-streaming OpenAI-compatible request"
                    );
                }
            }

            if used_fallback {
                match provider
                    .create_message(&system, &messages, &model, max_tokens, &tools)
                    .await
                {
                    Ok(resp) => {
                        for event in Self::response_to_stream_events(resp) {
                            yield event;
                        }
                    }
                    Err(err) => {
                        yield StreamEvent::Error(crate::provider::provider_error_to_api(err));
                        yield StreamEvent::StreamEnd;
                    }
                }
            }
        })
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

#[derive(Debug, Default)]
struct OpenAiSseState {
    tool_calls: HashMap<usize, OpenAiToolCallState>,
}

#[derive(Debug, Default)]
struct OpenAiToolCallState {
    id: Option<String>,
    name: Option<String>,
    buffered_arguments: String,
    start_emitted: bool,
}

fn parse_openai_sse_stream(
    stream: impl futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
) -> impl futures::Stream<Item = StreamEvent> + Send + 'static {
    async_stream::stream! {
        let mut inner = Box::pin(stream);
        let mut buffer = String::new();
        let mut state = OpenAiSseState::default();

        loop {
            if let Some(pos) = find_sse_message_boundary(&buffer) {
                let message = buffer[..pos].to_string();
                buffer.drain(..=pos);
                for event in parse_openai_sse_message(&message, &mut state) {
                    yield event;
                }
                continue;
            }

            match inner.next().await {
                Some(Ok(chunk)) => buffer.push_str(&String::from_utf8_lossy(&chunk)),
                Some(Err(err)) => {
                    yield StreamEvent::Error(ApiError::from(err));
                    break;
                }
                None => break,
            }
        }

        for event in finish_openai_tool_calls(&mut state) {
            yield event;
        }
        yield StreamEvent::StreamEnd;
    }
}

fn find_sse_message_boundary(buffer: &str) -> Option<usize> {
    if let Some(pos) = buffer.find("\r\n\r\n") {
        return Some(pos + 3);
    }
    buffer
        .as_bytes()
        .windows(2)
        .position(|w| w == b"\n\n")
        .map(|pos| pos + 1)
}

fn parse_openai_sse_message(message: &str, state: &mut OpenAiSseState) -> Vec<StreamEvent> {
    let payload = message
        .lines()
        .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
        .collect::<Vec<_>>()
        .join("\n");
    if payload.is_empty() || payload == "[DONE]" {
        return Vec::new();
    }

    let Ok(value) = serde_json::from_str::<serde_json::Value>(&payload) else {
        return Vec::new();
    };

    let mut events = Vec::new();
    if let Some(choices) = value.get("choices").and_then(serde_json::Value::as_array) {
        for choice in choices {
            if let Some(delta) = choice.get("delta") {
                if let Some(content) = delta.get("content").and_then(serde_json::Value::as_str) {
                    if !content.is_empty() {
                        events.push(StreamEvent::TextDelta(content.to_string()));
                    }
                }
                if let Some(tool_calls) = delta
                    .get("tool_calls")
                    .and_then(serde_json::Value::as_array)
                {
                    for tool_delta in tool_calls {
                        events.extend(handle_openai_tool_delta(tool_delta, state));
                    }
                }
            }

            if choice
                .get("finish_reason")
                .and_then(serde_json::Value::as_str)
                .is_some()
            {
                events.extend(finish_openai_tool_calls(state));
            }
        }
    }

    if let Some(usage) = value.get("usage") {
        events.push(StreamEvent::UsageSummary(
            OpenAiCompatibleProvider::parse_usage(usage),
        ));
    }

    events
}

fn handle_openai_tool_delta(
    tool_delta: &serde_json::Value,
    state: &mut OpenAiSseState,
) -> Vec<StreamEvent> {
    let Some(index) = tool_delta
        .get("index")
        .and_then(serde_json::Value::as_u64)
        .map(|v| v as usize)
    else {
        return Vec::new();
    };

    let tool_state = state.tool_calls.entry(index).or_default();
    if let Some(id) = tool_delta.get("id").and_then(serde_json::Value::as_str) {
        if !id.is_empty() {
            tool_state.id = Some(id.to_string());
        }
    }
    if let Some(name) = tool_delta
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(serde_json::Value::as_str)
    {
        if !name.is_empty() {
            tool_state.name = Some(name.to_string());
        }
    }

    let mut events = Vec::new();
    if !tool_state.start_emitted {
        if let (Some(id), Some(name)) = (tool_state.id.clone(), tool_state.name.clone()) {
            events.push(StreamEvent::ToolCallBlockStart {
                id: id.clone(),
                name,
            });
            tool_state.start_emitted = true;
            if !tool_state.buffered_arguments.is_empty() {
                events.push(StreamEvent::ToolCallInputDelta {
                    id,
                    delta: std::mem::take(&mut tool_state.buffered_arguments),
                });
            }
        }
    }

    if let Some(arguments) = tool_delta
        .get("function")
        .and_then(|f| f.get("arguments"))
        .and_then(serde_json::Value::as_str)
    {
        if !arguments.is_empty() {
            if tool_state.start_emitted {
                if let Some(id) = tool_state.id.clone() {
                    events.push(StreamEvent::ToolCallInputDelta {
                        id,
                        delta: arguments.to_string(),
                    });
                }
            } else {
                tool_state.buffered_arguments.push_str(arguments);
            }
        }
    }

    events
}

fn finish_openai_tool_calls(state: &mut OpenAiSseState) -> Vec<StreamEvent> {
    let mut entries: Vec<_> = state.tool_calls.drain().collect();
    entries.sort_by_key(|(index, _)| *index);

    let mut events = Vec::new();
    for (_, mut tool_state) in entries {
        if !tool_state.start_emitted {
            if let (Some(id), Some(name)) = (tool_state.id.clone(), tool_state.name.clone()) {
                events.push(StreamEvent::ToolCallBlockStart {
                    id: id.clone(),
                    name,
                });
                tool_state.start_emitted = true;
                if !tool_state.buffered_arguments.is_empty() {
                    events.push(StreamEvent::ToolCallInputDelta {
                        id: id.clone(),
                        delta: std::mem::take(&mut tool_state.buffered_arguments),
                    });
                }
            }
        }

        if tool_state.start_emitted {
            if let Some(id) = tool_state.id {
                events.push(StreamEvent::ToolCallBlockEnd {
                    id,
                    input: serde_json::Value::Null,
                });
            }
        }
    }

    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_standard_headers_includes_coderex_headers() {
        let provider = OpenAiCompatibleProvider::new("openai", "https://api.openai.com", "k");
        let req = provider.client.post("https://example.com");
        let req = provider.apply_standard_headers(req, "req-42");
        let built = req.build().expect("request build");
        let headers = built.headers();
        assert!(headers.get("User-Agent").is_some());
        assert_eq!(headers.get("X-CodeRex-Client").unwrap(), "cli");
        assert_eq!(headers.get("X-CodeRex-Request-Id").unwrap(), "req-42");
    }

    #[test]
    fn apply_standard_headers_adds_dashscope_profile_headers() {
        let provider = OpenAiCompatibleProvider::new("openai", "https://example.com", "k")
            .with_provider_profile("dashscope");
        let req = provider.client.post("https://example.com");
        let req = provider.apply_standard_headers(req, "req-42");
        let built = req.build().expect("request build");
        let headers = built.headers();
        assert_eq!(
            headers.get("X-DashScope-UserAgent").unwrap(),
            &OpenAiCompatibleProvider::user_agent()
        );
        assert_eq!(headers.get("X-DashScope-AuthType").unwrap(), "api_key");
        assert_eq!(headers.get("X-DashScope-CacheControl").unwrap(), "enable");
    }

    #[test]
    fn attach_metadata_adds_metadata_object() {
        let provider = OpenAiCompatibleProvider::new("openai", "https://api.openai.com", "k");
        let mut body = serde_json::json!({
            "model": "gpt-4o",
            "messages": []
        });
        provider.attach_metadata(&mut body, "req-99");
        assert!(body.get("metadata").is_some());
    }

    #[test]
    fn parse_openai_sse_message_emits_text_tool_and_usage() {
        let mut state = OpenAiSseState::default();
        let message = r#"data: {"choices":[{"index":0,"delta":{"content":"Hello","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"Read","arguments":"{\"file\":\"src/"}}]},"finish_reason":null}]}

"#;
        let events = parse_openai_sse_message(message, &mut state);
        assert!(matches!(&events[0], StreamEvent::TextDelta(text) if text == "Hello"));
        assert!(
            matches!(&events[1], StreamEvent::ToolCallBlockStart { id, name } if id == "call_1" && name == "Read")
        );
        assert!(
            matches!(&events[2], StreamEvent::ToolCallInputDelta { id, delta } if id == "call_1" && delta == "{\"file\":\"src/")
        );

        let finish = r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"lib.rs\"}"}}],"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":2,"completion_tokens":3}}

"#;
        let events = parse_openai_sse_message(finish, &mut state);
        assert!(
            matches!(&events[0], StreamEvent::ToolCallInputDelta { id, delta } if id == "call_1" && delta == "lib.rs\"}")
        );
        assert!(
            matches!(&events[1], StreamEvent::ToolCallBlockEnd { id, input } if id == "call_1" && input.is_null())
        );
        assert!(
            matches!(&events[2], StreamEvent::UsageSummary(usage) if usage.input_tokens == 2 && usage.output_tokens == 3)
        );
    }

    #[tokio::test]
    async fn parse_openai_sse_stream_handles_fragmented_chunks() {
        let payload = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hel\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n"
        );
        let chunks: Vec<Result<bytes::Bytes, reqwest::Error>> = payload
            .bytes()
            .map(|b| Ok(bytes::Bytes::from(vec![b])))
            .collect();
        let events: Vec<_> = parse_openai_sse_stream(futures::stream::iter(chunks))
            .collect()
            .await;
        assert!(matches!(&events[0], StreamEvent::TextDelta(delta) if delta == "hel"));
        assert!(matches!(&events[1], StreamEvent::TextDelta(delta) if delta == "lo"));
        assert!(matches!(events.last(), Some(StreamEvent::StreamEnd)));
    }
}
